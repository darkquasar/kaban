//! The text index of one catalog snapshot as a read-only Tantivy [`Directory`] (spec 11 T7):
//! every sealed tier and every pending sediment segment is one *pack*, and the directory
//! serves the segment files of all of them under one synthesized `meta.json`.
//!
//! A pack is the raw files of at most one Tantivy segment (footers included) and the identity
//! tombstones of the commits it holds. A tombstone hides the documents of its rid in every
//! OLDER pack (the order is the tiers' chain order, then the sediment's commit order), never
//! in its own pack or a newer one, so an update (tombstone and new document of one rid in one
//! commit) keeps its new document and a delete during a merge never resurrects.
//!
//! ```text
//!   pack = "KBXT" | version u8 | n u32 | n x rid i64 | has_segment u8
//!          [ uuid (32 hex) | max_doc u32 | files u8 | files x (code u8 | len u64 | bytes) ]
//! ```
//!
//! The directory holds only byte slices of the packs it was given: a tier's slices come from
//! the caller's mapping of the container, which the caller's pin protects while it lives.

use crate::schema::Fields;
use crate::{Error, Result};
use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tantivy::directory::error::{DeleteError, LockError, OpenReadError, OpenWriteError};
use tantivy::directory::{
    Directory, DirectoryLock, FileHandle, Lock, OwnedBytes, WatchCallback, WatchHandle, WritePtr,
};
use tantivy::index::{SegmentComponent, SegmentId};
use tantivy::schema::IndexRecordOption;
use tantivy::{DocId, DocSet, Index, IndexSettings, Searcher, Segment, TERMINATED, Term};

const MAGIC: &[u8; 4] = b"KBXT";
const VERSION: u8 = 1;
const META: &str = "meta.json";

/// The segment files a pack carries, by code (the index into this table).
const COMPONENTS: [(SegmentComponent, &str); 6] = [
    (SegmentComponent::Postings, ".idx"),
    (SegmentComponent::Positions, ".pos"),
    (SegmentComponent::FastFields, ".fast"),
    (SegmentComponent::FieldNorms, ".fieldnorm"),
    (SegmentComponent::Terms, ".term"),
    (SegmentComponent::Store, ".store"),
];

/// The pack of `tombstones` and the files of `segment` (none: a pack of tombstones only), read
/// raw from `raw`, the directory under the segment's index, so every file keeps its footer.
///
/// # Errors
/// A segment file that cannot be read.
pub(crate) fn encode(
    tombstones: &[i64],
    segment: Option<&Segment>,
    raw: &dyn Directory,
) -> Result<Vec<u8>> {
    let mut out = MAGIC.to_vec();
    out.push(VERSION);
    out.extend(len_u32(tombstones.len())?.to_le_bytes());
    for rid in tombstones {
        out.extend(rid.to_le_bytes());
    }
    let Some(segment) = segment else {
        out.push(0);
        return Ok(out);
    };
    out.push(1);
    out.extend(segment.id().uuid_string().as_bytes());
    out.extend(segment.meta().max_doc().to_le_bytes());
    let mut files = Vec::new();
    for (code, (component, _)) in COMPONENTS.iter().enumerate() {
        match raw.open_read(&segment.relative_path(*component)) {
            Ok(file) => files.push((code, file.read_bytes()?)),
            Err(OpenReadError::FileDoesNotExist(_)) => {}
            Err(e) => return Err(Error::Tantivy(e.into())),
        }
    }
    out.push(u8::try_from(files.len()).map_err(|_| Error::Corrupt("text pack"))?);
    for (code, bytes) in files {
        out.push(u8::try_from(code).map_err(|_| Error::Corrupt("text pack"))?);
        out.extend((bytes.len() as u64).to_le_bytes());
        out.extend(bytes.as_slice());
    }
    Ok(out)
}

/// Whether `pack` carries a segment (a pack of a commit that only deleted carries none).
#[must_use]
pub fn has_segment(pack: &[u8]) -> bool {
    let head = MAGIC.len() + 1;
    let Some(n) = pack
        .get(head..head + 4)
        .and_then(|b| b.try_into().ok())
        .map(u32::from_le_bytes)
    else {
        return false;
    };
    let flag = usize::try_from(n)
        .ok()
        .and_then(|n| n.checked_mul(8))
        .and_then(|t| t.checked_add(head + 4));
    flag.and_then(|at| pack.get(at)) == Some(&1)
}

