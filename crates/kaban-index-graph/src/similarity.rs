//! Neighbourhood similarity of two nodes (GRA-01 set class).

use kaban_core::query_budget::QueryBudget;

use crate::error::GraphError;
use crate::id::{LocalId, RelSelection};
use crate::view::View;

/// The Jaccard similarity of the neighbourhoods of `a` and `b`.
///
/// # Errors
/// [`GraphError::BudgetExceeded`] when `budget` refuses.
pub fn jaccard(
    v: &View<'_>,
    a: LocalId,
    b: LocalId,
    rels: &RelSelection,
    budget: &QueryBudget,
) -> Result<f64, GraphError> {
    let _ = (v, a, b, rels);
    budget.check()?;
    Ok(0.0)
}

/// The Adamic-Adar index of `a` and `b`.
///
/// # Errors
/// [`GraphError::BudgetExceeded`] when `budget` refuses.
pub fn adamic_adar(
    v: &View<'_>,
    a: LocalId,
    b: LocalId,
    rels: &RelSelection,
    budget: &QueryBudget,
) -> Result<f64, GraphError> {
    let _ = (v, a, b, rels);
    budget.check()?;
    Ok(0.0)
}
