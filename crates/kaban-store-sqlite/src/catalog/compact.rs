//! Compaction claims and their publication (spec 11 KER-07, PC-9, 7.4).
//!
//! ```text
//!   claim   => one commit: every input live and unclaimed? => cat_claim rows, else None
//!   (merge work outside the writer lock: seal, fold or purge appends its extents)
//!   publish => one commit through the committer (`Catalog::commit`), generation g:
//!                added extents published, inputs' chain rows replaced by them,
//!                inputs dead_since g, consumed sediment dead = g, claim rows deleted
//!   reclaim => later: punches the dead extents, deletes the dead sediment in chunks
//! ```
//!
//! Two compactions never claim one input: the claim commits before any merge work, and a
//! claim of an input that is claimed, retired or unknown claims nothing.
//!
//! The chain rule of a switch: each affected view names its own change ([`ViewSwitch`]): a
//! run of consecutive extents of its chain, in chain order, gives way to added extents in the
//! same place (an empty run appends, as a seal does to `main`). A run that is not consecutive
//! is refused, so a fold never moves later layers before surviving ones, and a retired extent
//! that any chain still holds after the switch is refused, so a fold in one view never takes a
//! base another view still reads. A pin reads its chains inside its own read transaction
//! (`Pin::load_view`), so a pin taken before the switch still sees the old rows and one taken
//! after sees only the new ones. Consumed sediment rows are only marked dead in the switch
//! (`cat_sediment.dead`, the switch's generation) and pins skip dead rows by the same snapshot
//! rule; reclamation deletes them later, in chunks (`Catalog::reclaim`), so the switch
//! transaction stays short (PC-9 item 1). A refused switch rolls back and hands its claim back
//! ([`PublishFailed`]): the caller retries or releases it ([`Catalog::release`]).
//!
//! [`Catalog::compact`] schedules the units by the `[catalog]` policy and [`Catalog::purge`]
//! rewrites what holds a purged record; both run every unit through the same claim, merge,
//! append and publish, under a maintenance budget per unit (spec 11 7.6).

#![cfg_attr(
    not(feature = "failpoints"),
    expect(
        dead_code,
        reason = "T6's compact (seal, fold, purge) claims and publishes"
    )
)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, PoisonError};
use std::time::{Duration, Instant};

use kaban_core::config::SealOps;
use kaban_core::query_budget::{BudgetExceeded, QueryBudget};
use kaban_core::{BackendSource, Durability, Result, StoreError, ViewName};
use rusqlite::{Connection, OptionalExtension};

use super::Catalog;
use super::checkpoint::CheckpointReport;
use super::class::{ExtentClass, ExtentLocation, IndexClass};
use super::commit::Txn;
use super::fail;
use super::failpoint::Window;
use super::kbx::Unpublished;
use super::pin::{Pin, PinnedView};
use super::reclaim::ReclaimReport;
use super::tables::{ExtentId, ExtentRef, Generation, SedimentRow, SedimentSeq, sql_int, uint};
use crate::sql_error::SqlResultExt;

/// A compaction's claim on its input extents, recorded in `cat_claim` under its own token.
/// Not `Clone`: a claim is published once.
#[derive(Debug)]
pub(crate) struct Claim {
    token: String,
    inputs: Vec<ExtentId>,
}

impl Claim {
    /// The claimed extents, in the order they were asked for.
    pub(crate) fn inputs(&self) -> &[ExtentId] {
        &self.inputs
    }
}

/// One compaction's switch: the extents it wrote, how each affected view's chain changes, the
/// inputs it retires and the sediment rows it sealed, under the claim it holds.
#[derive(Debug)]
pub(crate) struct Publication {
    /// The claim the compaction holds; every replaced and retired extent is one of its inputs.
    pub(crate) claim: Claim,
    /// The extents it appended.
    pub(crate) added: Vec<Unpublished>,
    /// The chain changes, applied in order.
    pub(crate) views: Vec<ViewSwitch>,
    /// The extents it retires: no chain may hold one after the switch.
    pub(crate) retired: Vec<ExtentId>,
    /// The sediment rows it sealed into `added`.
    pub(crate) sediment_consumed: Vec<SedimentSeq>,
}

/// One change of one view's chain: the run `replaced` (consecutive in the chain, in chain
/// order; empty: the end of the chain) gives way to the added extents `added` (indices into
/// [`Publication::added`], in chain order).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ViewSwitch {
    /// The view whose chain changes.
    pub(crate) view: ViewName,
    /// The run of its chain that goes.
    pub(crate) replaced: Vec<ExtentId>,
    /// The added extents that take its place.
    pub(crate) added: Vec<usize>,
}

