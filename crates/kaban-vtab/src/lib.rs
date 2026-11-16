//! SQLite table-valued functions for the unified query surface (spec 11 QRY-07, QRY-08).
//!
//! A [`TableFunction`] is plain safe Rust: it declares its columns, costs candidate plans and
//! produces rows. [`register`] installs it on a connection as an eponymous-only virtual table.
//! A [`RequestScope`] charges a request's SQLite allocations and table-function scans to one
//! `QueryBudget`; scans outside a scope are refused. The allocator adapter behind it is
//! installed once per process, by [`initialize_for_embedding`] or, with the `process-bootstrap`
//! feature, by the `bootstrap` module's startup constructor before `main`.
//!
//! This crate is the workspace's sole `unsafe` exception (see `docs/developer/SAFETY.md`): the
//! virtual-table glue, the allocator adapter, the embedding initializer, the request scope's
//! connection settings and the startup constructor.

#![deny(unsafe_op_in_unsafe_fn)]

// Modules are private; the supported surface is the flat re-export list below.
mod adapter;
mod allocator;
#[cfg(feature = "process-bootstrap")]
pub mod bootstrap;
mod budget_scope;
mod plan;
mod runtime;
mod value;

pub use adapter::{FnError, Rows, TableFunction, register};
pub use budget_scope::{QueryConnection, QueryError, RequestScope, ScopeActive};
pub use plan::{Arg, BoundArg, ColumnDef, ColumnKind, OrderTerm, Plan, PlanInput};
pub use runtime::{InitError, Runtime, initialize_for_embedding};
pub use value::Value;

/// Test-only access to the allocator scope seam. Not a supported API: the request-scoped budget
/// scope drives the same seam in production.
#[doc(hidden)]
pub mod test_hooks {
    use kaban_core::query_budget::QueryBudget;

    pub use crate::allocator::ScopeGuard;

    /// Charge SQLite allocations made on this thread to `budget` until the guard drops.
    #[must_use]
    pub fn scope(budget: &QueryBudget) -> ScopeGuard {
        crate::allocator::enter_scope(budget)
    }

    /// Make the next charge attempt on this thread panic inside the allocator callback.
    #[cfg(feature = "test-hooks")]
    pub fn panic_on_next_charge() {
        crate::allocator::inject_panic();
    }
}
