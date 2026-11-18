//! Scored ranking: `PageRank` and personalised `PageRank` (GRA-01, GRA-07).
//!
//! Both run over the directed view in f64 with one sequential pass per loop, so every sum is
//! reduced in the same order on every run and thread. The present nodes, the outgoing links by
//! position, the score vectors, the ranks and the result are reserved on the caller's budget.

use std::cmp::Reverse;

use kaban_core::query_budget::QueryBudget;

use crate::charge::{Charged, Grow, Hold, Phase, Ticker, sort_checked};
use crate::error::GraphError;
use crate::id::{Direction, LocalId, RelSelection, StableOrder, TieRank};
use crate::similarity::count;
use crate::traverse::RowCap;
use crate::view::View;

/// Parameters of [`pagerank`] and [`ppr`] (agent-selected defaults, reversible).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RankParams {
    /// The probability of following an edge, in `[0, 1]`; default 0.85.
    pub damping: f64,
    /// The most power iterations run, at least 1; default 50.
    pub iterations: u32,
    /// Iteration stops early once the L1 change of one iteration is below this, finite and
    /// `>= 0`; default 1e-9.
    pub tolerance: f64,
}

impl Default for RankParams {
    fn default() -> Self {
        Self {
            damping: 0.85,
            iterations: 50,
            tolerance: 1e-9,
        }
    }
}

/// The result of [`pagerank`] and [`ppr`]: the highest-scored nodes and whether more existed.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Ranked {
    /// (node, score) by descending score, then ascending [`StableOrder`] rank.
    pub rows: Vec<(LocalId, f64)>,
    /// Whether more present nodes were scored than the cap returned: the rows are then the top
    /// of the ranking, not every node.
    pub truncated: bool,
}

/// `PageRank` over the directed view along `rels`: the top `cap` present nodes by descending
/// score, ties broken by ascending `order` rank before the cap cuts, and whether more existed.
///
/// Semantics (GRA-07): every selected edge is one link (parallel edges of different relations
/// count separately); the scores start uniform and one power iteration gives each node
/// `(1 - damping) / N` plus `damping` times its inflow (each node's score split evenly over
/// its links) and the dangling mass (the summed score of nodes without links) spread over all
/// `N` nodes. It stops after `iterations`, or earlier once the L1 change of an iteration is
/// below `tolerance`. Scores sum to 1.
///
/// Work: the present nodes and every outgoing list are read once; each iteration is linear in
/// nodes plus links; the ranking sorts the nodes once (an interruptible heapsort). All loops are
/// metered.
///
/// # Errors
/// [`GraphError::CapMissing`] for `RowCap(0)`; [`GraphError::InvalidParam`] naming a parameter
/// out of range; [`GraphError::BudgetExceeded`] when `budget` refuses; any error of `order`.
pub fn pagerank(
    v: &View<'_>,
    rels: &RelSelection,
    p: RankParams,
    order: &dyn StableOrder,
    cap: RowCap,
    budget: &QueryBudget,
) -> Result<Charged<Ranked>, GraphError> {
    rank(v, None, rels, p, order, cap, budget)
}

/// Personalised `PageRank` from `seeds`: as [`pagerank`], with the teleport mass `1 - damping`
/// and the starting scores spread evenly over the distinct present seeds instead of over every
/// node. The dangling mass is still spread over all nodes. No present seed gives no rows.
///
/// # Errors
/// As [`pagerank`].
pub fn ppr(
    v: &View<'_>,
    seeds: &[LocalId],
    rels: &RelSelection,
    p: RankParams,
    order: &dyn StableOrder,
    cap: RowCap,
    budget: &QueryBudget,
) -> Result<Charged<Ranked>, GraphError> {
    rank(v, Some(seeds), rels, p, order, cap, budget)
}

/// Refuse parameters out of range.
fn check(p: RankParams) -> Result<(), GraphError> {
    if !(0.0..=1.0).contains(&p.damping) {
        return Err(GraphError::InvalidParam { name: "damping" });
    }
    if p.iterations == 0 {
        return Err(GraphError::InvalidParam { name: "iterations" });
    }
    if !(p.tolerance.is_finite() && p.tolerance >= 0.0) {
        return Err(GraphError::InvalidParam { name: "tolerance" });
    }
    Ok(())
}

