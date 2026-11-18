//! Reclamation of retired extents (spec 11 PC-8, KER-05, KER-10, PC-9).
//!
//! ```text
//!   barrier:  latest generation L => checkpoint_tick => WAL wholly backfilled?
//!               yes => every generation <= L is in the main file (durable = L)
//!               no  => durable = 0: nothing retired may be destroyed this round
//!   phase 1:  scan pins/ for the lowest live P => commit reclaim_floor = max(F, min(P, latest))
//!   phase 2:  re-scan => a live pin below the floor: pin_race_retry, destroy nothing
//!   destroy:  dead_since <= floor and dead_since <= durable => release the range (punch or
//!             zero-fill), synced, then drop the rows; everything else retired stays pending
//!   tidy:     delete consumed sediment rows in chunks, one short commit each
//! ```
//!
//! A reader that locks after the re-scan began its transaction after the floor commit, so it
//! sees the floor and retries (`Catalog::acquire`); one that locked before is seen by the
//! re-scan. The barrier is a checked condition (KER-10): the checkpoint's own result row says
//! the WAL is backfilled, so a power loss cannot bring back a catalog that names destroyed
//! bytes. Only punched bytes count as reclaimed space; zero-filled bytes are reported apart.

#![cfg_attr(
    not(feature = "failpoints"),
    expect(dead_code, reason = "T6's compact runs the reclamation")
)]

use kaban_core::{Durability, Result};
use rusqlite::Connection;

use super::Catalog;
use super::failpoint::Window;
use super::pin::latest_generation;
use super::pins;
use super::tables::{ExtentId, sql_int, uint};
use crate::sql_error::SqlResultExt;

/// How many consumed sediment rows one tidy commit deletes.
const SEDIMENT_CHUNK: u64 = 4096;

/// What one reclamation did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ReclaimReport {
    /// The `reclaim_floor` it committed.
    pub(crate) floor: u64,
    /// The retired extents it destroyed, in id order.
    pub(crate) reclaimed: Vec<ExtentId>,
    /// The retired extents left for a later round (still pinned, behind the barrier, in a
    /// pin race, or cold), in id order.
    pub(crate) pending: Vec<ExtentId>,
    /// The allocated space of the container that the release freed, in bytes.
    pub(crate) released_bytes: u64,
    /// The length of the ranges a punch processed (a partial block is only zeroed by it).
    pub(crate) punched_range_bytes: u64,
    /// Bytes only overwritten with zeros: logically gone, no space reclaimed.
    pub(crate) zero_filled_bytes: u64,
    /// Its re-scan found a live pin below the floor: it destroyed nothing.
    pub(crate) pin_race_retry: bool,
    /// The WAL was not backfilled through a commit that retired an extent no pin reaches.
    pub(crate) barrier_pending: bool,
    /// The platform's full sync is refused, so the barrier never holds (`degraded_durability`).
    pub(crate) degraded_durability: bool,
    /// The catalog cannot deallocate container ranges (`space_reclaim_unsupported`).
    pub(crate) space_reclaim_unsupported: bool,
    /// Consumed sediment rows deleted.
    pub(crate) sediment_deleted: u64,
}

impl Catalog {
    /// Reclaim what no pin can reach any more, behind the KER-10 barrier (the module's steps):
    /// destroy the retired extents at or below both the committed floor and the durable
    /// generation, report the rest as pending, then delete consumed sediment in chunks. A
    /// catalog without a container (in memory) destroys nothing.
    ///
    /// # Errors
    /// The checkpoint's, the scans', the commits' and the releases' errors;
    /// `StoreError::StorageFatal` for a failed sync (the rows of the released ranges stay).
    pub(crate) fn reclaim(&self, conn: &mut Connection) -> Result<ReclaimReport> {
        let latest = self.read(conn, latest_generation)?;
        let durable = if self.checkpoint_tick(conn)?.backfilled {
            latest
        } else {
            0
        };
        let mut report = self.reclaim_through(conn, durable)?;
        report.sediment_deleted = self.delete_dead_sediment(conn)?;
        Ok(report)
    }