/// A publication that failed, and the claim it held: its rows are still committed, so the
/// caller releases it ([`Catalog::release`]) or retries with it.
#[derive(Debug)]
pub(crate) struct PublishFailed {
    /// Why it failed (nothing of the switch was published).
    pub(crate) error: StoreError,
    /// The claim, still held.
    pub(crate) claim: Claim,
}

impl From<Box<PublishFailed>> for StoreError {
    fn from(f: Box<PublishFailed>) -> Self {
        f.error
    }
}

/// A switch refused: the commit rolls back and publishes nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SwitchRefused {
    /// The claim's rows are gone (recovery cleared them).
    ClaimLost,
    /// A retired extent is not one of the claim's inputs.
    Unclaimed(ExtentId),
    /// A consumed sediment row is unknown or already consumed.
    SedimentConsumed(SedimentSeq),
    /// A view's replaced run is not consecutive in its chain, in chain order.
    NotConsecutive(String),
    /// An added extent is placed in no view, or a view places one that does not exist.
    Unplaced(usize),
    /// A retired extent is still in the chain of a view after the switch.
    StillNeeded {
        /// The retired extent.
        extent: ExtentId,
        /// A view whose chain still holds it.
        view: String,
    },
}

impl std::fmt::Display for SwitchRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ClaimLost => f.write_str("catalog_switch_refused: the claim is no longer held"),
            Self::Unclaimed(id) => write!(
                f,
                "catalog_switch_refused: extent {} is not an input of the claim",
                id.0
            ),
            Self::SedimentConsumed(seq) => write!(
                f,
                "catalog_switch_refused: sediment row {} is unknown or already consumed",
                seq.0
            ),
            Self::NotConsecutive(view) => write!(
                f,
                "catalog_switch_refused: the run replaced in view {view} is not consecutive in its chain"
            ),
            Self::Unplaced(i) => write!(
                f,
                "catalog_switch_refused: added extent {i} is placed in no view or does not exist"
            ),
            Self::StillNeeded { extent, view } => write!(
                f,
                "catalog_switch_refused: extent {} is retired but view {view} still holds it",
                extent.0
            ),
        }
    }
}

impl std::error::Error for SwitchRefused {}

impl From<SwitchRefused> for StoreError {
    fn from(e: SwitchRefused) -> Self {
        Self::Backend {
            source: BackendSource::new(e),
        }
    }
}

/// Claims made by this process so far: with the process id and the clock, a token no other
/// claim shares.
static CLAIMS: AtomicU64 = AtomicU64::new(0);

