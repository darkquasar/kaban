//! The exclusive maintenance condition (spec 05 SS-12): what lets a purge's erasure claim the
//! index artefacts cleaned rather than "other readers outstanding or unknown". Without IPC the
//! only evidence that no other reader maps the merged-away segments is that no other reader
//! exists at all, established from the kernel's lock tables: the database side is the store's
//! owner lock (every opener of the SQLite store marks its presence there; [`DatabaseHold`] is
//! that lock as the composition root hands it in), the index side is the readers' mark of
//! this module ([`Presence`]): every adapter opened on the directory, in this process or
//! another, with or without a store of its own, holds `.kaban-readers.lock` shared for its
//! lifetime, and the erasure holds it exclusive (T4 review t4-2). A third lock,
//! `.kaban-maintenance.lock`, admits one erasure at a time, which is what makes the erasure's
//! own mark safe to give up while it tries the exclusive one. All are non-blocking and held
//! for the erasure; none is a seam for anything else.
//!
//! ```text
//!   open(dir):   readers mark, shared, for the adapter's lifetime
//!   purge(rids): writer lock => database hold (no other opener) => maintenance lock (one
//!                erasure) => own mark released, readers mark exclusive (no other reader)
//!                => erase (merge, reload, GC, listing) => mark back to shared
//!                => cleaned iff files gone AND established ; else qualified, as T2 left it
//! ```

use std::any::Any;
use std::fs::{File, TryLockError};
use std::path::Path;
use std::time::{Duration, Instant};
use tantivy::directory::error::LockError;
use tantivy::directory::{DirectoryLock, Lock, MmapDirectory};
use tantivy::{Directory, Index};

/// The database side of the condition: `Some(hold)` establishes that this process alone has
/// the database open, for as long as the hold lives; `None` when another process has it (or
/// nothing can tell). The hold is opaque here: the store crate's own guard, boxed by the
/// composition root.
pub trait DatabaseHold: Send + Sync {
    /// Try to establish the hold now, never waiting.
    ///
    /// # Errors
    /// The lock file could not be opened or locked.
    fn try_exclusive(&self) -> kaban_core::Result<Option<Box<dyn Any + Send>>>;
}

/// The lock of a maintenance in the index directory (beside Tantivy's writer lock): one
/// erasure at a time claims the directory.
fn maintenance_lock() -> Lock {
    Lock {
        filepath: ".kaban-maintenance.lock".into(),
        is_blocking: false,
    }
}

/// The readers' mark file in the index directory. A `.lock` name: a pack never copies it.
const READERS_LOCK: &str = ".kaban-readers.lock";

/// How long an opener waits for an erasure that holds the readers' mark exclusive (an
/// erasure: milliseconds) before it is refused rather than opened unmarked.
const MARK_WAIT: Duration = Duration::from_secs(5);

/// One reader's presence on the index directory: a shared `flock` on [`READERS_LOCK`] through
/// a descriptor of its own, held for the adapter's lifetime and dropped by the kernel with it.
/// `flock` is per open file description, so two adapters of one process are two marks, and
/// the erasure's exclusive attempt sees every one of them.
pub(crate) struct Presence {
    file: File,
}

impl Presence {
    /// Mark a reader on `dir` (the file is created when absent), waiting up to [`MARK_WAIT`]
    /// for an erasure holding the mark exclusive.
    ///
    /// # Errors
    /// [`crate::Error::MaintenanceHeld`] when the wait passes; the file could not be opened.
    pub(crate) fn mark(dir: &Path) -> crate::Result<Self> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(dir.join(READERS_LOCK))?;
        shared(&file)?;
        Ok(Self { file })
    }

    /// The exclusive mark in place of this reader's own: `Some` when no other reader is
    /// marked (given back on drop), `None` when one is (this reader's mark is back already).
    /// Only under the maintenance lock: it keeps every other erasure out while this reader is
    /// momentarily unmarked, so nothing can claim the directory in that window.
    fn exclusive(&self) -> crate::Result<Option<ReadersExclusive<'_>>> {
        self.file.unlock()?;
        match self.file.try_lock() {
            Ok(()) => Ok(Some(ReadersExclusive(self))),
            Err(TryLockError::WouldBlock) => {
                shared(&self.file)?;
                Ok(None)
            }
            Err(TryLockError::Error(e)) => {
                let remarked = shared(&self.file);
                Err(remarked.err().unwrap_or_else(|| e.into()))
            }
        }
    }
}

