//! Sinks over the `ChangeLog` (engine-spec EN-E10b, EN-F15; engine-plan E2.0; correction RP-6,
//! SD-10): anything that reads `changes` rows is a [`Sink`]; the in-binary projections and the
//! `kaban sync` bookkeeping are the default sink (`kaban-api`), the reserved `reconcile`
//! consumer and any future `Replicator` (core-spec F17) implement the same trait.
//!
//! [`run_sink`] is the one runner: it pages the log through a [`ChangeLogStore`], hands each
//! page to the sink and acknowledges the sink's position under the caller's lease fence. The
//! fence is the runner's, never the sink's; a lost lease is reported, never retried under the
//! same lease, and the cursor never advances without the current fence (the store refuses a
//! stale fence with `LeaseLost`, MD-13). A sink's own writes between `consume` and the ack are
//! idempotent by EN-E10a, which is how a successor's replay after a lost lease stays harmless
//! (engine-plan §19 R-1 records the stronger write-path fence as a residual, not implemented).
//!
//! Installing a consumer (spec 05 SS-06) registers its cursor first, under its own lease, with
//! `ChangeLogStore::changes_register`, and only then takes its initial snapshot or runs
//! [`run_sink`] from the returned cursor: pruning keeps every row after a registered cursor,
//! however far the other consumers have advanced. Each consumer has its own [`SinkId`], so a
//! new one is added without touching another's code.

use crate::changelog::Change;
use crate::error::{Result, StoreError};
use crate::lease::Lease;
use crate::store::ChangeLogStore;

/// A sink's name: its `projections` cursor row and the lease it runs under share it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SinkId(pub &'static str);

impl SinkId {
    /// The default sink of `kaban sync` (M5-19): the in-binary projections and the sync
    /// bookkeeping. The name `export` predates the sink vocabulary and is kept so the
    /// `projections` cursor and the `export` lease of an existing database stay valid.
    pub const EXPORT: Self = Self("export");
    /// The reserved asynchronous reconcile consumer (EN-F15; the loop itself is deferred,
    /// OWN-68 SC-12). Its reserved lease is `reconcile`, with versioned cursor `reconcile:1`;
    /// no binary schedules this consumer, and reconciliation remains synchronous.
    pub const RECONCILE: Self = Self("reconcile");
    /// The text-index projector of an index with its own storage (spec 05 SS-09): its cursor
    /// is retention only, the index's own commit payload is its resume point.
    pub const TEXT_INDEX: Self = Self("text-index");
}

impl std::fmt::Display for SinkId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

/// What the sink has durably applied: the `seq` its cursor may advance to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ack {
    /// The last `seq` applied; the runner acknowledges it under its fence.
    pub upto: u64,
}

/// A consumer of `ChangeLog` rows (EN-E10b `consume(&mut self, changes) -> Ack`).
pub trait Sink: Send + Sync {
    /// The sink's cursor and lease name.
    fn id(&self) -> SinkId;
    /// Apply one page of rows (oldest first, never empty) and say how far they are durably
    /// applied. Applying is idempotent (EN-E10a): a page may be seen twice after a lost lease.
    ///
    /// # Errors
    /// The sink's own failures; the runner propagates them without acknowledging anything.
    fn consume(&mut self, changes: &[Change]) -> Result<Ack>;
}

/// What one [`run_sink`] call did. A plain data record (exhaustive on purpose, like [`Ack`]):
/// callers and tests compare it whole.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SinkReport {
    /// Rows handed to the sink (0 when the log had nothing after the cursor).
    pub consumed: u64,
    /// The position acknowledged under the fence, when the ack went through.
    pub acked: Option<u64>,
    /// The store refused the ack: a successor acquired the lease between the read and the
    /// ack. The caller stops consuming under this lease (SR-2).
    pub lease_lost: bool,
}

/// One page of the `ChangeLog` through `sink`: `changes_read(cursor_after, limit)`, then
/// `sink.consume(&rows)`, then `changes_ack(sink.id(), ack.upto, lease.fence)`. An empty page
/// acknowledges nothing. A `LeaseLost` from the ack is reported (`lease_lost: true`,
/// `acked: None`), never retried under the same lease.
///
/// `lease.name` must be the sink's id: the store accepts an ack only from a lease of the
/// consumer's own name, so any other lease is refused as `LeaseLost`.
///
/// # Errors
/// The sink's own failures (nothing acknowledged) and backend errors other than `LeaseLost`.
pub fn run_sink(
    log: &dyn ChangeLogStore,
    sink: &mut dyn Sink,
    lease: &Lease,
    cursor_after: u64,
    limit: usize,
) -> Result<SinkReport> {
    let rows = log.changes_read(cursor_after, limit)?;
    if rows.is_empty() {
        return Ok(SinkReport::default());
    }
    let consumed = u64::try_from(rows.len()).unwrap_or(u64::MAX);
    let ack = sink.consume(&rows)?;
    match log.changes_ack(sink.id().0, ack.upto, lease.fence) {
        Ok(()) => Ok(SinkReport {
            consumed,
            acked: Some(ack.upto),
            lease_lost: false,
        }),
        Err(StoreError::LeaseLost { .. }) => Ok(SinkReport {
            consumed,
            acked: None,
            lease_lost: true,
        }),
        Err(e) => Err(e),
    }
}
