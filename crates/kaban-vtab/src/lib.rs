//! SQLite table-valued functions for the unified query surface (spec 11 QRY-07, QRY-08).
//!
//! A [`TableFunction`] is plain safe Rust: it declares its columns, costs candidate plans and
//! produces rows. [`register`] installs it on a connection as an eponymous-only virtual table.
//! This crate is the workspace's sole `unsafe` exception (see `docs/developer/SAFETY.md`): the
//! `unsafe impl` blocks that meet rusqlite's virtual-table contracts live in the private
//! `adapter` module.

#![deny(unsafe_op_in_unsafe_fn)]

// Modules are private; the supported surface is the flat re-export list below.
mod adapter;
mod plan;
mod value;

pub use adapter::{FnError, Rows, TableFunction, register};
pub use plan::{Arg, BoundArg, ColumnDef, ColumnKind, OrderTerm, Plan, PlanInput};
pub use value::Value;