/// The shared mark on `file`, waiting up to [`MARK_WAIT`] for an exclusive holder.
fn shared(file: &File) -> crate::Result<()> {
    let deadline = Instant::now() + MARK_WAIT;
    loop {
        match file.try_lock_shared() {
            Ok(()) => return Ok(()),
            Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(TryLockError::WouldBlock) => return Err(crate::Error::MaintenanceHeld),
            Err(TryLockError::Error(e)) => return Err(e.into()),
        }
    }
}

/// The readers' mark held exclusive by the erasure; back to the reader's shared mark on drop
/// (under the maintenance lock still, so nothing can refuse the shared mark then).
struct ReadersExclusive<'p>(&'p Presence);

impl Drop for ReadersExclusive<'_> {
    fn drop(&mut self) {
        let file = &self.0.file;
        if let Err(error) = file.unlock().and_then(|()| file.lock_shared()) {
            tracing::debug!(%error, "the readers' mark was not given back as shared");
        }
    }
}

/// Empty `dir` for a rebuild (spec 05 SS-09; review t8a-1) without unlinking a lock file
/// another adapter may hold: the writer lock and the readers' mark are taken exclusive,
/// non-blocking, every other entry is removed, and the lock files stay in place (their inodes
/// are what every holder, in this or another process, has locked). `false` when a writer or a
/// reader holds the directory now: nothing is removed, the caller waits or refuses.
///
/// # Errors
/// The directory or a lock file could not be opened, or an entry could not be removed.
pub fn clear_for_rebuild(dir: &Path) -> crate::Result<bool> {
    let directory = MmapDirectory::open(dir).map_err(tantivy::TantivyError::from)?;
    let _writer = match directory.acquire_lock(&tantivy::directory::INDEX_WRITER_LOCK) {
        Ok(lock) => lock,
        Err(LockError::LockBusy) => return Ok(false),
        Err(LockError::IoError(e)) => return Err(crate::Error::Io(std::io::Error::other(e))),
    };
    let readers = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join(READERS_LOCK))?;
    match readers.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => return Ok(false),
        Err(TryLockError::Error(e)) => return Err(e.into()),
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_name().to_string_lossy().ends_with(".lock") {
            continue;
        }
        if entry.file_type()?.is_dir() {
            std::fs::remove_dir_all(entry.path())?;
        } else {
            std::fs::remove_file(entry.path())?;
        }
    }
    Ok(true)
}

/// The established condition: every side held until dropped. Field order is drop order: the
/// readers' mark goes back to shared while the maintenance lock is still held.
pub(crate) struct Exclusive<'p> {
    _readers: ReadersExclusive<'p>,
    _directory: DirectoryLock,
    _database: Box<dyn Any + Send>,
}

/// Establish the condition on `index`'s directory, with this adapter's `presence` on it and
/// `database`'s hold: `None` when any side is held elsewhere (another opener of the database,
/// another erasure, another reader of the directory), when no database hold was wired, or
/// when the index has no directory (then nothing is claimed).
pub(crate) fn establish<'p>(
    index: &Index,
    presence: Option<&'p Presence>,
    database: Option<&dyn DatabaseHold>,
) -> kaban_core::Result<Option<Exclusive<'p>>> {
    let (Some(presence), Some(database)) = (presence, database) else {
        return Ok(None);
    };
    let Some(database) = database.try_exclusive()? else {
        return Ok(None);
    };
    let directory = match index.directory().acquire_lock(&maintenance_lock()) {
        Ok(lock) => lock,
        Err(LockError::LockBusy) => return Ok(None),
        Err(LockError::IoError(e)) => {
            return Err(crate::Error::Io(std::io::Error::other(e)).into());
        }
    };
    let Some(readers) = presence.exclusive()? else {
        return Ok(None);
    };
    Ok(Some(Exclusive {
        _readers: readers,
        _directory: directory,
        _database: database,
    }))
}
