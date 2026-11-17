//! Pins (spec 11 PC-8, INV-3): a reader's view of one catalog generation, race-free against
//! reclamation. [`Catalog::with_snapshot`] evaluates inside the pin's transaction;
//! [`Catalog::pin`] copies the index inputs and returns a detached pin.
//!
//! The copy is charged to the reader's budget before it is made (PC-17): the rows are counted
//! first, every allocation the pin keeps (one vector per class, each payload and cold path,
//! the container path) is reserved at its exact capacity, and only then copied into that
//! capacity, never past it. Loading and the snapshot's statements obey the budget's deadline:
//! the copy loops check it at bounded intervals, a progress handler interrupts a long
//! statement, and a cancelled or expired budget turns any outcome into its error.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use kaban_core::query_budget::{
    ALLOCATION_METADATA_BYTES, BudgetExceeded, BudgetLease, QueryBudget,
};
use kaban_core::{BackendSource, Result, StoreError};
use rusqlite::{Connection, Row, Transaction};
use tantivy::directory::OwnedBytes;

use super::Catalog;
use super::class::{ExtentClass, ExtentLocation};
use super::failpoint::Window;
use super::kbx::{Mapping, failed};
use super::pins::PinLock;
use super::tables::{ExtentId, ExtentRef, Generation, SedimentRow, SedimentSeq, uint};
use crate::rows::bad_enum;
use crate::sql_error::SqlResultExt;

/// How many rows a copy loop copies between two budget checks.
const CHECK_EVERY: usize = 256;

/// How many SQLite virtual-machine steps pass between two budget checks of a statement.
const PROGRESS_STEPS: i32 = 1000;

/// One collection per extent class in a fixed layout: no map nodes to account for.
#[derive(Debug)]
struct ByClass<T> {
    rows: [Vec<T>; ExtentClass::COUNT],
    /// The counted rows of each class: what was reserved.
    counted: [usize; ExtentClass::COUNT],
}

impl<T> ByClass<T> {
    /// Empty collections with the reserved capacities.
    fn with_capacities(counted: &[usize; ExtentClass::COUNT]) -> Self {
        Self {
            rows: std::array::from_fn(|i| Vec::with_capacity(counted[i])),
            counted: *counted,
        }
    }
    fn get(&self, class: ExtentClass) -> &[T] {
        &self.rows[class.index()]
    }
    /// Add `row` to `class` within its counted rows. A row past them means the snapshot
    /// changed after it was counted: refused, the copy never grows past its reservation.
    fn push(&mut self, class: ExtentClass, row: T) -> Result<()> {
        let rows = &mut self.rows[class.index()];
        if rows.len() >= self.counted[class.index()] {
            return Err(StoreError::Backend {
                source: BackendSource::new(std::io::Error::other(
                    "the pinned snapshot holds more rows than it counted",
                )),
            });
        }
        rows.push(row);
        Ok(())
    }
}

/// One pinned generation: its extents and sediment by class, and the container they live in.
#[derive(Debug)]
pub(crate) struct Pin {
    generation: Generation,
    kbx: PathBuf,
    /// The container's mapping, shared with the catalog's other pins (excluded from the
    /// copy's charge: a shared immutable mapping, PC-17).
    mapping: Arc<Mapping>,
    extents: ByClass<ExtentRef>,
    sediment: ByClass<SedimentRow>,
    /// The copy's charge against the reader's budget, released with the pin.
    _copy: BudgetLease,
    /// The lock at the candidate generation that keeps reclamation off what the pin can
    /// reach (PC-8); `None` for an in-memory database, which has no container.
    _lock: Option<PinLock>,
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
        self.extents.get(class)
    }
    /// The pinned sediment rows of `class`, in sequence order.
    pub(crate) fn sediment(&self, class: ExtentClass) -> &[SedimentRow] {
        self.sediment.get(class)
    }
    /// The bytes of `e`, one of this pin's extents: a slice of the container's mapping,
    /// re-mapped when the span lies beyond it, bounds-checked against the file. The slice is
    /// what the pin protects: read it while the pin lives (once the pin is released,
    /// reclamation may zero the range under a slice still held).
    ///
    /// # Errors
    /// `kbx::ContainerError` for a span beyond the container or a container that cannot be
    /// mapped, and for a cold extent (`ExtentLocation::File`), which this stream never reads.
    pub(crate) fn bytes(&self, e: &ExtentRef) -> Result<OwnedBytes> {
        match &e.location {
            ExtentLocation::Kbx { offset, len } => self.mapping.span(&self.kbx, *offset, *len),
            ExtentLocation::File { relpath } => Err(failed(
                "read a cold extent outside",
                &self.kbx,
                std::io::Error::new(std::io::ErrorKind::Unsupported, relpath.clone()),
            )),
        }
    }
    /// The container's path.
    pub(crate) fn kbx_path(&self) -> &Path {
        &self.kbx
    }
}

