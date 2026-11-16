//! Pins (spec 11 PC-8, INV-3): a reader's view of one catalog generation, race-free against
//! reclamation. [`Catalog::with_snapshot`] evaluates inside the pin's transaction;
//! [`Catalog::pin`] copies the index inputs and returns a detached pin.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use kaban_core::query_budget::{BudgetExceeded, BudgetLease, QueryBudget};
use kaban_core::{BackendSource, Result, StoreError};
use rusqlite::{Connection, Transaction};
use tantivy::directory::OwnedBytes;

use super::class::{ExtentClass, ExtentLocation};
use super::tables::{ExtentRef, Generation, SedimentRow, SedimentSeq, uint};
use super::{Catalog, not_yet};
use crate::rows::bad_enum;
use crate::sql_error::SqlResultExt;

/// One pinned generation: its extents and sediment by class, and the container they live in.
#[derive(Debug)]
#[expect(
    dead_code,
    reason = "T5 Step 4 and the index readers (T7, T11, T16) read the pinned extents"
)]
pub(crate) struct Pin {
    generation: Generation,
    kbx: PathBuf,
    extents: BTreeMap<ExtentClass, Vec<ExtentRef>>,
    sediment: BTreeMap<ExtentClass, Vec<SedimentRow>>,
    /// The copy's charge against the reader's budget, released with the pin.
    _copy: BudgetLease,
}

#[expect(
    dead_code,
    reason = "T5 Step 4 and the index readers (T7, T11, T16) read the pinned extents"
)]
impl Pin {
    /// The pinned generation (the actual generation h of PC-8).
    pub(crate) fn generation(&self) -> Generation {
        self.generation
    }
    /// The pinned extents of `class`.
    pub(crate) fn extents(&self, class: ExtentClass) -> &[ExtentRef] {
        self.extents.get(&class).map_or(&[], Vec::as_slice)
    }
    /// The pinned sediment rows of `class`, in sequence order.
    pub(crate) fn sediment(&self, class: ExtentClass) -> &[SedimentRow] {
        self.sediment.get(&class).map_or(&[], Vec::as_slice)
    }
    /// The bytes of `e`: a slice of a mapping of the container.
    ///
    /// # Errors
    /// Always, until T5 Step 4 maps the container: an unsupported-operation
    /// `StoreError::Backend`.
    #[expect(clippy::unused_self, reason = "T5 Step 4 maps the pin's container")]
    pub(crate) fn bytes(&self, e: &ExtentRef) -> Result<OwnedBytes> {
        let _ = e;
        Err(not_yet("Pin::bytes"))
    }
    /// The container's path.
    pub(crate) fn kbx_path(&self) -> &Path {
        &self.kbx
    }
}

impl Catalog {
    /// Pin the generation the read transaction sees and run `f` with that transaction; both
    /// end on every exit. Acquisition and execution share `budget`.
    ///
    /// Not yet the race-free acquisition of PC-8: the pin lock and the `reclaim_floor` retry
    /// arrive with reclamation (T5 Step 4). Until then nothing reclaims, so the transaction's
    /// snapshot alone is the pinned generation.
    ///
    /// # Errors
    /// The closure's, the budget's or SQLite's error.
    #[expect(
        dead_code,
        reason = "the bounded SQL statements of T12 and T15 run in it"
    )]
    pub(crate) fn with_snapshot<T>(
        &self,
        conn: &mut Connection,
        budget: &QueryBudget,
        f: impl FnOnce(&Transaction<'_>, &Pin) -> Result<T>,
    ) -> Result<T> {
        self.read(conn, |tx| {
            let pin = self.load(tx, budget)?;
            f(tx, &pin)
        })
    }

    /// Pin as [`Self::with_snapshot`] does, copy every index input under `budget`, end the
    /// transaction and return the detached pin.
    ///
    /// # Errors
    /// The budget's or SQLite's error.
    pub(crate) fn pin(&self, conn: &mut Connection, budget: &QueryBudget) -> Result<Pin> {
        self.read(conn, |tx| self.load(tx, budget))
    }

    /// The pinned generation's inputs as `tx` sees them: the copy is charged to `budget`
    /// before it is made (PC-17).
    fn load(&self, tx: &Transaction<'_>, budget: &QueryBudget) -> Result<Pin> {
        budget.check().map_err(budget_exceeded)?;
        let generation = tx
            .query_row(
                "SELECT COALESCE(max(generation), 0) FROM cat_generation",
                [],
                |r| uint(r, 0),
            )
            .sql()?;
        let (rows, bytes) = tx
            .query_row(
                "SELECT (SELECT count(*) FROM cat_extent WHERE retired_at IS NULL)
                        + (SELECT count(*) FROM cat_sediment),
                        (SELECT COALESCE(sum(length(payload)), 0) FROM cat_sediment)",
                [],
                |r| Ok((uint(r, 0)?, uint(r, 1)?)),
            )
            .sql()?;
        let per_row =
            u64::try_from(size_of::<SedimentRow>().max(size_of::<ExtentRef>())).unwrap_or(u64::MAX);
        let charge = rows.saturating_mul(per_row).saturating_add(bytes);
        let copy = budget.reserve(charge).map_err(budget_exceeded)?;
        Ok(Pin {
            generation: Generation(generation),
            kbx: self.kbx.clone(),
            extents: live_extents(tx)?,
            sediment: sediment(tx)?,
            _copy: copy,
        })
    }
}

fn class_of(idx: usize, name: &str) -> rusqlite::Result<ExtentClass> {
    ExtentClass::parse(name).ok_or_else(|| bad_enum(idx, name))
}

fn sediment(tx: &Transaction<'_>) -> Result<BTreeMap<ExtentClass, Vec<SedimentRow>>> {
    let mut out: BTreeMap<ExtentClass, Vec<SedimentRow>> = BTreeMap::new();
    let mut stmt = tx
        .prepare("SELECT seq, class, payload FROM cat_sediment ORDER BY seq")
        .sql()?;
    let rows = stmt
        .query_map([], |r| {
            Ok(SedimentRow {
                seq: SedimentSeq(uint(r, 0)?),
                class: class_of(1, &r.get::<_, String>(1)?)?,
                payload: r.get(2)?,
            })
        })
        .sql()?;
    for row in rows {
        let row = row.sql()?;
        out.entry(row.class).or_default().push(row);
    }
    Ok(out)
}

fn live_extents(tx: &Transaction<'_>) -> Result<BTreeMap<ExtentClass, Vec<ExtentRef>>> {
    let mut out: BTreeMap<ExtentClass, Vec<ExtentRef>> = BTreeMap::new();
    let mut stmt = tx
        .prepare(
            "SELECT class, generation, kbx_offset, kbx_len, relpath FROM cat_extent
             WHERE retired_at IS NULL ORDER BY id",
        )
        .sql()?;
    let rows = stmt
        .query_map([], |r| {
            let location = match r.get::<_, Option<String>>(4)? {
                Some(relpath) => ExtentLocation::File { relpath },
                None => ExtentLocation::Kbx {
                    offset: uint(r, 2)?,
                    len: uint(r, 3)?,
                },
            };
            Ok(ExtentRef {
                class: class_of(0, &r.get::<_, String>(0)?)?,
                generation: Generation(uint(r, 1)?),
                location,
            })
        })
        .sql()?;
    for row in rows {
        let row = row.sql()?;
        out.entry(row.class).or_default().push(row);
    }
    Ok(out)
}

fn budget_exceeded(e: BudgetExceeded) -> StoreError {
    StoreError::Backend {
        source: BackendSource::new(e),
    }
}
