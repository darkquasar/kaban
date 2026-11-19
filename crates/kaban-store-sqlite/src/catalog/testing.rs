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
use kaban_core::{Durability, Result, StoreError, ViewName};
use rusqlite::Connection;
use tantivy::directory::OwnedBytes;

pub use super::failpoint::{BudgetWindow, Window};
use super::io;
use super::kbx::failed;
use super::tables::sql_int;
use crate::sql_error::SqlResultExt;

use super::Catalog;
use super::capture::{BatchChanges, CaptureOp};
use super::class::{ExtentClass, ExtentLocation, IndexClass};
use super::commit::Stage;
use super::compact::{Claim, CompactBudget, CompactReport, Publication, PurgeReport, ViewSwitch};
use super::graph_class::{GraphClass, LoadedView, load_view};
use super::pin::{Pin, PinnedView, budget_exceeded};
use super::tables::{ExtentId, ExtentRef, SedimentRow, SedimentSeq};
use crate::SqliteStore;

/// The classes the test helpers load sediment for: the test class's alone.
const TEST: &[ExtentClass] = &[ExtentClass::Test];

/// The environment variable that arms one named crash point in a child process
/// (`catalog::fail`): the child kills itself with `SIGKILL` there.
pub const FAILPOINT_ARM: &str = super::fail::ARM;

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
    install(store, None)
}

/// [`install_edge_sediment_class`], with a test class whose `seal_due` answers `due` whatever
/// its pending rows are.
pub fn install_edge_sediment_class_due(store: &SqliteStore, due: bool) -> Observed {
    install(store, Some(due))
}

fn install(store: &SqliteStore, due: Option<bool>) -> Observed {
    let observed = Observed::default();
    store.catalog().register(Arc::new(EdgeSediment {
        observed: observed.clone(),
        due,
    }));
    observed
}

/// Pin `store`'s catalog and return the test class's sediment payloads of `main`, in
/// sequence order.
///
/// # Errors
/// The pin's errors.
pub fn pin_sediment(store: &SqliteStore) -> Result<Vec<String>> {
    let budget = QueryBudget::new(u64::MAX, None);
    let conn = store.lock();
    let pin = store
        .catalog()
        .pin(&conn, &budget, &[(ViewName::Main, TEST)])?;
    Ok(pin
        .load_view(&conn, &ViewName::Main, TEST)?
        .sediment()
        .iter()
        .filter(|row| row.class == ExtentClass::Test)
        .map(|row| String::from_utf8_lossy(&row.payload).into_owned())
        .collect())
}

/// Append `bytes` to `store`'s container as an extent of the test class and publish it into
/// `main` in one commit; returns its extent id.
///
/// # Errors
/// The append's or the commit's errors.
pub fn append_and_publish(store: &SqliteStore, bytes: &[u8]) -> Result<u64> {
    let extent = store.catalog().append_extent(ExtentClass::Test, bytes)?;
    let id = store
        .catalog()
        .commit(&mut store.lock(), Durability::Normal, |txn| {
            txn.publish(extent, &[ViewName::Main])
        })?;
    Ok(id.0)
}

/// [`append_and_publish`], also returning how long the publication's commit held the write
/// transaction (KER-04's publish hold).
///
/// # Errors
/// The append's or the commit's errors.
pub fn append_and_publish_held(
    store: &SqliteStore,
    bytes: &[u8],
) -> Result<(u64, std::time::Duration)> {
    let extent = store.catalog().append_extent(ExtentClass::Test, bytes)?;
    let (id, hold) = store
        .catalog()
        .commit_held(&mut store.lock(), Durability::Normal, |txn| {
            txn.publish(extent, &[ViewName::Main])
        })?;
    Ok((id.0, hold))
}

/// Append every one of `extents` to `store`'s container with one write and one sync, and
/// publish each as its own extent of the test class into `main` in one commit; returns their
/// ids in order.
///
/// # Errors
/// The append's or the commit's errors.
pub fn append_and_publish_all(store: &SqliteStore, extents: &[Vec<u8>]) -> Result<Vec<u64>> {
    let whole = extents.concat();
    let appended = store.catalog().append_extent(ExtentClass::Test, &whole)?;
    let Some(parts) = appended.split(extents) else {
        unreachable!("the parts sum to the appended range")
    };
    store
        .catalog()
        .commit(&mut store.lock(), Durability::Normal, |txn| {
            parts
                .into_iter()
                .map(|part| txn.publish(part, &[ViewName::Main]).map(|id| id.0))
                .collect()
        })
}

/// A detached pin of `store`'s catalog with `main` loaded ([`Catalog::pin`]).
#[derive(Debug)]
pub struct TestPin {
    pin: Pin,
    main: Arc<PinnedView>,
}