    /// PC-8's two phases, destroying only extents retired at or below `durable` as well as at
    /// or below the floor (the barrier's verdict; `u64::MAX` for none, which only the test
    /// reclamation of the pin races passes).
    ///
    /// # Errors
    /// As [`Self::reclaim`].
    pub(crate) fn reclaim_through(
        &self,
        conn: &mut Connection,
        durable: u64,
    ) -> Result<ReclaimReport> {
        let Some(pins) = self.pins.clone() else {
            return Ok(ReclaimReport::default());
        };
        let latest = self.read(conn, latest_generation)?;
        let lowest = pins::lowest_live(&pins)?.map_or(latest, |p| p.min(latest));
        let floor = self.commit(conn, Durability::Normal, |txn| {
            txn.conn()
                .query_row(
                    "UPDATE cat_meta SET value = max(value, ?1) WHERE key = 'reclaim_floor'
                     RETURNING value",
                    [sql_int(lowest).sql()?],
                    |r| uint(r, 0),
                )
                .sql()
        })?;
        self.failpoint(Window::ReclaimFloorCommitted);
        let pin_race_retry = pins::lowest_live(&pins)?.is_some_and(|p| p < floor);
        let retired = self.read(conn, |tx| {
            let mut stmt = tx
                .prepare(
                    "SELECT id, dead_since, kbx_offset, kbx_len FROM cat_extent
                     WHERE dead_since IS NOT NULL ORDER BY id",
                )
                .sql()?;
            let rows = stmt
                .query_map([], |r| {
                    let span = match r.get::<_, Option<i64>>(2)? {
                        Some(_) => Some((uint(r, 2)?, uint(r, 3)?)),
                        None => None,
                    };
                    Ok((ExtentId(uint(r, 0)?), uint(r, 1)?, span))
                })
                .sql()?;
            rows.collect::<rusqlite::Result<Vec<_>>>().sql()
        })?;
        let mut report = ReclaimReport {
            floor,
            pin_race_retry,
            ..ReclaimReport::default()
        };
        let mut doomed: Vec<(ExtentId, (u64, u64))> = Vec::new();
        for (id, dead_since, span) in retired {
            let unpinned = !pin_race_retry && dead_since <= floor;
            match span {
                Some(span) if unpinned && dead_since <= durable => doomed.push((id, span)),
                _ => {
                    report.barrier_pending |= unpinned && dead_since > durable;
                    report.pending.push(id);
                }
            }
        }
        let ranges: Vec<(u64, u64)> = doomed.iter().map(|&(_, span)| span).collect();
        let released = self.reclaim_ranges(&ranges)?;
        if !doomed.is_empty() {
            // the ranges are released and synced: their rows go (a failed sync kept them)
            self.commit(conn, Durability::Normal, |txn| {
                for (id, _) in &doomed {
                    txn.conn()
                        .execute(
                            "DELETE FROM cat_extent WHERE id = ?1",
                            [sql_int(id.0).sql()?],
                        )
                        .sql()?;
                }
                Ok(())
            })?;
        }
        report.reclaimed = doomed.into_iter().map(|(id, _)| id).collect();
        report.released_bytes = released.punched;
        report.punched_range_bytes = released.punched;
        report.zero_filled_bytes = released.zero_filled;
        report.space_reclaim_unsupported = self.degraded_reclamation();
        Ok(report)
    }

    /// Delete the sediment rows a compaction switch consumed, [`SEDIMENT_CHUNK`] rows per
    /// commit, until none is left; returns how many went. A pin reads them only inside its own
    /// read transaction, so a pin taken before the switch keeps its copy.
    fn delete_dead_sediment(&self, conn: &mut Connection) -> Result<u64> {
        let mut deleted = 0_u64;
        loop {
            let n = self.commit(conn, Durability::Normal, |txn| {
                txn.conn()
                    .execute(
                        "DELETE FROM cat_sediment WHERE seq IN
                           (SELECT seq FROM cat_sediment WHERE dead IS NOT NULL
                            ORDER BY seq LIMIT ?1)",
                        [sql_int(SEDIMENT_CHUNK).sql()?],
                    )
                    .sql()
            })?;
            let n = u64::try_from(n).unwrap_or(u64::MAX);
            deleted = deleted.saturating_add(n);
            if n < SEDIMENT_CHUNK {
                return Ok(deleted);
            }
        }
    }
}
