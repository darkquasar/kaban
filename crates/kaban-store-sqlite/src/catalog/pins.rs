//! The pin locks of `kaban.kbx.pins/` (spec 11 PC-8, KER-03): one file per locked generation,
//! held with `flock(LOCK_SH)` by every reader pinned at or above it, judged live by its lock
//! alone, never by its existence.
//!
//! ```text
//!   reader:  open-or-create pins/<g> => flock(SH|NB) => pins/<g> still names that file?
//!              no => retry          yes => held until the pin ends => close => tidy pins/<g>
//!   tidy:    open pins/<g> => flock(EX|NB) granted and still that file => unlink
//!              refused => a reader holds it: leave it
//!   scan:    every pins/<n> => flock(EX|NB) refused => live at n; granted => stale: unlink
//! ```
//!
//! A `flock` belongs to an open file description, so a tidier or a scan in the reader's own
//! process is refused exactly as one in another process. A file is unlinked only under its
//! exclusive lock, and a reader checks after locking that the path still names the file it
//! locked, so no reader holds a lock a scan cannot see. A crashed holder's lock dies with its
//! process: its file stays behind unlocked, and the next tidy or scan finds it stale and
//! removes it, so it never blocks reclamation.
//!
//! The directory also holds the two read-admission locks of the scheduled checkpoint
//! ([`admit`], `super::checkpoint`).

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use kaban_core::Result;
use kaban_core::query_budget::QueryBudget;
use rustix::fs::{FlockOperation, Mode, OFlags};
use rustix::io::Errno;

use super::kbx::failed;
use super::pin::budget_exceeded;

/// How long a reader waits before it tries again for a pin lock a tidier holds for an instant.
const LOCK_RETRY: Duration = Duration::from_micros(200);

/// One reader's shared lock on `pins/<generation>`: released, and its file tidied, on drop.
#[derive(Debug)]
pub(crate) struct PinLock {
    /// The locked file; `None` only while the lock is being dropped.
    file: Option<File>,
    /// The pins directory (shared with the catalog: no allocation per lock).
    pins: Arc<Path>,
    /// The locked generation.
    generation: u64,
}

impl PinLock {
    /// Lock `pins/<generation>` shared, creating its file where it is missing, within
    /// `budget`'s deadline. A lock a tidier holds for an instant, or a file a tidier unlinked
    /// before this lock, is retried.
    ///
    /// # Errors
    /// `ContainerError` for a pins directory or lock file that cannot be opened or locked;
    /// the budget's error once it is cancelled or past its deadline.
    pub(crate) fn acquire(pins: &Arc<Path>, generation: u64, budget: &QueryBudget) -> Result<Self> {
        let path = lock_path(pins, generation);
        loop {
            let file =
                open(&path, OFlags::CREATE).map_err(|e| failed("open the pin lock", &path, e))?;
            match rustix::fs::flock(&file, FlockOperation::NonBlockingLockShared) {
                Ok(())
                    if names(&path, &file)
                        .map_err(|e| failed("check the pin lock", &path, e))? =>
                {
                    return Ok(Self {
                        file: Some(file),
                        pins: Arc::clone(pins),
                        generation,
                    });
                }
                // unlinked by a tidier before this lock, or held by one right now
                Ok(()) | Err(Errno::WOULDBLOCK) => {}
                Err(e) => return Err(failed("lock the pin lock", &path, e)),
            }
            drop(file);
            budget.check().map_err(budget_exceeded)?;
            std::thread::sleep(LOCK_RETRY);
        }
    }
}

impl Drop for PinLock {
    fn drop(&mut self) {
        // closing the file releases the lock; then the file goes unless another reader holds it
        drop(self.file.take());
        let _ = tidy(&lock_path(&self.pins, self.generation));
    }
}

/// The lowest generation a live pin lock in `pins` holds, `None` when none is live. Stale
/// lock files (no holder: released, or left by a crashed process) are removed on the way.
///
/// # Errors
/// `ContainerError` for a pins directory or lock file that cannot be read, opened or locked.
pub(crate) fn lowest_live(pins: &Path) -> Result<Option<u64>> {
    let entries = std::fs::read_dir(pins).map_err(|e| failed("scan", pins, e))?;
    let mut lowest: Option<u64> = None;
    for entry in entries {
        let entry = entry.map_err(|e| failed("scan", pins, e))?;
        let Some(generation) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<u64>().ok())
        else {
            continue;
        };
        let path = entry.path();
        if live(&path).map_err(|e| failed("probe the pin lock", &path, e))? {
            lowest = Some(lowest.map_or(generation, |l| l.min(generation)));
        }
    }
    Ok(lowest)
}

/// The read-admission lock in the pins directory (`super::checkpoint`): shared by every reader
/// while its read transaction is open, exclusive for a draining checkpoint. Its name is no
/// generation, so the pin scans skip it; it is never unlinked.
pub(crate) const ADMISSION: &str = "admission";

/// The gate before [`ADMISSION`]: a reader passes it shared for an instant, a draining
/// checkpoint holds it exclusive, so no new reader is admitted while admitted ones drain.
pub(crate) const GATE: &str = "admission.gate";

/// The marker of a waiting checkpoint (cross-process): created, locked exclusive and renamed
/// into place by the tick before it waits for the gate, removed when it ends. A reader waits
/// while a marker exists whose lock is held; one whose lock is free was left by a tick that
/// died, and is ignored.
pub(crate) const PENDING: &str = "admission.pending";

/// Names this process's marker files while they are being put in place.
static MARKERS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// A checkpoint's registration as waiting ([`PENDING`]): removed (if it is still the one in
/// place) and unlocked on drop.
#[derive(Debug)]
pub(crate) struct Pending {
    file: Option<File>,
    path: PathBuf,
}