/// The shared body of [`pagerank`] (`seeds` is `None`) and [`ppr`].
fn rank(
    v: &View<'_>,
    seeds: Option<&[LocalId]>,
    rels: &RelSelection,
    p: RankParams,
    order: &dyn StableOrder,
    cap: RowCap,
    budget: &QueryBudget,
) -> Result<Charged<Ranked>, GraphError> {
    if cap.0 == 0 {
        return Err(GraphError::CapMissing { limit: "rows" });
    }
    check(p)?;
    budget.check()?;
    let tick = Ticker::new(budget);
    let mut nodes: Grow<'_, LocalId> = Grow::new(budget)?;
    for n in v.nodes(budget) {
        nodes.push(n?)?;
    }
    let total = nodes.len();
    let too_many = || GraphError::Corrupt("more present nodes than u32 positions".to_owned());
    // Outgoing links by position: node i links to targets[start[i]..start[i + 1]].
    let mut start: Grow<'_, usize> = Grow::with_capacity(budget, total + 1)?;
    let mut targets: Grow<'_, u32> = Grow::new(budget)?;
    start.push(0)?;
    for &n in nodes.iter() {
        tick.tick(Phase::Rank)?;
        for row in v.neighbours(n, rels, Direction::Out, budget) {
            let (m, _, _) = row?;
            if let Ok(j) = nodes.binary_search(&m) {
                targets.push(u32::try_from(j).map_err(|_| too_many())?)?;
            }
        }
        start.push(targets.len())?;
    }
    // The teleport vector: uniform, or even over the distinct present seeds.
    let mut teleport: Grow<'_, f64> = Grow::with_capacity(budget, total)?;
    for _ in 0..total {
        tick.tick(Phase::Init)?;
        teleport.push(0.0)?;
    }
    match seeds {
        None => {
            let even = 1.0 / count(total);
            for t in teleport.iter_mut() {
                tick.tick(Phase::Rank)?;
                *t = even;
            }
        }
        Some(seeds) => {
            let mut chosen = 0usize;
            for s in seeds {
                tick.tick(Phase::Rank)?;
                if let Ok(j) = nodes.binary_search(s)
                    && let Some(t) = teleport.get_mut(j)
                    && *t < 1.0
                {
                    *t = 1.0;
                    chosen += 1;
                }
            }
            if chosen == 0 {
                let (rows, lease) = Grow::new(budget)?.into_parts();
                let ranked = Ranked {
                    rows,
                    truncated: false,
                };
                return Ok(Charged::new(ranked, Hold::new(lease, None)));
            }
            let even = 1.0 / count(chosen);
            for t in teleport.iter_mut() {
                tick.tick(Phase::Rank)?;
                if *t > 0.0 {
                    *t = even;
                }
            }
        }
    }
    let mut score: Grow<'_, f64> = Grow::with_capacity(budget, total)?;
    let mut next: Grow<'_, f64> = Grow::with_capacity(budget, total)?;
    for &t in teleport.iter() {
        tick.tick(Phase::Init)?;
        score.push(t)?;
        next.push(0.0)?;
    }
    let links =
        |i: usize| start.get(i).copied().unwrap_or(0)..start.get(i + 1).copied().unwrap_or(0);
    for _ in 0..p.iterations {
        let mut dangling = 0.0;
        for (i, &s) in score.iter().enumerate() {
            tick.tick(Phase::Rank)?;
            next[i] = 0.0;
            if links(i).is_empty() {
                dangling += s;
            }
        }
        for (i, &s) in score.iter().enumerate() {
            tick.tick(Phase::Rank)?;
            let out = links(i);
            if out.is_empty() {
                continue;
            }
            let share = s / count(out.len());
            for &j in targets.get(out).unwrap_or_default() {
                tick.tick(Phase::Rank)?;
                if let Some(x) = next.get_mut(j as usize) {
                    *x += share;
                }
            }
        }
        let spread = dangling / count(total);
        let mut change = 0.0;
        for (j, x) in next.iter_mut().enumerate() {
            tick.tick(Phase::Rank)?;
            *x = (1.0 - p.damping) * teleport[j] + p.damping * (*x + spread);
            change += (*x - score[j]).abs();
        }
        std::mem::swap(&mut score, &mut next);
        if change < p.tolerance {
            break;
        }
    }
    drop((next, teleport, targets, start));
    // Rank: descending score, then ascending stable rank, before the cap cuts.
    let mut ranks: Grow<'_, TieRank> = Grow::with_capacity(budget, total)?;
    for &n in nodes.iter() {
        tick.tick(Phase::Rank)?;
        ranks.push(order.rank(n)?)?;
    }
    let mut by: Grow<'_, u32> = Grow::with_capacity(budget, total)?;
    for i in 0..total {
        tick.tick(Phase::Init)?;
        by.push(u32::try_from(i).map_err(|_| too_many())?)?;
    }
    // Scores are finite and non-negative, so their bit patterns order as their values.
    sort_checked(
        &mut by,
        |&i| (Reverse(score[i as usize].to_bits()), ranks[i as usize]),
        &tick,
    )?;
    let keep = total.min(usize::try_from(cap.0).unwrap_or(usize::MAX));
    let mut rows = Grow::with_capacity(budget, keep)?;
    for &i in by.iter().take(keep) {
        tick.tick(Phase::Rank)?;
        rows.push((nodes[i as usize], score[i as usize]))?;
    }
    let (rows, lease) = rows.into_parts();
    let ranked = Ranked {
        rows,
        truncated: total > keep,
    };
    Ok(Charged::new(ranked, Hold::new(lease, None)))
}