#[cfg(test)]
impl Pin {
    /// Every heap allocation the pin retains, as `(capacity in bytes, alignment)`: what a unit
    /// test checks the copy's charge against.
    pub(crate) fn retained_allocations(&self) -> Vec<(usize, usize)> {
        let mut out = vec![(self.kbx.capacity(), 1)];
        for rows in &self.extents.rows {
            out.push((
                rows.capacity() * size_of::<ExtentRef>(),
                align_of::<ExtentRef>(),
            ));
            for e in rows {
                if let ExtentLocation::File { relpath } = &e.location {
                    out.push((relpath.capacity(), 1));
                }
            }
        }
        for rows in &self.sediment.rows {
            out.push((
                rows.capacity() * size_of::<SedimentRow>(),
                align_of::<SedimentRow>(),
            ));
            out.extend(rows.iter().map(|r| (r.payload.capacity(), 1)));
        }
        out.retain(|(bytes, _)| *bytes > 0);
        out
    }
}

/// The writer connection with a progress handler that interrupts any statement once the
/// budget is cancelled or past its deadline; removed again however the pin ends.
struct Interruptible<'c>(&'c mut Connection);

impl<'c> Interruptible<'c> {
    fn new(conn: &'c mut Connection, budget: &QueryBudget) -> Result<Self> {
        let budget = budget.clone();
        conn.progress_handler(PROGRESS_STEPS, Some(move || budget.check().is_err()))
            .sql()?;
        Ok(Self(conn))
    }
    fn conn(&self) -> &Connection {
        self.0
    }
}

impl Drop for Interruptible<'_> {
    fn drop(&mut self) {
        let _ = self.0.progress_handler(0, None::<fn() -> bool>);
    }
}

/// `out`, unless the budget is cancelled, expired or exceeded: then the budget's error,
/// whatever the outcome was (an interrupted statement's error included).
fn settle<T>(budget: &QueryBudget, out: Result<T>) -> Result<T> {
    budget.check().map_err(budget_exceeded)?;
    out
}