impl Pending {
    /// Register a waiting checkpoint in `pins`: the marker is locked before it gets its name,
    /// so no reader ever finds it unlocked while the tick lives.
    ///
    /// # Errors
    /// `ContainerError` for a marker that cannot be created, locked or renamed.
    pub(crate) fn register(pins: &Path) -> Result<Self> {
        let n = MARKERS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let staged = pins.join(format!("{PENDING}.{}.{n}", std::process::id()));
        let file = open(&staged, OFlags::CREATE)
            .map_err(|e| failed("create the checkpoint marker", &staged, e))?;
        rustix::fs::flock(&file, FlockOperation::NonBlockingLockExclusive)
            .map_err(|e| failed("lock the checkpoint marker", &staged, e))?;
        let path = pins.join(PENDING);
        std::fs::rename(&staged, &path)
            .map_err(|e| failed("place the checkpoint marker", &path, e))?;
        Ok(Self {
            file: Some(file),
            path,
        })
    }
}

impl Drop for Pending {
    fn drop(&mut self) {
        if let Some(file) = self.file.take() {
            if names(&self.path, &file).unwrap_or(false) {
                let _ = rustix::fs::unlink(&self.path);
            }
            drop(file);
        }
    }
}

/// Whether a live checkpoint waits to drain the readers ([`PENDING`]).
fn checkpoint_waiting(pins: &Path) -> Result<bool> {
    let path = pins.join(PENDING);
    let file = match open(&path, OFlags::empty()) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(failed("open the checkpoint marker", &path, e)),
    };
    match rustix::fs::flock(&file, FlockOperation::NonBlockingLockShared) {
        // free: left by a tick that died
        Ok(()) => Ok(false),
        Err(Errno::WOULDBLOCK) => Ok(true),
        Err(e) => Err(failed("probe the checkpoint marker", &path, e)),
    }
}

/// A reader's admission (shared [`ADMISSION`]), released on drop.
#[derive(Debug)]
pub(crate) struct Admitted {
    _lock: File,
}

/// Admit a reader of the catalog in `pins` within `budget`'s deadline: wait while a checkpoint
/// is registered as waiting ([`PENDING`]), pass the gate shared, take the admission lock
/// shared, leave the gate. A checkpoint waits and drains for at most its own short budget.
///
/// # Errors
/// `ContainerError` for a lock file that cannot be opened or locked; the budget's error once it
/// is cancelled or past its deadline.
pub(crate) fn admit(pins: &Path, budget: &QueryBudget) -> Result<Admitted> {
    let wait = || budget.check().map(|()| true).map_err(budget_exceeded);
    while checkpoint_waiting(pins)? {
        wait()?;
        std::thread::sleep(LOCK_RETRY);
    }
    let shared = FlockOperation::NonBlockingLockShared;
    let gate = lock_while(&pins.join(GATE), shared, wait)?;
    let admission = match gate {
        Some(_) => lock_while(&pins.join(ADMISSION), shared, wait)?,
        None => None,
    };
    drop(gate);
    admission
        .map(|lock| Admitted { _lock: lock })
        .ok_or_else(|| {
            failed(
                "admit a reader in",
                pins,
                io::Error::from(io::ErrorKind::TimedOut),
            )
        })
}

/// Lock `path` (created owner-only where missing) with the non-blocking `op`, trying again
/// every [`LOCK_RETRY`] while `wait` says to; `None` once it says not to.
///
/// # Errors
/// `ContainerError` for a lock file that cannot be opened or locked; `wait`'s errors.
pub(crate) fn lock_while(
    path: &Path,
    op: FlockOperation,
    wait: impl Fn() -> Result<bool>,
) -> Result<Option<File>> {
    let file =
        open(path, OFlags::CREATE).map_err(|e| failed("open the admission lock", path, e))?;
    loop {
        match rustix::fs::flock(&file, op) {
            Ok(()) => return Ok(Some(file)),
            Err(Errno::WOULDBLOCK) => {}
            Err(e) => return Err(failed("lock the admission lock", path, e)),
        }
        if !wait()? {
            return Ok(None);
        }
        std::thread::sleep(LOCK_RETRY);
    }
}

/// The lock file of `generation` in `pins`.
fn lock_path(pins: &Path, generation: u64) -> PathBuf {
    pins.join(generation.to_string())
}

/// Open the lock file `path` read-only (`extra`: `CREATE` to create it owner-only), never
/// through a symlink.
fn open(path: &Path, extra: OFlags) -> io::Result<File> {
    let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | extra;
    Ok(File::from(rustix::fs::open(
        path,
        flags,
        Mode::RUSR | Mode::WUSR,
    )?))
}

/// Whether `path` still names the file `file` has open.
fn names(path: &Path, file: &File) -> io::Result<bool> {
    let held = rustix::fs::fstat(file)?;
    match rustix::fs::lstat(path) {
        Ok(now) => Ok(now.st_dev == held.st_dev && now.st_ino == held.st_ino),
        Err(Errno::NOENT) => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// Whether a reader holds the lock file `path`; one nobody holds is unlinked (under its
/// exclusive lock, and only while `path` still names it).
fn live(path: &Path) -> io::Result<bool> {
    let file = match open(path, OFlags::empty()) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    match rustix::fs::flock(&file, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => {}
        Err(Errno::WOULDBLOCK) => return Ok(true),
        Err(e) => return Err(e.into()),
    }
    if names(path, &file)? {
        match rustix::fs::unlink(path) {
            Ok(()) | Err(Errno::NOENT) => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(false)
}

/// Remove the lock file `path` unless a reader holds it.
fn tidy(path: &Path) -> io::Result<()> {
    live(path).map(|_| ())
}
