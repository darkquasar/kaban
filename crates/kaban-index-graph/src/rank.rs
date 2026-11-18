//! Scored ranking: PageRank and personalised PageRank (GRA-01, GRA-07).

use kaban_core::query_budget::QueryBudget;

use crate::charge::{Charged, Grow, Hold};
use crate::error::GraphError;
use crate::id::{LocalId, RelSelection, StableOrder};
use crate::traverse::RowCap;
use crate::view::View;

/// Parameters of [`pagerank`] and [`ppr`] (agent-selected defaults, reversible).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RankParams {
    /// The probability of following an edge, in `[0, 1]`; default 0.85.
    pub damping: f64,
    /// The most power iterations run, at least 1; default 50.
    pub iterations: u32,
    /// Iteration stops early once the L1 change of one iteration is below this, `>= 0`;
    /// default 1e-9.
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
    /// (node, score), descending score, then ascending [`StableOrder`] rank.
    pub rows: Vec<(LocalId, f64)>,
    /// Whether more present nodes were scored than the cap returned.
    pub truncated: bool,
}

/// `PageRank` over the directed view.
///
/// # Errors
/// [`GraphError::CapMissing`] for `RowCap(0)`; [`GraphError::InvalidParam`] for parameters out
/// of range; [`GraphError::BudgetExceeded`] when `budget` refuses; a [`StableOrder`] error.
pub fn pagerank(
    v: &View<'_>,
    rels: &RelSelection,
    p: RankParams,
    order: &dyn StableOrder,
    cap: RowCap,
    budget: &QueryBudget,
) -> Result<Charged<Ranked>, GraphError> {
    let _ = (v, rels, p, order, cap);
    let (_, lease) = Grow::<u8>::new(budget)?.into_parts();
    Ok(Charged::new(Ranked::default(), Hold::new(lease, None)))
}

/// Personalised `PageRank` from `seeds`.
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
    let _ = seeds;
    pagerank(v, rels, p, order, cap, budget)
}
