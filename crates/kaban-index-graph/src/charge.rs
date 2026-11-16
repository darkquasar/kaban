//! Budgeted allocation (PC-17): every query-owned collection of this crate is reserved against
//! the caller's [`QueryBudget`] before it is allocated, and the charge lives as long as the
//! collection that owns it.

use std::cell::Cell;
use std::convert::Infallible;
use std::sync::atomic::{AtomicU64, Ordering};

use kaban_core::query_budget::{BudgetExceeded, BudgetLease, GrowError, QueryBudget};

use crate::error::GraphError;
use crate::view::BUDGET_CHECK_INTERVAL;

/// One growing charge: each [`Charge::vec`] reserves its capacity first, then allocates.
#[derive(Debug)]
pub(crate) struct Charge {
    lease: BudgetLease,
}

impl Charge {
    /// An empty charge on `budget`; refused when the deadline passed or the query was cancelled.
    pub(crate) fn new(budget: &QueryBudget) -> Result<Self, GraphError> {
        Ok(Self {
            lease: budget.reserve(0)?,
        })
    }

    /// A vector of exactly `capacity` elements, charged as its allocation (capacity rounded to
    /// the element alignment plus the allocation metadata) before it is allocated.
    pub(crate) fn vec<T>(&mut self, capacity: usize) -> Result<Vec<T>, GraphError> {
        let size = size_of::<T>();
        if capacity == 0 || size == 0 {
            return Ok(Vec::new());
        }
        let bytes = size
            .checked_mul(capacity)
            .and_then(|b| u64::try_from(b).ok())
            .ok_or(GraphError::BudgetExceeded {
                limit: "memory",
                value: u64::MAX,
            })?;
        let align = u64::try_from(align_of::<T>()).unwrap_or(u64::MAX);
        let charge = QueryBudget::allocation_charge(bytes, align)?;
        self.lease
            .grow_with(charge, || Ok::<_, Infallible>(Vec::with_capacity(capacity)))
            .map_err(|e| match e {
                GrowError::Budget(b) => GraphError::from(b),
                GrowError::Alloc(never) => match never {},
            })
    }

    /// The lease holding every charge made so far.
    pub(crate) fn into_lease(self) -> BudgetLease {
        self.lease
    }
}

/// A function result together with the budget charge of the memory it owns (PC-17).
///
/// The charge stays live while the `Charged` value (or the [`Hold`] taken from it with
/// [`Charged::into_parts`]) lives, and is released when it is dropped. Callers that convert the
/// rows keep the hold alive until the converted output is released, and reserve their own
/// conversions on the same budget.
#[derive(Debug)]
pub struct Charged<T> {
    value: T,
    hold: Hold,
}

/// The budget charge of a [`Charged`] value; dropping it releases the bytes.
#[derive(Debug)]
pub struct Hold {
    leases: [Option<BudgetLease>; 2],
}

impl Hold {
    pub(crate) fn new(first: BudgetLease, second: Option<BudgetLease>) -> Self {
        Self {
            leases: [Some(first), second],
        }
    }

    /// The bytes this hold keeps charged.
    #[must_use]
    pub fn bytes(&self) -> u64 {
        self.leases
            .iter()
            .flatten()
            .map(BudgetLease::bytes)
            .fold(0, u64::saturating_add)
    }
}

impl<T> Charged<T> {
    pub(crate) fn new(value: T, hold: Hold) -> Self {
        Self { value, hold }
    }

    /// The result.
    #[must_use]
    pub fn value(&self) -> &T {
        &self.value
    }

    /// The bytes charged for the result.
    #[must_use]
    pub fn bytes(&self) -> u64 {
        self.hold.bytes()
    }

    /// The result and its charge, to be kept alive while the result (or a copy made from it)
    /// is in use.
    #[must_use]
    pub fn into_parts(self) -> (T, Hold) {
        (self.value, self.hold)
    }
}

impl<T> std::ops::Deref for Charged<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.value
    }
}

/// A growing query-owned vector (PC-17): every growth reserves the new buffer on the budget
/// before allocating it, and the old buffer's charge is released once that buffer is freed.
/// The charge always covers the live capacity.
#[derive(Debug)]
pub(crate) struct Grow<'b, T> {
    budget: &'b QueryBudget,
    items: Vec<T>,
    lease: BudgetLease,
}

/// The charge of a buffer of `capacity` elements of `T`: its bytes rounded to the alignment plus
/// the allocation metadata.
fn buffer_charge<T>(capacity: usize) -> Result<u64, GraphError> {
    let overflow = GraphError::BudgetExceeded {
        limit: "memory",
        value: u64::MAX,
    };
    let bytes = size_of::<T>()
        .checked_mul(capacity)
        .and_then(|b| u64::try_from(b).ok())
        .ok_or(overflow)?;
    let align = u64::try_from(align_of::<T>()).unwrap_or(u64::MAX);
    Ok(QueryBudget::allocation_charge(bytes, align)?)
}

impl<'b, T> Grow<'b, T> {
    /// An empty vector on `budget`; refused when the deadline passed or the query was cancelled.
    pub(crate) fn new(budget: &'b QueryBudget) -> Result<Self, GraphError> {
        Ok(Self {
            budget,
            items: Vec::new(),
            lease: budget.reserve(0)?,
        })
    }

    /// An empty vector with room for `capacity` elements, reserved first.
    pub(crate) fn with_capacity(
        budget: &'b QueryBudget,
        capacity: usize,
    ) -> Result<Self, GraphError> {
        let mut grow = Self::new(budget)?;
        grow.reserve(capacity)?;
        Ok(grow)
    }

