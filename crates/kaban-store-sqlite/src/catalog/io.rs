//! The catalog's I/O seam (spec 11 KER-11, KER-01): every sync of a catalog artefact, the
//! hole-punch probe and the release of a reclaimed container range go through here. A failed
//! sync is [`StoreError::StorageFatal`] naming its operation: attempted once, never retried in
//! place, and nothing it served is published.
//!
//! ```text
//!   append_extent:   pwrite => sync_data(kaban.kbx)                       [append_extent]
//!   create (open):   kaban.kbx => sync_file             [create_container]
//!                    kaban.kbx.alloc => sync_file       [create_alloc_lock]
//!                    kaban.kbx.pins/ => sync_dir(parent) [create_dir_entries]
//!   probe (open):    flock(EX) kaban.kbx.alloc => punch(keep size) [len, len + 4 KiB)
//!                    => Punch | ZeroFill (degraded_reclamation)
//!   reclaim_ranges:  release(range) per range (punch, or zero-fill) => sync_data(kaban.kbx)
//!                    => Released { punched, zero_filled }                 [reclaim_ranges]
//! ```
//!
//! Test builds (`cfg(test)`, `failpoints`) consult [`faults`] at every sync and at the probe:
//! a test fails the syncs of one operation, or makes the probe find no hole punching, for the
//! catalogs under one directory, and counts the syncs attempted.

use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;
use std::path::Path;

use kaban_core::{Result, StoreError};
use rustix::fs::{FlockOperation, Mode, OFlags};

use super::kbx::{alloc_path, failed, open_existing};

/// The sync of an appended extent's bytes.
pub(crate) const APPEND: &str = "append_extent";
/// The sync of a created (or found) container file.
pub(crate) const CREATE_CONTAINER: &str = "create_container";
/// The sync of a created (or found) allocation lock file.
pub(crate) const CREATE_ALLOC_LOCK: &str = "create_alloc_lock";
/// The sync of the directory holding the catalog's artefacts: their directory entries.
pub(crate) const CREATE_DIR_ENTRIES: &str = "create_dir_entries";
/// The sync after releasing reclaimed ranges.
pub(crate) const RECLAIM: &str = "reclaim_ranges";

/// The range the probe punches beyond the container's end.
const PROBE_LEN: u64 = 4096;
/// The zero buffer of a zero-fill, written repeatedly over a longer range.
const ZERO_CHUNK: usize = 64 * 1024;

/// How the catalog releases a reclaimed range (KER-11).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Reclamation {
    /// Keep-size hole punching: the blocks are freed, the range reads as zeros.
    Punch,
    /// The file system refuses hole punching: the range is overwritten with zeros, its blocks
    /// stay allocated (degraded reclamation: no space is reclaimed).
    ZeroFill,
}

/// What one release of reclaimed ranges did ([`super::Catalog::reclaim_ranges`], KER-11): the
/// bytes whose blocks a keep-size punch freed, and the bytes overwritten with zeros instead
/// (they read as zeros, but their blocks stay allocated: never counted as reclaimed space).
/// A mixed outcome (both nonzero) is a release whose punches the file system began refusing
/// part way; its zero-filled part is degraded reclamation like a whole zero-filled one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Released {
    /// Bytes released by punching: their space is reclaimed.
    pub(crate) punched: u64,
    /// Bytes released by zero-filling: no space is reclaimed.
    pub(crate) zero_filled: u64,
}

impl Released {
    /// This outcome and `other`'s together.
    #[must_use]
    pub(crate) fn and(self, other: Self) -> Self {
        Self {
            punched: self.punched.saturating_add(other.punched),
            zero_filled: self.zero_filled.saturating_add(other.zero_filled),
        }
    }
}

/// Sync `file`'s data (`fdatasync`) for the operation `op`, once.
///
/// # Errors
/// [`StoreError::StorageFatal`] with the sync's error.
pub(crate) fn sync_data(file: &File, path: &Path, op: &'static str) -> Result<()> {
    attempt(path, op, || file.sync_data())
}

/// Sync `file`'s data and metadata (`fsync`: a created file's inode) for `op`, once.
///
/// # Errors
/// [`StoreError::StorageFatal`] with the sync's error.
pub(crate) fn sync_file(file: &File, path: &Path, op: &'static str) -> Result<()> {
    attempt(path, op, || file.sync_all())
}

/// Sync the directory `dir` (`fsync`: the entries created in it) for `op`, once.
///
/// # Errors
/// [`StoreError::StorageFatal`] with the sync's error; a directory that cannot be opened as
/// `kbx::ContainerError`.
pub(crate) fn sync_dir(dir: &Path, op: &'static str) -> Result<()> {
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let fd = rustix::fs::open(dir, flags, Mode::empty()).map_err(|e| failed("open", dir, e))?;
    attempt(dir, op, || rustix::fs::fsync(&fd).map_err(io::Error::from))
}

