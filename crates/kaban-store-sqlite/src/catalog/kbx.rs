//! The extent container `kaban.kbx` beside the database (spec 11 KER-01, KER-11): append-only,
//! never shrunk, reclaimed by keep-size hole punching; `kaban.kbx.alloc` serialises range
//! reservations and `kaban.kbx.pins/` holds the readers' pin locks (`super::pins`).
//!
//! ```text
//!   append:  flock(EX) kaban.kbx.alloc => end = len(kbx), grow to end + n => unlock
//!            => pwrite(end, bytes) => fdatasync (super::io) => Unpublished { end, n }
//!   read:    Mapping (shared by a catalog's pins) => re-mapped when a span lies beyond it
//! ```

use std::fmt;
use std::fs::File;
use std::io;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use kaban_core::{BackendSource, Result, StoreError};
use rustix::fs::{FlockOperation, Mode, OFlags};
use sha2::{Digest, Sha256};
use tantivy::directory::{Directory, MmapDirectory, OwnedBytes};

use super::class::ExtentClass;
use super::io as catalog_io;
use super::pins::MaintenanceHold;

/// The container's file name.
pub(crate) const KBX_FILE: &str = "kaban.kbx";
/// The allocation lock's file name.
pub(crate) const ALLOC_FILE: &str = "kaban.kbx.alloc";
/// The pins directory's name.
pub(crate) const PINS_DIR: &str = "kaban.kbx.pins";

/// The container path beside `db_path`.
pub(crate) fn kbx_path(db_path: &Path) -> PathBuf {
    beside(db_path, KBX_FILE)
}

/// The allocation lock path beside `db_path`.
pub(crate) fn alloc_path(db_path: &Path) -> PathBuf {
    beside(db_path, ALLOC_FILE)
}

/// The pins directory beside `db_path`.
pub(crate) fn pins_dir(db_path: &Path) -> PathBuf {
    beside(db_path, PINS_DIR)
}

fn beside(db_path: &Path, name: &str) -> PathBuf {
    db_path.with_file_name(name)
}

/// Create the container, the allocation lock and the pins directory in the directory of
/// `beside` (the database or the container) where they are missing, owner-only (core-spec
/// S5), and make them durable: each file is synced, then the directory holding their entries
/// (spec 11 KER-11). An existing one is kept as it is and synced like a created one, so a
/// later open completes the durability of an open whose sync failed. A symlink or an entry of
/// the wrong type is refused.
///
/// ```text
///   kaban.kbx => fsync [create_container] => kaban.kbx.alloc => fsync [create_alloc_lock]
///   => kaban.kbx.pins/ => fsync(parent directory) [create_dir_entries]
/// ```
///
/// # Errors
/// `StoreError::FileMode` with the I/O error of a creation; `StoreError::StorageFatal` for a
/// failed sync (the open is refused, nothing is half admitted: a later open syncs again).
pub(crate) fn create_artefacts(beside: &Path) -> Result<()> {
    let kbx = kbx_path(beside);
    let container = create_file(&kbx).map_err(StoreError::FileMode)?;
    catalog_io::sync_file(&container, &kbx, catalog_io::CREATE_CONTAINER)?;
    let alloc = alloc_path(beside);
    let lock = create_file(&alloc).map_err(StoreError::FileMode)?;
    catalog_io::sync_file(&lock, &alloc, catalog_io::CREATE_ALLOC_LOCK)?;
    create_dir(&pins_dir(beside)).map_err(StoreError::FileMode)?;
    let dir = match kbx.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => Path::new("."),
    };
    catalog_io::sync_dir(dir, catalog_io::CREATE_DIR_ENTRIES)
}

fn create_file(path: &Path) -> io::Result<File> {
    let flags = OFlags::CREATE | OFlags::RDWR | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let fd = rustix::fs::open(path, flags, Mode::RUSR | Mode::WUSR)?;
    let stat = rustix::fs::fstat(&fd)?;
    if rustix::fs::FileType::from_raw_mode(stat.st_mode) != rustix::fs::FileType::RegularFile {
        return Err(wrong_type(path, "a regular file"));
    }
    // exact owner-only mode whatever the umask
    rustix::fs::fchmod(&fd, Mode::RUSR | Mode::WUSR)?;
    Ok(File::from(fd))
}

pub(super) fn create_dir(path: &Path) -> io::Result<()> {
    match rustix::fs::mkdir(path, Mode::RWXU) {
        Ok(()) => {
            // mkdir is filtered by umask. Restore owner write permission before a
            // recovery creates its maintenance lock, without following a replacement
            // symlink or changing the mode of an existing directory.
            let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
            let dir = rustix::fs::open(path, flags, Mode::empty())?;
            rustix::fs::fchmod(&dir, Mode::RWXU)?;
        }
        Err(rustix::io::Errno::EXIST) => {}
        Err(e) => return Err(e.into()),
    }
    if !std::fs::symlink_metadata(path)?.is_dir() {
        return Err(wrong_type(path, "a directory"));
    }
    Ok(())
}

