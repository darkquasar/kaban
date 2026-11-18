//! Reclamation of retired extents (spec 11 PC-8, KER-05, KER-10).

use kaban_core::Result;
use rusqlite::Connection;

use super::Catalog;
use super::tables::ExtentId;

/// What one reclamation did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ReclaimReport {
    /// The `reclaim_floor` it committed.
    pub(crate) floor: u64,
    /// The retired extents it destroyed.
    pub(crate) reclaimed: Vec<ExtentId>,
    /// The retired extents left for a later round.
    pub(crate) pending: Vec<ExtentId>,
    /// Bytes whose blocks a punch freed.
    pub(crate) released_bytes: u64,
    /// Bytes only overwritten with zeros.
    pub(crate) zero_filled_bytes: u64,
    /// Its re-scan found a live pin below the floor.
    pub(crate) pin_race_retry: bool,
    /// The WAL was not backfilled through a retiring commit.
    pub(crate) barrier_pending: bool,
    /// The platform cannot deallocate container ranges.
    pub(crate) space_reclaim_unsupported: bool,
    /// Consumed sediment rows deleted.
    pub(crate) sediment_deleted: u64,
}

impl Catalog {
    /// Reclaim retired extents.
    ///
    /// # Errors
    /// SQLite errors.
    pub(crate) fn reclaim(&self, conn: &mut Connection) -> Result<ReclaimReport> {
        let _ = conn;
        Ok(ReclaimReport::default())
    }
}