/// Run one sync of `op` on `path` (or its injected outcome); its failure is fatal.
fn attempt(path: &Path, op: &'static str, sync: impl FnOnce() -> io::Result<()>) -> Result<()> {
    match faults::injected(path, op) {
        None => sync(),
        Some(faults::Injected::Fail(e)) => Err(e),
        Some(faults::Injected::Skip) => Ok(()),
    }
    .map_err(|source| StoreError::StorageFatal { op, source })
}

/// Probe whether the file system of the container `kbx` punches keep-size holes: punch the
/// range just past its end, under the allocation lock, so no appender can have reserved it
/// and no live byte lies there (the file's length is kept). Costs one `flock`, one `fstat` and
/// one `fallocate`. Any refusal of the punch means zero-fill.
///
/// # Errors
/// `kbx::ContainerError` when the container or its allocation lock cannot be opened or locked.
pub(crate) fn probe_hole_punch(kbx: &Path) -> Result<Reclamation> {
    let file = open_existing(kbx).map_err(|e| failed("open", kbx, e))?;
    let alloc_path = alloc_path(kbx);
    let alloc = open_existing(&alloc_path).map_err(|e| failed("open", &alloc_path, e))?;
    rustix::fs::flock(&alloc, FlockOperation::LockExclusive)
        .map_err(|e| failed("lock", &alloc_path, e))?;
    let end = file
        .metadata()
        .map_err(|e| failed("measure", kbx, e))?
        .len();
    let mode = if faults::punch_refused(kbx) || punch(&file, end, PROBE_LEN).is_err() {
        Reclamation::ZeroFill
    } else {
        Reclamation::Punch
    };
    // closing `alloc` releases the lock
    drop(alloc);
    Ok(mode)
}

/// Release the `len` bytes at `offset` of the container `file` (at `kbx`) by `mode`, clamped
/// to the container's length: the container neither shrinks nor grows, and the range reads as
/// zeros afterwards. A punch the file system refuses falls back to a zero-fill, and the outcome
/// says which happened. Not synced: the caller syncs once after its ranges ([`RECLAIM`]).
///
/// # Errors
/// `kbx::ContainerError` for a failed measure, punch or write.
pub(crate) fn release(
    file: &File,
    kbx: &Path,
    offset: u64,
    len: u64,
    mode: Reclamation,
) -> Result<Released> {
    let size = file
        .metadata()
        .map_err(|e| failed("measure", kbx, e))?
        .len();
    let end = offset.saturating_add(len).min(size);
    let Some(len) = end.checked_sub(offset).filter(|&n| n > 0) else {
        return Ok(Released::default());
    };
    if mode == Reclamation::Punch {
        let punched = if faults::release_punch_refused(kbx) {
            Err(rustix::io::Errno::OPNOTSUPP)
        } else {
            punch(file, offset, len)
        };
        match punched {
            Ok(()) => {
                return Ok(Released {
                    punched: len,
                    zero_filled: 0,
                });
            }
            Err(e) if refused(e) => {}
            Err(e) => return Err(failed("punch a reclaimed range of", kbx, e)),
        }
    }
    zero_fill(file, offset, len).map_err(|e| failed("zero a reclaimed range of", kbx, e))?;
    Ok(Released {
        punched: 0,
        zero_filled: len,
    })
}

/// Overwrite the `len` bytes at `offset` of `file` with zeros.
fn zero_fill(file: &File, offset: u64, len: u64) -> io::Result<()> {
    let zeros = vec![0_u8; usize::try_from(len).map_or(ZERO_CHUNK, |n| n.min(ZERO_CHUNK))];
    let end = offset + len;
    let mut at = offset;
    while at < end {
        let n = zeros
            .len()
            .min(usize::try_from(end - at).unwrap_or(usize::MAX));
        file.write_all_at(&zeros[..n], at)?;
        at += n as u64;
    }
    Ok(())
}

/// Whether a punch failed because the file system does not punch (fall back to zero-fill).
fn refused(e: rustix::io::Errno) -> bool {
    e == rustix::io::Errno::OPNOTSUPP || e == rustix::io::Errno::NOSYS
}

/// Punch a keep-size hole of `len` bytes at `offset` of `file`.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn punch(file: &File, offset: u64, len: u64) -> rustix::io::Result<()> {
    use rustix::fs::FallocateFlags;
    rustix::fs::fallocate(
        file,
        FallocateFlags::PUNCH_HOLE | FallocateFlags::KEEP_SIZE,
        offset,
        len,
    )
}

/// No keep-size hole punching outside Linux: reclamation zero-fills there.
#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn punch(file: &File, offset: u64, len: u64) -> rustix::io::Result<()> {
    let _ = (file, offset, len);
    Err(rustix::io::Errno::OPNOTSUPP)
}

/// The I/O faults a test injects for the catalogs under a directory (test builds only): failed
/// syncs by operation, syncs counted but not performed, a probe that finds no hole punching,
/// and a count of the syncs attempted.
#[cfg(any(test, feature = "failpoints"))]
pub(crate) mod faults {
    use std::collections::HashMap;
    use std::io;
    use std::path::{Path, PathBuf};
    use std::sync::{Mutex, PoisonError};