impl Catalog {
    /// Pin by PC-8 and run `f` with the pin's own read transaction, held open while `f`
    /// evaluates rows and indexes; the transaction and the pin lock end on every exit.
    /// Acquisition and execution share `budget`: a statement of `f` is interrupted once it is
    /// cancelled or past its deadline, and then the budget's error is the outcome even when
    /// `f` returned `Ok`.
    ///
    /// # Errors
    /// [`OuterTransaction`] when `conn` already has a transaction open; the closure's, the
    /// budget's, the pin lock's or SQLite's error.
    pub(crate) fn with_snapshot<T>(
        &self,
        conn: &mut Connection,
        budget: &QueryBudget,
        f: impl FnOnce(&Transaction<'_>, &Pin) -> Result<T>,
    ) -> Result<T> {
        let conn = Interruptible::new(conn, budget)?;
        let out = self.acquire(conn.conn(), budget, |tx, pin| {
            let out = f(tx, &pin);
            drop(pin);
            settle(budget, out)
        });
        settle(budget, out)
    }

    /// Pin as [`Self::with_snapshot`] does, copy every index input under `budget`, end the
    /// transaction and return the detached pin. It keeps its candidate's pin lock (and the
    /// container's mapping) until it is dropped, after the store that took it if need be; it
    /// serves no statement that reads live rows.
    ///
    /// # Errors
    /// [`OuterTransaction`] when `conn` already has a transaction open; the budget's, the pin
    /// lock's or SQLite's error.
    pub(crate) fn pin(&self, conn: &mut Connection, budget: &QueryBudget) -> Result<Pin> {
        let conn = Interruptible::new(conn, budget)?;
        let out = self.acquire(conn.conn(), budget, |_, pin| Ok(pin));
        settle(budget, out)
    }

    /// PC-8 acquisition, then `f` over the pin and the transaction it was built from:
    ///
    /// 1. read a candidate generation g;
    /// 2. lock `kaban.kbx.pins/<g>` shared ([`PinLock`]);
    /// 3. begin a fresh read transaction and read `reclaim_floor` F and the actual generation
    ///    h in it (its first read, so its snapshot starts after the lock);
    /// 4. `g < F`: a reclamation passed the candidate before the lock, so its punch may have
    ///    destroyed what g saw: end the transaction, unlock and retry from h; otherwise copy
    ///    the pin from that transaction ([`Self::load`]), reporting h and keeping the lock at
    ///    g as the conservative guard, and run `f` before the transaction ends.
    ///
    /// Every round checks `budget`; every early exit (an error, a retry, an unwind) drops the
    /// transaction (rolled back) and the lock.
    fn acquire<T, F>(&self, conn: &Connection, budget: &QueryBudget, f: F) -> Result<T>
    where
        F: FnOnce(&Transaction<'_>, Pin) -> Result<T>,
    {
        if !conn.is_autocommit() {
            return Err(StoreError::Backend {
                source: BackendSource::new(OuterTransaction),
            });
        }
        let mut candidate = self.read(conn, latest_generation)?;
        let mut f = f;
        loop {
            budget.check().map_err(budget_exceeded)?;
            self.failpoint(Window::ReaderCandidate);
            let lock = self
                .pins
                .as_ref()
                .map(|pins| PinLock::acquire(pins, candidate, budget))
                .transpose()?;
            self.failpoint(Window::ReaderLocked);
            let attempt = self.read(conn, move |tx| {
                let (floor, actual) = floor_and_latest(tx)?;
                if candidate < floor {
                    return Ok(Attempt::Retry {
                        candidate: actual.max(floor),
                        f,
                    });
                }
                self.failpoint(Window::ReaderInTransaction);
                let pin = self.load(tx, budget, Generation(actual), lock)?;
                f(tx, pin).map(Attempt::Pinned)
            })?;
            match attempt {
                Attempt::Pinned(out) => return Ok(out),
                Attempt::Retry {
                    candidate: next,
                    f: back,
                } => {
                    self.retried();
                    candidate = next;
                    f = back;
                }
            }
        }
    }

    /// The pinned generation's inputs as `tx` sees them: counted, charged to `budget`, then
    /// copied into exactly the reserved capacity (PC-17), with `lock` the pin's guard.
    fn load(
        &self,
        tx: &Transaction<'_>,
        budget: &QueryBudget,
        generation: Generation,
        lock: Option<PinLock>,
    ) -> Result<Pin> {
        budget.check().map_err(budget_exceeded)?;
        let plan = CopyPlan::count(tx, &self.kbx)?;
        let copy = budget.reserve(plan.charge).map_err(budget_exceeded)?;
        let kbx = self.kbx.clone();
        let extents = self.copy_extents(tx, budget, &plan.extents)?;
        let sediment = self.copy_sediment(tx, budget, &plan.sediment)?;
        budget.check().map_err(budget_exceeded)?;
        Ok(Pin {
            generation,
            kbx,
            mapping: Arc::clone(&self.mapping),
            extents,
            sediment,
            _copy: copy,
            _lock: lock,
        })
    }

    fn copy_sediment(
        &self,
        tx: &Transaction<'_>,
        budget: &QueryBudget,
        counts: &[usize; ExtentClass::COUNT],
    ) -> Result<ByClass<SedimentRow>> {
        let mut out = ByClass::with_capacities(counts);
        let mut stmt = tx
            .prepare("SELECT seq, class, payload FROM cat_sediment ORDER BY seq")
            .sql()?;
        let mut rows = stmt.query([]).sql()?;
        let mut n = 0;
        while let Some(r) = rows.next().sql()? {
            self.copying(budget, n)?;
            let class = class_of(r, 1).sql()?;
            let row = SedimentRow {
                seq: SedimentSeq(uint(r, 0).sql()?),
                class,
                payload: r.get(2).sql()?,
            };
            out.push(class, row)?;
            n += 1;
        }
        Ok(out)
    }

    fn copy_extents(
        &self,
        tx: &Transaction<'_>,
        budget: &QueryBudget,
        counts: &[usize; ExtentClass::COUNT],
    ) -> Result<ByClass<ExtentRef>> {
        let mut out = ByClass::with_capacities(counts);
        let mut stmt = tx
            .prepare(
                "SELECT class, generation, kbx_offset, kbx_len, relpath, id FROM cat_extent
                 WHERE dead_since IS NULL ORDER BY id",
            )
            .sql()?;
        let mut rows = stmt.query([]).sql()?;
        let mut n = 0;
        while let Some(r) = rows.next().sql()? {
            self.copying(budget, n)?;
            let class = class_of(r, 0).sql()?;
            let location = match r.get::<_, Option<String>>(4).sql()? {
                Some(relpath) => ExtentLocation::File { relpath },
                None => ExtentLocation::Kbx {
                    offset: uint(r, 2).sql()?,
                    len: uint(r, 3).sql()?,
                },
            };
            let row = ExtentRef {
                id: ExtentId(uint(r, 5).sql()?),
                class,
                generation: Generation(uint(r, 1).sql()?),
                location,
            };
            out.push(class, row)?;
            n += 1;
        }
        Ok(out)
    }

    /// Before copying row `n` of a loop: the budget check of every [`CHECK_EVERY`]th row.
    #[cfg_attr(
        not(test),
        expect(clippy::unused_self, reason = "a unit test observes the copy")
    )]
    fn copying(&self, budget: &QueryBudget, n: usize) -> Result<()> {
        #[cfg(test)]
        self.copied(n);
        if n.is_multiple_of(CHECK_EVERY) {
            budget.check().map_err(budget_exceeded)?;
        }
        Ok(())
    }
}

