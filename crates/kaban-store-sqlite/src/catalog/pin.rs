//! Pins (spec 11 PC-8, INV-3, 7.5): a reader's view of one catalog generation, race-free against
//! reclamation. [`Catalog::with_snapshot`] evaluates inside the pin's transaction;
//! [`Catalog::pin`] loads the views it is asked for and returns a detached pin.
//!
//! A pin copies no index input when it is taken. A view's inputs (its extent chain and, for
//! `main`, the pending sediment) load on first touch ([`Pin::load_view`]) inside the pin's read
//! transaction and stay cached in the pin for its lifetime. Each load is charged to the pin's
//! budget before it is made (PC-17): the rows are counted first, every allocation the view
//! keeps (the view itself, its name, one vector per kind of row, each payload and cold path) is
//! reserved at its exact capacity, and only then copied into that capacity, never past it. The
//! pin itself charges its container path and shared lock. Loading obeys the budget's deadline:
//! the copy loops check it at bounded intervals, and a cancelled or expired budget turns any
//! outcome into its error. A long statement is interrupted by the connection's own progress
//! handler (a query connection's), which a pin leaves in place.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use kaban_core::query_budget::{
    ALLOCATION_METADATA_BYTES, BudgetExceeded, BudgetLease, QueryBudget,
};
use kaban_core::{BackendSource, Result, StoreError, ViewName};
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

/// The one allocation of a pin's shared lock (`Arc`: its two counts and the lock), as
/// `(bytes, alignment)`: charged with the pin.
const SHARED_LOCK: (usize, usize) = (
    2 * size_of::<usize>() + size_of::<PinLock>(),
    align_of::<PinLock>(),
);

/// The one allocation of a loaded view (`Arc`: its two counts and the view), as `(bytes,
/// alignment)`: charged with the view.
const VIEW: (usize, usize) = (
    2 * size_of::<usize>() + size_of::<PinnedView>(),
    align_of::<PinnedView>(),
);

/// How many views a pin's cache holds before it first grows.
const FIRST_VIEWS: usize = 4;

/// One pinned generation: the container its extents live in, and the views loaded so far.
#[derive(Debug)]
pub(crate) struct Pin {
    generation: Generation,
    /// The candidate generation whose lock the pin holds (PC-8), at or below `generation`.
    candidate: u64,
    kbx: PathBuf,
    /// The container's mapping, shared with the catalog's other pins (excluded from the
    /// charge: a shared immutable mapping, PC-17). It holds no pin lock; only the bytes
    /// [`Pin::bytes`] hands out do.
    mapping: Arc<Mapping>,
    /// The budget every view load is charged to.
    budget: QueryBudget,
    views: Mutex<Views>,
    /// The charge of the container path and the shared lock, released with the pin.
    _base: BudgetLease,
    /// The lock at the candidate generation that keeps reclamation off what the pin can
    /// reach (PC-8), shared with every byte span the pin hands out; `None` for an in-memory
    /// database, which has no container.
    lock: Option<Arc<PinLock>>,
    /// A unit test's observer of every row a load copies.
    #[cfg(test)]
    copied: Option<CopyHook>,
}

/// The views a pin loaded, and the charge of the buffer that holds them.
#[derive(Debug, Default)]
struct Views {
    loaded: Vec<Arc<PinnedView>>,
    _buffer: Option<BudgetLease>,
}

/// One view's inputs as a pin loaded them: its extent chain, oldest first, and its pending
/// sediment rows, in sequence order. Its charge is released when the last holder drops it.
#[derive(Debug)]
pub(crate) struct PinnedView {
    name: Box<str>,
    chain: Vec<ExtentRef>,
    sediment: Vec<SedimentRow>,
    _copy: BudgetLease,
}

impl PinnedView {
    /// The view's extent chain, oldest first.
    pub(crate) fn chain(&self) -> &[ExtentRef] {
        &self.chain
    }
    /// The view's pending sediment rows, in sequence order (only `main` has any: the commit
    /// path stages into the memory view).
    pub(crate) fn sediment(&self) -> &[SedimentRow] {
        &self.sediment
    }
}

#[expect(
    dead_code,
    reason = "the index readers (T7, T11, T16) read the pinned extents"
)]
impl Pin {
    /// The pinned generation (the actual generation h of PC-8).
    pub(crate) fn generation(&self) -> Generation {
        self.generation
    }

