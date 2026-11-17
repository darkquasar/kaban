//! Test access to the catalog for the integration suites (`failpoints` feature only; not a
//! supported interface): a test index class whose `stage` writes one sediment row per edge
//! change and records the change set it was handed, a pin of that class's sediment, and the
//! container's extents of the test class: appended and published, pinned and read, retired and
//! reclaimed by a test reclamation that follows T6's two-phase rule (PC-8) and releases ranges
//! through the catalog's primitive, with hooks in the pin protocol's windows ([`Window`]), and
//! the I/O faults of the catalog's I/O seam ([`IoFaults`]).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use kaban_core::query_budget::QueryBudget;
use kaban_core::{Durability, Result};
use rusqlite::Connection;

pub use super::failpoint::Window;
use super::io;
use super::kbx::{Unpublished, failed};
use super::pins;
use super::tables::{sql_int, uint};
use crate::sql_error::SqlResultExt;

use super::Catalog;
use super::capture::{BatchChanges, CaptureOp};
use super::class::{ExtentClass, ExtentLocation, IndexClass, SealOutput};
use super::commit::Stage;
use super::pin::Pin;
use crate::SqliteStore;

/// What happened to an observed row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObservedOp {
    /// Inserted.
    Insert,
    /// Updated.
    Update,
    /// Deleted.
    Delete,
}

impl ObservedOp {
    fn of(op: CaptureOp) -> Self {
        match op {
            CaptureOp::Insert => Self::Insert,
            CaptureOp::Update => Self::Update,
            CaptureOp::Delete => Self::Delete,
        }
    }
    fn as_str(self) -> &'static str {
        match self {
            Self::Insert => "insert",
            Self::Update => "update",
            Self::Delete => "delete",
        }
    }
}

/// One change of a commit's change set as the test class saw it in `stage`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObservedChange {
    /// A node change, with the lifecycle on each side that exists.
    Node {
        /// `node.rid`.
        rid: i64,
        /// The operation.
        op: ObservedOp,
        /// The lifecycle before the change (`None`: the row did not exist).
        lifecycle_before: Option<String>,
        /// The lifecycle after the change (`None`: the row no longer exists).
        lifecycle_after: Option<String>,
    },
    /// An edge change by its identity.
    Edge {
        /// Source `rid`.
        src: i64,
        /// Target `rid`.
        dst: i64,
        /// Relationship type.
        rel: String,
        /// The operation.
        op: ObservedOp,
    },
}

/// The sediment payload the test class writes for one edge change.
#[must_use]
pub fn edge_payload(op: ObservedOp, src: i64, dst: i64, rel: &str) -> String {
    format!("{} {src} {dst} {rel}", op.as_str())
}

/// The change sets the installed test class was handed, one per commit, in commit order.
#[derive(Debug, Clone, Default)]
pub struct Observed(Arc<Mutex<Vec<Vec<ObservedChange>>>>);

