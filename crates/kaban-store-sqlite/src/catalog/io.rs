//! The catalog's I/O seam (spec 11 KER-11, REC-06): the data sync of the container and the
//! hole-punch probe. A failed sync is fatal and never retried (T5 Step 5 adds the
//! `StorageFatal` mapping and its failure injection here).

use std::fmt;
use std::fs::File;
use std::io;

use kaban_core::{BackendSource, Result, StoreError};

/// Sync `file`'s data for the operation `op` (`fdatasync`), once: the one call site of every
/// container sync, which T5 Step 5 makes injectable and maps to `StorageFatal`.
///
/// # Errors
/// [`SyncFailed`] with the I/O error.
pub(crate) fn sync_data(file: &File, op: &'static str) -> Result<()> {
    file.sync_data().map_err(|source| StoreError::Backend {
        source: BackendSource::new(SyncFailed { op, source }),
    })
}

/// A failed data sync of the container (KER-11: never retried in place).
#[derive(Debug)]
pub(crate) struct SyncFailed {
    /// The operation that synced.
    pub(crate) op: &'static str,
    /// The cause.
    pub(crate) source: io::Error,
}

impl fmt::Display for SyncFailed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "catalog: {} could not sync the container", self.op)
    }
}

impl std::error::Error for SyncFailed {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

/// Whether keep-size hole punching works on `file`'s filesystem. Without it the catalog zero
/// fills reclaimed ranges and reports `degraded_reclamation`. Not probed yet: reports `false`,
/// the conservative answer, until T5 Step 5.
#[expect(dead_code, reason = "T5 Step 5 probes at catalog open")]
pub(crate) fn probe_hole_punch(file: &File) -> bool {
    let _ = file;
    false
}