impl Catalog {
    /// Claim `inputs` for one compaction: recorded in `cat_claim` in one committed transaction,
    /// before any merge work. `None`, with nothing recorded, when any input is already claimed,
    /// retired or unknown, so two compactions never claim the same extent (KER-07).
    ///
    /// # Errors
    /// SQLite errors (a repeated input among them).
    pub(crate) fn claim(
        &self,
        conn: &mut Connection,
        inputs: &[ExtentId],
    ) -> Result<Option<Claim>> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        let token = format!(
            "{}-{}-{}",
            std::process::id(),
            now.as_nanos(),
            CLAIMS.fetch_add(1, Ordering::Relaxed)
        );
        let claimed_at = i64::try_from(now.as_secs()).unwrap_or(i64::MAX);
        self.commit(conn, Durability::Normal, |txn| {
            for id in inputs {
                let free: bool = txn
                    .conn()
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM cat_extent e
                          WHERE e.id = ?1 AND e.dead_since IS NULL
                            AND NOT EXISTS(SELECT 1 FROM cat_claim c WHERE c.extent = e.id))",
                        [sql_int(id.0).sql()?],
                        |r| r.get(0),
                    )
                    .sql()?;
                if !free {
                    return Ok(None);
                }
            }
            for id in inputs {
                txn.conn()
                    .execute(
                        "INSERT INTO cat_claim(extent, claim, claimed_at) VALUES (?1, ?2, ?3)",
                        rusqlite::params![sql_int(id.0).sql()?, token, claimed_at],
                    )
                    .sql()?;
            }
            Ok(Some(Claim {
                token: token.clone(),
                inputs: inputs.to_vec(),
            }))
        })
    }

    /// Publish one compaction's switch through the committer (`Catalog::commit`), as one new
    /// generation: the added extents join the chains the retired ones leave (the module's
    /// chain rule), the retired extents are dead since that generation, the consumed sediment
    /// rows are marked dead in it, and the claim ends. Returns the generation.
    ///
    /// # Errors
    /// [`PublishFailed`] with the claim, still held, and the reason: [`SwitchRefused`] (behind
    /// `StoreError::Backend`) for a lost claim, an unclaimed retired extent or a consumed
    /// sediment row that is unknown or already consumed; the publication refusals of
    /// [`Txn::publish`]; SQLite errors. Any of them rolls the whole switch back.
    pub(crate) fn publish(
        &self,
        conn: &mut Connection,
        p: Publication,
    ) -> std::result::Result<Generation, Box<PublishFailed>> {
        let Publication {
            claim,
            added,
            views,
            retired,
            sediment_consumed,
        } = p;
        let switched = self.commit(conn, Durability::Normal, |txn| {
            let generation = txn.generation()?;
            let g = sql_int(generation.0).sql()?;
            let held: u64 = txn
                .conn()
                .query_row(
                    "SELECT count(*) FROM cat_claim WHERE claim = ?1",
                    [&claim.token],
                    |r| uint(r, 0),
                )
                .sql()?;
            if usize::try_from(held).ok() != Some(claim.inputs.len()) {
                return Err(SwitchRefused::ClaimLost.into());
            }
            let replaced = views.iter().flat_map(|v| &v.replaced);
            if let Some(id) = retired
                .iter()
                .chain(replaced)
                .find(|id| !claim.inputs.contains(id))
            {
                return Err(SwitchRefused::Unclaimed(*id).into());
            }
            let placed = |i: usize| views.iter().any(|v| v.added.contains(&i));
            let unknown = views
                .iter()
                .flat_map(|v| &v.added)
                .copied()
                .find(|i| *i >= added.len());
            if let Some(i) = unknown.or_else(|| (0..added.len()).find(|i| !placed(*i))) {
                return Err(SwitchRefused::Unplaced(i).into());
            }
            let mut ids = Vec::with_capacity(added.len());
            for extent in added {
                ids.push(txn.publish(extent, &[])?);
            }
            for switch in &views {
                switch_view(txn, switch, &ids)?;
            }
            for id in &retired {
                let holder: Option<String> = txn
                    .conn()
                    .query_row(
                        "SELECT view FROM cat_view_extent WHERE extent_id = ?1 LIMIT 1",
                        [sql_int(id.0).sql()?],
                        |r| r.get(0),
                    )
                    .optional()
                    .sql()?;
                if let Some(view) = holder {
                    return Err(SwitchRefused::StillNeeded { extent: *id, view }.into());
                }
                txn.conn()
                    .execute(
                        "UPDATE cat_extent SET dead_since = ?1 WHERE id = ?2 AND dead_since IS NULL",
                        rusqlite::params![g, sql_int(id.0).sql()?],
                    )
                    .sql()?;
            }
            for seq in &sediment_consumed {
                let marked = txn
                    .conn()
                    .execute(
                        "UPDATE cat_sediment SET dead = ?1 WHERE seq = ?2 AND dead IS NULL",
                        rusqlite::params![g, sql_int(seq.0).sql()?],
                    )
                    .sql()?;
                if marked != 1 {
                    return Err(SwitchRefused::SedimentConsumed(*seq).into());
                }
            }
            txn.conn()
                .execute("DELETE FROM cat_claim WHERE claim = ?1", [&claim.token])
                .sql()?;
            fail::point("publish.before_commit");
            Ok(generation)
        });
        switched.map_err(|error| Box::new(PublishFailed { error, claim }))
    }

    /// Release `claim` without publishing (after a refused publication, 7.6): its inputs can be
    /// claimed again.
    ///
    /// # Errors
    /// SQLite errors.
    pub(crate) fn release(&self, conn: &mut Connection, claim: Claim) -> Result<()> {
        let Claim { token, .. } = claim;
        self.commit(conn, Durability::Normal, |txn| {
            txn.conn()
                .execute("DELETE FROM cat_claim WHERE claim = ?1", [&token])
                .sql()
                .map(|_| ())
        })
    }
}

