//! The extent container `kaban.kbx` beside the database (spec 11 KER-01, KER-11): append-only,
//! never shrunk, reclaimed by keep-size hole punching; `kaban.kbx.alloc` serialises range
//! reservations and `kaban.kbx.pins/` holds the readers' pin locks.

use std::io;
use std::path::{Path, PathBuf};

use kaban_core::{Result, StoreError};
use rustix::fs::{Mode, OFlags};

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
#[expect(
    dead_code,
    reason = "T5 Steps 4-5 deliver the container I/O and the pin protocol that use it"
)]
pub(crate) struct Unpublished {
    /// The class the bytes belong to.
    pub(crate) class: ExtentClass,
    /// Where they were written.
    pub(crate) location: ExtentLocation,
}
