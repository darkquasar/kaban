//! The scheduled checkpoint (spec 11 KER-06, KER-10, PC-9 item 3): writer connections never
//! checkpoint on their own (`wal_autocheckpoint=0`), so this tick keeps the WAL bounded and
//! gives reclamation its barrier.
//!
//! ```text
//!   reader (Catalog::acquire)            checkpoint_tick
//!   waits while a tick is pending        pending marker (locked, then named)
//!   gate SH (instant) => admission SH    gate EX (no new reader passes)
//!   => read transaction ... ends         => admission EX (admitted readers drained)
//!   => admission released                => wal_checkpoint(RESTART) => both released
//! ```
//!
//! Both locks live in `kaban.kbx.pins/` (`pins::admit`) and are taken without blocking,
//! retried within a budget: a reader's within its query budget, the tick's within
//! [`TICK_BUDGET`]. Readers keep their transactions bounded (KER-06), so a waiting tick drains
//! them instead of being overtaken by ever newer readers. A tick that cannot drain in time
//! still checkpoints what it can and reports it. A detached pin holds admission only while its
//! transaction is open.

#![cfg_attr(
    not(feature = "failpoints"),
    expect(
        dead_code,
        reason = "T6's compact, one-shot writes and serve run the tick"
    )
)]

use std::fs::File;
use std::path::Path;
use std::time::{Duration, Instant};

use kaban_core::Result;
use rusqlite::Connection;
use rustix::fs::FlockOperation;

use super::Catalog;
use super::pins;
use crate::sql_error::SqlResultExt;

/// How long one tick may wait, for the admission locks and for SQLite's readers together.
const TICK_BUDGET: Duration = Duration::from_millis(250);

/// The WAL size a restart truncates the log to (`journal_size_limit`), in bytes.
const JOURNAL_SIZE_LIMIT: i64 = 64 * 1024 * 1024;

/// The operation a failed sync of the checkpoint names (`StoreError::StorageFatal`).
const CHECKPOINT: &str = "catalog_checkpoint";

/// What one checkpoint tick did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct CheckpointReport {
    /// The tick held both admission locks while it checkpointed (always for a catalog
    /// without a container, which admits no reader).
    pub(crate) drained: bool,
    /// The checkpoint's result row says every WAL frame is in the main file (KER-10).
    pub(crate) backfilled: bool,
    /// Frames in the WAL when it checkpointed.
    pub(crate) wal_frames: u64,
    /// Frames it checkpointed.
    pub(crate) checkpointed_frames: u64,
    /// The platform's full sync is refused (the open's `F_FULLFSYNC` probe on macOS, or the
    /// pragmas did not take): the checkpoint is no KER-10 barrier.
    pub(crate) degraded_durability: bool,
}

/// The tick's hold of both admission locks and its waiting marker, released (admission
/// first) on drop.
#[derive(Debug)]
struct Drained {
    _admission: File,
    _gate: File,
    _pending: pins::Pending,
}

impl Catalog {
    /// Checkpoint `conn`'s database with `wal_checkpoint(RESTART)` under `journal_size_limit`,
    /// after draining the admitted readers (the module's protocol), all within [`TICK_BUDGET`].
    /// Called by compaction, by one-shot writes after their acknowledgement and periodically
    /// by `serve`.
    ///
    /// # Errors
    /// The admission locks' errors; `StoreError::StorageFatal` ([`CHECKPOINT`]) for a failed
    /// sync of the checkpoint; SQLite errors.
    pub(crate) fn checkpoint_tick(&self, conn: &Connection) -> Result<CheckpointReport> {
        let deadline = Instant::now() + TICK_BUDGET;
        conn.pragma_update(None, "journal_size_limit", JOURNAL_SIZE_LIMIT)
            .sql()?;
        // the pragmas make SQLite ask for full syncs; only the open's probe says they work
        let degraded_durability = !full_fsync(conn)? || self.full_sync_refused;
        let drained = match self.pins.as_deref() {
            Some(pins) => drain(pins, deadline)?,
            None => None,
        };
        let report_drained = self.pins.is_none() || drained.is_some();
        // SQLite waits for readers outside the admission (bounded reads) until the deadline
        let wait: i64 = conn
            .query_row("PRAGMA busy_timeout", [], |r| r.get(0))
            .sql()?;
        conn.busy_timeout(deadline.saturating_duration_since(Instant::now()))
            .sql()?;
        let outcome = conn
            .query_row("PRAGMA wal_checkpoint(RESTART)", [], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            })
            .sql_at(CHECKPOINT);
        let restored = conn.pragma_update(None, "busy_timeout", wait).sql();
        drop(drained);
        let (busy, log, checkpointed) = outcome?;
        restored?;
        Ok(CheckpointReport {
            drained: report_drained,
            backfilled: busy == 0 || (log >= 0 && checkpointed == log),
            wal_frames: u64::try_from(log).unwrap_or(0),
            checkpointed_frames: u64::try_from(checkpointed).unwrap_or(0),
            degraded_durability,
        })
    }
}

/// Register as waiting ([`pins::PENDING`]), then take the gate and the admission lock
/// exclusively before `deadline`; `None` when either is still held then (by a passing or an
/// admitted reader).
fn drain(pins: &Path, deadline: Instant) -> Result<Option<Drained>> {
    // registered first: no reader that looks after this passes the gate any more
    let pending = pins::Pending::register(pins)?;
    let before = || Ok(Instant::now() < deadline);
    let Some(gate) = pins::lock_while(
        &pins.join(pins::GATE),
        FlockOperation::NonBlockingLockExclusive,
        before,
    )?
    else {
        return Ok(None);
    };
    let admission = pins::lock_while(
        &pins.join(pins::ADMISSION),
        FlockOperation::NonBlockingLockExclusive,
        before,
    )?;
    Ok(admission.map(|admission| Drained {
        _admission: admission,
        _gate: gate,
        _pending: pending,
    }))
}

/// Make the checkpoint's syncs full syncs where the platform has them (macOS `fullfsync`,
/// `checkpoint_fullfsync`: KER-10); whether they took effect (always elsewhere).
fn full_fsync(conn: &Connection) -> Result<bool> {
    if !cfg!(target_os = "macos") {
        return Ok(true);
    }
    conn.execute_batch("PRAGMA fullfsync = ON; PRAGMA checkpoint_fullfsync = ON")
        .sql()?;
    let on: i64 = conn
        .query_row("PRAGMA checkpoint_fullfsync", [], |r| r.get(0))
        .sql()?;
    Ok(on == 1)
}