impl Observed {
    /// Every change set seen so far.
    #[must_use]
    pub fn change_sets(&self) -> Vec<Vec<ObservedChange>> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// Install the test class on `store`'s catalog: every later commit stages it. Returns the
/// handle on the change sets it sees.
pub fn install_edge_sediment_class(store: &SqliteStore) -> Observed {
    let observed = Observed::default();
    store.catalog().register(Arc::new(EdgeSediment {
        observed: observed.clone(),
    }));
    observed
}

/// Pin `store`'s catalog and return the test class's sediment payloads, in sequence order.
///
/// # Errors
/// The pin's errors.
pub fn pin_sediment(store: &SqliteStore) -> Result<Vec<String>> {
    let budget = QueryBudget::new(u64::MAX, None);
    let pin = store.catalog().pin(&mut store.lock(), &budget)?;
    Ok(pin
        .sediment(ExtentClass::Test)
        .iter()
        .map(|row| String::from_utf8_lossy(&row.payload).into_owned())
        .collect())
}

/// Append `bytes` to `store`'s container as an extent of the test class and publish it in one
/// commit; returns its extent id.
///
/// # Errors
/// The append's or the commit's errors.
pub fn append_and_publish(store: &SqliteStore, bytes: &[u8]) -> Result<u64> {
    let extent = store.catalog().append_extent(ExtentClass::Test, bytes)?;
    let id = store
        .catalog()
        .commit(&mut store.lock(), Durability::Normal, |txn| {
            txn.publish(extent)
        })?;
    Ok(id.0)
}

/// Append every one of `extents` to `store`'s container with one write and one sync, and
/// publish each as its own extent of the test class in one commit; returns their ids in order.
///
/// # Errors
/// The append's or the commit's errors.
pub fn append_and_publish_all(store: &SqliteStore, extents: &[Vec<u8>]) -> Result<Vec<u64>> {
    let whole = extents.concat();
    let appended = store.catalog().append_extent(ExtentClass::Test, &whole)?;
    let ExtentLocation::Kbx { offset: start, .. } = appended.location else {
        unreachable!("an append names a container range")
    };
    let mut offset = start;
    let mut parts = Vec::with_capacity(extents.len());
    for bytes in extents {
        let len = bytes.len() as u64;
        parts.push(Unpublished {
            class: ExtentClass::Test,
            location: ExtentLocation::Kbx { offset, len },
        });
        offset += len;
    }
    store
        .catalog()
        .commit(&mut store.lock(), Durability::Normal, |txn| {
            parts
                .into_iter()
                .map(|part| txn.publish(part).map(|id| id.0))
                .collect()
        })
}

/// A detached pin of `store`'s catalog ([`Catalog::pin`]).
#[derive(Debug)]
pub struct TestPin(Pin);

impl TestPin {
    /// The pin's view.
    #[must_use]
    pub fn view(&self) -> PinView<'_> {
        PinView(&self.0)
    }
}

/// A pin as a test reads it: the test class's extents and their bytes.
#[derive(Debug, Clone, Copy)]
pub struct PinView<'p>(&'p Pin);

impl PinView<'_> {
    /// The pinned generation.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.0.generation().0
    }
    /// The ids of the pinned extents of the test class, in id order.
    #[must_use]
    pub fn extents(&self) -> Vec<u64> {
        self.0
            .extents(ExtentClass::Test)
            .iter()
            .map(|e| e.id.0)
            .collect()
    }
    /// Where the pinned test-class extent `id` lies in the container (`(offset, len)`), `None`
    /// when the pin does not hold it or it is not in the container.
    #[must_use]
    pub fn span(&self, id: u64) -> Option<(u64, u64)> {
        self.0
            .extents(ExtentClass::Test)
            .iter()
            .find(|e| e.id.0 == id)
            .and_then(|e| match e.location {
                ExtentLocation::Kbx { offset, len } => Some((offset, len)),
                ExtentLocation::File { .. } => None,
            })
    }
    /// A copy of the bytes of the pinned test-class extent `id`, `None` when the pin does not
    /// hold it.
    ///
    /// # Errors
    /// [`Pin::bytes`]'s errors.
    pub fn read(&self, id: u64) -> Result<Option<Vec<u8>>> {
        let Some(e) = self
            .0
            .extents(ExtentClass::Test)
            .iter()
            .find(|e| e.id.0 == id)
        else {
            return Ok(None);
        };
        Ok(Some(self.0.bytes(e)?.as_slice().to_vec()))
    }
}

/// Pin `store`'s catalog under `budget` and return the detached pin.
///
/// # Errors
/// The pin's errors.
pub fn pin(store: &SqliteStore, budget: &QueryBudget) -> Result<TestPin> {
    Ok(TestPin(store.catalog().pin(&mut store.lock(), budget)?))
}

