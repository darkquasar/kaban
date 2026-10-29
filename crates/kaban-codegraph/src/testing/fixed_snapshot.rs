//! [`FixedSnapshot`]: resolve one final-named snapshot file wherever it lies, `exact: true`,
//! with the same admission as the production path (`open_ro`, `validate_for_attach`) and no
//! cache (every resolve is a fresh open, nothing is retained).

use std::path::PathBuf;
use std::sync::Arc;

use crate::discover::{HeadInput, NoSnapshot, snapshot_ref};
use crate::error::SnapshotError;
use crate::provider::{Resolved, SnapshotProvider};
use crate::reader::CodeGraphReader;
use crate::sqlite::{FINAL_SUFFIX, SqliteSnapshot, validate_for_attach};

/// A provider pinned to one snapshot file; the head is not consulted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FixedSnapshot {
    /// The snapshot file, `<commit40>.cg.db` in any readable directory.
    pub path: PathBuf,
}

impl FixedSnapshot {
    /// A provider over `path`.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

impl SnapshotProvider for FixedSnapshot {
    fn resolve(&self, _repo_identity: &str, _head: &HeadInput) -> Result<Resolved, NoSnapshot> {
        let snapshot = SqliteSnapshot::open_ro(&self.path).map_err(reason)?;
        let commit = self
            .path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_suffix(FINAL_SUFFIX))
            .ok_or(NoSnapshot::CorruptOnly)?;
        let meta = snapshot.meta().map_err(|_| NoSnapshot::CorruptOnly)?;
        let mut r#ref = snapshot_ref(&meta, snapshot.schema_version(), commit)
            .ok_or(NoSnapshot::CorruptOnly)?;
        r#ref.exact = true;
        let attach = validate_for_attach(&self.path).map_err(reason)?;
        Ok(Resolved {
            snapshot: Arc::new(snapshot),
            r#ref,
            attach,
        })
    }
}

/// The `NoSnapshot` projection of one file's admission failure (cg-join-v1 §4).
fn reason(e: SnapshotError) -> NoSnapshot {
    match e {
        SnapshotError::SnapshotTooNew { .. } => NoSnapshot::TooNewOnly,
        SnapshotError::Io { class: "not_found" } => NoSnapshot::NoFile,
        _ => NoSnapshot::CorruptOnly,
    }
}