/// Apply `switch` to its view's chain: the replaced run, which must be consecutive and in
/// chain order, gives way to the added extents (`ids` by index) in its place; an empty run
/// appends them. Positions are renumbered from 0.
fn switch_view(txn: &Txn<'_>, switch: &ViewSwitch, ids: &[ExtentId]) -> Result<()> {
    let conn = txn.conn();
    let view = switch.view.to_string();
    let old: Vec<ExtentId> = {
        let mut stmt = conn
            .prepare("SELECT extent_id FROM cat_view_extent WHERE view = ?1 ORDER BY position")
            .sql()?;
        let rows = stmt.query_map([&view], |r| uint(r, 0)).sql()?;
        rows.map(|r| r.map(ExtentId))
            .collect::<rusqlite::Result<_>>()
            .sql()?
    };
    let run = switch.replaced.as_slice();
    let at = match run.first() {
        None => old.len(),
        Some(first) => old
            .iter()
            .position(|id| id == first)
            .filter(|&p| old.get(p..p + run.len()) == Some(run))
            .ok_or_else(|| SwitchRefused::NotConsecutive(view.clone()))?,
    };
    let added = switch.added.iter().filter_map(|&i| ids.get(i).copied());
    let chain: Vec<ExtentId> = old[..at]
        .iter()
        .copied()
        .chain(added)
        .chain(old[at + run.len()..].iter().copied())
        .collect();
    conn.execute("DELETE FROM cat_view_extent WHERE view = ?1", [&view])
        .sql()?;
    let mut insert = conn
        .prepare("INSERT INTO cat_view_extent(view, position, extent_id) VALUES (?1, ?2, ?3)")
        .sql()?;
    for (position, id) in (0_u64..).zip(&chain) {
        insert
            .execute(rusqlite::params![
                view,
                sql_int(position).sql()?,
                sql_int(id.0).sql()?
            ])
            .sql()?;
    }
    Ok(())
}

/// The maintenance budget of one unit of work (one seal, one fold, one class's purge; spec 11
/// 7.6): the memory it may hold and how long it may run. The configured one is
/// [`Catalog::maintenance_budget`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactBudget {
    /// The memory ceiling, in bytes.
    pub memory_bytes: u64,
    /// The deadline of one unit.
    pub deadline: Duration,
}

impl CompactBudget {
    /// A fresh query budget for one unit of work.
    fn unit(self) -> QueryBudget {
        QueryBudget::new(self.memory_bytes, Some(self.deadline))
    }
}

/// What a unit of maintenance work does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkKind {
    /// Seal sediment rows into an extent.
    Seal,
    /// Fold consecutive extents of one view into one.
    Fold,
    /// Rewrite extents without purged records.
    Purge,
}

/// One unit of work that published.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Compacted {
    /// The index class (its catalog name).
    pub class: &'static str,
    /// What it did.
    pub kind: WorkKind,
    /// The generation its switch published.
    pub generation: u64,
    /// The extents it replaced, in chain order.
    pub extents: Vec<u64>,
    /// How many sediment rows it sealed.
    pub sediment_rows: usize,
}

/// Why a unit of work was skipped (it published nothing and holds no claim).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// Another compaction holds a claim on one of its inputs, or an input is retired.
    Claimed,
    /// `maintenance_budget_refused`: its work ran out of its maintenance budget (7.6).
    Budget(BudgetExceeded),
    /// Its switch was refused (the refusal's text): a consumed sediment row, a changed chain.
    Refused(String),
}

/// One unit of work that was skipped, with its inputs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    /// The index class (its catalog name).
    pub class: &'static str,
    /// What it would have done.
    pub kind: WorkKind,
    /// The extents it would have replaced.
    pub extents: Vec<u64>,
    /// How many sediment rows it would have sealed.
    pub sediment_rows: usize,
    /// Why it was skipped.
    pub reason: SkipReason,
}

/// What one compaction did ([`Catalog::compact`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CompactReport {
    /// The units that published, in the order they ran.
    pub published: Vec<Compacted>,
    /// The units skipped, in the order they ran (a refused unit before its smaller retry).
    pub skipped: Vec<Skipped>,
    /// The reclamation that followed.
    pub reclaim: ReclaimReport,
    /// SQLite free pages the bounded incremental vacuum released.
    pub vacuumed_pages: u64,
    /// The checkpoint tick, when the configured interval had elapsed since the last one.
    pub checkpoint: Option<CheckpointReport>,
}