/// Run `f` inside a snapshot of `store`'s catalog under `budget` ([`Catalog::with_snapshot`]).
///
/// # Errors
/// The snapshot's or `f`'s errors.
pub fn with_snapshot<T>(
    store: &SqliteStore,
    budget: &QueryBudget,
    f: impl FnOnce(PinView<'_>) -> Result<T>,
) -> Result<T> {
    store
        .catalog()
        .with_snapshot(&mut store.lock(), budget, |_, pin| f(PinView(pin)))
}

/// Install `hook` in `window` of `store`'s catalog (`None` removes it). A reader's windows run
/// on the reader's thread with its connection held; the reclamation's on the thread of
/// [`reclaim`], with no connection held. A hook may block to hold its window open; the test
/// bounds that wait.
pub fn set_failpoint(
    store: &SqliteStore,
    window: Window,
    hook: Option<Arc<dyn Fn() + Send + Sync>>,
) {
    store.catalog().failpoints.set(window, hook);
}

/// How often `store`'s readers retried because a reclamation's floor passed their candidate
/// generation (PC-8 step 4).
#[must_use]
pub fn pin_retries(store: &SqliteStore) -> u64 {
    store.catalog().failpoints.retries()
}

/// Retire the extents `ids` in one commit, as T6's publication switch does: dead since the
/// generation that commit publishes. Returns that generation.
///
/// # Errors
/// The commit's errors.
pub fn retire(store: &SqliteStore, ids: &[u64]) -> Result<u64> {
    store
        .catalog()
        .commit(&mut store.lock(), Durability::Normal, |txn| {
            let generation = txn.generation()?.0;
            for id in ids {
                txn.conn()
                    .execute(
                        "UPDATE cat_extent SET dead_since = ?1 WHERE id = ?2 AND dead_since IS NULL",
                        rusqlite::params![sql_int(generation).sql()?, sql_int(*id).sql()?],
                    )
                    .sql()?;
            }
            Ok(generation)
        })
}

/// What one test reclamation did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reclaimed {
    /// The `reclaim_floor` it committed.
    pub floor: u64,
    /// The extents whose container ranges it released and whose rows it dropped, in id order.
    pub punched: Vec<u64>,
    /// Whether its re-scan found a live pin below the floor (`pin_race_retry`): then it
    /// released nothing.
    pub pin_race_retry: bool,
}

/// Reclaim `store`'s container by T6's two-phase rule (PC-8), releasing ranges through the
/// catalog's own primitive ([`Catalog::reclaim_ranges`]: punched, or zero-filled when
/// reclamation is degraded):
/// scan the live pin locks for the lowest generation P (the latest generation when none is
/// live), commit `reclaim_floor = max(floor, P)`, [`Window::ReclaimFloorCommitted`], re-scan;
/// a live pin below the floor makes it a `pin_race_retry` that releases nothing, else it
/// releases every container range of an extent dead since a generation at or below the floor
/// and, once that release is synced, drops those rows (a failed sync keeps them).
///
/// # Errors
/// The scans', commits' and container writes' errors; a catalog without a container.
pub fn reclaim(store: &SqliteStore) -> Result<Reclaimed> {
    let catalog = store.catalog();
    let Some(pins) = catalog.pins.as_deref() else {
        return Err(failed(
            "reclaim",
            &catalog.kbx,
            std::io::Error::new(std::io::ErrorKind::NotFound, "no container"),
        ));
    };
    let latest = catalog.read(&store.lock(), |tx| {
        tx.query_row(
            "SELECT COALESCE(max(generation), 0) FROM cat_generation",
            [],
            |r| uint(r, 0),
        )
        .sql()
    })?;
    let lowest = pins::lowest_live(pins)?.map_or(latest, |p| p.min(latest));
    let floor = catalog.commit(&mut store.lock(), Durability::Normal, |txn| {
        txn.conn()
            .query_row(
                "UPDATE cat_meta SET value = max(value, ?1) WHERE key = 'reclaim_floor'
                 RETURNING value",
                [sql_int(lowest).sql()?],
                |r| uint(r, 0),
            )
            .sql()
    })?;
    catalog.failpoint(Window::ReclaimFloorCommitted);
    if pins::lowest_live(pins)?.is_some_and(|p| p < floor) {
        return Ok(Reclaimed {
            floor,
            punched: Vec::new(),
            pin_race_retry: true,
        });
    }
    let doomed = catalog.read(&store.lock(), |tx| {
        let mut stmt = tx
            .prepare(
                "SELECT id, kbx_offset, kbx_len FROM cat_extent
                 WHERE dead_since IS NOT NULL AND dead_since <= ?1 AND kbx_offset IS NOT NULL
                 ORDER BY id",
            )
            .sql()?;
        let rows = stmt
            .query_map([sql_int(floor).sql()?], |r| {
                Ok((uint(r, 0)?, uint(r, 1)?, uint(r, 2)?))
            })
            .sql()?;
        rows.collect::<rusqlite::Result<Vec<_>>>().sql()
    })?;
    let ranges: Vec<(u64, u64)> = doomed
        .iter()
        .map(|&(_, offset, len)| (offset, len))
        .collect();
    catalog.reclaim_ranges(&ranges)?;
    let punched: Vec<u64> = doomed.into_iter().map(|(id, _, _)| id).collect();
    catalog.commit(&mut store.lock(), Durability::Normal, |txn| {
        for id in &punched {
            txn.conn()
                .execute(
                    "DELETE FROM cat_extent WHERE id = ?1",
                    [sql_int(*id).sql()?],
                )
                .sql()?;
        }
        Ok(())
    })?;
    Ok(Reclaimed {
        floor,
        punched,
        pin_race_retry: false,
    })
}

