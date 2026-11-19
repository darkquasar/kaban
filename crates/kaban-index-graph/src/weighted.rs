//! Functions over edge attributes (GRA-03): the weighted shortest path and the time-window
//! expansion. Attributes are read through the one accessor, so base, layer and sediment
//! attributes are treated alike.

use kaban_core::query_budget::QueryBudget;

use crate::charge::{Charged, Grow, Hold, Phase, Ticker};
use crate::error::GraphError;
use crate::id::{Direction, LocalId, RelSelection, StableOrder, TieRank};
use crate::traverse::{Caps, Reached, check_caps, expand_window};
use crate::view::View;

/// A weighted path: (total weight, nodes from the source to the destination).
pub type WeightedPath = (i64, Vec<LocalId>);

/// A heap entry: (cost, hops, node rank, parent rank, node, parent). Ranks are unique per node,
/// so the order of the first four fields decides.
type Entry = (i64, u32, TieRank, TieRank, u32, u32);

/// Push `e` onto the min-heap `h`.
fn heap_push(h: &mut Grow<'_, Entry>, e: Entry) -> Result<(), GraphError> {
    h.push(e)?;
    let mut i = h.len() - 1;
    while i > 0 {
        let parent = (i - 1) / 2;
        if h[parent] <= h[i] {
            break;
        }
        h.swap(parent, i);
        i = parent;
    }
    Ok(())
}

/// Pop the least entry of the min-heap `h`.
fn heap_pop(h: &mut Grow<'_, Entry>) -> Option<Entry> {
    let last = h.len().checked_sub(1)?;
    h.swap(0, last);
    let top = h[last];
    h.truncate(last);
    let mut i = 0;
    loop {
        let (left, right) = (2 * i + 1, 2 * i + 2);
        let mut smallest = i;
        if left < h.len() && h[left] < h[smallest] {
            smallest = left;
        }
        if right < h.len() && h[right] < h[smallest] {
            smallest = right;
        }
        if smallest == i {
            return Some(top);
        }
        h.swap(i, smallest);
        i = smallest;
    }
}