/// What one purge did ([`Catalog::purge`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PurgeReport {
    /// One entry per class whose extents were rewritten.
    pub published: Vec<Compacted>,
    /// The classes whose rewrite was skipped: the caller purges again.
    pub skipped: Vec<Skipped>,
    /// The reclamation that followed.
    pub reclaim: ReclaimReport,
}

impl Catalog {
    /// The configured maintenance budget (`[catalog]` `maintenance_memory_mib`,
    /// `maintenance_deadline_ms`).
    pub(crate) fn maintenance_budget(&self) -> CompactBudget {
        CompactBudget {
            memory_bytes: self.config().maintenance_memory_mib.saturating_mul(1 << 20),
            deadline: Duration::from_millis(self.config().maintenance_deadline_ms),
        }
    }

    /// Compact every registered class by the `[catalog]` policy, then reclaim:
    ///
    /// ```text
    ///   maintenance hold (shared) => pin every view
    ///   per class: seal  main's pending rows of the class >= seal_ops   (rows: n, n/2 .. 1)
    ///              fold  a view's class run past fold_depth or the delta ratio
    ///                    (the newest k of its unshared run: k, k/2 .. 2)
    ///   each unit: claim => merge under its budget => append => publish (one ViewSwitch)
    ///   => reclaim => bounded incremental vacuum => checkpoint tick once its interval elapsed
    /// ```
    ///
    /// A claim another compaction holds skips the unit (`Claimed`); a unit its budget refuses
    /// publishes nothing, hands its claim back and is retried smaller, and the smallest one
    /// refused is reported, never raised; a refused switch is released and reported. A catalog
    /// without a container (in memory) seals and folds nothing.
    ///
    /// # Errors
    /// SQLite, container and storage errors (a budget refusal is a skip, never an error).
    pub(crate) fn compact(
        &self,
        conn: &mut Connection,
        budget: CompactBudget,
    ) -> Result<CompactReport> {
        let mut report = CompactReport::default();
        if let Some(_hold) = self.maintenance_hold()? {
            let pinned = self.pin_every_view(conn, budget)?;
            for class in self.classes() {
                self.seal_class(conn, &pinned, class.as_ref(), budget, &mut report)?;
                for view in &pinned.views {
                    self.fold_view(conn, &pinned, view, class.as_ref(), budget, &mut report)?;
                }
            }
        }
        report.reclaim = self.reclaim(conn)?;
        report.vacuumed_pages = self.vacuum(conn)?;
        if self.checkpoint_due() {
            report.checkpoint = Some(self.checkpoint_tick(conn)?);
            *self
                .last_tick
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = Some(Instant::now());
        }
        Ok(report)
    }

    /// Purge `rids` from every class (spec 11 7.8: explicit omission): claim every live extent
    /// of the class in any view, ask the class to rewrite each one without them, publish the
    /// rewrites in place of the old extents in every chain that holds them (one switch per
    /// class), retire the old ones, then reclaim. A class whose claim or switch is refused, or
    /// whose rewrite its budget refuses, is reported skipped: the caller purges again.
    ///
    /// # Errors
    /// SQLite, container and storage errors.
    pub(crate) fn purge(&self, conn: &mut Connection, rids: &[i64]) -> Result<PurgeReport> {
        let mut report = PurgeReport::default();
        if let Some(_hold) = self.maintenance_hold()? {
            let budget = self.maintenance_budget();
            let pinned = self.pin_every_view(conn, budget)?;
            for class in self.classes() {
                let mut extents: Vec<ExtentRef> = Vec::new();
                for e in pinned.views.iter().flat_map(|v| v.chain()) {
                    if e.class == class.class() && !extents.contains(e) {
                        extents.push(e.clone());
                    }
                }
                if extents.is_empty() {
                    continue;
                }
                let unit = Unit {
                    class: class.as_ref(),
                    kind: WorkKind::Purge,
                    extents,
                    rows: Vec::new(),
                };
                let outcome = self.run_unit(conn, &unit, || {
                    let work = budget.unit();
                    let mut added = Vec::new();
                    let mut views = Vec::new();
                    let mut retired = Vec::new();
                    for e in &unit.extents {
                        let Some(bytes) = class.purge(&pinned.pin, e, rids, &work)? else {
                            continue;
                        };
                        for view in pinned.views.iter().filter(|v| v.chain().contains(e)) {
                            views.push(ViewSwitch {
                                view: view_name(view)?,
                                replaced: vec![e.id],
                                added: vec![added.len()],
                            });
                        }
                        added.push(bytes);
                        retired.push(e.id);
                    }
                    Ok((!added.is_empty()).then_some(Planned {
                        added,
                        views,
                        retired,
                        sediment: Vec::new(),
                    }))
                })?;
                outcome.record(&unit, &mut report.published, &mut report.skipped);
            }
        }
        report.reclaim = self.reclaim(conn)?;
        Ok(report)
    }

