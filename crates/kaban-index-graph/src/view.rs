//! The merged view over a base, sealed layers and sediment records: the one neighbour accessor.
//!
//! Sources are read oldest first (base, layers, sediment); for every key the newest source that
//! records it decides. Every source list is sorted, so lists merge linearly with no sort.

use std::collections::BTreeMap;
use std::collections::btree_map;
use std::iter::Peekable;
use std::ops::Range;

use crate::base::Base;
use crate::id::{Direction, LocalId, RelId, RelSelection};
use crate::layer::{EdgeMap, Edges, Extent, Layer, Net};
use crate::op::{Attr, OpKind, Records};

/// A read view: a base, sealed layers oldest first, then sediment records oldest first.
#[derive(Debug)]
pub struct View<'a> {
    base: Option<&'a Base>,
    layers: &'a [Layer],
    /// The sediment's net change, indexed once at construction so reads merge sorted lists.
    sediment: Net<'a>,
    /// Whether the newest layer's union covers exactly this chain (its depth is the layer count).
    union: bool,
}

impl<'a> View<'a> {
    /// A view over `base`, `layers` (oldest first) and `sediment` (oldest first).
    #[must_use]
    pub fn new(base: Option<&'a Base>, layers: &'a [Layer], sediment: &'a [Records]) -> View<'a> {
        let mut net = Net::default();
        for r in sediment {
            net.apply(r);
        }
        let union = layers
            .last()
            .is_some_and(|l| usize::try_from(l.extent.depth).ok() == Some(layers.len()));
        View {
            base,
            layers,
            sediment: net,
            union,
        }
    }

    /// The sealed layers, oldest first.
    pub(crate) fn layers(&self) -> &'a [Layer] {
        self.layers
    }

    /// Extents oldest first: the base, then the layers.
    fn extents(&self) -> impl DoubleEndedIterator<Item = &'a Extent> + 'a {
        self.base
            .map(|b| &b.extent)
            .into_iter()
            .chain(self.layers.iter().map(|l| &l.extent))
    }

    /// The id domain covering every source.
    pub(crate) fn domain(&self) -> u64 {
        self.extents()
            .map(|e| e.domain)
            .chain([self.sediment.domain()])
            .max()
            .unwrap_or(0)
    }

