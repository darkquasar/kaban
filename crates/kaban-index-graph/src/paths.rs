//! Path functions: shortest path and meeting points.

use std::cmp::Ordering;

use kaban_core::query_budget::QueryBudget;

use crate::charge::{Charged, Grow, Hold, Phase, Ticker};
use crate::error::GraphError;
use crate::id::{Direction, LocalId, RelSelection};
use crate::traverse::{Caps, Cut, Want, check_caps, depth_of, search};
use crate::view::View;

/// A shortest outgoing path from `src` to `dst` within `caps`, `src` and `dst` included.
///
/// `Some` when the search within `caps` reaches `dst` (`src` itself gives a one-node path);
/// `None` only when the search saw every node `src` reaches, so no path exists at any depth.
/// Among the shortest paths the lexicographically smallest wins: from `src`, each step takes
/// the lowest-id successor that is still on a shortest path to `dst`.
///
/// Caps and completeness, as [`crate::reachable`]: a rows or depth cut that keeps the search
/// from settling the answer refuses rather than reading as `None`. A path found is exact even
/// when the rows cap cut the level where `dst` was found: a cut ends the search at its level,
/// so every shallower level is complete (no shorter path is hidden) and the cut level
/// contributes only `dst` itself (no smaller path through a dropped node of that level
/// exists). The depth bound only limits how far the search looks; a found path is never
/// longer than `caps.depth`.
///
/// Work: one breadth-first search to the depth of `dst`, then one backward pass over the
/// outgoing lists of the levels in between to mark the nodes on a shortest path, and one
/// forward walk. The levels, the marks and the path are reserved on `budget`; the path holds its
/// charge.
///
/// # Errors
/// [`GraphError::CapExceeded`] naming `rows` or `depth` (with that cap's value) when a cut
/// kept the search from settling the answer; plus the [`crate::expand`] errors.
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
        settle: true,
        window: None,
    };
    let s = search(v, &[src], rels, Direction::Out, caps, want, budget)?;
    let Some(len) = depth_of(&s.seen, dst.0) else {
        return match s.cut {
            None => Ok(None),
            Some(cut) => Err(cut.refusal(caps)),
        };
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
    let tick = Ticker::new(budget);
    let mut on: Grow<'_, bool> = Grow::with_capacity(budget, order.len())?;
    for _ in 0..order.len() {
        tick.tick(Phase::Init)?;
        on.push(false)?;
    }
    let Some(last) = find(len, dst) else {
        return Ok(None);
    };
    if let Some(mark) = on.get_mut(last) {
        *mark = true;
    }
    for d in (1..len).rev() {
        for i in level(d) {
            tick.tick(Phase::Paths)?;
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

/// The nodes reachable along outgoing `rels` from both `a` and `b` (each node counting itself
/// at depth 0 when present) whose summed depth is smallest, ascending; empty only when no node
/// is reachable from both at any depth.
///
/// The caps bound the two searches, and an answer is returned only when it is the exact one:
/// a node a cut search did not keep lies at depth `l` or deeper on that side, where `l` is the
/// rows cut level or `caps.depth + 1` when a node lies past the depth bound; its summed depth
/// is therefore at least the smallest such `l` over both sides. When the found minimum is below
/// that floor no hidden node can equal or beat it and the minima are returned; otherwise (or
/// when nothing common was found and a side was cut) the call refuses naming the cap that set
/// the floor.
///
/// Work: two breadth-first searches as [`crate::closure`] does (each settled at the depth bound
/// as [`crate::reachable`] does), then one linear merge of their seen sets (both sorted by id)
/// to find the minimum and collect it. The result holds its charge.
///
/// # Errors
/// [`GraphError::CapExceeded`] naming `rows` or `depth` (with that cap's value) when a cut
/// could hide a node at or below the found minimum, and naming `rows` when more than
/// `caps.rows` nodes share the exact minimum (`rows` bounds the result; the minima are not
/// truncated); plus the [`crate::expand`] errors.
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
        settle: true,
        window: None,
    };
    let sa = search(v, &[a], rels, Direction::Out, caps, want, budget)?;
    let sb = search(v, &[b], rels, Direction::Out, caps, want, budget)?;
    let tick = Ticker::new(budget);
    let mut best: Option<u16> = None;
    let mut out: Grow<'_, LocalId> = Grow::new(budget)?;
    let (mut at_a, mut at_b) = (0, 0);
    while let (Some(&(na, da)), Some(&(nb, db))) = (sa.seen.get(at_a), sb.seen.get(at_b)) {
        tick.tick(Phase::Meet)?;
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
    let floor = [sa.cut, sb.cut]
        .into_iter()
        .flatten()
        .min_by_key(|cut: &Cut| cut.floor(caps));
    if let Some(cut) = floor
        && best.is_none_or(|low| low >= cut.floor(caps))
    {
        return Err(cut.refusal(caps));
    }
    if out.len() > usize::try_from(caps.rows).unwrap_or(usize::MAX) {
        return Err(GraphError::CapExceeded {
            limit: "rows",
            max: u64::from(caps.rows),
        });
    }
    let (out, lease) = out.into_parts();
    Ok(Charged::new(out, Hold::new(lease, None)))
}