impl TestPin {
    /// The pin's view.
    #[must_use]
    pub fn view(&self) -> PinView<'_> {
        PinView {
            pin: &self.pin,
            main: &self.main,
        }
    }
}

/// A pin as a test reads it: the test class's extents of `main` and their bytes.
#[derive(Debug, Clone, Copy)]
pub struct PinView<'p> {
    pin: &'p Pin,
    main: &'p PinnedView,
}

impl PinView<'_> {
    /// The pinned generation.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.pin.generation().0
    }
    /// The test class's extents in `main`'s chain, in chain order.
    fn test_extents(&self) -> impl Iterator<Item = &ExtentRef> {
        self.main
            .chain()
            .iter()
            .filter(|e| e.class == ExtentClass::Test)
    }
    /// The ids of the pinned extents of the test class, in chain order (publication order).
    #[must_use]
    pub fn extents(&self) -> Vec<u64> {
        self.test_extents().map(|e| e.id.0).collect()
    }
    /// Where the pinned test-class extent `id` lies in the container (`(offset, len)`), `None`
    /// when the pin does not hold it or it is not in the container.
    #[must_use]
    pub fn span(&self, id: u64) -> Option<(u64, u64)> {
        self.test_extents()
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
        Ok(self.bytes(id)?.map(|bytes| bytes.as_slice().to_vec()))
    }
    /// The bytes of the pinned test-class extent `id` as [`Pin::bytes`] returns them, `None`
    /// when the pin does not hold it.
    ///
    /// # Errors
    /// [`Pin::bytes`]'s errors.
    pub fn bytes(&self, id: u64) -> Result<Option<OwnedBytes>> {
        self.bytes_of(*self, id)
    }
    /// The bytes of `other`'s test-class extent `id`, asked of this pin ([`Pin::bytes`]),
    /// `None` when `other` does not hold it.
    ///
    /// # Errors
    /// [`Pin::bytes`]'s errors, a refusal among them when this pin does not hold the extent.
    pub fn bytes_of(&self, other: PinView<'_>, id: u64) -> Result<Option<OwnedBytes>> {
        let Some(e) = other.test_extents().find(|e| e.id.0 == id) else {
            return Ok(None);
        };
        self.pin.bytes(e).map(Some)
    }
}

/// Pin `store`'s catalog under `budget` and return the detached pin.
///
/// # Errors
/// The pin's errors.
pub fn pin(store: &SqliteStore, budget: &QueryBudget) -> Result<TestPin> {
    let conn = store.lock();
    let pin = store
        .catalog()
        .pin(&conn, budget, &[(ViewName::Main, TEST)])?;
    let main = pin.load_view(&conn, &ViewName::Main, TEST)?;
    Ok(TestPin { pin, main })
}

/// Run `f` inside a snapshot of `store`'s catalog under `budget` ([`Catalog::with_snapshot`]),
/// with `main` loaded in the snapshot's transaction.
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
        .with_snapshot(&store.lock(), budget, |tx, pin| {
            let main = pin.load_view(tx, &ViewName::Main, TEST)?;
            f(PinView { pin, main: &main })
        })
}

/// Install `hook` in `window` of `store`'s catalog (`None` removes it). A reader's windows run
/// on the reader's thread with its connection held; the reclamation's on the thread of
/// [`reclaim`] or [`reclaim_retired`], with its own store's connection held. A hook may block
/// to hold its window open; the test bounds that wait.
pub fn set_failpoint(
    store: &SqliteStore,
    window: Window,
    hook: Option<Arc<dyn Fn() + Send + Sync>>,
) {
    store.catalog().failpoints.set(window, hook);
}

/// A deterministic test hook that cancels or replaces a budget with an expired one.
pub type BudgetHook = Arc<dyn Fn(BudgetWindow, &mut QueryBudget) + Send + Sync>;

/// Set the maintenance budget hook (`None` removes it). No waiting is needed to expire a
/// budget: the test can replace it with a zero-duration budget at the selected boundary.
pub fn set_budget_hook(store: &SqliteStore, hook: Option<BudgetHook>) {
    store.catalog().failpoints.set_budget(hook);
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
    /// The released bytes whose blocks a punch freed (reclaimed space).
    pub punched_bytes: u64,
    /// The released bytes overwritten with zeros instead (no space reclaimed).
    pub zero_filled_bytes: u64,
}

