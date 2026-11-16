//! Budgeted allocation (PC-17): every query-owned collection of this crate is reserved against
//! the caller's [`QueryBudget`] before it is allocated, and the charge lives as long as the
//! collection that owns it.

use std::convert::Infallible;

use kaban_core::query_budget::{BudgetLease, GrowError, QueryBudget};

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

/// Checks the budget once every [`BUDGET_CHECK_INTERVAL`] units of work of a loop that does not
/// read through the accessor (which checks on its own).
#[derive(Debug)]
pub(crate) struct Ticker<'b> {
    budget: &'b QueryBudget,
    work: u64,
}

impl<'b> Ticker<'b> {
    pub(crate) fn new(budget: &'b QueryBudget) -> Self {
        Self { budget, work: 0 }
    }

    /// One unit of work; at every interval boundary the budget is checked.
    pub(crate) fn tick(&mut self) -> Result<(), GraphError> {
        self.work += 1;
        if self.work.is_multiple_of(BUDGET_CHECK_INTERVAL) {
            self.budget.check()?;
        }
        Ok(())
    }
}
