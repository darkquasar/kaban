//! Path functions: shortest path and meeting points.

use std::cmp::Ordering;

use kaban_core::query_budget::QueryBudget;

use crate::charge::{Charged, Grow, Hold, Ticker};
use crate::error::GraphError;
use crate::id::{Direction, LocalId, RelSelection};
use crate::traverse::{Caps, Want, check_caps, depth_of, search};
use crate::view::View;

/// A shortest outgoing path from `src` to `dst` within `caps`, `src` and `dst` included.
///
/// `dst` must be reachable as [`crate::reachable`] defines it (`src` itself gives a one-node
/// path). Among the shortest paths the lexicographically smallest wins: from `src`, each step
/// takes the lowest-id successor that is still on a shortest path to `dst`.
///
/// Work: one breadth-first search to the depth of `dst`, then one backward pass over the
/// outgoing lists of the levels in between to mark the nodes on a shortest path, and one
/// forward walk. The levels, the marks and the path are reserved on `budget`; the path holds its
/// charge.
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
    check_caps(caps)?;
    budget.check()?;
    if src == dst {
        if !v.contains(src) {
            return Ok(None);
        }
        let mut path = Grow::with_capacity(budget, 1)?;
        path.push(src)?;
        let (path, lease) = path.into_parts();
        return Ok(Some(Charged::new(path, Hold::new(lease, None))));
    }
    let want = Want {
        edges: false,
        stop: Some(dst),
    };
    let s = search(v, &[src], rels, Direction::Out, caps, want, budget)?;
    let Some(len) = depth_of(&s.seen, dst.0) else {
        return Ok(None);
    };
    let order = &s.order[..];
    // The slice of `order` holding depth `d` (sorted by id).
    let level = |d: u8| {
        let start = order.partition_point(|&(_, x)| x < d);
        let end = order.partition_point(|&(_, x)| x <= d);
        start..end
    };
    let find = |d: u8, m: LocalId| {
        let r = level(d);
        order
            .get(r.clone())
            .and_then(|l| l.binary_search_by_key(&m, |&(n, _)| n).ok())
            .map(|i| r.start + i)
    };
    // Backwards: mark every node of depth d < len with a successor marked at depth d + 1.
    let mut on: Grow<'_, bool> = Grow::with_capacity(budget, order.len())?;
    for _ in 0..order.len() {
        on.push(false)?;
    }
    let Some(last) = find(len, dst) else {
        return Ok(None);
    };
    if let Some(mark) = on.get_mut(last) {
        *mark = true;
    }
    let mut tick = Ticker::new(budget);
    for d in (1..len).rev() {
        for i in level(d) {
            tick.tick()?;
            let Some(&(u, _)) = order.get(i) else {
                continue;
            };
            let mut hit = false;
            for row in v.neighbours(u, rels, Direction::Out, budget) {
                let (m, _, _) = row?;
                if find(d + 1, m).is_some_and(|j| on.get(j) == Some(&true)) {
                    hit = true;
                    break;
                }
            }
            if let Some(mark) = on.get_mut(i) {
                *mark = hit;
            }
        }
    }
    // Forwards: the lowest marked successor at each depth.
    let mut path = Grow::with_capacity(budget, usize::from(len) + 1)?;
    path.push(src)?;
    let mut cur = src;
    for d in 1..=len {
        let mut next = None;
        for row in v.neighbours(cur, rels, Direction::Out, budget) {
            let (m, _, _) = row?;
            if find(d, m).is_some_and(|j| on.get(j) == Some(&true)) {
                next = Some(m);
                break;
            }
        }
        let Some(m) = next else {
            return Err(GraphError::Corrupt(format!(
                "no successor on a shortest path at depth {d}"
            )));
        };
        path.push(m)?;
        cur = m;
    }
    let (path, lease) = path.into_parts();
    Ok(Some(Charged::new(path, Hold::new(lease, None))))
}

/// The nodes reached along outgoing `rels` from both `a` and `b` within `caps` (each node
/// counting itself at depth 0 when present) whose summed depth is smallest, ascending.
///
/// Work: two breadth-first searches as [`crate::closure`] does, then one linear merge of their
/// seen sets (both sorted by id) to find the minimum and collect it. The result holds its
/// charge.
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
    check_caps(caps)?;
    let want = Want {
        edges: false,
        stop: None,
    };
    let sa = search(v, &[a], rels, Direction::Out, caps, want, budget)?;
    let sb = search(v, &[b], rels, Direction::Out, caps, want, budget)?;
    let mut tick = Ticker::new(budget);
    let mut best: Option<u16> = None;
    let mut out: Grow<'_, LocalId> = Grow::new(budget)?;
    let (mut at_a, mut at_b) = (0, 0);
    while let (Some(&(na, da)), Some(&(nb, db))) = (sa.seen.get(at_a), sb.seen.get(at_b)) {
        tick.tick()?;
        match na.cmp(&nb) {
            Ordering::Less => at_a += 1,
            Ordering::Greater => at_b += 1,
            Ordering::Equal => {
                let sum = u16::from(da) + u16::from(db);
                if best.is_none_or(|low| sum < low) {
                    best = Some(sum);
                    out.truncate(0);
                }
                if best == Some(sum) {
                    out.push(LocalId(na))?;
                }
                at_a += 1;
                at_b += 1;
            }
        }
    }
    let (out, lease) = out.into_parts();
    Ok(Charged::new(out, Hold::new(lease, None)))
}