/// Reclaim `store`'s container by T6's two-phase rule (PC-8) without the KER-10 barrier
/// ([`Catalog::reclaim_through`] with every generation taken as durable; [`reclaim_retired`]
/// is the catalog's own reclamation): scan the live pin locks for the lowest generation P (the
/// latest generation when none is live), commit `reclaim_floor = max(floor, P)`,
/// [`Window::ReclaimFloorCommitted`], re-scan; a live pin below the floor makes it a
/// `pin_race_retry` that releases nothing, else it releases every container range of an extent
/// dead since a generation at or below the floor through the catalog's own primitive
/// ([`Catalog::reclaim_ranges`]: punched, or zero-filled when reclamation is degraded) and,
/// once that release is synced, drops those rows (a failed sync keeps them).
///
/// # Errors
/// The scans', commits' and container writes' errors; a catalog without a container.
pub fn reclaim(store: &SqliteStore) -> Result<Reclaimed> {
    let catalog = store.catalog();
    if catalog.pins.is_none() {
        return Err(failed(
            "reclaim",
            &catalog.kbx,
            std::io::Error::new(std::io::ErrorKind::NotFound, "no container"),
        ));
    }
    let r = catalog.reclaim_through(&mut store.lock(), u64::MAX)?;
    Ok(Reclaimed {
        floor: r.floor,
        punched: r.reclaimed,
        pin_race_retry: r.pin_race_retry,
        punched_bytes: r.punched_range_bytes,
        zero_filled_bytes: r.zero_filled_bytes,
    })
}

/// A compaction's claim on extents of a store's catalog ([`Catalog::claim`]).
#[derive(Debug)]
pub struct TestClaim(Claim);

impl TestClaim {
    /// The claimed extent ids, in the order they were asked for.
    #[must_use]
    pub fn inputs(&self) -> Vec<u64> {
        self.0.inputs().iter().map(|id| id.0).collect()
    }
}

/// Claim the extents `ids` of `store`'s catalog ([`Catalog::claim`]); `None` when any of them
/// is already claimed or not live.
///
/// # Errors
/// The claim's errors.
pub fn claim(store: &SqliteStore, ids: &[u64]) -> Result<Option<TestClaim>> {
    let ids: Vec<ExtentId> = ids.iter().copied().map(ExtentId).collect();
    Ok(store
        .catalog()
        .claim(&mut store.lock(), &ids)?
        .map(TestClaim))
}

/// Publish a compaction of `store`'s catalog ([`Catalog::publish`]): `added` appended as
/// test-class extents with one write; in each of `switches` (view name, replaced run) the run
/// gives way to every added extent; `retired` and the sediment rows `consumed` (by sequence)
/// go under `claim`. Returns the generation it published, or the error with the claim, still
/// held.
///
/// # Errors
/// The append's or the publication's error, with the claim.
pub fn publish(
    store: &SqliteStore,
    claim: TestClaim,
    added: &[Vec<u8>],
    switches: &[(&str, &[u64])],
    retired: &[u64],
    consumed: &[u64],
) -> std::result::Result<u64, Box<(StoreError, TestClaim)>> {
    let appended = if added.is_empty() {
        Ok(Vec::new())
    } else {
        store
            .catalog()
            .append_extent(ExtentClass::Test, &added.concat())
            .map(|whole| whole.split(added).unwrap_or_default())
    };
    let added_parts = match appended {
        Ok(parts) => parts,
        Err(e) => return Err(Box::new((e, claim))),
    };
    let mut views = Vec::with_capacity(switches.len());
    for (view, replaced) in switches {
        let view = match view.parse::<ViewName>() {
            Ok(view) => view,
            Err(e) => {
                let e = StoreError::Backend {
                    source: kaban_core::BackendSource::new(e),
                };
                return Err(Box::new((e, claim)));
            }
        };
        views.push(ViewSwitch {
            view,
            replaced: replaced.iter().copied().map(ExtentId).collect(),
            added: (0..added_parts.len()).collect(),
        });
    }
    let p = Publication {
        claim: claim.0,
        added: added_parts,
        views,
        retired: retired.iter().copied().map(ExtentId).collect(),
        sediment_consumed: consumed.iter().copied().map(SedimentSeq).collect(),
    };
    store
        .catalog()
        .publish(&mut store.lock(), p, None)
        .map(|(g, _)| g.0)
        .map_err(|f| Box::new((f.error, TestClaim(f.claim))))
}

/// Release `claim` without publishing ([`Catalog::release`]).
///
/// # Errors
/// The release's errors.
pub fn release(store: &SqliteStore, claim: TestClaim) -> Result<()> {
    store.catalog().release(&mut store.lock(), claim.0)
}

/// Append `bytes` as an extent of the test class and publish it into each of `views` (by
/// name) in one commit; returns its extent id.
///
/// # Errors
/// A view name that does not parse; the append's or the commit's errors.
pub fn append_and_publish_into(store: &SqliteStore, bytes: &[u8], views: &[&str]) -> Result<u64> {
    let views = views
        .iter()
        .map(|v| v.parse::<ViewName>())
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| StoreError::Backend {
            source: kaban_core::BackendSource::new(e),
        })?;
    let extent = store.catalog().append_extent(ExtentClass::Test, bytes)?;
    let id = store
        .catalog()
        .commit(&mut store.lock(), Durability::Normal, |txn| {
            txn.publish(extent, &views)
        })?;
    Ok(id.0)
}

