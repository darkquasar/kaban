//! Neighbourhood similarity of two nodes (GRA-01 set class).
//!
//! A node's neighbourhood here is its distinct neighbours along `rels` in either direction
//! (itself only through a self-loop), read from the accessor's id-ordered lists and intersected
//! by galloping, with no sort (GRA-06). An absent node has an empty neighbourhood.

use kaban_core::query_budget::QueryBudget;

use crate::charge::{Phase, Ticker};
use crate::error::GraphError;
use crate::id::{Direction, LocalId, RelSelection};
use crate::structure::{distinct, intersection};
use crate::view::View;

/// A neighbour count as f64; exact below 2^53, and ids are below 2^32.
#[allow(clippy::cast_precision_loss)]
pub(crate) fn count(n: usize) -> f64 {
    n as f64
}

/// The Jaccard similarity of the neighbourhoods of `a` and `b`: shared neighbours over the
/// neighbours of either; 0 when both are empty.
///
/// Work: both neighbourhoods are collected and intersected; they are reserved on `budget`.
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
    budget.check()?;
    let na = distinct(v, a, rels, Direction::Both, budget)?;
    let nb = distinct(v, b, rels, Direction::Both, budget)?;
    let shared = intersection(&na, &nb, budget)?.len();
    let either = na.len() + nb.len() - shared;
    if either == 0 {
        return Ok(0.0);
    }
    Ok(count(shared) / count(either))
}

/// The Adamic-Adar index of `a` and `b`: the sum over their shared neighbours `z`, ascending,
/// of `1 / ln |N(z)|`; a `z` with fewer than two neighbours adds 0 (its logarithm is 0).
///
/// Work: both neighbourhoods are collected and intersected, then each shared neighbour's list
/// is read once to count its neighbours; the lists are reserved on `budget`.
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
    budget.check()?;
    let na = distinct(v, a, rels, Direction::Both, budget)?;
    let nb = distinct(v, b, rels, Direction::Both, budget)?;
    let shared = intersection(&na, &nb, budget)?;
    let tick = Ticker::new(budget);
    let mut sum = 0.0;
    for &z in shared.iter() {
        tick.tick(Phase::Similarity)?;
        let mut degree = 0usize;
        let mut last = None;
        for row in v.neighbours(z, rels, Direction::Both, budget) {
            let (m, _, _) = row?;
            if last != Some(m) {
                last = Some(m);
                degree += 1;
            }
        }
        if degree >= 2 {
            sum += 1.0 / count(degree).ln();
        }
    }
    Ok(sum)
}
