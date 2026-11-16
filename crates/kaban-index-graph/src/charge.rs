//! Budgeted allocation (PC-17): every query-owned collection of this crate is reserved against
//! the caller's [`QueryBudget`] before it is allocated, and the charge lives as long as the
//! collection that owns it.

use std::convert::Infallible;

use kaban_core::query_budget::{BudgetLease, GrowError, QueryBudget};

use crate::error::GraphError;

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

impl<T: Default> Charged<T> {
    /// An empty result holding no bytes.
    pub(crate) fn empty(budget: &QueryBudget) -> Result<Self, GraphError> {
        Ok(Self::new(T::default(), Hold::new(budget.reserve(0)?, None)))
    }
}
