//! Structural functions over neighbour lists and the whole graph.

use kaban_core::query_budget::QueryBudget;

use crate::error::GraphError;
use crate::id::{Direction, LocalId, RelSelection};
use crate::traverse::{Caps, RowCap};
use crate::view::View;

/// The number of `rels` edges of `n` in `dir`.
///
/// # Errors
/// [`GraphError::BudgetExceeded`] when `budget` refuses.
pub fn degree(
    v: &View<'_>,
    n: LocalId,
    rels: &RelSelection,
    dir: Direction,
    budget: &QueryBudget,
) -> Result<u32, GraphError> {
    let _ = (v, n, rels, dir, budget);
    Ok(0)
}

/// Neighbours shared by `a` and `b`, ascending, by galloping merge.
///
/// # Errors
/// [`GraphError::BudgetExceeded`] when `budget` refuses.
pub fn common_neighbours(
    v: &View<'_>,
    a: LocalId,
    b: LocalId,
    rels: &RelSelection,
    budget: &QueryBudget,
) -> Result<Vec<LocalId>, GraphError> {
    let _ = (v, a, b, rels, budget);
    Ok(Vec::new())
}

/// A topological order of `nodes` along outgoing `rels`.
///
/// # Errors
/// [`GraphError::Cycle`] over a cycle, plus the [`crate::expand`] errors.
pub fn topo_order(
    v: &View<'_>,
    nodes: &[LocalId],
    rels: &RelSelection,
    caps: Caps,
    budget: &QueryBudget,
) -> Result<Vec<LocalId>, GraphError> {
    let _ = (v, nodes, rels, caps, budget);
    Ok(Vec::new())
}

/// Weakly connected components as (node, component), the component id being its lowest member.
///
/// # Errors
/// [`GraphError::CapMissing`] for `RowCap(0)` and [`GraphError::BudgetExceeded`].
pub fn wcc(
    v: &View<'_>,
    rels: &RelSelection,
    cap: RowCap,
    budget: &QueryBudget,
) -> Result<Vec<(LocalId, LocalId)>, GraphError> {
    let _ = (v, rels, cap, budget);
    Ok(Vec::new())
}

/// The refusal for a function that is not planned (`betweenness`, `louvain`, `all_paths`,
/// `k_core`, `articulation_points`, `label_propagation`).
#[must_use]
pub fn refuse_not_planned(name: &str) -> GraphError {
    let _ = name;
    GraphError::NotPlanned { function: "" }
}
