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

#![cfg_attr(
    not(feature = "failpoints"),
    expect(
        dead_code,
        reason = "T6's compact (seal, fold, purge) claims and publishes"
    )
)]

use std::sync::atomic::{AtomicU64, Ordering};

use kaban_core::{BackendSource, Durability, Result, StoreError, ViewName};
use rusqlite::{Connection, OptionalExtension};

use super::Catalog;
use super::commit::Txn;
use super::kbx::Unpublished;
use super::tables::{ExtentId, Generation, SedimentSeq, sql_int, uint};
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