/// The test-class extent ids in the chain of the view `view` (by name) at the latest
/// generation, in chain order.
///
/// # Errors
/// A view name that does not parse; the pin's errors.
pub fn chain(store: &SqliteStore, view: &str) -> Result<Vec<u64>> {
    let view = view.parse::<ViewName>().map_err(|e| StoreError::Backend {
        source: kaban_core::BackendSource::new(e),
    })?;
    let budget = QueryBudget::new(u64::MAX, None);
    let conn = store.lock();
    let pin = store
        .catalog()
        .pin(&conn, &budget, &[(view.clone(), TEST)])?;
    Ok(pin
        .load_view(&conn, &view, TEST)?
        .chain()
        .iter()
        .filter(|e| e.class == ExtentClass::Test)
        .map(|e| e.id.0)
        .collect())
}

/// Pin `store`'s catalog and return the test class's pending sediment rows of `main` as
/// `(sequence, payload)`, in sequence order.
///
/// # Errors
/// The pin's errors.
pub fn pin_sediment_rows(store: &SqliteStore) -> Result<Vec<(u64, String)>> {
    let budget = QueryBudget::new(u64::MAX, None);
    let conn = store.lock();
    let pin = store
        .catalog()
        .pin(&conn, &budget, &[(ViewName::Main, TEST)])?;
    Ok(pin
        .load_view(&conn, &ViewName::Main, TEST)?
        .sediment()
        .iter()
        .filter(|row| row.class == ExtentClass::Test)
        .map(|row| {
            (
                row.seq.0,
                String::from_utf8_lossy(&row.payload).into_owned(),
            )
        })
        .collect())
}

/// What one reclamation of the catalog ([`Catalog::reclaim`]) did.
#[derive(Debug, Clone, PartialEq, Eq)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent report flags, each a named outcome"
)]
pub struct ReclaimOutcome {
    /// The `reclaim_floor` it committed.
    pub floor: u64,
    /// The retired extents it destroyed, in id order.
    pub reclaimed: Vec<u64>,
    /// The retired extents it left for a later round, in id order.
    pub pending: Vec<u64>,
    /// The container's allocated space the release freed, in bytes (reclaimed space).
    pub released_bytes: u64,
    /// The length of the ranges a punch processed (a partial block is only zeroed).
    pub punched_range_bytes: u64,
    /// The released bytes only overwritten with zeros (no space reclaimed).
    pub zero_filled_bytes: u64,
    /// Its re-scan found a live pin below the floor: it destroyed nothing.
    pub pin_race_retry: bool,
    /// The WAL was not backfilled through a retiring commit: those extents stay.
    pub barrier_pending: bool,
    /// The platform's full sync is refused: the barrier never holds.
    pub degraded_durability: bool,
    /// The platform cannot deallocate container ranges (zero-fill only).
    pub space_reclaim_unsupported: bool,
    /// Consumed sediment rows it deleted.
    pub sediment_deleted: u64,
}

/// Reclaim `store`'s retired extents with the catalog's own reclamation ([`Catalog::reclaim`]:
/// PC-8's two phases behind the KER-10 barrier), unlike [`reclaim`], which plays the two
/// phases alone.
///
/// # Errors
/// The reclamation's errors.
pub fn reclaim_retired(store: &SqliteStore) -> Result<ReclaimOutcome> {
    let r = store.catalog().reclaim(&mut store.lock())?;
    Ok(ReclaimOutcome {
        floor: r.floor,
        reclaimed: r.reclaimed,
        pending: r.pending,
        released_bytes: r.released_bytes,
        punched_range_bytes: r.punched_range_bytes,
        zero_filled_bytes: r.zero_filled_bytes,
        pin_race_retry: r.pin_race_retry,
        barrier_pending: r.barrier_pending,
        degraded_durability: r.degraded_durability,
        space_reclaim_unsupported: r.space_reclaim_unsupported,
        sediment_deleted: r.sediment_deleted,
    })
}

/// What one checkpoint tick ([`Catalog::checkpoint_tick`]) did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Checkpoint {
    /// It drained the admitted readers within its budget.
    pub drained: bool,
    /// Every WAL frame is in the main file.
    pub backfilled: bool,
    /// Frames in the WAL.
    pub wal_frames: u64,
    /// Frames checkpointed.
    pub checkpointed_frames: u64,
}