    /// The neighbours of `n` in ascending neighbour id order (then relation; for
    /// [`Direction::Both`], outgoing before incoming at an equal neighbour and relation), by a
    /// linear merge with no sort. Absent endpoints are suppressed. The iterator borrows the view
    /// and the selection.
    pub fn neighbours<'v>(
        &'v self,
        n: LocalId,
        rels: &'v RelSelection,
        dir: Direction,
    ) -> impl Iterator<Item = (LocalId, RelId, Attr)> + 'v {
        let present = self.contains(n);
        let side = |out: bool| present.then(|| self.side(n.0, out, rels).peekable());
        Neighbours {
            out: matches!(dir, Direction::Out | Direction::Both)
                .then(|| side(true))
                .flatten(),
            inc: matches!(dir, Direction::In | Direction::Both)
                .then(|| side(false))
                .flatten(),
        }
    }

    /// One direction's merged list of `n`.
    fn side<'v>(&'v self, n: u32, out: bool, rels: &'v RelSelection) -> Side<'v> {
        let mut sources: Vec<Peekable<EdgeSource<'v>>> = Vec::with_capacity(self.layers.len() + 2);
        let skip_layers = self.union
            && self.layers.last().is_some_and(|l| {
                l.extent
                    .union
                    .as_ref()
                    .is_some_and(|(o, i)| !if out { o } else { i }.contains(n))
            });
        let layer_extents = self.layers.iter().map(|l| &l.extent);
        let extents = self
            .base
            .map(|b| &b.extent)
            .into_iter()
            .chain(layer_extents.take(if skip_layers { 0 } else { self.layers.len() }));
        for e in extents {
            let edges = e.edges(out);
            if let Some(range) = edges.list(n) {
                sources.push(EdgeSource::Extent { edges, range }.peekable());
            }
        }
        let map: &'v EdgeMap = if out {
            &self.sediment.out
        } else {
            &self.sediment.inc
        };
        sources
            .push(EdgeSource::Sediment(map.range((n, 0, 0)..=(n, u32::MAX, u16::MAX))).peekable());
        Side {
            view: self,
            rels,
            merge: NewestWins { sources },
        }
    }

    /// Whether `n` is present.
    #[must_use]
    pub fn contains(&self, n: LocalId) -> bool {
        if let Some(&kind) = self.sediment.nodes.get(&n.0) {
            return kind == OpKind::Add;
        }
        self.extents()
            .rev()
            .find_map(|e| e.presence.get(n.0))
            .is_some_and(|kind| kind == OpKind::Add)
    }

    /// Present nodes in ascending id order, enumerated from presence.
    pub fn nodes(&self) -> impl Iterator<Item = LocalId> + '_ {
        let mut sources: Vec<Peekable<PresenceSource<'_>>> = self
            .extents()
            .map(|e| (Box::new(e.presence.iter()) as PresenceSource<'_>).peekable())
            .collect();
        let sediment = self.sediment.nodes.iter().map(|(&n, &k)| (n, k));
        sources.push((Box::new(sediment) as PresenceSource<'_>).peekable());
        NewestWins { sources }
            .filter(|&(_, kind)| kind == OpKind::Add)
            .map(|(n, _)| LocalId(n))
    }

    /// Fact `key` of `n` (newest record wins); `None` when removed, never set or `n` is absent.
    #[must_use]
    pub fn fact(&self, n: LocalId, key: u8) -> Option<&[u8]> {
        if !self.contains(n) {
            return None;
        }
        let (kind, payload) = match self.sediment.facts.get(&(n.0, key)) {
            Some(&found) => found,
            None => self.extents().rev().find_map(|e| e.facts.get(n.0, key))?,
        };
        (kind == OpKind::Add).then_some(payload)
    }

    /// Every fact change merged oldest to newest (presence not applied).
    pub(crate) fn merged_facts(&self) -> BTreeMap<(u32, u8), (OpKind, &'a [u8])> {
        let mut map = BTreeMap::new();
        for e in self.extents() {
            map.extend(e.facts.iter().map(|(k, kind, p)| (k, (kind, p))));
        }
        map.extend(self.sediment.facts.iter().map(|(&k, &v)| (k, v)));
        map
    }
}

/// One presence source: an extent section or the sediment index.
type PresenceSource<'v> = Box<dyn Iterator<Item = (u32, OpKind)> + 'v>;

/// A k-way merge of sorted keyed sources, oldest first: per key the newest source wins.
struct NewestWins<S: Iterator> {
    sources: Vec<Peekable<S>>,
}

impl<K: Ord + Copy, V, S: Iterator<Item = (K, V)>> Iterator for NewestWins<S> {
    type Item = (K, V);

    fn next(&mut self) -> Option<(K, V)> {
        let mut best: Option<(K, usize)> = None;
        for (i, s) in self.sources.iter_mut().enumerate() {
            if let Some(&(k, _)) = s.peek()
                && best.is_none_or(|(b, _)| k <= b)
            {
                best = Some((k, i));
            }
        }
        let (key, winner) = best?;
        let mut value = None;
        for (i, s) in self.sources.iter_mut().enumerate() {
            if s.peek().is_some_and(|&(k, _)| k == key) {
                let item = s.next();
                if i == winner {
                    value = item.map(|(_, v)| v);
                }
            }
        }
        value.map(|v| (key, v))
    }
}

/// One source of a node's list: an extent section range or the sediment index.
enum EdgeSource<'v> {
    Extent {
        edges: &'v Edges,
        range: Range<usize>,
    },
    Sediment(btree_map::Range<'v, (u32, u32, u16), (OpKind, Attr)>),
}

impl Iterator for EdgeSource<'_> {
    type Item = ((u32, u16), (OpKind, Attr));

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Extent { edges, range } => {
                let (nbr, rel, kind, attr) = edges.entry(range.next()?)?;
                Some(((nbr, rel), (kind, attr)))
            }
            Self::Sediment(range) => range
                .next()
                .map(|(&(_, nbr, rel), &value)| ((nbr, rel), value)),
        }
    }
}

/// One direction's merged, filtered list.
struct Side<'v> {
    view: &'v View<'v>,
    rels: &'v RelSelection,
    merge: NewestWins<EdgeSource<'v>>,
}

impl Iterator for Side<'_> {
    type Item = (LocalId, RelId, Attr);

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let ((nbr, rel), (kind, attr)) = self.merge.next()?;
            if kind == OpKind::Add
                && self.rels.contains(RelId(rel))
                && self.view.contains(LocalId(nbr))
            {
                return Some((LocalId(nbr), RelId(rel), attr));
            }
        }
    }
}

/// Both directions merged by (neighbour, relation), outgoing first.
struct Neighbours<'v> {
    out: Option<Peekable<Side<'v>>>,
    inc: Option<Peekable<Side<'v>>>,
}

impl Iterator for Neighbours<'_> {
    type Item = (LocalId, RelId, Attr);

    fn next(&mut self) -> Option<Self::Item> {
        let key = |s: &mut Option<Peekable<Side<'_>>>| {
            s.as_mut().and_then(|s| s.peek().map(|&(m, r, _)| (m, r)))
        };
        let take_out = match (key(&mut self.out), key(&mut self.inc)) {
            (Some(o), Some(i)) => o <= i,
            (Some(_), None) => true,
            (None, _) => false,
        };
        let side = if take_out {
            &mut self.out
        } else {
            &mut self.inc
        };
        side.as_mut()?.next()
    }
}
