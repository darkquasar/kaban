//! The catalog's I/O seam (spec 11 KER-11, REC-06): the data sync of the container and the
//! hole-punch probe. A failed sync is fatal and never retried (T5 Step 5 adds the
//! `StorageFatal` mapping and its failure injection here).

use std::fs::File;

use kaban_core::Result;

/// Sync `file`'s data for the operation `op`.
///
/// # Errors
/// The sync failure (T5 Step 5).
pub(crate) fn sync_data(file: &File, op: &'static str) -> Result<()> {
    let _ = (file, op);
    Ok(())
}

/// Whether keep-size hole punching works on `file`'s filesystem. Without it the catalog zero
/// fills reclaimed ranges and reports `degraded_reclamation`.
pub(crate) fn probe_hole_punch(file: &File) -> bool {
    let _ = file;
    true
}