    /// Seal `class`'s pending rows of `main` once they reach its trigger, halving the rows on
    /// each budget refusal down to one.
    fn seal_class(
        &self,
        conn: &mut Connection,
        pinned: &Pinned,
        class: &dyn IndexClass,
        budget: CompactBudget,
        report: &mut CompactReport,
    ) -> Result<()> {
        let Some(main) = pinned.views.iter().find(|v| view_is_main(v)) else {
            return Ok(());
        };
        let rows: Vec<&SedimentRow> = main
            .sediment()
            .iter()
            .filter(|r| r.class == class.class())
            .collect();
        let trigger = seal_trigger(class.class(), &self.config().seal_ops);
        if rows.is_empty() || rows.len() < usize::try_from(trigger).unwrap_or(usize::MAX) {
            return Ok(());
        }
        let main_name = view_name(main)?;
        let mut n = rows.len();
        loop {
            let unit = Unit {
                class,
                kind: WorkKind::Seal,
                extents: Vec::new(),
                rows: rows[..n].to_vec(),
            };
            let outcome = self.run_unit(conn, &unit, || {
                let work = budget.unit();
                Ok(class
                    .seal(&pinned.pin, &unit.rows, &work)?
                    .map(|bytes| Planned {
                        added: vec![bytes],
                        views: vec![ViewSwitch {
                            view: main_name.clone(),
                            replaced: Vec::new(),
                            added: vec![0],
                        }],
                        retired: Vec::new(),
                        sediment: unit.rows.iter().map(|r| r.seq).collect(),
                    }))
            })?;
            let retry = outcome.is_budget() && n > 1;
            outcome.record(&unit, &mut report.published, &mut report.skipped);
            if !retry {
                return Ok(());
            }
            n /= 2;
        }
    }

