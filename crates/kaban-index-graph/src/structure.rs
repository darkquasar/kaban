//! Structural functions over neighbour lists and the whole graph.

use std::cmp::Reverse;
use std::collections::BinaryHeap;

use kaban_core::query_budget::QueryBudget;

use crate::charge::{Charge, Charged, Grow, Hold, Ticker};
use crate::error::GraphError;
use crate::id::{Direction, LocalId, RelSelection};
use crate::traverse::{Caps, RowCap, Want, check_caps, search};
use crate::view::View;

/// The number of `rels` edges of `n` in `dir` (edges, not distinct neighbours; with
/// [`Direction::Both`] a self-loop counts once per direction).
///
/// Work: one pass over the list(s); nothing is allocated beyond the accessor's cursors.
///
/// # Errors
/// [`GraphError::BudgetExceeded`] when `budget` refuses; [`GraphError::CapExceeded`] when the
/// count does not fit a `u32`.
pub fn degree(
    v: &View<'_>,
    n: LocalId,
    rels: &RelSelection,
    dir: Direction,
    budget: &QueryBudget,
) -> Result<u32, GraphError> {
    budget.check()?;
    let mut count = 0u64;
    for row in v.neighbours(n, rels, dir, budget) {
        row?;
        count += 1;
    }
    u32::try_from(count).map_err(|_| GraphError::CapExceeded {
        limit: "degree",
        max: u64::from(u32::MAX),
    })
}

/// The distinct neighbours of `n` ignoring direction, ascending: the [`Direction::Both`] stream
/// is non-decreasing by neighbour, so equal-neighbour runs collapse without a sort.
fn distinct<'b>(
    v: &View<'_>,
    n: LocalId,
    rels: &RelSelection,
    budget: &'b QueryBudget,
) -> Result<Grow<'b, LocalId>, GraphError> {
    let mut out = Grow::new(budget)?;
    for row in v.neighbours(n, rels, Direction::Both, budget) {
        let (m, _, _) = row?;
        if out.last() != Some(&m) {
            out.push(m)?;
        }
    }
    Ok(out)
}

/// The first index at or after `from` whose element is at least `x`: an exponential probe,
/// then a binary search inside the probed window.
fn gallop(s: &[LocalId], from: usize, x: LocalId) -> usize {
    let rest = s.get(from..).unwrap_or_default();
    let mut bound = 1;
    while rest.get(bound).is_some_and(|&y| y < x) {
        bound *= 2;
    }
    let window = rest.get(..=bound).unwrap_or(rest);
    from + window.partition_point(|&y| y < x)
}

/// Neighbours shared by `a` and `b` ignoring direction, each once, ascending.
///
/// Work: both distinct neighbour lists are collected from the accessor (runs collapsed, no
/// sort), then each element of the shorter list gallops through the longer one, so the cost is
/// O(s log(l / s)) comparisons for lists of length s <= l. The lists and the result are
/// reserved on `budget`; the result holds its charge.
///
/// # Errors
/// [`GraphError::BudgetExceeded`] when `budget` refuses.
pub fn common_neighbours(
    v: &View<'_>,
    a: LocalId,
    b: LocalId,
    rels: &RelSelection,
    budget: &QueryBudget,
) -> Result<Charged<Vec<LocalId>>, GraphError> {
    budget.check()?;
    let na = distinct(v, a, rels, budget)?;
    let nb = distinct(v, b, rels, budget)?;
    let (short, long) = if na.len() <= nb.len() {
        (&na, &nb)
    } else {
        (&nb, &na)
    };
    let mut out = Grow::with_capacity(budget, short.len())?;
    let tick = Ticker::new(budget);
    let mut at = 0;
    for &x in short.iter() {
        tick.tick()?;
        at = gallop(long, at, x);
        if long.get(at) == Some(&x) {
            out.push(x)?;
            at += 1;
        }
    }
    let (out, lease) = out.into_parts();
    Ok(Charged::new(out, Hold::new(lease, None)))
}

/// The position of `m` in the sorted `set`.
fn index(set: &[(u32, u8)], m: LocalId) -> Option<usize> {
    set.binary_search_by_key(&m.0, |&(id, _)| id).ok()
}