    /// The inputs of `view` at this pin's generation (spec 11 7.5): on first touch they are
    /// read through `conn`, which must be inside the pin's own read transaction (the one
    /// [`Catalog::with_snapshot`] hands its closure), charged to the pin's budget and cached
    /// in the pin for its lifetime; a later call returns the cached view and reads nothing.
    ///
    /// # Errors
    /// [`OutsidePin`] when `conn` is not inside a transaction at this pin's generation; the
    /// budget's or SQLite's error.
    pub(crate) fn load_view(&self, conn: &Connection, view: &ViewName) -> Result<Arc<PinnedView>> {
        let name = view.to_string();
        let mut views = self.views.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(loaded) = views.loaded.iter().find(|v| *v.name == *name) {
            return Ok(Arc::clone(loaded));
        }
        self.inside(conn)?;
        self.budget.check().map_err(budget_exceeded)?;
        let plan = ViewPlan::count(conn, &name, *view == ViewName::Main)?;
        // the cache's buffer grows before the view joins it: its new capacity is charged too
        let grown = (views.loaded.len() == views.loaded.capacity())
            .then(|| (views.loaded.capacity() * 2).max(FIRST_VIEWS));
        let buffer = match grown {
            Some(capacity) => {
                let bytes = capacity
                    .checked_mul(size_of::<Arc<PinnedView>>())
                    .ok_or_else(overflow)?;
                let charge = allocation(bytes, align_of::<Arc<PinnedView>>())?;
                Some(self.budget.reserve(charge).map_err(budget_exceeded)?)
            }
            None => None,
        };
        let copy = self.budget.reserve(plan.charge).map_err(budget_exceeded)?;
        let chain = self.copy_chain(conn, &name, plan.chain)?;
        let sediment = if plan.sediment > 0 {
            self.copy_sediment(conn, plan.sediment)?
        } else {
            Vec::new()
        };
        self.budget.check().map_err(budget_exceeded)?;
        let loaded = Arc::new(PinnedView {
            name: name.into_boxed_str(),
            chain,
            sediment,
            _copy: copy,
        });
        if let (Some(capacity), Some(lease)) = (grown, buffer) {
            let mut loaded = Vec::with_capacity(capacity);
            loaded.append(&mut views.loaded);
            *views = Views {
                loaded,
                _buffer: Some(lease),
            };
        }
        views.loaded.push(Arc::clone(&loaded));
        Ok(loaded)
    }

