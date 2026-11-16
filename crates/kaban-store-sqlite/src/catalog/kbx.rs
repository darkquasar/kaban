//! The extent container `kaban.kbx` beside the database (spec 11 KER-01, KER-11): append-only,
//! never shrunk, reclaimed by keep-size hole punching; `kaban.kbx.alloc` serialises range
//! reservations and `kaban.kbx.pins/` holds the readers' pin locks.

use std::path::{Path, PathBuf};

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