fn len_u32(n: usize) -> Result<u32> {
    u32::try_from(n).map_err(|_| Error::Corrupt("text pack"))
}

/// One decoded pack: its tombstones and its segment, if any.
struct Pack {
    tombstones: Vec<i64>,
    segment: Option<PackSegment>,
}

struct PackSegment {
    id: SegmentId,
    max_doc: u32,
    files: Vec<(usize, OwnedBytes)>,
}

/// A reading cursor over one pack's bytes; every slice it hands out shares them.
struct Cursor<'a> {
    bytes: &'a OwnedBytes,
    at: usize,
}

impl Cursor<'_> {
    fn take(&mut self, n: usize) -> Result<OwnedBytes> {
        let end = self
            .at
            .checked_add(n)
            .filter(|end| *end <= self.bytes.len())
            .ok_or(Error::Corrupt("text pack"))?;
        let out = self.bytes.slice(self.at..end);
        self.at = end;
        Ok(out)
    }
    fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        self.take(N)?
            .as_slice()
            .try_into()
            .map_err(|_| Error::Corrupt("text pack"))
    }
}

fn decode(bytes: &OwnedBytes) -> Result<Pack> {
    let mut c = Cursor { bytes, at: 0 };
    if &c.array::<4>()? != MAGIC || c.array::<1>()? != [VERSION] {
        return Err(Error::Corrupt("text pack"));
    }
    let n = u32::from_le_bytes(c.array()?);
    let mut tombstones = Vec::with_capacity(usize::try_from(n).unwrap_or(0).min(bytes.len() / 8));
    for _ in 0..n {
        tombstones.push(i64::from_le_bytes(c.array()?));
    }
    if c.array::<1>()? == [0] {
        return Ok(Pack {
            tombstones,
            segment: None,
        });
    }
    let uuid = c.take(32)?;
    let id = std::str::from_utf8(uuid.as_slice())
        .ok()
        .and_then(|s| SegmentId::from_uuid_string(s).ok())
        .ok_or(Error::Corrupt("text pack"))?;
    let max_doc = u32::from_le_bytes(c.array()?);
    let mut files = Vec::new();
    for _ in 0..c.array::<1>()?[0] {
        let code = usize::from(c.array::<1>()?[0]);
        if code >= COMPONENTS.len() {
            return Err(Error::Corrupt("text pack"));
        }
        let len = usize::try_from(u64::from_le_bytes(c.array()?))
            .map_err(|_| Error::Corrupt("text pack"))?;
        files.push((code, c.take(len)?));
    }
    Ok(Pack {
        tombstones,
        segment: Some(PackSegment { id, max_doc, files }),
    })
}

/// The packs of one snapshot opened as one index, in pack order.
pub(crate) struct Opened {
    pub(crate) index: Index,
    /// Each pack's tombstones, in pack order.
    tombstones: Vec<Vec<i64>>,
    /// The pack each segment came from.
    positions: HashMap<SegmentId, usize>,
}

/// Open `packs` (oldest first) as one read-only index with the crate's schema.
///
/// # Errors
/// A malformed pack ([`Error::Corrupt`]) or an index Tantivy refuses to open.
pub(crate) fn open(packs: &[OwnedBytes]) -> Result<Opened> {
    let mut files = HashMap::new();
    let mut segments = Vec::new();
    let mut tombstones = Vec::with_capacity(packs.len());
    let mut positions = HashMap::new();
    for (position, bytes) in packs.iter().enumerate() {
        let pack = decode(bytes)?;
        tombstones.push(pack.tombstones);
        if let Some(segment) = pack.segment {
            for (code, file) in segment.files {
                let name = format!("{}{}", segment.id.uuid_string(), COMPONENTS[code].1);
                files.insert(PathBuf::from(name), file);
            }
            segments.push(serde_json::json!({
                "segment_id": segment.id,
                "max_doc": segment.max_doc,
                "deletes": null,
            }));
            positions.insert(segment.id, position);
        }
    }
    let meta = serde_json::to_vec(&serde_json::json!({
        "index_settings": IndexSettings::default(),
        "segments": segments,
        "schema": crate::schema::schema().0,
        "opstamp": 0,
    }))?;
    let index = Index::open(PackDirectory {
        files: Arc::new(files),
        meta: Arc::new(meta),
    })?;
    crate::analyzers::register(&index);
    Ok(Opened {
        index,
        tombstones,
        positions,
    })
}

