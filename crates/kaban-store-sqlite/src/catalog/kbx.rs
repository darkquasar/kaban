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
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use kaban_core::{BackendSource, Result, StoreError};
use rustix::fs::{FlockOperation, Mode, OFlags};
use tantivy::directory::{Directory, MmapDirectory, OwnedBytes};

use super::class::{ExtentClass, ExtentLocation};

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
/// S5), and open none of them for longer: an existing one is kept as it is. A symlink or an
/// entry of the wrong type is refused.
///
/// # Errors
/// `StoreError::FileMode` with the I/O error.
pub(crate) fn create_artefacts(beside: &Path) -> Result<()> {
    create_file(&kbx_path(beside)).map_err(StoreError::FileMode)?;
    create_file(&alloc_path(beside)).map_err(StoreError::FileMode)?;
    create_dir(&pins_dir(beside)).map_err(StoreError::FileMode)
}

fn create_file(path: &Path) -> io::Result<()> {
    let flags = OFlags::CREATE | OFlags::RDWR | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let fd = rustix::fs::open(path, flags, Mode::RUSR | Mode::WUSR)?;
    let stat = rustix::fs::fstat(&fd)?;
    if rustix::fs::FileType::from_raw_mode(stat.st_mode) != rustix::fs::FileType::RegularFile {
        return Err(wrong_type(path, "a regular file"));
    }
    // exact owner-only mode whatever the umask
    rustix::fs::fchmod(&fd, Mode::RUSR | Mode::WUSR)?;
    Ok(())
}

fn create_dir(path: &Path) -> io::Result<()> {
    match rustix::fs::mkdir(path, Mode::RWXU) {
        Ok(()) | Err(rustix::io::Errno::EXIST) => {}
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

/// Bytes written and synced to the container but not yet named by any generation: a later
/// commit publishes them, or reclamation finds them unreferenced.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use = "an unpublished extent is garbage until a commit publishes it"]
pub(crate) struct Unpublished {
    /// The class the bytes belong to.
    pub(crate) class: ExtentClass,
    /// Where they were written.
    pub(crate) location: ExtentLocation,
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
/// [`ContainerError`] for a failed open, lock, growth or write; the sync's error
/// ([`super::io::sync_data`]).
pub(crate) fn append(kbx: &Path, class: ExtentClass, bytes: &[u8]) -> Result<Unpublished> {
    let len = u64::try_from(bytes.len())
        .map_err(|e| failed("size an extent for", kbx, io::Error::other(e)))?;
    let file = open_existing(kbx).map_err(|e| failed("open", kbx, e))?;
    let offset = reserve(kbx, &file, len)?;
    file.write_all_at(bytes, offset)
        .map_err(|e| failed("write an extent to", kbx, e))?;
    super::io::sync_data(&file, "append_extent")?;
    Ok(Unpublished {
        class,
        location: ExtentLocation::Kbx { offset, len },
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