/// The counted rows of a pin and the charge of the allocations it will keep.
#[derive(Debug)]
struct CopyPlan {
    extents: [usize; ExtentClass::COUNT],
    sediment: [usize; ExtentClass::COUNT],
    charge: u64,
}

impl CopyPlan {
    /// Count what `tx` holds and charge every allocation of the copy by the budget's rule for
    /// one allocation ([`QueryBudget::allocation_charge`]): the container path's clone, one
    /// vector per class at its exact capacity, and each payload and cold path. Every copied
    /// string and blob is allocated at exactly its length.
    fn count(tx: &Transaction<'_>, kbx: &Path) -> Result<Self> {
        let mut plan = Self {
            extents: [0; ExtentClass::COUNT],
            sediment: [0; ExtentClass::COUNT],
            charge: allocation(kbx.as_os_str().len(), 1)?,
        };
        plan.count_class_rows(
            tx,
            "SELECT class, count(*), COALESCE(sum(length(payload)), 0),
                    COALESCE(sum(length(payload) > 0), 0)
             FROM cat_sediment GROUP BY class",
            Rows::Sediment,
        )?;
        plan.count_class_rows(
            tx,
            "SELECT class, count(*), COALESCE(sum(length(CAST(relpath AS BLOB))), 0),
                    COALESCE(sum(length(CAST(relpath AS BLOB)) > 0), 0)
             FROM cat_extent WHERE dead_since IS NULL GROUP BY class",
            Rows::Extents,
        )?;
        Ok(plan)
    }