    /// The bytes of `e`, an extent of a view this pin loaded: a slice of the container's
    /// mapping, re-mapped when the span lies beyond it, bounds-checked against the file. The
    /// bytes carry the pin's lock: the extent stays protected from reclamation for as long as
    /// any clone or slice of them lives, after the pin itself is dropped. Protection is not
    /// snapshot validity: once [`Catalog::with_snapshot`] returns, its SQL transaction is
    /// over, and bytes that escaped it keep only their extent's protection.
    ///
    /// # Errors
    /// `kbx::ContainerError` for an extent no loaded view of this pin holds, a span beyond the
    /// container or a container that cannot be mapped, and for a cold extent
    /// (`ExtentLocation::File`), which this stream never reads.
    pub(crate) fn bytes(&self, e: &ExtentRef) -> Result<OwnedBytes> {
        if !self.holds(e) {
            return Err(failed(
                "read an extent this pin does not hold from",
                &self.kbx,
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("extent {}", e.id.0),
                ),
            ));
        }
        let span = match &e.location {
            ExtentLocation::Kbx { offset, len } => self.mapping.span(&self.kbx, *offset, *len)?,
            ExtentLocation::File { relpath } => {
                return Err(failed(
                    "read a cold extent outside",
                    &self.kbx,
                    std::io::Error::new(std::io::ErrorKind::Unsupported, relpath.clone()),
                ));
            }
        };
        Ok(match &self.lock {
            Some(lock) => kaban_vtab::retain_owner(span, Arc::clone(lock)),
            None => span,
        })
    }

    /// Whether `e` is in the chain of a view this pin loaded.
    fn holds(&self, e: &ExtentRef) -> bool {
        let views = self.views.lock().unwrap_or_else(PoisonError::into_inner);
        views.loaded.iter().any(|v| v.chain.contains(e))
    }

    /// The container's path.
    pub(crate) fn kbx_path(&self) -> &Path {
        &self.kbx
    }

    /// Refuse `conn` unless it is inside a read transaction at this pin's generation that the
    /// pin's lock still guards (the floor has not passed its candidate): the pin's own.
    fn inside(&self, conn: &Connection) -> Result<()> {
        if conn.is_autocommit() {
            return Err(outside());
        }
        let (floor, actual) = floor_and_latest(conn)?;
        if actual != self.generation.0 || self.candidate < floor {
            return Err(outside());
        }
        Ok(())
    }

    fn copy_sediment(&self, conn: &Connection, counted: usize) -> Result<Vec<SedimentRow>> {
        let mut out = Vec::with_capacity(counted);
        let mut stmt = conn
            .prepare("SELECT seq, class, payload FROM cat_sediment ORDER BY seq")
            .sql()?;
        let mut rows = stmt.query([]).sql()?;
        while let Some(r) = rows.next().sql()? {
            self.copying(out.len())?;
            let row = SedimentRow {
                seq: SedimentSeq(uint(r, 0).sql()?),
                class: class_of(r, 1).sql()?,
                payload: r.get(2).sql()?,
            };
            within(&mut out, counted, row)?;
        }
        Ok(out)
    }

    fn copy_chain(&self, conn: &Connection, view: &str, counted: usize) -> Result<Vec<ExtentRef>> {
        let mut out = Vec::with_capacity(counted);
        let mut stmt = conn
            .prepare(
                "SELECT e.class, e.generation, e.kbx_offset, e.kbx_len, e.relpath, e.id
                 FROM cat_view_extent v JOIN cat_extent e ON e.id = v.extent_id
                 WHERE v.view = ?1 AND e.dead_since IS NULL ORDER BY v.position",
            )
            .sql()?;
        let mut rows = stmt.query([view]).sql()?;
        while let Some(r) = rows.next().sql()? {
            self.copying(out.len())?;
            let location = match r.get::<_, Option<String>>(4).sql()? {
                Some(relpath) => ExtentLocation::File { relpath },
                None => ExtentLocation::Kbx {
                    offset: uint(r, 2).sql()?,
                    len: uint(r, 3).sql()?,
                },
            };
            let row = ExtentRef {
                id: ExtentId(uint(r, 5).sql()?),
                class: class_of(r, 0).sql()?,
                generation: Generation(uint(r, 1).sql()?),
                location,
            };
            within(&mut out, counted, row)?;
        }
        Ok(out)
    }

    /// Before copying row `n` of a loop: the budget check of every [`CHECK_EVERY`]th row.
    fn copying(&self, n: usize) -> Result<()> {
        #[cfg(test)]
        if let Some(hook) = &self.copied {
            (hook.0)(n);
        }
        if n.is_multiple_of(CHECK_EVERY) {
            self.budget.check().map_err(budget_exceeded)?;
        }
        Ok(())
    }
}

/// A unit test's observer of the rows a load copies, as the pin keeps it.
#[cfg(test)]
#[derive(Clone)]
pub(crate) struct CopyHook(pub(crate) Arc<dyn Fn(usize) + Send + Sync>);

#[cfg(test)]
impl std::fmt::Debug for CopyHook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CopyHook")
    }
}

#[cfg(test)]
impl Pin {
    /// Every heap allocation the pin retains, as `(capacity in bytes, alignment)`: what a unit
    /// test checks the charge against.
    pub(crate) fn retained_allocations(&self) -> Vec<(usize, usize)> {
        let mut out = vec![(self.kbx.capacity(), 1)];
        if self.lock.is_some() {
            out.push(SHARED_LOCK);
        }
        let views = self.views.lock().unwrap_or_else(PoisonError::into_inner);
        out.push((
            views.loaded.capacity() * size_of::<Arc<PinnedView>>(),
            align_of::<Arc<PinnedView>>(),
        ));
        for v in &views.loaded {
            out.push(VIEW);
            out.push((v.name.len(), 1));
            out.push((
                v.chain.capacity() * size_of::<ExtentRef>(),
                align_of::<ExtentRef>(),
            ));
            for e in &v.chain {
                if let ExtentLocation::File { relpath } = &e.location {
                    out.push((relpath.capacity(), 1));
                }
            }
            out.push((
                v.sediment.capacity() * size_of::<SedimentRow>(),
                align_of::<SedimentRow>(),
            ));
            out.extend(v.sediment.iter().map(|r| (r.payload.capacity(), 1)));
        }
        out.retain(|(bytes, _)| *bytes > 0);
        out
    }
}