    /// Room for `extra` more elements; grows by doubling, reserving before allocating.
    pub(crate) fn reserve(&mut self, extra: usize) -> Result<(), GraphError> {
        let need = self
            .items
            .len()
            .checked_add(extra)
            .ok_or(GraphError::BudgetExceeded {
                limit: "memory",
                value: u64::MAX,
            })?;
        if need <= self.items.capacity() {
            return Ok(());
        }
        let capacity = need.max(self.items.capacity().saturating_mul(2)).max(4);
        let lease = self.budget.reserve(buffer_charge::<T>(capacity)?)?;
        let mut items = Vec::with_capacity(capacity);
        items.append(&mut self.items);
        // The old buffer is freed here, then its charge is released with the old lease.
        self.items = items;
        self.lease = lease;
        Ok(())
    }

    /// Append `item`, growing first when full.
    pub(crate) fn push(&mut self, item: T) -> Result<(), GraphError> {
        self.reserve(1)?;
        self.items.push(item);
        Ok(())
    }

    /// Keep the first `len` elements (the capacity and its charge stay).
    pub(crate) fn truncate(&mut self, len: usize) {
        self.items.truncate(len);
    }

    /// The elements and the lease charging their buffer.
    pub(crate) fn into_parts(self) -> (Vec<T>, BudgetLease) {
        (self.items, self.lease)
    }
}

impl<T: PartialEq> Grow<'_, T> {
    /// Remove consecutive equal elements.
    pub(crate) fn dedup(&mut self) {
        self.items.dedup();
    }
}

impl<T> std::ops::Deref for Grow<'_, T> {
    type Target = [T];

    fn deref(&self) -> &[T] {
        &self.items
    }
}

impl<T> std::ops::DerefMut for Grow<'_, T> {
    fn deref_mut(&mut self) -> &mut [T] {
        &mut self.items
    }
}

/// Metered work units done in this process so far (instrumentation): every unit counted by a
/// budgeted [`Ticker`], published at its interval boundaries and when it is dropped.
static WORK_UNITS: AtomicU64 = AtomicU64::new(0);

/// Metered units of query work done in this process so far, for tests and diagnostics of the
/// bounded-work contract (PC-17).
///
/// A unit is one bounded step of query work: one source entry advanced or compared in a merge,
/// one bitvector word visited, one cursor prepared, one loop iteration of a structural function,
/// one decoded record, one validated entry or one sort step. Every budgeted loop of this crate
/// counts its units here; the count is published every [`BUDGET_CHECK_INTERVAL`] units and when
/// the counting loop ends, so it can lag the work by less than one interval per live loop. The
/// counter is process-wide: concurrent queries add to the same count.
#[must_use]
pub fn work_units() -> u64 {
    WORK_UNITS.load(Ordering::SeqCst)
}

/// Counts units of work and checks the budget once every [`BUDGET_CHECK_INTERVAL`] units,
/// starting before the first unit. A ticker without a budget (writer paths) neither checks nor
/// publishes.
///
/// Iterator sources that cannot return an error use [`Ticker::step`]: once a check fails the
/// ticker remembers the breach, every later step refuses, and the owning loop reports the breach
/// from [`Ticker::stopped`].
#[derive(Debug)]
pub(crate) struct Ticker<'b> {
    budget: Option<&'b QueryBudget>,
    units: Cell<u64>,
    published: Cell<u64>,
    failed: Cell<Option<BudgetExceeded>>,
}

impl<'b> Ticker<'b> {
    pub(crate) fn new(budget: &'b QueryBudget) -> Self {
        Self::maybe(Some(budget))
    }

    /// A ticker on `budget`, or an unmetered one for writer paths.
    pub(crate) fn maybe(budget: Option<&'b QueryBudget>) -> Self {
        Self {
            budget,
            units: Cell::new(0),
            published: Cell::new(0),
            failed: Cell::new(None),
        }
    }

    /// An unmetered ticker (writer paths): it never refuses.
    pub(crate) fn unmetered() -> Ticker<'static> {
        Ticker::maybe(None)
    }

    /// One unit of work; at every interval boundary (and before the first unit) the budget is
    /// checked.
    pub(crate) fn tick(&self) -> Result<(), GraphError> {
        if let Some(e) = self.failed.get() {
            return Err(e.into());
        }
        let units = self.units.get();
        if units.is_multiple_of(BUDGET_CHECK_INTERVAL) {
            self.publish();
            if let Some(budget) = self.budget
                && let Err(e) = budget.check()
            {
                self.failed.set(Some(e));
                return Err(e.into());
            }
        }
        self.units.set(units + 1);
        Ok(())
    }

    /// [`Ticker::tick`] for sources that cannot return an error: `false` once the budget refused.
    pub(crate) fn step(&self) -> bool {
        self.tick().is_ok()
    }

    /// The breach a failed step recorded, if any.
    pub(crate) fn stopped(&self) -> Result<(), GraphError> {
        match self.failed.get() {
            Some(e) => Err(e.into()),
            None => Ok(()),
        }
    }

    fn publish(&self) {
        if self.budget.is_some() {
            let units = self.units.get();
            WORK_UNITS.fetch_add(units - self.published.get(), Ordering::SeqCst);
            self.published.set(units);
        }
    }
}

impl Drop for Ticker<'_> {
    fn drop(&mut self) {
        self.publish();
    }
}