    /// Add the rows `sql` counts per class (`class, rows, variable bytes, variable
    /// allocations`) to the plan.
    fn count_class_rows(&mut self, tx: &Transaction<'_>, sql: &str, rows: Rows) -> Result<()> {
        let mut stmt = tx.prepare(sql).sql()?;
        let mut counted = stmt.query([]).sql()?;
        while let Some(r) = counted.next().sql()? {
            let class = class_of(r, 0).sql()?;
            let n = usize::try_from(uint(r, 1).sql()?).map_err(too_large)?;
            let (bytes, allocations) = (uint(r, 2).sql()?, uint(r, 3).sql()?);
            let (slot, element) = match rows {
                Rows::Sediment => (
                    &mut self.sediment[class.index()],
                    (size_of::<SedimentRow>(), align_of::<SedimentRow>()),
                ),
                Rows::Extents => (
                    &mut self.extents[class.index()],
                    (size_of::<ExtentRef>(), align_of::<ExtentRef>()),
                ),
            };
            *slot = n;
            let vector = allocation(n.checked_mul(element.0).ok_or_else(overflow)?, element.1)?;
            let variable = allocations
                .checked_mul(ALLOCATION_METADATA_BYTES)
                .and_then(|m| m.checked_add(bytes))
                .ok_or_else(overflow)?;
            self.charge = self
                .charge
                .checked_add(vector)
                .and_then(|c| c.checked_add(variable))
                .ok_or_else(overflow)?;
        }
        Ok(())
    }
}

/// Which rows a count covers.
#[derive(Debug, Clone, Copy)]
enum Rows {
    Sediment,
    Extents,
}

/// The charge of one allocation of `bytes` at `align`; nothing for an empty one (no
/// allocation is made).
fn allocation(bytes: usize, align: usize) -> Result<u64> {
    if bytes == 0 {
        return Ok(0);
    }
    let bytes = u64::try_from(bytes).map_err(too_large)?;
    let align = u64::try_from(align).map_err(too_large)?;
    QueryBudget::allocation_charge(bytes, align).map_err(budget_exceeded)
}

fn overflow() -> StoreError {
    budget_exceeded(BudgetExceeded {
        limit: kaban_core::query_budget::Limit::Memory,
        value: u64::MAX,
    })
}

fn too_large(_: std::num::TryFromIntError) -> StoreError {
    overflow()
}

/// The class column `idx` of `row` names, read in place (no transient string).
fn class_of(row: &Row<'_>, idx: usize) -> rusqlite::Result<ExtentClass> {
    let name = row.get_ref(idx)?.as_str()?;
    ExtentClass::parse(name).ok_or_else(|| bad_enum(idx, name))
}

pub(super) fn budget_exceeded(e: BudgetExceeded) -> StoreError {
    StoreError::Backend {
        source: BackendSource::new(e),
    }
}

/// One round of [`Catalog::acquire`]: the closure's outcome, or the candidate to retry from
/// with the closure handed back.
enum Attempt<T, F> {
    Pinned(T),
    Retry { candidate: u64, f: F },
}

/// The latest generation `tx` sees.
fn latest_generation(tx: &Transaction<'_>) -> Result<u64> {
    tx.query_row(
        "SELECT COALESCE(max(generation), 0) FROM cat_generation",
        [],
        |r| uint(r, 0),
    )
    .sql()
}

/// `reclaim_floor` and the latest generation, in one statement of `tx`.
fn floor_and_latest(tx: &Transaction<'_>) -> Result<(u64, u64)> {
    tx.query_row(
        "SELECT (SELECT value FROM cat_meta WHERE key = 'reclaim_floor'),
                (SELECT COALESCE(max(generation), 0) FROM cat_generation)",
        [],
        |r| Ok((uint(r, 0)?, uint(r, 1)?)),
    )
    .sql()
}

/// A snapshot or pin asked of a connection that already has a transaction open: PC-8 pins
/// only from a fresh read transaction begun after its lock, never inside a competing outer
/// one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OuterTransaction;

impl std::fmt::Display for OuterTransaction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("catalog: a pin needs a connection without an open transaction")
    }
}

impl std::error::Error for OuterTransaction {}