/// Add `row` to `rows` within the `counted` rows reserved for them. A row past them means the
/// snapshot changed after it was counted: refused, the copy never grows past its reservation.
fn within<T>(rows: &mut Vec<T>, counted: usize, row: T) -> Result<()> {
    if rows.len() >= counted {
        return Err(StoreError::Backend {
            source: BackendSource::new(std::io::Error::other(
                "the pinned snapshot holds more rows than it counted",
            )),
        });
    }
    rows.push(row);
    Ok(())
}

/// `out`, unless the budget is cancelled, expired or exceeded: then the budget's error,
/// whatever the outcome was (an interrupted statement's error included).
fn settle<T>(budget: &QueryBudget, out: Result<T>) -> Result<T> {
    budget.check().map_err(budget_exceeded)?;
    out
}

impl Catalog {
    /// Pin by PC-8 and run `f` with the pin's own read transaction, held open while `f`
    /// evaluates rows and indexes; the transaction ends on every exit, and the pin lock with
    /// it unless bytes taken from the pin ([`Pin::bytes`]) still live: those keep the lock,
    /// and so their extents' protection from reclamation, but not the snapshot. The pin
    /// copies nothing until `f` loads a view ([`Pin::load_view`] with the transaction). `conn`
    /// is borrowed shared, as [`Catalog::read`] borrows it. Acquisition and execution share
    /// `budget`: once it is cancelled or past its deadline the budget's error is the outcome,
    /// even when `f` returned `Ok`. Interrupting a running statement is the connection's own
    /// progress handler's (a query connection installs one, `kaban_vtab`): the snapshot
    /// leaves it in place and installs none.
    ///
    /// # Errors
    /// [`OuterTransaction`] when `conn` already has a transaction open; the closure's, the
    /// budget's, the pin lock's or SQLite's error.
    pub(crate) fn with_snapshot<T>(
        &self,
        conn: &Connection,
        budget: &QueryBudget,
        f: impl FnOnce(&Transaction<'_>, &Pin) -> Result<T>,
    ) -> Result<T> {
        let out = self.acquire(conn, budget, |tx, pin| {
            let out = f(tx, &pin);
            drop(pin);
            settle(budget, out)
        });
        settle(budget, out)
    }

    /// Pin as [`Self::with_snapshot`] does, load each of `views` under `budget`, end the
    /// transaction and return the detached pin. It keeps its candidate's pin lock (and the
    /// container's mapping) until it is dropped, after the store that took it if need be, and
    /// bytes taken from it keep that lock in the same way while any of them lives; it serves
    /// no statement that reads live rows, and loads no other view.
    ///
    /// # Errors
    /// [`OuterTransaction`] when `conn` already has a transaction open; the budget's, the pin
    /// lock's or SQLite's error.
    pub(crate) fn pin(
        &self,
        conn: &Connection,
        budget: &QueryBudget,
        views: &[ViewName],
    ) -> Result<Pin> {
        let out = self.acquire(conn, budget, |tx, pin| {
            for view in views {
                pin.load_view(tx, view)?;
            }
            Ok(pin)
        });
        settle(budget, out)
    }

    /// PC-8 acquisition, then `f` over the pin and the transaction it was built from:
    ///
    /// 1. read a candidate generation g;
    /// 2. lock `kaban.kbx.pins/<g>` shared ([`PinLock`]);
    /// 3. begin a fresh read transaction and read `reclaim_floor` F and the actual generation
    ///    h in it (its first read, so its snapshot starts after the lock);
    /// 4. `g < F`: a reclamation passed the candidate before the lock, so its punch may have
    ///    destroyed what g saw: end the transaction, unlock and retry from h; otherwise build
    ///    the pin in that transaction ([`Self::new_pin`]), reporting h and keeping the lock at
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
                let pin = self.new_pin(budget, Generation(actual), candidate, lock)?;
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

    /// A pin at `generation` holding `lock` at `candidate`, with no view loaded: its container
    /// path and shared lock charged to `budget` (PC-17).
    fn new_pin(
        &self,
        budget: &QueryBudget,
        generation: Generation,
        candidate: u64,
        lock: Option<PinLock>,
    ) -> Result<Pin> {
        budget.check().map_err(budget_exceeded)?;
        let shared_lock = match lock {
            Some(_) => allocation(SHARED_LOCK.0, SHARED_LOCK.1)?,
            None => 0,
        };
        let charge = allocation(self.kbx.as_os_str().len(), 1)?
            .checked_add(shared_lock)
            .ok_or_else(overflow)?;
        let base = budget.reserve(charge).map_err(budget_exceeded)?;
        Ok(Pin {
            generation,
            candidate,
            kbx: self.kbx.clone(),
            mapping: Arc::clone(&self.mapping),
            budget: budget.clone(),
            views: Mutex::default(),
            _base: base,
            lock: lock.map(Arc::new),
            #[cfg(test)]
            copied: self.copy_hook(),
        })
    }
}

/// The counted rows of one view and the charge of the allocations it will keep.
#[derive(Debug)]
struct ViewPlan {
    chain: usize,
    sediment: usize,
    charge: u64,
}

impl ViewPlan {
    /// Count what `conn` holds of the view `name` (its pending sediment too when
    /// `with_sediment`) and charge every allocation of its copy by the budget's rule for one
    /// allocation ([`QueryBudget::allocation_charge`]): the view itself, its name, one vector
    /// per kind of row at its exact capacity, and each payload and cold path. Every copied
    /// string and blob is allocated at exactly its length.
    fn count(conn: &Connection, name: &str, with_sediment: bool) -> Result<Self> {
        let (chain, chain_charge) = count_rows(
            conn,
            "SELECT count(*), COALESCE(sum(length(CAST(e.relpath AS BLOB))), 0),
                    COALESCE(sum(length(CAST(e.relpath AS BLOB)) > 0), 0)
             FROM cat_view_extent v JOIN cat_extent e ON e.id = v.extent_id
             WHERE v.view = ?1 AND e.dead_since IS NULL",
            [name],
            (size_of::<ExtentRef>(), align_of::<ExtentRef>()),
        )?;
        let (sediment, sediment_charge) = if with_sediment {
            count_rows(
                conn,
                "SELECT count(*), COALESCE(sum(length(payload)), 0),
                        COALESCE(sum(length(payload) > 0), 0)
                 FROM cat_sediment",
                [],
                (size_of::<SedimentRow>(), align_of::<SedimentRow>()),
            )?
        } else {
            (0, 0)
        };
        let charge = [
            allocation(VIEW.0, VIEW.1)?,
            allocation(name.len(), 1)?,
            chain_charge,
            sediment_charge,
        ]
        .into_iter()
        .try_fold(0_u64, u64::checked_add)
        .ok_or_else(overflow)?;
        Ok(Self {
            chain,
            sediment,
            charge,
        })
    }
}

/// The rows `sql` counts (`rows, variable bytes, variable allocations`) and the charge of
/// their copy: one vector of `element`s at its exact capacity plus the variable allocations.
fn count_rows(
    conn: &Connection,
    sql: &str,
    params: impl rusqlite::Params,
    element: (usize, usize),
) -> Result<(usize, u64)> {
    let (n, bytes, allocations) = conn
        .query_row(sql, params, |r| Ok((uint(r, 0)?, uint(r, 1)?, uint(r, 2)?)))
        .sql()?;
    let n = usize::try_from(n).map_err(too_large)?;
    let vector = allocation(n.checked_mul(element.0).ok_or_else(overflow)?, element.1)?;
    let variable = allocations
        .checked_mul(ALLOCATION_METADATA_BYTES)
        .and_then(|m| m.checked_add(bytes))
        .ok_or_else(overflow)?;
    let charge = vector.checked_add(variable).ok_or_else(overflow)?;
    Ok((n, charge))
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

/// `reclaim_floor` and the latest generation, in one statement on `conn`.
fn floor_and_latest(conn: &Connection) -> Result<(u64, u64)> {
    conn.query_row(
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

/// A view asked of a pin through a connection outside the pin's own transaction (none open,
/// or one at another generation): a view loads only inside the snapshot it belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OutsidePin;

impl std::fmt::Display for OutsidePin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("catalog: a view loads only inside its pin's own transaction")
    }
}

impl std::error::Error for OutsidePin {}

fn outside() -> StoreError {
    StoreError::Backend {
        source: BackendSource::new(OutsidePin),
    }
}