    /// What a test injected in place of one sync.
    pub(crate) enum Injected {
        /// The sync fails with this error.
        Fail(io::Error),
        /// The sync is counted and not performed.
        Skip,
    }

    /// The faults of one directory.
    struct Faults {
        dir: PathBuf,
        failing: Vec<&'static str>,
        skipping: bool,
        no_punch: bool,
        no_release_punch: bool,
        no_full_sync: bool,
        calls: HashMap<&'static str, u64>,
    }

    /// Every directory a test injects under (process-wide: a catalog is opened inside a
    /// store's open, before a test could reach the catalog).
    static REGISTRY: Mutex<Vec<Faults>> = Mutex::new(Vec::new());

    /// Run `f` on the faults of the directory holding `path`, if a test injects there.
    fn with<T>(path: &Path, f: impl FnOnce(&mut Faults) -> T) -> Option<T> {
        let mut registry = REGISTRY.lock().unwrap_or_else(PoisonError::into_inner);
        registry
            .iter_mut()
            .find(|faults| path.starts_with(&faults.dir))
            .map(f)
    }

    /// Count a sync of `op` on `path`; what replaces it, if anything (a failure wins).
    pub(super) fn injected(path: &Path, op: &'static str) -> Option<Injected> {
        with(path, |faults| {
            *faults.calls.entry(op).or_default() += 1;
            if faults.failing.contains(&op) {
                Some(Injected::Fail(io::Error::from(rustix::io::Errno::IO)))
            } else {
                faults.skipping.then_some(Injected::Skip)
            }
        })
        .flatten()
    }

    /// Whether the probe of the container `kbx` must find no hole punching.
    pub(super) fn punch_refused(kbx: &Path) -> bool {
        with(kbx, |faults| faults.no_punch).unwrap_or(false)
    }

    /// Whether a release in the container `kbx` must find its punch refused (as a file system
    /// that stops punching after the open's probe).
    pub(super) fn release_punch_refused(kbx: &Path) -> bool {
        with(kbx, |faults| faults.no_release_punch).unwrap_or(false)
    }

    /// Start injecting under `dir` (replacing an earlier registration of it).
    pub(crate) fn install(dir: &Path) {
        let mut registry = REGISTRY.lock().unwrap_or_else(PoisonError::into_inner);
        registry.retain(|faults| faults.dir != dir);
        registry.push(Faults {
            dir: dir.to_path_buf(),
            failing: Vec::new(),
            skipping: false,
            no_punch: false,
            no_release_punch: false,
            no_full_sync: false,
            calls: HashMap::new(),
        });
    }
    /// Stop injecting under `dir`.
    pub(crate) fn remove(dir: &Path) {
        REGISTRY
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|faults| faults.dir != dir);
    }
    /// Fail every later sync of `op` under `dir`.
    pub(crate) fn fail_sync(dir: &Path, op: &'static str) {
        with(dir, |faults| faults.failing.push(op));
    }
    /// Let every later sync under `dir` reach the file system again.
    pub(crate) fn heal(dir: &Path) {
        with(dir, |faults| faults.failing.clear());
    }
    /// Count every later sync under `dir` without performing it.
    pub(crate) fn skip_syncs(dir: &Path) {
        with(dir, |faults| faults.skipping = true);
    }
    /// Make the hole-punch probe under `dir` find no hole punching.
    pub(crate) fn no_punch(dir: &Path) {
        with(dir, |faults| faults.no_punch = true);
    }
    /// Make every later release under `dir` find its punch refused.
    pub(crate) fn no_release_punch(dir: &Path) {
        with(dir, |faults| faults.no_release_punch = true);
    }
    /// Make the full-sync probe under `dir` find the full sync refused.
    pub(crate) fn no_full_sync(dir: &Path) {
        with(dir, |faults| faults.no_full_sync = true);
    }
    /// How many syncs of `op` under `dir` were attempted.
    pub(crate) fn sync_calls(dir: &Path, op: &str) -> u64 {
        with(dir, |faults| faults.calls.get(op).copied().unwrap_or(0)).unwrap_or(0)
    }
}

/// The I/O faults are a test-build hook (see the test-build module): nothing is injected.
#[cfg(not(any(test, feature = "failpoints")))]
mod faults {
    use std::io;
    use std::path::Path;

    /// What a test injects in place of one sync (never constructed in this build).
    pub(super) enum Injected {
        Fail(io::Error),
        Skip,
    }

    #[inline]
    pub(super) fn injected(path: &Path, op: &'static str) -> Option<Injected> {
        let _ = (path, op);
        None
    }

    #[inline]
    pub(super) fn punch_refused(kbx: &Path) -> bool {
        let _ = kbx;
        false
    }

    #[inline]
    pub(super) fn release_punch_refused(kbx: &Path) -> bool {
        let _ = kbx;
        false
    }
}
