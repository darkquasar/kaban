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
