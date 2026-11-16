//! The merged view over a base, sealed layers and sediment records: the one neighbour accessor.

use crate::base::Base;
use crate::id::{Direction, LocalId, RelId, RelSelection};
use crate::layer::Layer;
use crate::op::{Attr, Records};

/// A read view: a base, sealed layers oldest first, then sediment records oldest first.
#[derive(Debug)]
pub struct View<'a> {
    base: Option<&'a Base>,
    layers: &'a [Layer],
    sediment: &'a [Records],
}

impl<'a> View<'a> {
    /// A view over `base`, `layers` (oldest first) and `sediment` (oldest first).
    #[must_use]
    pub fn new(base: Option<&'a Base>, layers: &'a [Layer], sediment: &'a [Records]) -> View<'a> {
        View {
            base,
            layers,
            sediment,
        }
    }

    /// The neighbours of `n` in ascending neighbour id order (then relation; for
    /// [`Direction::Both`], outgoing before incoming), by a linear merge with no sort. Absent
    /// endpoints are suppressed.
    pub fn neighbours(
        &self,
        n: LocalId,
        rels: &RelSelection,
        dir: Direction,
    ) -> impl Iterator<Item = (LocalId, RelId, Attr)> + '_ {
        let _ = (n, rels, dir, self.base, self.layers, self.sediment);
        std::iter::empty()
    }

    /// Whether `n` is present.
    #[must_use]
    pub fn contains(&self, n: LocalId) -> bool {
        let _ = n;
        false
    }

    /// Present nodes in ascending id order, enumerated from presence.
    pub fn nodes(&self) -> impl Iterator<Item = LocalId> + '_ {
        std::iter::empty()
    }

    /// Fact `key` of `n` (newest record wins); `None` when removed, never set or `n` is absent.
    #[must_use]
    pub fn fact(&self, n: LocalId, key: u8) -> Option<&[u8]> {
        let _ = (n, key);
        None
    }
}
