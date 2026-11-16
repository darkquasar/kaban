//! Bounded multi-hop traversal (GRA-04, GRA-08).
//!
//! Every traversal is one level-by-level breadth-first search through the one accessor,
//! [`View::neighbours`]. Its working set (sources, the sorted seen set, a level's candidates and
//! met edges) and its output are reserved on the caller's budget before they grow; the output's
//! charge is returned with it in a [`Charged`] value. The accessor checks the budget every
//! [`crate::BUDGET_CHECK_INTERVAL`] units of work; the search's own loops check at the same
//! interval.
//! There is no recursion.

use kaban_core::query_budget::QueryBudget;

use crate::charge::{Charged, Grow, Hold, Ticker, sort_checked};
use crate::error::GraphError;
use crate::id::{Direction, LocalId, RelId, RelSelection};
use crate::view::View;

/// Caps of a traversal; zero means missing and is refused (GRA-04).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Caps {
    /// Maximum depth in hops.
    pub depth: u8,
    /// Maximum rows (reached nodes) returned.
    pub rows: u32,
}

/// The cap of a whole-graph function; zero means missing and is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RowCap(pub u32);

/// A traversal result: every reached node once with its first depth, every edge met (GRA-08).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Reached {
    /// Reached nodes with the depth they were first reached at, in (depth, local id) order. The
    /// sources are not listed.
    pub nodes: Vec<(LocalId, u8)>,
    /// Every edge met within the bound, as (source, relation, destination), each once.
    pub edges: Vec<(LocalId, RelId, LocalId)>,
    /// Whether `caps.rows` cut the result.
    pub truncated: bool,
}

/// Refuse a zero cap; runs before any neighbour access.
pub(crate) fn check_caps(caps: Caps) -> Result<(), GraphError> {
    if caps.depth == 0 {
        return Err(GraphError::CapMissing { limit: "depth" });
    }
    if caps.rows == 0 {
        return Err(GraphError::CapMissing { limit: "rows" });
    }
    Ok(())
}

/// The first depth of `m` in a seen set sorted by id.
pub(crate) fn depth_of(seen: &[(u32, u8)], m: u32) -> Option<u8> {
    seen.binary_search_by_key(&m, |&(id, _)| id)
        .ok()
        .and_then(|i| seen.get(i))
        .map(|&(_, d)| d)
}

/// The state a search leaves behind.
pub(crate) struct Search<'b> {
    /// Every kept node with its first depth, sorted by id; present sources at depth 0.
    pub(crate) seen: Grow<'b, (u32, u8)>,
    /// Kept non-source nodes in (depth, id) order.
    pub(crate) order: Grow<'b, (LocalId, u8)>,
    /// The edges met, when asked for.
    pub(crate) edges: Option<Grow<'b, (LocalId, RelId, LocalId)>>,
    /// Whether the rows cap cut a level.
    pub(crate) truncated: bool,
}

/// What a search collects and when it stops early.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Want {
    /// Collect the edges met.
    pub(crate) edges: bool,
    /// Stop after the level that reaches this node.
    pub(crate) stop: Option<LocalId>,
}

/// One edge met while expanding `parent`: (parent, relation, neighbour, outgoing).
type Met = (u32, u16, u32, bool);