    /// Fold `class`'s run in `view` once its layers reach the fold depth or outweigh the base
    /// by the delta ratio: the newest k extents of the run no other view holds, k halving on
    /// each budget refusal down to two.
    fn fold_view(
        &self,
        conn: &mut Connection,
        pinned: &Pinned,
        view: &PinnedView,
        class: &dyn IndexClass,
        budget: CompactBudget,
        report: &mut CompactReport,
    ) -> Result<()> {
        let chain = view.chain();
        let own: Vec<&ExtentRef> = chain.iter().filter(|e| e.class == class.class()).collect();
        let Some((base, layers)) = own.split_first() else {
            return Ok(());
        };
        let layer_bytes: u64 = layers.iter().map(|e| kbx_len(e)).sum();
        let cfg = self.config();
        let deep = layers.len() >= usize::try_from(cfg.fold_depth).unwrap_or(usize::MAX);
        #[expect(
            clippy::cast_precision_loss,
            reason = "a ratio of byte counts: the policy needs no exact value"
        )]
        let heavy = layer_bytes as f64 > cfg.fold_delta_ratio * kbx_len(base) as f64;
        if !(deep || heavy) {
            return Ok(());
        }
        // the newest consecutive run of the class's extents that no other view holds
        let shared = |e: &ExtentRef| {
            pinned
                .views
                .iter()
                .any(|v| !std::ptr::eq(v.as_ref(), view) && v.chain().contains(e))
        };
        let Some(last) = chain.iter().rposition(|e| e.class == class.class()) else {
            return Ok(());
        };
        let first = chain[..=last]
            .iter()
            .rposition(|e| e.class != class.class() || shared(e))
            .map_or(0, |p| p + 1);
        let run = &chain[first..=last];
        if run.len() < 2 {
            return Ok(());
        }
        let name = view_name(view)?;
        let mut k = run.len();
        loop {
            let unit = Unit {
                class,
                kind: WorkKind::Fold,
                extents: run[run.len() - k..].to_vec(),
                rows: Vec::new(),
            };
            let ids: Vec<ExtentId> = unit.extents.iter().map(|e| e.id).collect();
            let outcome = self.run_unit(conn, &unit, || {
                let work = budget.unit();
                Ok(class
                    .fold(&pinned.pin, &unit.extents, &work)?
                    .map(|bytes| Planned {
                        added: vec![bytes],
                        views: vec![ViewSwitch {
                            view: name.clone(),
                            replaced: ids.clone(),
                            added: vec![0],
                        }],
                        retired: ids.clone(),
                        sediment: Vec::new(),
                    }))
            })?;
            let retry = outcome.is_budget() && k > 2;
            outcome.record(&unit, &mut report.published, &mut report.skipped);
            if !retry {
                return Ok(());
            }
            k = (k / 2).max(2);
        }
    }

    /// One unit of work: claim its extents, run `work` (the merge, under its budget), append
    /// what it wrote and publish the switch. Every path that publishes nothing hands the claim
    /// back ([`Self::release`]).
    fn run_unit(
        &self,
        conn: &mut Connection,
        unit: &Unit<'_>,
        work: impl FnOnce() -> Result<Option<Planned>>,
    ) -> Result<Outcome> {
        let inputs: Vec<ExtentId> = unit.extents.iter().map(|e| e.id).collect();
        let Some(claim) = self.claim(conn, &inputs)? else {
            return Ok(Outcome::Skipped(SkipReason::Claimed));
        };
        self.failpoint(Window::CompactClaimed);
        let planned = match work() {
            Ok(Some(planned)) => planned,
            Ok(None) => {
                self.release(conn, claim)?;
                return Ok(Outcome::Nothing);
            }
            Err(e) => {
                self.release(conn, claim)?;
                return match budget_refusal(&e) {
                    Some(refused) => Ok(Outcome::Skipped(SkipReason::Budget(refused))),
                    None => Err(e),
                };
            }
        };
        match unit.kind {
            WorkKind::Seal => fail::point("seal.mid"),
            WorkKind::Fold => fail::point("fold.mid"),
            WorkKind::Purge => {}
        }
        let mut added = Vec::with_capacity(planned.added.len());
        for bytes in &planned.added {
            match self.append_extent(unit.class.class(), bytes) {
                Ok(extent) => added.push(extent),
                Err(e) => {
                    self.release(conn, claim)?;
                    return Err(e);
                }
            }
        }
        fail::point("publish.after_append");
        let published = self.publish(
            conn,
            Publication {
                claim,
                added,
                views: planned.views,
                retired: planned.retired,
                sediment_consumed: planned.sediment,
            },
        );
        match published {
            Ok(generation) => Ok(Outcome::Published(generation)),
            Err(failed) => {
                let PublishFailed { error, claim } = *failed;
                self.release(conn, claim)?;
                match error {
                    StoreError::StorageFatal { .. } => Err(error),
                    refused => Ok(Outcome::Skipped(SkipReason::Refused(refusal_text(
                        &refused,
                    )))),
                }
            }
        }
    }

    /// Pin the latest generation with every view that has a chain, and `main` (its sediment),
    /// loaded under the maintenance budget.
    fn pin_every_view(&self, conn: &Connection, budget: CompactBudget) -> Result<Pinned> {
        let names: Vec<String> = self.read(conn, |tx| {
            let mut stmt = tx
                .prepare("SELECT DISTINCT view FROM cat_view_extent ORDER BY view")
                .sql()?;
            let rows = stmt.query_map([], |r| r.get(0)).sql()?;
            rows.collect::<rusqlite::Result<_>>().sql()
        })?;
        let mut views = vec![ViewName::Main];
        for name in names {
            let view: ViewName = name.parse().map_err(|e| StoreError::Backend {
                source: BackendSource::new(e),
            })?;
            if !views.contains(&view) {
                views.push(view);
            }
        }
        // the copy of the chains and pending rows has no ceiling of its own: the unit budgets
        // bound the merge work
        let pin = self.pin(
            conn,
            &QueryBudget::new(u64::MAX, Some(budget.deadline)),
            &views,
        )?;
        let loaded = views
            .iter()
            .map(|v| pin.load_view(conn, v))
            .collect::<Result<Vec<_>>>()?;
        Ok(Pinned { pin, views: loaded })
    }

    /// Release up to [`VACUUM_PAGES`] of SQLite's free pages (`auto_vacuum = INCREMENTAL`) in
    /// one commit; how many it released.
    fn vacuum(&self, conn: &mut Connection) -> Result<u64> {
        self.commit(conn, Durability::Normal, |txn| {
            let free = |c: &Connection| -> Result<u64> {
                c.query_row("PRAGMA freelist_count", [], |r| uint(r, 0))
                    .sql()
            };
            let before = free(txn.conn())?;
            txn.conn()
                .execute_batch(&format!("PRAGMA incremental_vacuum({VACUUM_PAGES})"))
                .sql()?;
            Ok(before.saturating_sub(free(txn.conn())?))
        })
    }

    /// Whether the scheduled checkpoint's interval (`[catalog]` `checkpoint_interval_ms`) has
    /// elapsed since compaction last ticked it (always before the first tick).
    fn checkpoint_due(&self) -> bool {
        let interval = Duration::from_millis(self.config().checkpoint_interval_ms);
        self.last_tick
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_none_or(|at| at.elapsed() >= interval)
    }
}