impl Opened {
    /// The union of every pack's tombstones, sorted: what a merge of these packs carries on to
    /// hide the documents of older packs.
    pub(crate) fn all_tombstones(&self) -> Vec<i64> {
        let mut all: Vec<i64> = self.tombstones.iter().flatten().copied().collect();
        all.sort_unstable();
        all.dedup();
        all
    }

    /// The documents of each segment of `searcher` (by segment ordinal) that a newer pack's
    /// tombstone hides, or whose rid is in `drop`.
    ///
    /// # Errors
    /// A segment's rid postings cannot be read.
    pub(crate) fn hidden(
        &self,
        searcher: &Searcher,
        fields: &Fields,
        drop: &[i64],
    ) -> Result<Vec<Vec<DocId>>> {
        let by_position: HashMap<usize, usize> = searcher
            .segment_readers()
            .iter()
            .enumerate()
            .filter_map(|(ord, r)| self.positions.get(&r.segment_id()).map(|p| (*p, ord)))
            .collect();
        let mut out = vec![Vec::new(); searcher.segment_readers().len()];
        // the tombstones of every pack newer than the one being visited
        let mut newer: HashSet<i64> = drop.iter().copied().collect();
        for position in (0..self.tombstones.len()).rev() {
            if let Some(&ord) = by_position.get(&position) {
                let inverted = searcher.segment_readers()[ord].inverted_index(fields.rid)?;
                for rid in &newer {
                    let term = Term::from_field_i64(fields.rid, *rid);
                    if let Some(mut postings) =
                        inverted.read_postings(&term, IndexRecordOption::Basic)?
                    {
                        let mut doc = postings.doc();
                        while doc != TERMINATED {
                            out[ord].push(doc);
                            doc = postings.advance();
                        }
                    }
                }
                out[ord].sort_unstable();
            }
            newer.extend(self.tombstones[position].iter().copied());
        }
        Ok(out)
    }
}

/// See the module doc: the files of the packs and the synthesized `meta.json`. Every write,
/// delete and lock is refused or elided, as for a captured index.
#[derive(Debug, Clone)]
struct PackDirectory {
    files: Arc<HashMap<PathBuf, OwnedBytes>>,
    meta: Arc<Vec<u8>>,
}

fn read_only(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::ReadOnlyFilesystem,
        format!("the text snapshot is immutable: {}", path.display()),
    )
}

impl Directory for PackDirectory {
    fn get_file_handle(
        &self,
        path: &Path,
    ) -> std::result::Result<Arc<dyn FileHandle>, OpenReadError> {
        self.files
            .get(path)
            .map(|f| Arc::new(f.clone()) as Arc<dyn FileHandle>)
            .ok_or_else(|| OpenReadError::FileDoesNotExist(path.to_path_buf()))
    }

    fn delete(&self, path: &Path) -> std::result::Result<(), DeleteError> {
        Err(DeleteError::IoError {
            io_error: Arc::new(read_only(path)),
            filepath: path.to_path_buf(),
        })
    }

    fn exists(&self, path: &Path) -> std::result::Result<bool, OpenReadError> {
        Ok(path == Path::new(META) || self.files.contains_key(path))
    }

    fn open_write(&self, path: &Path) -> std::result::Result<WritePtr, OpenWriteError> {
        Err(OpenWriteError::wrap_io_error(
            read_only(path),
            path.to_path_buf(),
        ))
    }

    fn atomic_read(&self, path: &Path) -> std::result::Result<Vec<u8>, OpenReadError> {
        if path == Path::new(META) {
            return Ok(self.meta.as_ref().clone());
        }
        Err(OpenReadError::FileDoesNotExist(path.to_path_buf()))
    }

    fn atomic_write(&self, path: &Path, _data: &[u8]) -> io::Result<()> {
        Err(read_only(path))
    }

    fn sync_directory(&self) -> io::Result<()> {
        Ok(())
    }

    fn acquire_lock(&self, _lock: &Lock) -> std::result::Result<DirectoryLock, LockError> {
        Ok(DirectoryLock::from(Box::new(())))
    }

    fn watch(&self, _watch_callback: WatchCallback) -> tantivy::Result<WatchHandle> {
        Ok(WatchHandle::empty())
    }
}
