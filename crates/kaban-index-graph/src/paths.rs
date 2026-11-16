//! Path functions: shortest path and meeting points.

use kaban_core::query_budget::QueryBudget;

use crate::charge::Charged;
use crate::error::GraphError;
use crate::id::{LocalId, RelSelection};
use crate::traverse::Caps;
use crate::view::View;

/// A shortest outgoing path from `src` to `dst` within `caps`, ties broken by lowest id.
///
/// # Errors
/// As [`crate::expand`].
pub fn shortest_path(
    v: &View<'_>,
    src: LocalId,
    dst: LocalId,
    rels: &RelSelection,
    caps: Caps,
    budget: &QueryBudget,
) -> Result<Option<Charged<Vec<LocalId>>>, GraphError> {
    let _ = (v, src, dst, rels, caps, budget);
    Ok(None)
}

/// Nodes reachable from both `a` and `b` with minimal summed depth.
///
/// # Errors
/// As [`crate::expand`].
pub fn meet(
    v: &View<'_>,
    a: LocalId,
    b: LocalId,
    rels: &RelSelection,
    caps: Caps,
    budget: &QueryBudget,
) -> Result<Charged<Vec<LocalId>>, GraphError> {
    let _ = (v, a, b, rels, caps, budget);
    Charged::empty(budget)
}