/// Level-by-level breadth-first search from the present `src` along `rels` in `dir`.
///
/// Level `d + 1` holds the nodes first met from level `d`, sorted by id; when there are more
/// than the remaining rows only the lowest ids are kept, `truncated` is set and the search ends.
/// A list stops being read once it has offered more new nodes than the remaining rows (the cut
/// is then certain), so a hub costs at most rows + 1 new entries per list. Edges are kept when
/// both endpoints are kept. With [`Direction::Both`] the outgoing and the incoming list are read
/// separately and an edge is reported only from the endpoint expanded first (a self-loop from
/// its outgoing side), so every directed edge appears once and reciprocal edges both appear.
///
/// Work: each kept node at depth below `caps.depth` reads its lists once (bounded as above);
/// each level sorts its candidates (an interruptible heapsort) and merges them into the seen
/// set, O(seen) per level. Every loop is metered and checked every
/// [`crate::BUDGET_CHECK_INTERVAL`] units.
pub(crate) fn search<'b>(
    v: &View<'_>,
    src: &[LocalId],
    rels: &RelSelection,
    dir: Direction,
    caps: Caps,
    want: Want,
    budget: &'b QueryBudget,
) -> Result<Search<'b>, GraphError> {
    budget.check()?;
    let tick = Ticker::new(budget);
    let mut frontier: Grow<'b, u32> = Grow::with_capacity(budget, src.len())?;
    for &n in src {
        tick.tick()?;
        if v.contains(n) {
            frontier.push(n.0)?;
        }
    }
    sort_checked(&mut frontier, |&n| n, &tick)?;
    frontier.dedup(&tick)?;
    let mut seen: Grow<'b, (u32, u8)> = Grow::with_capacity(budget, frontier.len())?;
    for &n in frontier.iter() {
        tick.tick()?;
        seen.push((n, 0))?;
    }
    let mut order = Grow::new(budget)?;
    let mut edges = if want.edges {
        Some(Grow::new(budget)?)
    } else {
        None
    };
    let mut rows_left = usize::try_from(caps.rows).unwrap_or(usize::MAX);
    let mut truncated = false;
    let passes: &[Direction] = match dir {
        Direction::Out => &[Direction::Out],
        Direction::In => &[Direction::In],
        Direction::Both => &[Direction::Out, Direction::In],
    };
    for depth in 0..caps.depth {
        if frontier.is_empty() {
            break;
        }
        let mut cand: Grow<'b, u32> = Grow::new(budget)?;
        let mut met: Grow<'b, Met> = Grow::new(budget)?;
        for &n in frontier.iter() {
            tick.tick()?;
            for &pass in passes {
                let mut last = None;
                let mut fresh = 0usize;
                for row in v.neighbours(LocalId(n), rels, pass, budget) {
                    let (m, rel, _) = row?;
                    let m = m.0;
                    let old = depth_of(&seen, m);
                    if want.edges {
                        let expanded_before =
                            old.is_some_and(|d| d < depth || (d == depth && m < n));
                        let elsewhere = dir == Direction::Both
                            && (expanded_before || (pass == Direction::In && m == n));
                        if !elsewhere {
                            met.push((n, rel.0, m, pass == Direction::Out))?;
                        }
                    }
                    if old.is_none() && last != Some(m) {
                        last = Some(m);
                        cand.push(m)?;
                        fresh += 1;
                        if fresh > rows_left {
                            break;
                        }
                    }
                }
            }
        }
        sort_checked(&mut cand, |&m| m, &tick)?;
        cand.dedup(&tick)?;
        if cand.len() > rows_left {
            truncated = true;
            cand.truncate(rows_left);
        }
        rows_left -= cand.len();
        let next = depth + 1;
        if let Some(edges) = &mut edges {
            for &(n, rel, m, out) in met.iter() {
                tick.tick()?;
                if depth_of(&seen, m).is_some() || cand.binary_search(&m).is_ok() {
                    let rel = RelId(rel);
                    edges.push(if out {
                        (LocalId(n), rel, LocalId(m))
                    } else {
                        (LocalId(m), rel, LocalId(n))
                    })?;
                }
            }
        }
        drop(met);
        order.reserve(cand.len())?;
        for &m in cand.iter() {
            tick.tick()?;
            order.push((LocalId(m), next))?;
        }
        seen = merge(&seen, &cand, next, budget, &tick)?;
        let found = want.stop.is_some_and(|s| cand.binary_search(&s.0).is_ok());
        frontier = cand;
        if truncated || found {
            break;
        }
    }
    Ok(Search {
        seen,
        order,
        edges,
        truncated,
    })
}

/// `seen` (sorted by id) merged with the new `level` ids (sorted, none seen) at `depth`, by a
/// linear merge into a newly reserved vector.
fn merge<'b>(
    seen: &[(u32, u8)],
    level: &[u32],
    depth: u8,
    budget: &'b QueryBudget,
    tick: &Ticker<'_>,
) -> Result<Grow<'b, (u32, u8)>, GraphError> {
    let mut out = Grow::with_capacity(budget, seen.len() + level.len())?;
    let (mut i, mut j) = (0, 0);
    loop {
        tick.tick()?;
        let item = match (seen.get(i), level.get(j)) {
            (Some(&s), Some(&l)) if s.0 < l => {
                i += 1;
                s
            }
            (_, Some(&l)) => {
                j += 1;
                (l, depth)
            }
            (Some(&s), None) => {
                i += 1;
                s
            }
            (None, None) => break,
        };
        out.push(item)?;
    }
    Ok(out)
}

/// Expand from `src` along `rels` in `dir` within `caps` (GRA-08): every reached node once with
/// its first depth, in (depth, local id) order, sources not listed; every edge met within the
/// bound once, including edges back to nodes already reached; the edges are listed in the
/// order of the node expanded (depth, id), then neighbour-list order, outgoing before incoming.
/// When `caps.rows` cuts, the lowest ids of the cut depth are kept, `truncated` is set and only
/// edges between kept nodes (and sources) are listed, those met before the cut.
///
/// Absent sources are ignored. The result holds the charge of its two vectors.
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
    check_caps(caps)?;
    let want = Want {
        edges: true,
        stop: None,
    };
    let s = search(v, src, rels, dir, caps, want, budget)?;
    let (nodes, nodes_lease) = s.order.into_parts();
    let (edges, edges_lease) = match s.edges {
        Some(edges) => edges.into_parts(),
        None => Grow::new(budget)?.into_parts(),
    };
    let reached = Reached {
        nodes,
        edges,
        truncated: s.truncated,
    };
    Ok(Charged::new(
        reached,
        Hold::new(nodes_lease, Some(edges_lease)),
    ))
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
    expand(v, src, rels, Direction::Out, caps, budget)
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
    expand(v, src, rels, Direction::In, caps, budget)
}

/// Whether `dst` is reachable from `src` along outgoing `rels` within `caps`: `dst` is among
/// the nodes [`closure`] returns, or `dst` is `src` and present (zero hops). The search stops
/// at the depth that reaches `dst`.
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
    check_caps(caps)?;
    if src == dst {
        budget.check()?;
        return Ok(v.contains(src));
    }
    let want = Want {
        edges: false,
        stop: Some(dst),
    };
    let s = search(v, &[src], rels, Direction::Out, caps, want, budget)?;
    Ok(depth_of(&s.seen, dst.0).is_some())
}
