//! Bounded multi-hop traversal (GRA-04, GRA-08).

use kaban_core::query_budget::QueryBudget;

use crate::charge::Charged;
use crate::error::GraphError;
use crate::id::{Direction, LocalId, RelId, RelSelection};
use crate::view::View;

/// Caps of a traversal; zero means missing and is refused (GRA-04).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Caps {
    /// Maximum depth in hops.
    pub depth: u8,
    /// Maximum rows returned.
    pub rows: u32,
}

/// The cap of a whole-graph function; zero means missing and is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RowCap(pub u32);

/// A traversal result: every reached node once with its first depth, every edge met (GRA-08).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Reached {
    /// Reached nodes with the depth they were first reached at, in (depth, local id) order.
    pub nodes: Vec<(LocalId, u8)>,
    /// Every edge met within the bound, as (source, relation, destination).
    pub edges: Vec<(LocalId, RelId, LocalId)>,
    /// Whether `caps.rows` cut the result.
    pub truncated: bool,
}

/// Expand from `src` along `rels` in `dir` within `caps`.
///
/// # Errors
/// [`GraphError::CapMissing`] for a zero cap (before any neighbour access) and
/// [`GraphError::BudgetExceeded`] when `budget` refuses.
pub fn expand(
    v: &View<'_>,
    src: &[LocalId],
    rels: &RelSelection,
    dir: Direction,
    caps: Caps,
    budget: &QueryBudget,
) -> Result<Charged<Reached>, GraphError> {
    let _ = (v, src, rels, dir, caps, budget);
    Charged::empty(budget)
}

/// [`expand`] along outgoing edges.
///
/// # Errors
/// As [`expand`].
pub fn closure(
    v: &View<'_>,
    src: &[LocalId],
    rels: &RelSelection,
    caps: Caps,
    budget: &QueryBudget,
) -> Result<Charged<Reached>, GraphError> {
    let _ = (v, src, rels, caps, budget);
    Charged::empty(budget)
}

/// [`expand`] along incoming edges.
///
/// # Errors
/// As [`expand`].
pub fn reverse_closure(
    v: &View<'_>,
    src: &[LocalId],
    rels: &RelSelection,
    caps: Caps,
    budget: &QueryBudget,
) -> Result<Charged<Reached>, GraphError> {
    let _ = (v, src, rels, caps, budget);
    Charged::empty(budget)
}

/// Whether `dst` is reachable from `src` along outgoing `rels` within `caps`.
///
/// # Errors
/// As [`expand`].
pub fn reachable(
    v: &View<'_>,
    src: LocalId,
    dst: LocalId,
    rels: &RelSelection,
    caps: Caps,
    budget: &QueryBudget,
) -> Result<bool, GraphError> {
    let _ = (v, src, dst, rels, caps, budget);
    Ok(false)
}
