//! The scheduled checkpoint (spec 11 KER-06, PC-9 item 3).

use kaban_core::Result;
use rusqlite::Connection;

use super::Catalog;

/// What one checkpoint tick did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct CheckpointReport {
    /// The tick drained the admitted readers.
    pub(crate) drained: bool,
    /// Every WAL frame is in the main file.
    pub(crate) backfilled: bool,
    /// Frames in the WAL.
    pub(crate) wal_frames: u64,
    /// Frames checkpointed.
    pub(crate) checkpointed_frames: u64,
    /// A full fsync was refused.
    pub(crate) degraded_durability: bool,
}

impl Catalog {
    /// Run one checkpoint.
    ///
    /// # Errors
    /// SQLite errors.
    pub(crate) fn checkpoint_tick(&self, conn: &Connection) -> Result<CheckpointReport> {
        let _ = conn;
        Ok(CheckpointReport::default())
    }
}