/// Run one checkpoint tick on `store`'s writer connection.
///
/// # Errors
/// The tick's errors.
pub fn checkpoint_tick(store: &SqliteStore) -> Result<Checkpoint> {
    let r = store.catalog().checkpoint_tick(&store.lock())?;
    Ok(Checkpoint {
        drained: r.drained,
        backfilled: r.backfilled,
        wal_frames: r.wal_frames,
        checkpointed_frames: r.checkpointed_frames,
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
    /// Count every later sync without performing it: for a test whose subject is not
    /// durability (the pin races), so it does not pay a device sync per reclamation. Faults
    /// set by [`Self::fail_sync`] still fail.
    pub fn skip_syncs(&self) {
        io::faults::skip_syncs(&self.0);
    }
    /// Let every later sync reach the file system again.
    pub fn heal(&self) {
        io::faults::heal(&self.0);
    }
    /// Make every later hole-punch probe find no hole punching.
    pub fn no_punch(&self) {
        io::faults::no_punch(&self.0);
    }
    /// Make every later full-sync probe find the platform's full sync refused (the macOS
    /// `F_FULLFSYNC` a file system may not honour): set before the store opens.
    pub fn no_full_sync(&self) {
        io::faults::no_full_sync(&self.0);
    }
    /// Make every later release of a reclaimed range find its punch refused, as a file
    /// system that stops punching after the open's probe found it punching.
    pub fn no_release_punch(&self) {
        io::faults::no_release_punch(&self.0);
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

/// Whether `store`'s catalog reclaims by zero-filling (KER-11): its open's probe found no hole
/// punching, or a release found its punch refused since.
#[must_use]
pub fn degraded_reclamation(store: &SqliteStore) -> bool {
    store.catalog().degraded_reclamation()
}

/// The test class: one sediment row per edge change; records every change set it is handed.
struct EdgeSediment {
    observed: Observed,
    /// What `seal_due` answers; `None`: one row is one operation, due at `ops` rows.
    due: Option<bool>,
}

impl IndexClass for EdgeSediment {
    fn classes(&self) -> &[ExtentClass] {
        &[ExtentClass::Test]
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
    /// One operation per row, unless the test fixed the answer.
    fn seal_due(&self, rows: &[&SedimentRow], ops: u32) -> Result<bool> {
        Ok(self.due.unwrap_or(rows.len() >= ops as usize))
    }
    /// The payloads, one per line, in sequence order.
    fn seal(
        &self,
        _pin: &Pin,
        rows: &[&SedimentRow],
        budget: &QueryBudget,
    ) -> Result<Option<Vec<u8>>> {
        let len: usize = rows.iter().map(|r| r.payload.len() + 1).sum();
        let _held = budget.reserve(len as u64).map_err(budget_exceeded)?;
        let mut out = Vec::with_capacity(len);
        for row in rows {
            out.extend_from_slice(&row.payload);
            out.push(b'\n');
        }
        Ok(Some(out))
    }
    /// The extents' bytes, concatenated in chain order.
    fn fold(
        &self,
        pin: &Pin,
        extents: &[ExtentRef],
        budget: &QueryBudget,
    ) -> Result<Option<Vec<u8>>> {
        let spans = extents
            .iter()
            .map(|e| pin.bytes(e))
            .collect::<Result<Vec<_>>>()?;
        let len: usize = spans.iter().map(OwnedBytes::len).sum();
        let _held = budget.reserve(len as u64).map_err(budget_exceeded)?;
        Ok(Some(
            spans
                .iter()
                .flat_map(OwnedBytes::as_slice)
                .copied()
                .collect(),
        ))
    }
    /// The extent without the lines (sealed edge payloads) whose source or target is one of
    /// `rids`; `None` when no line names one.
    fn purge(
        &self,
        pin: &Pin,
        extent: &ExtentRef,
        rids: &[i64],
        budget: &QueryBudget,
    ) -> Result<Option<Vec<u8>>> {
        let bytes = pin.bytes(extent)?;
        let _held = budget
            .reserve(bytes.len() as u64)
            .map_err(budget_exceeded)?;
        let names = |line: &[u8]| {
            std::str::from_utf8(line).is_ok_and(|l| {
                l.split_whitespace()
                    .skip(1)
                    .take(2)
                    .any(|rid| rid.parse().is_ok_and(|rid: i64| rids.contains(&rid)))
            })
        };
        let lines: Vec<&[u8]> = bytes.as_slice().split_inclusive(|b| *b == b'\n').collect();
        if !lines.iter().any(|l| names(l)) {
            return Ok(None);
        }
        Ok(Some(
            lines
                .into_iter()
                .filter(|l| !names(l))
                .flatten()
                .copied()
                .collect(),
        ))
    }
    fn rebuild(&self, _catalog: &Catalog, _rows: &mut Connection) -> Result<()> {
        Ok(())
    }
}

/// Compact `store`'s catalog under `budget` ([`Catalog::compact`]).
///
/// # Errors
/// The compaction's errors.
pub fn compact(store: &SqliteStore, budget: CompactBudget) -> Result<CompactReport> {
    store.catalog().compact(store.connection(), budget)
}

/// Purge `rids` from every class's extents of `store`'s catalog ([`Catalog::purge`]).
///
/// # Errors
/// The purge's errors.
pub fn purge(store: &SqliteStore, rids: &[i64]) -> Result<PurgeReport> {
    store.catalog().purge(store.connection(), rids)
}

/// Append `bytes` to `store`'s container as a test-class extent and never publish it: a leaked
/// range, as a compaction that died between its append and its switch leaves one.
///
/// # Errors
/// The append's errors.
pub fn append_unpublished(store: &SqliteStore, bytes: &[u8]) -> Result<()> {
    let leaked = store.catalog().append_extent(ExtentClass::Test, bytes)?;
    drop(leaked);
    Ok(())
}

/// Set the label of the view `view` (by name) of `store`'s catalog ([`Catalog::set_view_label`]).
///
/// # Errors
/// A view name that does not parse; the commit's errors.
pub fn set_view_label(store: &SqliteStore, view: &str, label: Option<&str>) -> Result<()> {
    let view = view.parse::<ViewName>().map_err(|e| StoreError::Backend {
        source: kaban_core::BackendSource::new(e),
    })?;
    store
        .catalog()
        .set_view_label(&mut store.lock(), &view, label)
}

/// Run `sql` in one catalog commit of `store`: a test's corruption of the catalog rows.
///
/// # Errors
/// The commit's errors.
pub fn corrupt(store: &SqliteStore, sql: &str) -> Result<()> {
    store
        .catalog()
        .commit(&mut store.lock(), Durability::Normal, |txn| {
            txn.conn().execute_batch(sql).sql()
        })
}

/// Insert `n` committed node rows in one catalog commit of `store`, `rid` = `k` for `k` in
/// `0..n` and a `NodeId` that sorts as the rid (a bench's corpus nodes).
///
/// # Errors
/// The commit's errors.
pub fn seed_nodes(store: &SqliteStore, n: u32) -> Result<()> {
    store
        .catalog()
        .commit(&mut store.lock(), Durability::Normal, |txn| {
            let mut insert = txn
                .conn()
                .prepare(
                    "INSERT INTO node(rid, id, kind, text, created_at, updated_at, workspace_id,
                                      content_hash)
                     VALUES (?1, ?2, 'Fact', ?3, '2024-01-01T00:00:00Z',
                             '2024-01-01T00:00:00Z', 'default', ?4)",
                )
                .sql()?;
            for k in 0..n {
                insert
                    .execute(rusqlite::params![
                        k,
                        format!("00000000-0000-7000-8000-{k:012x}"),
                        format!("node {k}"),
                        format!("{k:064x}"),
                    ])
                    .sql()?;
            }
            Ok(())
        })
}

/// Mark every pending graph sediment row of `store` consumed at its own generation, in one
/// catalog commit (a bench whose base already holds what those rows staged).
///
/// # Errors
/// The commit's errors.
pub fn consume_graph_sediment(store: &SqliteStore) -> Result<()> {
    corrupt(
        store,
        "UPDATE cat_sediment SET dead = generation
         WHERE class = 'graph_layer' AND dead IS NULL",
    )
}

/// `main`'s memory graph at one snapshot, as the graph suites read it (T11): the merged view,
/// local ids by `NodeId` and back, relation ids and the public tie order (prepared only when
/// asked for). Local ids never leave the store outside these suites (INV-2).
pub struct MainGraph<'v> {
    loaded: &'v LoadedView<'v>,
    view: kaban_index_graph::View<'v>,
    budget: &'v QueryBudget,
}

impl<'v> MainGraph<'v> {
    /// The merged view: base, layers and pending sediment.
    #[must_use]
    pub fn view(&self) -> &kaban_index_graph::View<'v> {
        &self.view
    }
    /// The local id of the present node `id`.
    ///
    /// # Errors
    /// SQLite errors.
    pub fn local(&self, id: &kaban_core::NodeId) -> Result<Option<kaban_index_graph::LocalId>> {
        self.loaded.node_id_to_rid(id)
    }
    /// The public id of the present node `n`.
    ///
    /// # Errors
    /// SQLite errors.
    pub fn node_id(&self, n: kaban_index_graph::LocalId) -> Result<Option<kaban_core::NodeId>> {
        self.loaded.rid_to_node_id(n)
    }
    /// The id of the declared relation `rel`.
    #[must_use]
    pub fn rel(&self, rel: &str) -> Option<kaban_index_graph::RelId> {
        self.loaded.rel_id(rel)
    }
    /// The public `NodeId` tie order of the snapshot ([`LoadedView::stable_order`]: this snapshot's
    /// identity ceiling over the shared committed keys, extended under the query's budget).
    ///
    /// # Errors
    /// The budget's and SQLite's errors.
    pub fn order(&self) -> Result<std::sync::Arc<dyn kaban_index_graph::StableOrder>> {
        let order = self.loaded.stable_order(self.budget)?;
        Ok(order)
    }
}

/// Run `f` over `main`'s memory graph in one snapshot of `store` under `budget` (the pin, the
/// load and every query share it).
///
/// # Errors
/// The snapshot's, the load's or `f`'s errors.
pub fn with_main_graph<T>(
    store: &SqliteStore,
    budget: &QueryBudget,
    f: impl FnOnce(&MainGraph<'_>) -> Result<T>,
) -> Result<T> {
    with_main_graph_of(store, GRAPH, budget, f)
}

/// [`with_main_graph`] over `main`'s code graph (the code class's extents, as T16 loads them).
///
/// # Errors
/// As [`with_main_graph`].
pub fn with_main_code_graph<T>(
    store: &SqliteStore,
    budget: &QueryBudget,
    f: impl FnOnce(&MainGraph<'_>) -> Result<T>,
) -> Result<T> {
    with_main_graph_of(store, CODE, budget, f)
}

/// The memory graph's classes and the code graph's, layer first (`IndexClass::classes`).
const GRAPH: &[ExtentClass] = &[ExtentClass::GraphLayer, ExtentClass::GraphBase];
const CODE: &[ExtentClass] = &[ExtentClass::CodeLayer, ExtentClass::CodeBase];

fn with_main_graph_of<T>(
    store: &SqliteStore,
    classes: &[ExtentClass],
    budget: &QueryBudget,
    f: impl FnOnce(&MainGraph<'_>) -> Result<T>,
) -> Result<T> {
    store
        .catalog()
        .with_snapshot(&store.lock(), budget, |tx, pin| {
            let loaded = load_view(tx, pin, &ViewName::Main, classes, budget)?;
            let graph = MainGraph {
                loaded: &loaded,
                view: loaded.view(budget)?,
                budget,
            };
            f(&graph)
        })
}

/// Advance the idle clock of this process's graph read cache by `by`, without a real wait.
pub fn advance_graph_cache_clock(by: std::time::Duration) {
    super::graph_class::advance_cache_clock(by);
}

/// Whether this process's graph read cache holds an entry of `store`'s container.
///
/// # Errors
/// The snapshot's errors.
pub fn graph_cache_holds(store: &SqliteStore) -> Result<bool> {
    let budget = QueryBudget::new(u64::MAX, None);
    store
        .catalog()
        .with_snapshot(&store.lock(), &budget, |_, pin| {
            Ok(super::graph_class::cache_holds(pin.kbx_path()))
        })
}

/// The local ids of `store`'s present nodes (their rows not `purged`), ascending: a bench's
/// start nodes.
///
/// # Errors
/// SQLite errors; a rid outside the local id range.
pub fn present_local_ids(store: &SqliteStore) -> Result<Vec<kaban_index_graph::LocalId>> {
    let conn = store.lock();
    let mut stmt = conn
        .prepare("SELECT rid FROM node WHERE lifecycle <> 'purged' ORDER BY rid")
        .sql()?;
    let rids = stmt.query_map([], |r| r.get::<_, i64>(0)).sql()?;
    rids.map(|rid| {
        let rid = rid.sql()?;
        u32::try_from(rid)
            .map(kaban_index_graph::LocalId)
            .map_err(|e| StoreError::Backend {
                source: kaban_core::BackendSource::new(e),
            })
    })
    .collect()
}

/// A database's size on disk ([`db_footprint`]).
#[derive(Debug)]
pub struct DbFootprint {
    /// `page_count * page_size`.
    pub bytes: i64,
    /// Each table's and index's bytes from `dbstat`, largest first (`Err` with SQLite's text
    /// when `dbstat` is not compiled in).
    pub objects: std::result::Result<Vec<(String, i64)>, String>,
}

/// `store`'s database on disk after a checkpoint (a bench's footprint).
///
/// # Errors
/// SQLite errors of the checkpoint or the page count.
pub fn db_footprint(store: &SqliteStore) -> Result<DbFootprint> {
    let conn = store.lock();
    conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
        .sql()?;
    let bytes: i64 = conn
        .query_row(
            "SELECT c.page_count * s.page_size FROM pragma_page_count() c, pragma_page_size() s",
            [],
            |r| r.get(0),
        )
        .sql()?;
    let objects = conn
        .prepare("SELECT name, sum(pgsize) FROM dbstat GROUP BY name ORDER BY 2 DESC")
        .and_then(|mut stmt| {
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()
        })
        .map_err(|e| e.to_string());
    Ok(DbFootprint { bytes, objects })
}

/// Rebuild `store`'s memory graph from its rows ([`GraphClass::rebuild_main`]).
///
/// # Errors
/// The rebuild's errors.
pub fn rebuild_graph(store: &SqliteStore) -> Result<()> {
    GraphClass::rebuild_main(store.catalog(), &mut store.lock())
}

/// Seal the pending graph rows under `budget`, without publishing the output. The input pin
/// uses its own budget so tests can observe the unit's released charges on both outcomes.
///
/// # Errors
/// The pin or seal errors, including a maintenance budget refusal.
pub fn seal_graph(store: &SqliteStore, budget: &QueryBudget) -> Result<Option<Vec<u8>>> {
    let conn = store.lock();
    let pin_budget = QueryBudget::new(u64::MAX, None);
    let classes = &[ExtentClass::GraphLayer, ExtentClass::GraphBase];
    let pin = store
        .catalog()
        .pin(&conn, &pin_budget, &[(ViewName::Main, classes)])?;
    let main = pin.load_view(&conn, &ViewName::Main, classes)?;
    let rows: Vec<_> = main.sediment().iter().collect();
    GraphClass::new().seal(&pin, &rows, budget)
}

/// What `main`'s memory graph holds at one generation ([`graph_contents`]).
#[derive(Debug, Clone)]
pub struct GraphContents {
    /// The bytes of every graph extent of the chain, oldest first.
    pub extents: Vec<Vec<u8>>,
    /// The payloads of the pending graph sediment rows, in sequence order.
    pub sediment: Vec<Vec<u8>>,
}

/// What `main`'s memory graph holds at the latest generation of `store`.
///
/// # Errors
/// The pin's errors.
pub fn graph_contents(store: &SqliteStore) -> Result<GraphContents> {
    let budget = QueryBudget::new(u64::MAX, None);
    let conn = store.lock();
    let classes = &[ExtentClass::GraphLayer, ExtentClass::GraphBase];
    let pin = store
        .catalog()
        .pin(&conn, &budget, &[(ViewName::Main, classes)])?;
    let main = pin.load_view(&conn, &ViewName::Main, classes)?;
    let graph = |c: ExtentClass| matches!(c, ExtentClass::GraphBase | ExtentClass::GraphLayer);
    let extents = main
        .chain()
        .iter()
        .filter(|e| graph(e.class))
        .map(|e| pin.bytes(e).map(|b| b.as_slice().to_vec()))
        .collect::<Result<_>>()?;
    let sediment = main
        .sediment()
        .iter()
        .filter(|r| r.class == ExtentClass::GraphLayer)
        .map(|r| r.payload.clone())
        .collect();
    Ok(GraphContents { extents, sediment })
}

/// Append `bytes` (a graph base) to `store`'s container and publish it as `main`'s graph base
/// in one commit: a bench's corpus. Returns its extent id.
///
/// # Errors
/// The append's or the commit's errors.
pub fn publish_graph_base(store: &SqliteStore, bytes: &[u8]) -> Result<u64> {
    publish_base(store, ExtentClass::GraphBase, bytes)
}

/// [`publish_graph_base`] for a code graph base (`bytes` in the graph format).
///
/// # Errors
/// As [`publish_graph_base`].
pub fn publish_code_base(store: &SqliteStore, bytes: &[u8]) -> Result<u64> {
    publish_base(store, ExtentClass::CodeBase, bytes)
}

fn publish_base(store: &SqliteStore, class: ExtentClass, bytes: &[u8]) -> Result<u64> {
    let extent = store.catalog().append_extent(class, bytes)?;
    let id = store
        .catalog()
        .commit(&mut store.lock(), Durability::Normal, |txn| {
            txn.publish(extent, &[ViewName::Main])
        })?;
    Ok(id.0)
}

/// Stage `payload` (encoded graph records) as one pending graph sediment row of `main` in its
/// own commit, as a commit's staging writes one: a bench's pending ops.
///
/// # Errors
/// The commit's errors.
pub fn stage_graph_sediment(store: &SqliteStore, payload: &[u8]) -> Result<()> {
    store
        .catalog()
        .commit(&mut store.lock(), Durability::Normal, |txn| {
            let generation = sql_int(txn.generation()?.0).sql()?;
            txn.conn()
                .execute(
                    "INSERT INTO cat_sediment(class, generation, payload) VALUES (?1, ?2, ?3)",
                    rusqlite::params![ExtentClass::GraphLayer.as_str(), generation, payload],
                )
                .sql()?;
            Ok(())
        })
}
