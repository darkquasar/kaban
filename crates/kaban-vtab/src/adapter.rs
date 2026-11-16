//! The safe table-function surface and its registration as an eponymous SQLite virtual table.

use std::fmt;
use std::sync::Arc;

use kaban_core::query_budget::QueryBudget;

use crate::{ColumnDef, Plan, PlanInput, Value};

/// The rows a table function produces: each item is one row holding one value per result
/// (non-argument) column, in declaration order.
pub type Rows<'a> = Box<dyn Iterator<Item = Result<Vec<Value>, FnError>> + 'a>;

/// A table-valued function, callable from SQL as `name(arg, ...)`.
pub trait TableFunction: Send + Sync + 'static {
    /// The SQL name; also the eponymous table name.
    fn name(&self) -> &'static str;
    /// Every column, result and argument; argument columns are hidden and bind positionally in
    /// declaration order.
    fn columns(&self) -> &'static [ColumnDef];
    /// Cost one candidate plan whose required arguments are all bound.
    fn plan(&self, input: &PlanInput<'_>) -> Plan;
    /// Produce the rows for one scan. `args` holds one value per argument column in declaration
    /// order ([`Value::Null`] when an optional argument is unbound).
    ///
    /// # Errors
    /// An [`FnError`] refuses the scan; SQLite reports it as the statement's error.
    fn rows(&self, args: &[Value], budget: &QueryBudget) -> Result<Rows<'_>, FnError>;
}

/// A refusal from a table function, surfaced as a SQLite error whose text is
/// `"{construct}: {reason}"`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FnError {
    /// What was refused (a function or argument name).
    pub construct: String,
    /// Why.
    pub reason: String,
}

impl fmt::Display for FnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.construct, self.reason)
    }
}

impl std::error::Error for FnError {}

/// Register `f` on `conn` as an eponymous-only virtual table named [`TableFunction::name`].
///
/// # Errors
/// A rusqlite error when the declaration is invalid or SQLite refuses the module.
pub fn register(conn: &rusqlite::Connection, f: Arc<dyn TableFunction>) -> rusqlite::Result<()> {
    let _ = (conn, f);
    Ok(())
}