/// A cheapest outgoing path from `src` to `dst` along `rels` by [`crate::Attr::weight`], as
/// (total weight, nodes from `src` to `dst`); `None` only when the search saw every node `src`
/// reaches, so no path exists. `src` itself gives `(0, [src])` when present.
///
/// The path is the cheapest by (total weight, hops), and among those each node's predecessor is
/// the one with the lowest `order` rank (equal-cost choices follow the stable public order, not
/// local ids).
///
/// Correctness relies on the store's invariant that every weight is non-negative (the store
/// refuses a negative weight at write time). The search stops once `dst` settles, so it reads
/// only the lists of the nodes settled before it; a negative weight among the edges it reads is
/// refused, but that check is a defence, not a guarantee: a negative edge the search never
/// reads is not seen.
///
/// Caps bound the search itself and a cut never reads as an answer:
/// - `rows`: the search settles at most `caps.rows` nodes (`src` included) and refuses when
///   it would settle more before `dst`;
/// - `depth`: a node settled `caps.depth` hops from `src` is not expanded further. Its edges
///   are still read and recorded. With `dst` settled, the call refuses when a recorded edge
///   gives a lower cost than the path found (a cheaper path may lie past the bound); without
///   it, unless each recorded edge leads to a settled node at no greater cost (the bound may
///   hide `dst`).
///
/// Work: a Dijkstra search that stops when `dst` settles; each settled node's outgoing list is
/// read once and every edge read pushes at most one heap entry. The heap, the settled set, the
/// edges past the depth bound and the path are reserved on `budget`; the path holds its
/// charge.
///
/// # Errors
/// [`GraphError::CapMissing`] for a zero cap; [`GraphError::CapExceeded`] naming `rows` or
/// `depth` as above, or `cost` when a total weight overflows; [`GraphError::NegativeWeight`]
/// naming the first negative edge read; [`GraphError::BudgetExceeded`] when `budget` refuses;
/// any error of `order`.
pub fn weighted_shortest_path(
    v: &View<'_>,
    src: LocalId,
    dst: LocalId,
    rels: &RelSelection,
    caps: Caps,
    order: &dyn StableOrder,
    budget: &QueryBudget,
) -> Result<Option<Charged<WeightedPath>>, GraphError> {
    check_caps(caps)?;
    budget.check()?;
    if !v.contains(src) || !v.contains(dst) {
        return Ok(None);
    }
    let tick = Ticker::new(budget);
    let mut heap: Grow<'_, Entry> = Grow::new(budget)?;
    // Settled nodes as (node, parent, hops, cost), sorted by node.
    let mut settled: Grow<'_, (u32, u32, u32, i64)> = Grow::new(budget)?;
    // Edges not followed past the depth bound, as (destination, cost through the edge).
    let mut past: Grow<'_, (u32, i64)> = Grow::new(budget)?;
    let find = |settled: &[(u32, u32, u32, i64)], m: u32| {
        settled
            .binary_search_by_key(&m, |&(n, ..)| n)
            .ok()
            .and_then(|i| settled.get(i).copied())
    };
    let rows = usize::try_from(caps.rows).unwrap_or(usize::MAX);
    let depth = u32::from(caps.depth);
    let mut found = None;
    let source_rank = order.rank(src)?;
    heap_push(&mut heap, (0, 0, source_rank, source_rank, src.0, src.0))?;
    loop {
        tick.tick(Phase::Weighted)?;
        let Some((cost, hops, rank, _, node, parent)) = heap_pop(&mut heap) else {
            break;
        };
        let Err(at) = settled.binary_search_by_key(&node, |&(n, ..)| n) else {
            continue;
        };
        if settled.len() == rows {
            return Err(GraphError::CapExceeded {
                limit: "rows",
                max: u64::from(caps.rows),
            });
        }
        settled.insert(at, (node, parent, hops, cost))?;
        if node == dst.0 {
            found = Some((cost, hops));
            break;
        }
        for row in v.neighbours(LocalId(node), rels, Direction::Out, budget) {
            let (m, rel, attr) = row?;
            tick.tick(Phase::Weighted)?;
            if attr.weight < 0 {
                return Err(GraphError::NegativeWeight {
                    src: LocalId(node),
                    rel,
                    dst: m,
                });
            }
            if find(&settled, m.0).is_some() {
                continue;
            }
            let cost = cost
                .checked_add(attr.weight)
                .ok_or(GraphError::CapExceeded {
                    limit: "cost",
                    max: i64::MAX.unsigned_abs(),
                })?;
            if hops >= depth {
                past.push((m.0, cost))?;
                continue;
            }
            heap_push(&mut heap, (cost, hops + 1, order.rank(m)?, rank, m.0, node))?;
        }
    }
    for &(m, cost) in past.iter() {
        tick.tick(Phase::Weighted)?;
        let cut = match found {
            Some((found_cost, _)) => cost < found_cost,
            None => find(&settled, m).is_none_or(|(.., settled_cost)| settled_cost > cost),
        };
        if cut {
            return Err(GraphError::CapExceeded {
                limit: "depth",
                max: u64::from(caps.depth),
            });
        }
    }
    let Some((cost, hops)) = found else {
        return Ok(None);
    };
    drop((heap, past));
    let mut path = Grow::with_capacity(budget, hops as usize + 1)?;
    let mut at = dst.0;
    path.push(dst)?;
    while at != src.0 {
        tick.tick(Phase::Weighted)?;
        let Some((_, parent, ..)) = find(&settled, at) else {
            return Err(GraphError::Corrupt("unsettled node on a path".to_owned()));
        };
        at = parent;
        path.push(LocalId(at))?;
    }
    path.reverse();
    let (path, lease) = path.into_parts();
    Ok(Some(Charged::new((cost, path), Hold::new(lease, None))))
}

/// [`crate::expand`] along outgoing `rels` edges whose [`crate::Attr::ts`] lies in the
/// inclusive `window` `(from, to)`: the nodes reached through such edges with their first
/// depth, every such edge met, and the truncation flag, as [`crate::expand`] defines them.
///
/// # Errors
/// As [`crate::expand`], and [`GraphError::InvalidParam`] naming `window` when `from > to`.
pub fn time_window_expand(
    v: &View<'_>,
    src: &[LocalId],
    rels: &RelSelection,
    window: (i64, i64),
    caps: Caps,
    budget: &QueryBudget,
) -> Result<Charged<Reached>, GraphError> {
    check_caps(caps)?;
    if window.0 > window.1 {
        return Err(GraphError::InvalidParam { name: "window" });
    }
    expand_window(v, src, rels, Direction::Out, caps, Some(window), budget)
}