fn wrong_type(path: &Path, expected: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("{} is not {expected}", path.display()),
    )
}

/// The identity of one container file: its device and inode. An [`Unpublished`] range carries
/// the identity of the container it was appended to, and a catalog publishes only ranges of its
/// own container (END-MAJ-01).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ContainerId {
    dev: u64,
    ino: u64,
}

impl ContainerId {
    /// The identity of the open container `file`.
    fn of(file: &File) -> io::Result<Self> {
        Ok(Self::from_metadata(&file.metadata()?))
    }

    /// The identity of the container at `kbx` (never through a symlink).
    ///
    /// # Errors
    /// The I/O error of reading its metadata (an absent container among them).
    pub(crate) fn at(kbx: &Path) -> io::Result<Self> {
        Ok(Self::from_metadata(&std::fs::symlink_metadata(kbx)?))
    }

    fn from_metadata(meta: &std::fs::Metadata) -> Self {
        Self {
            dev: meta.dev(),
            ino: meta.ino(),
        }
    }
}

/// Bytes written and synced to the container but not yet named by any generation: the one
/// owner of their range until a commit publishes them ([`super::commit::Txn::publish`]
/// consumes it). It is made only by an append, is not `Clone` and names the container it was
/// appended to, so a range is published at most once and only by its container's catalog.
/// A token whose publishing commit rolls back is gone with it: its range stays unnamed (a leak
/// that T6's verify reports and its recovery reclaims, never published again). It carries the
/// SHA-256 of the bytes, computed by the append that had them, and a maintenance hold
/// (`super::pins::MAINTENANCE`) until the commit that publishes it ends, so recovery never
/// takes its range for a leak.
#[derive(Debug)]
#[must_use = "an unpublished extent is garbage until a commit publishes it"]
pub(crate) struct Unpublished {
    class: ExtentClass,
    offset: u64,
    len: u64,
    container: ContainerId,
    sha256: [u8; 32],
    hold: Option<Arc<MaintenanceHold>>,
}

/// What a publication takes from an [`Unpublished`] token.
#[derive(Debug)]
pub(crate) struct Parts {
    pub(crate) class: ExtentClass,
    pub(crate) offset: u64,
    pub(crate) len: u64,
    pub(crate) container: ContainerId,
    pub(crate) sha256: [u8; 32],
    /// The maintenance hold, kept by the publishing commit until it ends.
    pub(crate) hold: Option<Arc<MaintenanceHold>>,
}

impl Unpublished {
    /// The container's range, its class, the container and the bytes' hash, consuming the
    /// token: the publication's view of it.
    pub(crate) fn into_parts(self) -> Parts {
        Parts {
            class: self.class,
            offset: self.offset,
            len: self.len,
            container: self.container,
            sha256: self.sha256,
            hold: self.hold,
        }
    }

    /// Divide this range into consecutive tokens, one per part of `bytes` (what was appended,
    /// in order), consuming it: every byte keeps one owner and each token the hash of its
    /// part. `None` when the parts do not sum to the range's length.
    #[cfg(any(test, feature = "failpoints"))]
    pub(crate) fn split(self, bytes: &[Vec<u8>]) -> Option<Vec<Self>> {
        let total = bytes
            .iter()
            .try_fold(0_u64, |sum, part| sum.checked_add(part.len() as u64))?;
        if total != self.len {
            return None;
        }
        let mut offset = self.offset;
        Some(
            bytes
                .iter()
                .map(|part| {
                    let len = part.len() as u64;
                    let token = Self {
                        class: self.class,
                        offset,
                        len,
                        container: self.container,
                        sha256: Sha256::digest(part).into(),
                        hold: self.hold.clone(),
                    };
                    offset += len;
                    token
                })
                .collect(),
        )
    }
}

/// A failed operation on a catalog artefact: what was done to which path, and why.
#[derive(Debug)]
pub(crate) struct ContainerError {
    /// What was being done.
    pub(crate) op: &'static str,
    /// The artefact.
    pub(crate) path: PathBuf,
    /// The cause.
    pub(crate) source: io::Error,
}

impl fmt::Display for ContainerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "catalog: {} {}", self.op, self.path.display())
    }
}

impl std::error::Error for ContainerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

/// `op` on `path` failed with `source`, as a store error.
pub(crate) fn failed(op: &'static str, path: &Path, source: impl Into<io::Error>) -> StoreError {
    StoreError::Backend {
        source: BackendSource::new(ContainerError {
            op,
            path: path.to_path_buf(),
            source: source.into(),
        }),
    }
}

