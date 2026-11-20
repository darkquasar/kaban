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
//!
//! Memory (PC-17): the mapping itself is the caller's and uncharged. Everything this module
//! allocates from the packs (each pack's decoded tombstones and fixed metadata, the hidden
//! documents and the tombstone sets they are found with) is reserved on the caller's
//! [`QueryBudget`] before it is allocated, and the charge lives as long as the value.
//!
//! Deadline and cancellation (PC-17): every loop this crate owns over the packs checks the
//! budget per pack and per [`TOMBSTONE_BATCH`] tombstones, a prefix's dictionary scan per
//! batch of terms (`crate::query`), and every Tantivy call a text search, seal or staging
//! makes (an index open, a search, a merge, a segment's finalize) is checked before and
//! after. A single library call is not interruptible (a fuzzy term's dictionary walk runs
//! inside the search): it is bounded by the inputs reserved before it.

use crate::schema::Fields;
use crate::{Error, Result};
use kaban_core::query_budget::{BudgetLease, GrowError, QueryBudget};
use std::collections::HashMap;
use std::convert::Infallible;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
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

/// The format fields of meta.json are identical for every catalog snapshot. Only the
/// segment list comes from the pinned packs.
static META_PREFIX: LazyLock<String> = LazyLock::new(|| {
    let settings = serde_json::json!(IndexSettings::default());
    let schema = serde_json::json!(crate::schema::schema().0);
    format!(r#"{{"index_settings":{settings},"opstamp":0,"schema":{schema},"segments":"#)
});

/// The tombstones a loop of this module visits between two budget checks.
pub(crate) const TOMBSTONE_BATCH: usize = 1024;

/// The fixed metadata one opened pack adds (its entries in the file map and in the
/// synthesized `meta.json`): a pack carries at most six files and one segment.
const PACK_METADATA_BYTES: u64 = 1024;

/// Charge `bytes` on `lease` before the allocation they pay for.
///
/// # Errors
/// The budget's refusal.
pub(crate) fn charge(lease: &mut BudgetLease, bytes: usize) -> Result<()> {
    let bytes = u64::try_from(bytes).map_err(|_| overflow())?;
    lease
        .grow_with(bytes, || Ok::<_, Infallible>(()))
        .map_err(|e| grown(&e))
}

/// An empty vector of exactly `capacity` elements, its allocation charged on `lease` first.
///
/// # Errors
/// The budget's refusal.
pub(crate) fn charged_vec<T>(lease: &mut BudgetLease, capacity: usize) -> Result<Vec<T>> {
    if capacity == 0 || size_of::<T>() == 0 {
        return Ok(Vec::new());
    }
    let bytes = size_of::<T>()
        .checked_mul(capacity)
        .and_then(|b| u64::try_from(b).ok())
        .ok_or_else(overflow)?;
    let align = u64::try_from(align_of::<T>()).map_err(|_| overflow())?;
    let charge = QueryBudget::allocation_charge(bytes, align)?;
    lease
        .grow_with(charge, || Ok::<_, Infallible>(Vec::with_capacity(capacity)))
        .map_err(|e| grown(&e))
}

fn grown(e: &GrowError<Infallible>) -> Error {
    match e {
        GrowError::Budget(refused) => Error::Budget(*refused),
        GrowError::Alloc(never) => match *never {},
    }
}

fn overflow() -> Error {
    Error::Budget(kaban_core::query_budget::BudgetExceeded {
        limit: kaban_core::query_budget::Limit::Memory,
        value: u64::MAX,
    })
}

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
/// The pack's bytes are charged to `budget` before they are allocated, and released when it
/// is returned: the caller owns the pack.
///
/// # Errors
/// A segment file that cannot be read; the budget's refusal.
pub(crate) fn encode(
    tombstones: &[i64],
    segment: Option<&Segment>,
    raw: &dyn Directory,
    budget: &QueryBudget,
) -> Result<Vec<u8>> {
    let mut files = Vec::new();
    if let Some(segment) = segment {
        for (code, (component, _)) in COMPONENTS.iter().enumerate() {
            match raw.open_read(&segment.relative_path(*component)) {
                // the RAM directory's own bytes, shared, not copied
                Ok(file) => files.push((code, file.read_bytes()?)),
                Err(OpenReadError::FileDoesNotExist(_)) => {}
                Err(e) => return Err(Error::Tantivy(e.into())),
            }
        }
    }
    let head = MAGIC.len() + 1 + 4 + tombstones.len() * 8 + 1;
    let body = segment.map_or(0, |_| {
        32 + 4 + 1 + files.iter().map(|(_, b)| 1 + 8 + b.len()).sum::<usize>()
    });
    let mut lease = budget.reserve(0)?;
    let mut out = charged_vec(&mut lease, head + body)?;
    out.extend_from_slice(MAGIC);
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

#[cfg(test)]
thread_local! {
    /// Called on every pack a loop of this module visits: the tests count the visits and
    /// cancel through it.
    pub(crate) static ON_PACK: std::cell::RefCell<Option<Box<dyn FnMut()>>> =
        const { std::cell::RefCell::new(None) };
}

/// One pack visited by a loop of this module: the budget's deadline and cancellation are
/// checked first.
fn visit(budget: &QueryBudget) -> Result<()> {
    budget.check()?;
    #[cfg(test)]
    ON_PACK.with_borrow_mut(|hook| {
        if let Some(hook) = hook {
            hook();
        }
    });
    Ok(())
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

/// `bytes` decoded, its tombstones charged on `lease` before they are allocated.
fn decode(bytes: &OwnedBytes, budget: &QueryBudget, lease: &mut BudgetLease) -> Result<Pack> {
    let mut c = Cursor { bytes, at: 0 };
    if &c.array::<4>()? != MAGIC || c.array::<1>()? != [VERSION] {
        return Err(Error::Corrupt("text pack"));
    }
    let n = u32::from_le_bytes(c.array()?);
    // a count past the pack's bytes fails below before it outgrows this capacity
    let mut tombstones = charged_vec(lease, usize::try_from(n).unwrap_or(0).min(bytes.len() / 8))?;
    for (i, _) in (0..n).enumerate() {
        if i % TOMBSTONE_BATCH == 0 {
            budget.check()?;
        }
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
    /// The charge of the decoded tombstones and the packs' metadata.
    _charge: BudgetLease,
}

/// Open `packs` (oldest first) as one read-only index with the crate's schema, its decoded
/// tombstones and metadata charged on `budget` for as long as the opened index lives.
///
/// # Errors
/// A malformed pack ([`Error::Corrupt`]), the budget's refusal, or an index Tantivy refuses
/// to open.
pub(crate) fn open(packs: &[OwnedBytes], budget: &QueryBudget) -> Result<Opened> {
    let mut lease = budget.reserve(0)?;
    charge(
        &mut lease,
        usize::try_from(PACK_METADATA_BYTES)
            .unwrap_or(usize::MAX)
            .saturating_mul(packs.len()),
    )?;
    let mut files = HashMap::with_capacity(packs.len().saturating_mul(COMPONENTS.len()));
    let mut segments = Vec::with_capacity(packs.len());
    let mut tombstones = charged_vec(&mut lease, packs.len())?;
    let mut positions = HashMap::with_capacity(packs.len());
    for (position, bytes) in packs.iter().enumerate() {
        visit(budget)?;
        let pack = decode(bytes, budget, &mut lease)?;
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
    let mut meta = META_PREFIX.as_bytes().to_vec();
    serde_json::to_writer(&mut meta, &segments)?;
    meta.push(b'}');
    budget.check()?;
    let index = Index::open(PackDirectory {
        files: Arc::new(files),
        meta: Arc::new(meta),
    })?;
    budget.check()?;
    crate::analyzers::register(&index);
    Ok(Opened {
        index,
        tombstones,
        positions,
        _charge: lease,
    })
}

impl Opened {
    /// The number of tombstones of every pack.
    fn tombstone_count(&self) -> usize {
        self.tombstones.iter().map(Vec::len).sum()
    }

    /// The union of every pack's tombstones, sorted, charged on `lease` before it is
    /// allocated: what a merge of these packs carries on to hide the documents of older packs.
    ///
    /// # Errors
    /// The budget's refusal.
    pub(crate) fn all_tombstones(&self, lease: &mut BudgetLease) -> Result<Vec<i64>> {
        let mut all = charged_vec(lease, self.tombstone_count())?;
        all.extend(self.tombstones.iter().flatten().copied());
        all.sort_unstable();
        all.dedup();
        Ok(all)
    }

    /// The documents of each segment of `searcher` (by segment ordinal) that a newer pack's
    /// tombstone hides, or whose rid is in `drop`, with the charge that pays for them and for
    /// the tombstone set they were found with (reserved on `budget` before each allocation).
    ///
    /// # Errors
    /// A segment's rid postings cannot be read; the budget's refusal.
    pub(crate) fn hidden(
        &self,
        searcher: &Searcher,
        fields: &Fields,
        drop: &[i64],
        budget: &QueryBudget,
    ) -> Result<(Vec<Vec<DocId>>, BudgetLease)> {
        let mut lease = budget.reserve(0)?;
        let readers = searcher.segment_readers();
        // the segment ordinal of each pack position
        let mut by_position: Vec<Option<usize>> = charged_vec(&mut lease, self.tombstones.len())?;
        by_position.resize(self.tombstones.len(), None);
        for (ord, reader) in readers.iter().enumerate() {
            if let Some(&position) = self.positions.get(&reader.segment_id()) {
                by_position[position] = Some(ord);
            }
        }
        let mut out: Vec<Vec<DocId>> = charged_vec(&mut lease, readers.len())?;
        out.resize_with(readers.len(), Vec::new);
        // the tombstones of every pack newer than the one being visited, sorted
        let mut newer: Vec<i64> = charged_vec(
            &mut lease,
            drop.len().saturating_add(self.tombstone_count()),
        )?;
        newer.extend_from_slice(drop);
        newer.sort_unstable();
        newer.dedup();
        for position in (0..self.tombstones.len()).rev() {
            visit(budget)?;
            if let Some(ord) = by_position[position] {
                let inverted = readers[ord].inverted_index(fields.rid)?;
                for (i, rid) in newer.iter().enumerate() {
                    if i % TOMBSTONE_BATCH == 0 {
                        budget.check()?;
                    }
                    let term = Term::from_field_i64(fields.rid, *rid);
                    if let Some(mut postings) =
                        inverted.read_postings(&term, IndexRecordOption::Basic)?
                    {
                        let docs = usize::try_from(postings.doc_freq()).unwrap_or(usize::MAX);
                        charge(&mut lease, docs.saturating_mul(size_of::<DocId>()))?;
                        out[ord].reserve_exact(docs);
                        let mut doc = postings.doc();
                        while doc != TERMINATED {
                            out[ord].push(doc);
                            doc = postings.advance();
                        }
                    }
                }
                out[ord].sort_unstable();
            }
            // within the capacity charged above: never reallocates
            newer.extend_from_slice(&self.tombstones[position]);
            newer.sort_unstable();
            newer.dedup();
        }
        Ok((out, lease))
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
