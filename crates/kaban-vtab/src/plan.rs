//! What a table function declares (its columns) and what it tells the SQLite planner (its plan).

use crate::Value;

/// The declared SQLite type of a column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnKind {
    /// `INTEGER` affinity.
    Integer,
    /// `REAL` affinity.
    Real,
    /// `TEXT` affinity.
    Text,
    /// `BLOB` affinity.
    Blob,
    /// No declared type: values pass through unchanged.
    Any,
}

/// Marks a column as an argument of the function. Argument columns are hidden: they bind
/// positionally in `f(a, b)` and do not appear in `SELECT *`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Arg {
    /// A plan that leaves a required argument unbound is refused.
    pub required: bool,
}

/// One declared column of a table function.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColumnDef {
    /// The column name, a plain SQL identifier.
    pub name: &'static str,
    /// The declared type.
    pub kind: ColumnKind,
    /// `Some` for an argument (hidden) column, `None` for a result column.
    pub argument: Option<Arg>,
}

/// An argument the candidate plan binds with an equality constraint.
#[derive(Debug, Clone, PartialEq)]
pub struct BoundArg {
    /// The argument column name.
    pub name: &'static str,
    /// The value, when SQLite knows it at plan time (a literal); `None` when it comes from an
    /// outer loop or a parameter.
    pub value: Option<Value>,
}

/// One `ORDER BY` term on a column of the function.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OrderTerm {
    /// The column name.
    pub name: &'static str,
    /// `true` for `DESC`.
    pub desc: bool,
}

/// The candidate plan SQLite asks the function to cost. Plans with an unbound required argument
/// are refused before the function sees them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlanInput<'a> {
    /// The bound arguments, in declaration order.
    pub bound: &'a [BoundArg],
    /// The requested output order, outermost term first.
    pub order_by: &'a [OrderTerm],
    /// Whether the statement carries a `LIMIT` SQLite could push into this scan.
    pub limit_known: bool,
}

/// The function's answer for one candidate plan.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Plan {
    /// Estimated cost (SQLite's `estimatedCost`; larger is worse).
    pub cost: f64,
    /// Estimated output rows.
    pub rows: i64,
    /// `true` only when the rows come out in exactly [`PlanInput::order_by`] order, so SQLite can
    /// skip its own sort.
    pub consumes_order: bool,
    /// `true` when the scan returns at most one row.
    pub point_lookup: bool,
}
