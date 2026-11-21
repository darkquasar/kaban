//! Inputs and bounded materialized output of a store query.

use crate::query_budget::QueryBudget;
use crate::{SqlValue, ViewName};
use std::time::Duration;

/// One SQL statement and its positional parameters at the selected view.
#[derive(Debug, Clone, PartialEq)]
pub struct Statement {
    /// SQL text.
    pub sql: String,
    /// Values bound to its placeholders.
    pub params: Vec<SqlValue>,
    /// View used by every read in the statement.
    pub view: ViewName,
}

/// Resource bounds for evaluating a statement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueryLimits {
    /// Time available to acquisition and execution together.
    pub time: Duration,
    /// Maximum number of final rows.
    pub rows: u32,
    /// Maximum bytes for the request's working storage and results.
    pub memory_bytes: u64,
    /// Maximum size of one SQL value, in bytes.
    pub value_bytes: u32,
}

impl Default for QueryLimits {
    fn default() -> Self {
        Self {
            time: Duration::from_secs(2),
            rows: 50,
            memory_bytes: 64 << 20,
            value_bytes: 16 << 20,
        }
    }
}

/// A single execution. Its deadline starts at construction, including any time before admission.
/// Clone its budget handle to cancel it from another thread, before or during execution.
/// Passing the request to the store consumes it, so a retry starts with new accounting.
#[derive(Debug)]
pub struct QueryExecution {
    limits: QueryLimits,
    budget: QueryBudget,
}

impl QueryExecution {
    /// Start one request with the supplied bounds.
    #[must_use]
    pub fn new(limits: QueryLimits) -> Self {
        Self {
            budget: QueryBudget::new(limits.memory_bytes, Some(limits.time)),
            limits,
        }
    }

    /// The same budget the allocator, SQL VM and Rust operators check.
    #[must_use]
    pub fn budget(&self) -> &QueryBudget {
        &self.budget
    }

    /// Hand ownership of the bounds and their matching budget to the executor.
    #[must_use]
    pub fn into_parts(self) -> (QueryLimits, QueryBudget) {
        (self.limits, self.budget)
    }
}

/// The bounded result, fully materialized before the statement releases its snapshot.
#[derive(Debug, Clone, PartialEq)]
pub struct RowSet {
    /// Column names in projection order.
    pub columns: Vec<String>,
    /// Rows in the statement's result order.
    pub rows: Vec<Vec<SqlValue>>,
    /// Whether the final row cap omitted additional results.
    pub truncated: bool,
}