/// How many free pages one compaction's incremental vacuum releases at most.
const VACUUM_PAGES: u32 = 1024;

/// A pin of every view with a chain, and the views it loaded (`main` first).
struct Pinned {
    pin: Pin,
    views: Vec<Arc<PinnedView>>,
}

/// One unit of maintenance work of one class, and its inputs.
struct Unit<'c> {
    class: &'c dyn IndexClass,
    kind: WorkKind,
    extents: Vec<ExtentRef>,
    rows: Vec<&'c SedimentRow>,
}

/// What a unit's merge wrote and how it switches the chains: the new extents' bytes, the
/// chain changes (indices into `added`), the extents it retires and the rows it consumes.
struct Planned {
    added: Vec<Vec<u8>>,
    views: Vec<ViewSwitch>,
    retired: Vec<ExtentId>,
    sediment: Vec<SedimentSeq>,
}

/// How one unit ended.
enum Outcome {
    /// Its switch published this generation.
    Published(Generation),
    /// It published nothing, for this reason.
    Skipped(SkipReason),
    /// Its class had nothing to write.
    Nothing,
}

impl Outcome {
    fn is_budget(&self) -> bool {
        matches!(self, Self::Skipped(SkipReason::Budget(_)))
    }

    /// Add this outcome of `unit` to `published` or `skipped`.
    fn record(self, unit: &Unit<'_>, published: &mut Vec<Compacted>, skipped: &mut Vec<Skipped>) {
        let class = unit.class.class().as_str();
        let extents = unit.extents.iter().map(|e| e.id.0).collect();
        let sediment_rows = unit.rows.len();
        match self {
            Self::Published(generation) => published.push(Compacted {
                class,
                kind: unit.kind,
                generation: generation.0,
                extents,
                sediment_rows,
            }),
            Self::Skipped(reason) => skipped.push(Skipped {
                class,
                kind: unit.kind,
                extents,
                sediment_rows,
                reason,
            }),
            Self::Nothing => {}
        }
    }
}

/// The pending operations that make `class` seal (`[catalog.seal_ops]`; the test class seals
/// under the graph trigger).
fn seal_trigger(class: ExtentClass, ops: &SealOps) -> u32 {
    match class {
        ExtentClass::TextTier => ops.text,
        ExtentClass::GraphBase | ExtentClass::GraphLayer => ops.graph,
        ExtentClass::CodeBase | ExtentClass::CodeLayer => ops.code,
        #[cfg(any(test, feature = "failpoints"))]
        ExtentClass::Test => ops.graph,
    }
}

/// The length of `e`'s bytes in the container (0 for a cold file).
fn kbx_len(e: &ExtentRef) -> u64 {
    match e.location {
        ExtentLocation::Kbx { len, .. } => len,
        ExtentLocation::File { .. } => 0,
    }
}

/// Whether `view` is `main`.
fn view_is_main(view: &PinnedView) -> bool {
    view.name() == ViewName::Main.to_string()
}

/// `view`'s name as a [`ViewName`].
fn view_name(view: &PinnedView) -> Result<ViewName> {
    view.name().parse().map_err(|e| StoreError::Backend {
        source: BackendSource::new(e),
    })
}

/// The budget refusal behind `e` (a class's `seal`, `fold` or `purge` failed its
/// reservation, its deadline or its cancellation), `None` for any other error.
fn budget_refusal(e: &StoreError) -> Option<BudgetExceeded> {
    match e {
        StoreError::Backend { source } => source.driver().downcast_ref::<BudgetExceeded>().copied(),
        _ => None,
    }
}

/// The text a refused switch is reported by: the refusal's own (`catalog_switch_refused: ...`).
fn refusal_text(e: &StoreError) -> String {
    match e {
        StoreError::Backend { source } => source.driver().to_string(),
        other => other.to_string(),
    }
}
