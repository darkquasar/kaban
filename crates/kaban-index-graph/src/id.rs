//! Identifiers, relation selections, directions and the stable public tie order.

use crate::error::GraphError;

/// A node's local id: the node row's `rid` (PC-1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LocalId(pub u32);

/// A relation type id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RelId(pub u16);

/// Which relations an operator follows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelSelection {
    /// Every relation.
    All,
    /// Only these relations, sorted and deduplicated (build with [`RelSelection::only`]).
    Only(Vec<RelId>),
}

impl RelSelection {
    /// A selection of exactly `ids`, sorted and deduplicated; an empty list selects nothing.
    #[must_use]
    pub fn only(mut ids: Vec<RelId>) -> Self {
        ids.sort_unstable();
        ids.dedup();
        Self::Only(ids)
    }

    /// Whether `rel` is selected (binary search; covers every `u16` value).
    #[must_use]
    pub fn contains(&self, rel: RelId) -> bool {
        match self {
            Self::All => true,
            Self::Only(ids) => ids.binary_search(&rel).is_ok(),
        }
    }
}

/// Edge direction relative to the node asked about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Edges leaving the node.
    Out,
    /// Edges entering the node.
    In,
    /// Both; at an equal neighbour and relation an outgoing edge comes first.
    Both,
}

/// A node's unique rank in public `NodeId` lexical order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TieRank(pub u32);

/// Ranks local ids in public `NodeId` order, prepared by the store from the selected snapshot.
pub trait StableOrder {
    /// The rank of `n`.
    ///
    /// # Errors
    /// A [`GraphError`] when `n` has no rank in the prepared order.
    fn rank(&self, n: LocalId) -> Result<TieRank, GraphError>;
}
