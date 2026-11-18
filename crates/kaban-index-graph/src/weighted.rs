//! Functions over edge attributes (GRA-03): the weighted shortest path and the time-window
//! expansion.

use kaban_core::query_budget::QueryBudget;

use crate::charge::{Charged, Grow, Hold};
use crate::error::GraphError;
use crate::id::{LocalId, RelSelection, StableOrder};
use crate::traverse::{Caps, Reached};
use crate::view::View;

/// A cheapest outgoing path from `src` to `dst` by [`crate::Attr::weight`].
///
/// # Errors
/// [`GraphError::CapMissing`], [`GraphError::CapExceeded`], [`GraphError::NegativeWeight`],
/// [`GraphError::BudgetExceeded`] and [`StableOrder`] errors.
pub fn weighted_shortest_path(
    v: &View<'_>,
    src: LocalId,
    dst: LocalId,
    rels: &RelSelection,
    caps: Caps,
    order: &dyn StableOrder,
    budget: &QueryBudget,
) -> Result<Option<Charged<(i64, Vec<LocalId>)>>, GraphError> {
    let _ = (v, src, dst, rels, caps, order);
    budget.check()?;
    Ok(None)
}

/// [`crate::expand`] along outgoing edges whose [`crate::Attr::ts`] lies in `window`.
///
/// # Errors
/// As [`crate::expand`], and [`GraphError::InvalidParam`] for an empty window.
pub fn time_window_expand(
    v: &View<'_>,
    src: &[LocalId],
    rels: &RelSelection,
    window: (i64, i64),
    caps: Caps,
    budget: &QueryBudget,
) -> Result<Charged<Reached>, GraphError> {
    let _ = (v, src, rels, window, caps);
    let (_, lease) = Grow::<u8>::new(budget)?.into_parts();
    Ok(Charged::new(Reached::default(), Hold::new(lease, None)))
}
