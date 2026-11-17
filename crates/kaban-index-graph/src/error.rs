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
    /// A cap does not cover the request (GRA-04): the answer needs more than the cap admits (a
    /// result larger than `rows`, a degree past `u32`), or the cap cut a search before it could
    /// settle a global answer, which is refused rather than returned as if complete.
    #[error("the {limit} cap of {max} does not cover the request")]
    CapExceeded {
        /// The cap that does not cover the request.
        limit: &'static str,
        /// That cap's value: the one the caller passed, or the fixed limit it stands for.
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
    ///
    /// The message is a short fixed-format diagnostic (a section name or a fixed phrase, at most
    /// a few dozen bytes, one per failed call). Its allocation is a bounded exemption from the
    /// query budget: it is not reserved or charged, unlike every query-owned collection.
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