/// Open the existing artefact `path` for reading and writing (never created here, never
/// through a symlink).
pub(crate) fn open_existing(path: &Path) -> io::Result<File> {
    let flags = OFlags::RDWR | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    Ok(File::from(rustix::fs::open(path, flags, Mode::empty())?))
}

/// Append `bytes` to the container `kbx` as an extent of `class`: reserve its range under a
/// short exclusive `flock` on the allocation lock beside it, then write and sync the bytes
/// outside that lock. The container only grows.
///
/// # Errors
/// [`ContainerError`] for a failed open, lock, growth or write; `StoreError::StorageFatal`
/// for a failed sync ([`super::io::sync_data`]: attempted once, and the range stays unnamed).
pub(crate) fn append(
    kbx: &Path,
    class: ExtentClass,
    bytes: &[u8],
    hold: Option<Arc<MaintenanceHold>>,
) -> Result<Unpublished> {
    let len = u64::try_from(bytes.len())
        .map_err(|e| failed("size an extent for", kbx, io::Error::other(e)))?;
    let file = open_existing(kbx).map_err(|e| failed("open", kbx, e))?;
    let offset = reserve(kbx, &file, len)?;
    file.write_all_at(bytes, offset)
        .map_err(|e| failed("write an extent to", kbx, e))?;
    catalog_io::sync_data(&file, kbx, catalog_io::APPEND)?;
    let container = ContainerId::of(&file).map_err(|e| failed("identify", kbx, e))?;
    Ok(Unpublished {
        class,
        offset,
        len,
        container,
        sha256: Sha256::digest(bytes).into(),
        hold,
    })
}

/// Reserve `len` bytes at the end of the container `file`: under the allocation lock, grow it
/// by `len` and return where the range starts. The lock is released when this returns.
fn reserve(kbx: &Path, file: &File, len: u64) -> Result<u64> {
    let alloc_path = alloc_path(kbx);
    let alloc = open_existing(&alloc_path).map_err(|e| failed("open", &alloc_path, e))?;
    rustix::fs::flock(&alloc, FlockOperation::LockExclusive)
        .map_err(|e| failed("lock", &alloc_path, e))?;
    let end = file
        .metadata()
        .map_err(|e| failed("measure", kbx, e))?
        .len();
    let grown = end.checked_add(len).ok_or_else(|| {
        failed(
            "grow",
            kbx,
            io::Error::other("the container would exceed 2^64 bytes"),
        )
    })?;
    if grown > end {
        file.set_len(grown).map_err(|e| failed("grow", kbx, e))?;
    }
    // closing `alloc` releases the lock
    Ok(end)
}

/// The container's mapping as the pins of one catalog share it: one whole-file mapping of
/// `kaban.kbx` through tantivy's `MmapDirectory`, replaced by a fresh one when a span lies
/// beyond it (PR-24: the directory caches a mapping per path and the container only grows).
/// A replaced mapping lives on while a slice of it does.
#[derive(Debug, Default)]
pub(crate) struct Mapping(Mutex<Option<OwnedBytes>>);

impl Mapping {
    /// The `len` bytes at `offset` of the container `kbx`, bounds-checked against the file.
    ///
    /// # Errors
    /// [`ContainerError`] when the container cannot be mapped or the span lies beyond it.
    pub(crate) fn span(&self, kbx: &Path, offset: u64, len: u64) -> Result<OwnedBytes> {
        let beyond = || {
            failed(
                "read a span beyond the end of",
                kbx,
                io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    format!("{len} bytes at offset {offset}"),
                ),
            )
        };
        let start = usize::try_from(offset).map_err(|_| beyond())?;
        let end = usize::try_from(len)
            .ok()
            .and_then(|len| start.checked_add(len))
            .ok_or_else(beyond)?;
        let mut mapped = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let current = match mapped.take() {
            Some(m) if m.len() >= end => m,
            // none yet, or the span lies beyond it: map the container as it is now
            _ => map(kbx)?,
        };
        let span = (end <= current.len()).then(|| current.slice(start..end));
        *mapped = Some(current);
        span.ok_or_else(beyond)
    }
}

/// A fresh whole-file mapping of the container `kbx` (a fresh directory: its cache holds no
/// older, shorter mapping).
fn map(kbx: &Path) -> Result<OwnedBytes> {
    let (Some(dir), Some(name)) = (kbx.parent(), kbx.file_name()) else {
        return Err(failed(
            "map",
            kbx,
            io::Error::new(io::ErrorKind::NotFound, "the catalog has no container"),
        ));
    };
    let directory =
        MmapDirectory::open(dir).map_err(|e| failed("map", kbx, io::Error::other(e)))?;
    directory
        .open_read(Path::new(name))
        .map_err(|e| failed("map", kbx, io::Error::other(e)))?
        .read_bytes()
        .map_err(|e| failed("map", kbx, e))
}
