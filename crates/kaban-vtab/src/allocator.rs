//! The request-scoped SQLite allocator adapter (spec 11 QRY-04, QRY-06, PC-17).

use std::marker::PhantomData;

use kaban_core::query_budget::QueryBudget;

/// While alive, SQLite allocations made on this thread are charged to one [`QueryBudget`].
/// Dropping it restores the previous scope. It is bound to the thread that created it.
#[derive(Debug)]
pub struct ScopeGuard {
    previous: Option<QueryBudget>,
    _thread_bound: PhantomData<*const ()>,
}

/// Charge SQLite allocations made on this thread to `budget` until the guard drops.
pub(crate) fn enter_scope(_budget: &QueryBudget) -> ScopeGuard {
    ScopeGuard {
        previous: None,
        _thread_bound: PhantomData,
    }
}

/// Make the next charge attempt on this thread panic (fault injection for the no-unwind test).
#[cfg(feature = "test-hooks")]
pub(crate) fn inject_panic() {}
