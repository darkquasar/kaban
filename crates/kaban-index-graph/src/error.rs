//! The one error type of the graph engine.

use kaban_core::query_budget::{BudgetExceeded, Limit};

use crate::id::LocalId;

/// Why a graph operation refused or failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GraphError {
    /// A required cap was absent or zero; refused before any neighbour access (GRA-04).
    #[error("cap missing: {limit}")]
    CapMissing {
        /// The cap that was missing.
        limit: &'static str,
    },
    /// A cap exceeded its configured maximum (GRA-04).
    #[error("cap exceeded: {limit} above {max}")]
    CapExceeded {
        /// The cap that was too large.
        limit: &'static str,
        /// The configured maximum.
        max: u64,
    },
    /// The function is not planned and is refused (GRA-01).
    #[error("graph function not planned: {function}")]
    NotPlanned {
        /// The refused function.
        function: &'static str,
    },
    /// A topological order was requested over a cycle.
    #[error("cycle through node {node:?}")]
    Cycle {
        /// A node on the cycle.
        node: LocalId,
    },
    /// The shared query budget was exceeded (PC-17).
    #[error("query budget exceeded: {limit} ({value})")]
    BudgetExceeded {
        /// The exceeded limit: `time`, `memory` or `cancelled`.
        limit: &'static str,
        /// The measured value at the breach.
        value: u64,
    },
    /// Encoded bytes are truncated, overflow or break a format invariant.
    #[error("corrupt graph bytes: {0}")]
    Corrupt(String),
}

impl From<BudgetExceeded> for GraphError {
    fn from(e: BudgetExceeded) -> Self {
        let limit = match e.limit {
            Limit::Time => "time",
            Limit::Memory => "memory",
            Limit::Cancelled => "cancelled",
        };
        Self::BudgetExceeded {
            limit,
            value: e.value,
        }
    }
}
