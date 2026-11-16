//! The catalog's I/O seam (spec 11 KER-11, REC-06): the data sync of the container and the
//! hole-punch probe. A failed sync is fatal and never retried (T5 Step 5 adds the
//! `StorageFatal` mapping and its failure injection here).

use std::fs::File;

use kaban_core::Result;

use super::not_yet;

/// Sync `file`'s data for the operation `op`.
///
/// # Errors
/// Always, until T5 Step 5 maps a sync failure to `StorageFatal`: an unsupported-operation
/// `StoreError::Backend`.
#[expect(dead_code, reason = "T5 Step 5 syncs the container through this seam")]
pub(crate) fn sync_data(file: &File, op: &'static str) -> Result<()> {
    let _ = (file, op);
    Err(not_yet("sync_data"))
}

/// Whether keep-size hole punching works on `file`'s filesystem. Without it the catalog zero
/// fills reclaimed ranges and reports `degraded_reclamation`. Not probed yet: reports `false`,
/// the conservative answer, until T5 Step 5.
#[expect(dead_code, reason = "T5 Step 5 probes at catalog open")]
pub(crate) fn probe_hole_punch(file: &File) -> bool {
    let _ = file;
    false
}