/// The I/O faults of every catalog whose artefacts lie under one directory (the catalog's I/O
/// seam, `catalog::io`): failed syncs by operation, a probe that finds no hole punching, and a
/// count of the syncs attempted. Removed when dropped. Each test injects under its own scratch
/// directory, so concurrent tests do not see each other's faults.
#[derive(Debug)]
pub struct IoFaults(PathBuf);

impl IoFaults {
    /// Start injecting under `dir` (nothing fails until a fault is set).
    #[must_use]
    pub fn install(dir: &Path) -> Self {
        io::faults::install(dir);
        Self(dir.to_path_buf())
    }
    /// Fail every later sync of the operation `op` (`append_extent`, `create_container`,
    /// `create_alloc_lock`, `create_dir_entries`, `reclaim_ranges`) with an I/O error.
    pub fn fail_sync(&self, op: &'static str) {
        io::faults::fail_sync(&self.0, op);
    }
    /// Let every later sync reach the file system again.
    pub fn heal(&self) {
        io::faults::heal(&self.0);
    }
    /// Make every later hole-punch probe find no hole punching.
    pub fn no_punch(&self) {
        io::faults::no_punch(&self.0);
    }
    /// How many syncs of `op` were attempted since [`Self::install`], failed ones included.
    #[must_use]
    pub fn sync_calls(&self, op: &str) -> u64 {
        io::faults::sync_calls(&self.0, op)
    }
}

impl Drop for IoFaults {
    fn drop(&mut self) {
        io::faults::remove(&self.0);
    }
}

/// Whether `store`'s catalog reclaims by zero-filling because its open's probe found no hole
/// punching (KER-11).
#[must_use]
pub fn degraded_reclamation(store: &SqliteStore) -> bool {
    store.catalog().degraded_reclamation()
}

/// The test class: one sediment row per edge change; records every change set it is handed.
struct EdgeSediment {
    observed: Observed,
}

impl IndexClass for EdgeSediment {
    fn class(&self) -> ExtentClass {
        ExtentClass::Test
    }
    fn stage(&self, txn: &mut Stage<'_>, changes: &BatchChanges) -> Result<()> {
        for e in changes.edges() {
            let payload = edge_payload(ObservedOp::of(e.op), e.src, e.dst, &e.rel);
            txn.put_sediment(ExtentClass::Test, payload.as_bytes())?;
        }
        let nodes = changes.nodes().iter().map(|n| ObservedChange::Node {
            rid: n.rid,
            op: ObservedOp::of(n.op),
            lifecycle_before: n.before.as_ref().map(|i| i.lifecycle.clone()),
            lifecycle_after: n.after.as_ref().map(|i| i.lifecycle.clone()),
        });
        let edges = changes.edges().iter().map(|e| ObservedChange::Edge {
            src: e.src,
            dst: e.dst,
            rel: e.rel.clone(),
            op: ObservedOp::of(e.op),
        });
        self.observed
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(nodes.chain(edges).collect());
        Ok(())
    }
    fn seal(&self, _pin: &Pin) -> Result<Option<SealOutput>> {
        Ok(None)
    }
    fn fold(&self, _pin: &Pin) -> Result<Option<SealOutput>> {
        Ok(None)
    }
    fn purge(&self, _pin: &Pin, _rids: &[i64]) -> Result<Option<SealOutput>> {
        Ok(None)
    }
    fn rebuild(&self, _catalog: &Catalog, _rows: &Connection) -> Result<()> {
        Ok(())
    }
}