/// A topological order of the subgraph that `nodes` reach along outgoing `rels` within `caps`
/// (the present `nodes` included, as [`crate::closure`] reaches them), ascending id first among
/// the nodes that are ready (Kahn's algorithm with a min-heap: the lexicographically smallest
/// order).
///
/// Work: one search as [`crate::closure`]; every outgoing list of the set is read twice (in-
/// degrees, then release). A cycle is found by walking from the lowest unreleased node to its
/// lowest unreleased predecessor as many steps as nodes remain, which ends on a cycle. The set,
/// the in-degrees, the heap and the result are reserved on `budget`; the result holds its
/// charge.
///
/// # Errors
/// [`GraphError::Cycle`] naming a node on a cycle; [`GraphError::CapExceeded`] when the rows cap
/// cuts the reached set (an order of part of it would mislead); plus the [`crate::expand`]
/// errors.
pub fn topo_order(
    v: &View<'_>,
    nodes: &[LocalId],
    rels: &RelSelection,
    caps: Caps,
    budget: &QueryBudget,
) -> Result<Charged<Vec<LocalId>>, GraphError> {
    check_caps(caps)?;
    let want = Want {
        edges: false,
        stop: None,
    };
    let reached = search(v, nodes, rels, Direction::Out, caps, want, budget)?;
    if reached.truncated {
        return Err(GraphError::CapExceeded {
            limit: "rows",
            max: u64::from(caps.rows),
        });
    }
    let set = &reached.seen[..];
    let tick = Ticker::new(budget);
    let mut indeg: Grow<'_, u32> = Grow::with_capacity(budget, set.len())?;
    for _ in set {
        tick.tick()?;
        indeg.push(0)?;
    }
    let corrupt = || GraphError::Corrupt("in-degree out of range".to_owned());
    for &(node, _) in set {
        tick.tick()?;
        let mut last = None;
        for row in v.neighbours(LocalId(node), rels, Direction::Out, budget) {
            let (succ, _, _) = row?;
            if last == Some(succ) {
                continue;
            }
            last = Some(succ);
            if let Some(count) = index(set, succ).and_then(|at| indeg.get_mut(at)) {
                *count = count.checked_add(1).ok_or_else(corrupt)?;
            }
        }
    }
    let mut charge = Charge::new(budget)?;
    let mut heap = BinaryHeap::from(charge.vec::<Reverse<usize>>(set.len())?);
    for (at, &count) in indeg.iter().enumerate() {
        tick.tick()?;
        if count == 0 {
            heap.push(Reverse(at));
        }
    }
    let mut out = Grow::with_capacity(budget, set.len())?;
    while let Some(Reverse(at)) = heap.pop() {
        tick.tick()?;
        let Some(&(node, _)) = set.get(at) else {
            return Err(corrupt());
        };
        out.push(LocalId(node))?;
        let mut last = None;
        for row in v.neighbours(LocalId(node), rels, Direction::Out, budget) {
            let (succ, _, _) = row?;
            if last == Some(succ) {
                continue;
            }
            last = Some(succ);
            let Some(next) = index(set, succ) else {
                continue;
            };
            let Some(count) = indeg.get_mut(next) else {
                return Err(corrupt());
            };
            *count = count.checked_sub(1).ok_or_else(corrupt)?;
            if *count == 0 {
                heap.push(Reverse(next));
            }
        }
    }
    if out.len() < set.len() {
        return Err(cycle(v, set, &indeg, rels, budget));
    }
    drop(heap);
    let (out, lease) = out.into_parts();
    Ok(Charged::new(out, Hold::new(lease, None)))
}

/// A node on a cycle among the unreleased nodes (in-degree above zero), each of which has an
/// unreleased predecessor: following the lowest one as many steps as there are nodes ends on a
/// cycle of that walk, which is a cycle of the graph.
fn cycle(
    v: &View<'_>,
    set: &[(u32, u8)],
    indeg: &[u32],
    rels: &RelSelection,
    budget: &QueryBudget,
) -> GraphError {
    let walk = || -> Result<LocalId, GraphError> {
        let unreleased = |m: LocalId| index(set, m).is_some_and(|j| indeg.get(j) > Some(&0));
        let start = indeg
            .iter()
            .position(|&d| d > 0)
            .and_then(|i| set.get(i))
            .ok_or_else(|| GraphError::Corrupt("no unreleased node".to_owned()))?;
        let mut cur = LocalId(start.0);
        let tick = Ticker::new(budget);
        for _ in 0..set.len() {
            tick.tick()?;
            let mut pred = None;
            for row in v.neighbours(cur, rels, Direction::In, budget) {
                let (p, _, _) = row?;
                if unreleased(p) {
                    pred = Some(p);
                    break;
                }
            }
            cur =
                pred.ok_or_else(|| GraphError::Corrupt("no unreleased predecessor".to_owned()))?;
        }
        Ok(cur)
    };
    match walk() {
        Ok(node) => GraphError::Cycle { node },
        Err(e) => e,
    }
}

