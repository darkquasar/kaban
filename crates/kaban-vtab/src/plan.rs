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

/// A comparison SQLite can push into a scan, on a result column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConstraintOp {
    /// `=`.
    Eq,
    /// `!=` or `<>`.
    Ne,
    /// `<`.
    Lt,
    /// `<=`.
    Le,
    /// `>`.
    Gt,
    /// `>=`.
    Ge,
}

/// A usable constraint SQLite offers a candidate plan on a result (non-argument) column, such as
/// `search.id = nodes.id` once `nodes` is the outer loop.
#[derive(Debug, Clone, PartialEq)]
pub struct ResultConstraint {
    /// The result column name.
    pub column: &'static str,
    /// The comparison: `column op value`.
    pub op: ConstraintOp,
    /// The right-hand value, when SQLite knows it at plan time (a literal); `None` when it comes
    /// from an outer loop or a parameter.
    pub value: Option<Value>,
}

/// A constraint the chosen plan consumes, with its value, as the scan receives it.
#[derive(Debug, Clone, PartialEq)]
pub struct BoundConstraint {
    /// The result column name.
    pub column: &'static str,
    /// The comparison: `column op value`.
    pub op: ConstraintOp,
    /// The right-hand value for this scan.
    pub value: Value,
}

/// What one scan of the chosen plan runs with.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Scan<'a> {
    /// One value per argument column in declaration order ([`Value::Null`] when an optional
    /// argument is unbound).
    pub args: &'a [Value],
    /// The constraints the plan consumed ([`Plan::consumes`]), in [`PlanInput::constraints`]
    /// order.
    pub bound: &'a [BoundConstraint],
    /// The plan's [`Plan::strategy`].
    pub strategy: i32,
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
    /// The usable constraints on result columns, at most [`MAX_CONSTRAINTS`]; bit `i` of
    /// [`Plan::consumes`] and [`Plan::omits`] refers to `constraints[i]`.
    pub constraints: &'a [ResultConstraint],
}

/// The most result-column constraints one candidate plan is offered (one bit each in
/// [`Plan::consumes`]).
pub const MAX_CONSTRAINTS: usize = 32;

/// The function's answer for one candidate plan.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
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
    /// Bit `i` set: the scan receives `constraints[i]` with its value in [`Scan::bound`] and may
    /// use it to produce fewer rows. SQLite still checks every row against it unless the same bit
    /// is set in [`Plan::omits`].
    pub consumes: u32,
    /// Bit `i` set (together with the same bit of [`Plan::consumes`]): the function guarantees
    /// every row it produces satisfies `constraints[i]`, so SQLite does not check it again. Set
    /// it only for a comparison the function applies with SQLite's own semantics.
    pub omits: u32,
    /// An opaque number handed back to the scan in [`Scan::strategy`], e.g. to select a point
    /// lookup over a full scan.
    pub strategy: i32,
}
