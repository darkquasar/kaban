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
pub use plan::{
    Arg, BoundArg, BoundConstraint, ColumnDef, ColumnKind, ConstraintOp, MAX_CONSTRAINTS,
    OrderTerm, Plan, PlanInput, ResultConstraint, Scan,
};
pub use runtime::{InitError, Runtime, initialize_for_embedding};
pub use value::Value;

/// Fault injection for the allocator tests (feature `test-hooks`, never enabled by a production
/// build). Not a supported API.
#[cfg(feature = "test-hooks")]
#[doc(hidden)]
pub mod test_hooks {
    /// Make the next charge attempt on this thread panic inside the allocator callback.
    pub fn panic_on_next_charge() {
        crate::allocator::inject_panic();
    }
}