/// The root of `x` in the forest `parent`, halving the path on the way; every step of the walk
/// is one unit of `t`, so a long walk is checked from inside.
fn find(parent: &mut [u32], mut x: usize, t: &Ticker<'_>) -> Result<usize, GraphError> {
    loop {
        t.tick()?;
        let Some(&p) = parent.get(x) else {
            return Ok(x);
        };
        let p = p as usize;
        if p == x {
            return Ok(x);
        }
        let Some(&grand) = parent.get(p) else {
            return Ok(p);
        };
        if let Some(slot) = parent.get_mut(x) {
            *slot = grand;
        }
        x = grand as usize;
    }
}

/// Weakly connected components as (node, component) for every present node in ascending id
/// order, the component id being its lowest member; the first `cap` rows are returned.
///
/// Work: the present nodes are listed once from presence, then each outgoing list is read once
/// into a union-find over their positions (path halving, the lower root kept, so a root is
/// always its component's lowest member). The node list, the forest and the result are
/// reserved on `budget`; the result holds its charge.
///
/// # Errors
/// [`GraphError::CapMissing`] for `RowCap(0)` and [`GraphError::BudgetExceeded`].
pub fn wcc(
    v: &View<'_>,
    rels: &RelSelection,
    cap: RowCap,
    budget: &QueryBudget,
) -> Result<Charged<Vec<(LocalId, LocalId)>>, GraphError> {
    if cap.0 == 0 {
        return Err(GraphError::CapMissing { limit: "rows" });
    }
    budget.check()?;
    let mut nodes: Grow<'_, LocalId> = Grow::new(budget)?;
    for n in v.nodes(budget) {
        nodes.push(n?)?;
    }
    let too_many = || GraphError::Corrupt("more present nodes than u32 positions".to_owned());
    let tick = Ticker::new(budget);
    let mut parent: Grow<'_, u32> = Grow::with_capacity(budget, nodes.len())?;
    for i in 0..nodes.len() {
        tick.tick()?;
        parent.push(u32::try_from(i).map_err(|_| too_many())?)?;
    }
    for (i, &n) in nodes.iter().enumerate() {
        tick.tick()?;
        for row in v.neighbours(n, rels, Direction::Out, budget) {
            let (m, _, _) = row?;
            let Ok(j) = nodes.binary_search(&m) else {
                continue;
            };
            let (ri, rj) = (find(&mut parent, i, &tick)?, find(&mut parent, j, &tick)?);
            let (low, high) = (ri.min(rj), ri.max(rj));
            if low != high
                && let Some(slot) = parent.get_mut(high)
            {
                *slot = u32::try_from(low).map_err(|_| too_many())?;
            }
        }
    }
    let rows = nodes
        .len()
        .min(usize::try_from(cap.0).unwrap_or(usize::MAX));
    let mut out = Grow::with_capacity(budget, rows)?;
    for (i, &n) in nodes.iter().take(rows).enumerate() {
        tick.tick()?;
        let root = find(&mut parent, i, &tick)?;
        let Some(&c) = nodes.get(root) else {
            return Err(too_many());
        };
        out.push((n, c))?;
    }
    let (out, lease) = out.into_parts();
    Ok(Charged::new(out, Hold::new(lease, None)))
}

/// The refusal for a function that is not planned (GRA-01): `betweenness`, `louvain`,
/// `all_paths`, `k_core`, `articulation_points` and `label_propagation` are refused with their
/// own name; any other name (matching is exact) is refused as `unknown`, because the query
/// surface resolves planned functions before it asks.
#[must_use]
pub fn refuse_not_planned(name: &str) -> GraphError {
    let function = match name {
        "betweenness" => "betweenness",
        "louvain" => "louvain",
        "all_paths" => "all_paths",
        "k_core" => "k_core",
        "articulation_points" => "articulation_points",
        "label_propagation" => "label_propagation",
        _ => "unknown",
    };
    GraphError::NotPlanned { function }
}
