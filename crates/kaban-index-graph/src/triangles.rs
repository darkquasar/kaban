//! Triangles: the local clustering coefficient and the whole-graph triangle count.
//!
//! Both read the graph as undirected and simple: two distinct nodes are adjacent when an edge
//! along `rels` joins them in either direction; self-loops and parallel edges add nothing.
//! Neighbour lists come from the accessor in id order and are intersected by merging, with no
//! sort (GRA-06).

use std::cmp::Ordering;

use kaban_core::query_budget::QueryBudget;

use crate::charge::{Grow, Phase, Ticker};
use crate::error::GraphError;
use crate::id::{Direction, LocalId, RelSelection};
use crate::similarity::count;
use crate::structure::{distinct, intersection};
use crate::traverse::RowCap;
use crate::view::View;

/// The local clustering coefficient of `n`: the links among its `k` other neighbours over the
/// `k (k - 1) / 2` possible pairs; 0 when `k < 2` (an absent `n` included).
///
/// Work: `n`'s list and each neighbour's list are read once; each neighbour's list is
/// intersected with the neighbours after it. The lists are reserved on `budget`.
///
/// # Errors
/// [`GraphError::BudgetExceeded`] when `budget` refuses.
pub fn clustering(
    v: &View<'_>,
    n: LocalId,
    rels: &RelSelection,
    budget: &QueryBudget,
) -> Result<f64, GraphError> {
    budget.check()?;
    let mut around = distinct(v, n, rels, Direction::Both, budget)?;
    if let Ok(at) = around.binary_search(&n) {
        let tail = around.len() - 1;
        around.copy_within(at + 1.., at);
        around.truncate(tail);
    }
    let k = around.len();
    if k < 2 {
        return Ok(0.0);
    }
    let tick = Ticker::new(budget);
    let mut links = 0usize;
    for (i, &u) in around.iter().enumerate() {
        tick.tick(Phase::Similarity)?;
        let of_u = distinct(v, u, rels, Direction::Both, budget)?;
        let later = around.get(i + 1..).unwrap_or_default();
        links += intersection(&of_u, later, budget)?.len();
    }
    Ok(2.0 * count(links) / (count(k) * count(k - 1)))
}

/// The number of triangles (unordered triples of pairwise adjacent distinct nodes) in the view
/// along `rels`.
///
/// `cap` bounds the present nodes the count covers: a graph with more is refused, never
/// counted in part (GRA-04).
///
/// Work: the present nodes are listed and every list is read once to keep, per node, its
/// neighbours with a higher id; each triangle `a < b < c` is then counted once, at `a` and `b`,
/// by merging the two higher-neighbour lists. The node list and the lists are reserved on
/// `budget`.
///
/// # Errors
/// [`GraphError::CapMissing`] for `RowCap(0)`; [`GraphError::CapExceeded`] naming `rows` when
/// more than `cap` nodes are present; [`GraphError::BudgetExceeded`] when `budget` refuses.
pub fn triangle_count(
    v: &View<'_>,
    rels: &RelSelection,
    cap: RowCap,
    budget: &QueryBudget,
) -> Result<u64, GraphError> {
    if cap.0 == 0 {
        return Err(GraphError::CapMissing { limit: "rows" });
    }
    budget.check()?;
    let limit = usize::try_from(cap.0).unwrap_or(usize::MAX);
    let mut nodes: Grow<'_, LocalId> = Grow::new(budget)?;
    for n in v.nodes(budget) {
        let n = n?;
        if nodes.len() == limit {
            return Err(GraphError::CapExceeded {
                limit: "rows",
                max: u64::from(cap.0),
            });
        }
        nodes.push(n)?;
    }
    let tick = Ticker::new(budget);
    // Higher neighbours by position: node i's are higher[start[i]..start[i + 1]].
    let mut start: Grow<'_, usize> = Grow::with_capacity(budget, nodes.len() + 1)?;
    let mut higher: Grow<'_, LocalId> = Grow::new(budget)?;
    start.push(0)?;
    for &u in nodes.iter() {
        tick.tick(Phase::Triangles)?;
        let mut last = None;
        for row in v.neighbours(u, rels, Direction::Both, budget) {
            let (m, _, _) = row?;
            if m > u && last != Some(m) {
                last = Some(m);
                higher.push(m)?;
            }
        }
        start.push(higher.len())?;
    }
    let list = |i: usize| {
        let range = start.get(i).copied().unwrap_or(0)..start.get(i + 1).copied().unwrap_or(0);
        higher.get(range).unwrap_or_default()
    };
    let mut triangles = 0u64;
    for i in 0..nodes.len() {
        tick.tick(Phase::Triangles)?;
        let of_a = list(i);
        for b in of_a {
            tick.tick(Phase::Triangles)?;
            let Ok(j) = nodes.binary_search(b) else {
                continue;
            };
            triangles += shared(of_a, list(j), &tick)?;
        }
    }
    Ok(triangles)
}

/// How many ids two ascending lists share, by a linear merge, one unit of `tick` per step.
fn shared(left: &[LocalId], right: &[LocalId], tick: &Ticker<'_>) -> Result<u64, GraphError> {
    let (mut at_left, mut at_right, mut both) = (0, 0, 0);
    while let (Some(x), Some(y)) = (left.get(at_left), right.get(at_right)) {
        tick.tick(Phase::Triangles)?;
        match x.cmp(y) {
            Ordering::Less => at_left += 1,
            Ordering::Greater => at_right += 1,
            Ordering::Equal => {
                both += 1;
                at_left += 1;
                at_right += 1;
            }
        }
    }
    Ok(both)
}
