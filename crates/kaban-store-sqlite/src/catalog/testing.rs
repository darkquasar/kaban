//! Test access to the catalog for the integration suites (`failpoints` feature only; not a
//! supported interface): a test index class whose `stage` writes one sediment row per edge
//! change and records the change set it was handed, a pin of that class's sediment, and the
//! container's extents of the test class: appended and published, pinned and read.

use std::sync::{Arc, Mutex, PoisonError};

use kaban_core::query_budget::QueryBudget;
use kaban_core::{Durability, Result};
use rusqlite::Connection;

use super::Catalog;
use super::capture::{BatchChanges, CaptureOp};
use super::class::{ExtentClass, IndexClass, SealOutput};
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
