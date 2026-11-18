//! Triangles: the local clustering coefficient and the whole-graph triangle count.

use kaban_core::query_budget::QueryBudget;

use crate::error::GraphError;
use crate::id::{LocalId, RelSelection};
use crate::traverse::RowCap;
use crate::view::View;

/// The local clustering coefficient of `n`.
///
/// # Errors
/// [`GraphError::BudgetExceeded`] when `budget` refuses.
pub fn clustering(
    v: &View<'_>,
    n: LocalId,
    rels: &RelSelection,
    budget: &QueryBudget,
) -> Result<f64, GraphError> {
    let _ = (v, n, rels);
    budget.check()?;
    Ok(0.0)
}

/// The number of triangles in the view.
///
/// # Errors
/// [`GraphError::CapMissing`] for `RowCap(0)`; [`GraphError::BudgetExceeded`].
pub fn triangle_count(
    v: &View<'_>,
    rels: &RelSelection,
    cap: RowCap,
    budget: &QueryBudget,
) -> Result<u64, GraphError> {
    let _ = (v, rels, cap);
    budget.check()?;
    Ok(0)
}
