//! The merged view over a base, sealed layers and sediment records: the one neighbour accessor.
//!
//! Sources are read oldest first (base, layers, sediment); for every key the newest source that
//! records it decides. Every source list is sorted, so lists merge linearly with no sort.
//!
//! Query-side work is charged and checked against the caller's [`QueryBudget`] (PC-17):
//! preparing the sediment reserves its vectors before allocating them and holds the charge
//! while the view lives; each [`View::neighbours`] and [`View::nodes`] iterator reserves its
//! cursors and holds that charge while it lives, and checks the budget before its first raw
//! read and after every [`BUDGET_CHECK_INTERVAL`] raw reads, whether or not a row was yielded.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;

use kaban_core::query_budget::{BudgetLease, QueryBudget};

use crate::base::Base;
use crate::charge::{Charge, Ticker};
use crate::error::GraphError;
use crate::id::{Direction, LocalId, RelId, RelSelection};
use crate::layer::{Edges, Extent, Layer};
use crate::op::{Attr, OpKind, Records};
use crate::presence::PresenceIter;
use crate::sediment::{SedEdge, SedNode, Sediment};

/// Raw reads between two budget checks of one accessor iterator. A raw read is one merged
/// entry consumed from the sources (a neighbour-list entry or a presence entry), counted
/// whether it is yielded or filtered out (tombstone, unselected relation, absent endpoint).
pub const BUDGET_CHECK_INTERVAL: u64 = 256;

/// A read view: a base, sealed layers oldest first, then sediment records oldest first.
#[derive(Debug)]
pub struct View<'a> {
    base: Option<&'a Base>,
    layers: &'a [Layer],
    /// The sediment's net change, prepared once at construction so reads merge sorted lists.
    sediment: Sediment<'a>,
    /// Whether the newest layer's union covers exactly this chain (its depth is the layer count).
    union: bool,
}

/// A cursor vector of `capacity`, charged when a charge is given (query paths).
fn cursors<T>(charge: Option<&mut Charge>, capacity: usize) -> Result<Vec<T>, GraphError> {
    match charge {
        Some(charge) => charge.vec(capacity),
        None => Ok(Vec::with_capacity(capacity)),
    }
}

impl<'a> View<'a> {
    /// A view over `base`, `layers` (oldest first) and `sediment` batches (oldest first).
    ///
    /// The sediment is prepared here: its node, edge and fact streams are copied into sorted
    /// vectors whose capacity is reserved against `budget` before allocation and stays charged
    /// until the view is dropped. Removals in the sediment are deletion barriers over the
    /// extents (see [`crate::NodeOp`]).
    ///
    /// # Errors
    /// [`GraphError::BudgetExceeded`] when `budget` refuses a reservation; nothing stays charged.
    pub fn new<R: AsRef<Records>>(
        base: Option<&'a Base>,
        layers: &'a [Layer],
        sediment: &'a [R],
        budget: &QueryBudget,
    ) -> Result<View<'a>, GraphError> {
        let sediment = Sediment::prepare(sediment, budget)?;
        let union = layers
            .last()
            .is_some_and(|l| usize::try_from(l.extent.depth).ok() == Some(layers.len()));
        Ok(View {
            base,
            layers,
            sediment,
            union,
        })
    }

    /// The base and layers of this view without its sediment: the chain a layer sealed over
    /// this view is opened on (writer paths).
    pub(crate) fn chain(&self) -> View<'a> {
        View {
            base: self.base,
            layers: self.layers,
            sediment: Sediment::empty(),
            union: self.union,
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

    /// The neighbours of `n`, by a linear merge with no sort. [`Direction::Out`] and
    /// [`Direction::In`] are strictly ascending by (neighbour, relation). [`Direction::Both`] is
    /// one merged stream non-decreasing by (neighbour, relation): the outgoing and the incoming
    /// list interleave, an outgoing row before an incoming row at an equal key; the row does
    /// not carry its direction (ask per direction when it matters). Tombstones, unselected
    /// relations and absent endpoints are suppressed. An absent `n` or an empty selection reads
    /// nothing.
    ///
    /// The iterator charges its cursors to `budget` while it lives and checks `budget` before
    /// its first raw read and after every [`BUDGET_CHECK_INTERVAL`] raw reads. After it yields
    /// an error it ends.
    ///
    /// # Errors
    /// Yields [`GraphError::BudgetExceeded`] when the budget refuses the cursors, the deadline
    /// passes or the query is cancelled.
    pub fn neighbours<'v>(
        &'v self,
        n: LocalId,
        rels: &'v RelSelection,
        dir: Direction,
        budget: &'v QueryBudget,
    ) -> impl Iterator<Item = Result<(LocalId, RelId, Attr), GraphError>> + 'v {
        let mut it = Neighbours {
            out: None,
            inc: None,
            out_head: None,
            inc_head: None,
            ticker: Ticker::new(budget),
            meter: Meter::new(Some(budget)),
            pending: None,
            done: false,
            lease: None,
        };
        let nothing = matches!(rels, RelSelection::Only(ids) if ids.is_empty());
        if nothing || !self.contains(n) {
            return it;
        }
        let t = &it.ticker;
        let prepared = Charge::new(budget).and_then(|mut charge| {
            let out = matches!(dir, Direction::Out | Direction::Both)
                .then(|| self.list(n.0, true, Some(rels), Some(&mut charge), t))
                .transpose()?;
            let inc = matches!(dir, Direction::In | Direction::Both)
                .then(|| self.list(n.0, false, Some(rels), Some(&mut charge), t))
                .transpose()?;
            Ok((out, inc, charge.into_lease()))
        });
        match prepared {
            Ok((out, inc, lease)) => {
                it.out = out;
                it.inc = inc;
                it.lease = Some(lease);
            }
            Err(e) => it.pending = Some(e),
        }
        it
    }

    /// One direction's merged list of `n`; `rels` filters by relation and presence. Preparing
    /// each extent's cursor is one unit of `t`.
    fn list<'v>(
        &'v self,
        n: u32,
        out: bool,
        rels: Option<&'v RelSelection>,
        charge: Option<&mut Charge>,
        t: &Ticker<'_>,
    ) -> Result<List<'v>, GraphError> {
        let mut sources: Vec<Cursor<EdgeSource<'v>>> = cursors(charge, self.layers.len() + 2)?;
        if !self.sediment.barrier(n) {
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
                t.count();
                let edges = e.edges(out);
                if let Some(range) = edges.list(n) {
                    sources.push(Cursor::new(EdgeSource::Extent { edges, range }));
                }
            }
        }
        t.count();
        sources.push(Cursor::new(EdgeSource::Sediment(
            self.sediment.list(n, out).iter(),
        )));
        Ok(List {
            view: self,
            rels,
            merge: NewestWins { sources },
        })
    }

    /// The surviving edges of `n` in one direction, presence not applied (writer paths).
    pub(crate) fn alive(&self, n: u32, out: bool) -> Vec<(u32, u16, Attr)> {
        let mut rows = Vec::new();
        let t = Ticker::unmetered();
        let Ok(mut list) = self.list(n, out, None, None, &t) else {
            return rows;
        };
        let mut meter = Meter::new(None);
        while let Ok(Some(row)) = list.next(&t, &mut meter) {
            rows.push(row);
        }
        rows
    }

    /// Nodes with an edge list in one direction in any source, ascending (writer paths).
    pub(crate) fn listed(&self, out: bool) -> BTreeSet<u32> {
        let mut set: BTreeSet<u32> = self
            .extents()
            .flat_map(|e| e.edges(out).touched())
            .collect();
        set.extend(self.sediment.listed(out));
        set
    }

    /// Whether `n` is present.
    #[must_use]
    pub fn contains(&self, n: LocalId) -> bool {
        if let Some(kind) = self.sediment.node(n.0) {
            return kind == OpKind::Add;
        }
        self.extents()
            .rev()
            .find_map(|e| e.presence.get(n.0))
            .is_some_and(|kind| kind == OpKind::Add)
    }

    /// Present nodes in ascending id order, enumerated from presence. The iterator charges its
    /// cursors to `budget` and checks it like [`View::neighbours`].
    ///
    /// # Errors
    /// Yields [`GraphError::BudgetExceeded`] as [`View::neighbours`] does.
    pub fn nodes<'v>(
        &'v self,
        budget: &'v QueryBudget,
    ) -> impl Iterator<Item = Result<LocalId, GraphError>> + 'v {
        self.present(Some(budget))
    }

    /// Present nodes; unbudgeted when `budget` is `None` (writer paths).
    pub(crate) fn present<'v>(&'v self, budget: Option<&'v QueryBudget>) -> Nodes<'v> {
        let mut it = Nodes {
            merge: NewestWins {
                sources: Vec::new(),
            },
            ticker: Ticker::maybe(budget),
            meter: Meter::new(budget),
            pending: None,
            done: false,
            lease: None,
        };
        let t = &it.ticker;
        let prepared = budget.map(Charge::new).transpose().and_then(|mut charge| {
            let mut sources: Vec<Cursor<PresenceSource<'v>>> =
                cursors(charge.as_mut(), self.layers.len() + 2)?;
            for e in self.extents() {
                t.count();
                sources.push(Cursor::new(PresenceSource::Extent(e.presence.iter())));
            }
            t.count();
            sources.push(Cursor::new(PresenceSource::Sediment(
                self.sediment.nodes().iter(),
            )));
            Ok((sources, charge.map(Charge::into_lease)))
        });
        match prepared {
            Ok((sources, lease)) => {
                it.merge.sources = sources;
                it.lease = lease;
            }
            Err(e) => it.pending = Some(e),
        }
        it
    }

    /// Fact `key` of `n` (newest record wins); `None` when removed, never set, deleted by a
    /// removal of `n` or `n` is absent.
    #[must_use]
    pub fn fact(&self, n: LocalId, key: u8) -> Option<&[u8]> {
        if !self.contains(n) {
            return None;
        }
        let (kind, payload) = match self.sediment.fact(n.0, key) {
            Some(found) => found,
            None if self.sediment.barrier(n.0) => return None,
            None => self.extents().rev().find_map(|e| e.facts.get(n.0, key))?,
        };
        (kind == OpKind::Add).then_some(payload)
    }

    /// Every surviving fact, presence not applied (writer paths).
    pub(crate) fn alive_facts(&self) -> BTreeMap<(u32, u8), &'a [u8]> {
        let mut map = BTreeMap::new();
        for e in self.extents() {
            map.extend(
                e.facts
                    .iter()
                    .filter(|((n, _), _, _)| !self.sediment.barrier(*n))
                    .map(|(k, kind, p)| (k, (kind, p))),
            );
        }
        map.extend(
            self.sediment
                .facts()
                .iter()
                .map(|f| (f.key, (f.kind, f.payload))),
        );
        map.into_iter()
            .filter(|(_, (kind, _))| *kind == OpKind::Add)
            .map(|(k, (_, p))| (k, p))
            .collect()
    }

    /// The keys of the surviving facts of `n` (writer paths).
    pub(crate) fn alive_fact_keys(&self, n: u32) -> Vec<u8> {
        let mut map: BTreeMap<u8, OpKind> = BTreeMap::new();
        if !self.sediment.barrier(n) {
            for e in self.extents() {
                map.extend(e.facts.of_node(n).map(|((_, k), kind, _)| (k, kind)));
            }
        }
        map.extend(
            self.sediment
                .facts()
                .iter()
                .filter(|f| f.key.0 == n)
                .map(|f| (f.key.1, f.kind)),
        );
        map.into_iter()
            .filter(|&(_, kind)| kind == OpKind::Add)
            .map(|(k, _)| k)
            .collect()
    }
}

/// The merged-key check of the accessor before the work meter (checks every
/// [`BUDGET_CHECK_INTERVAL`] merged keys).
#[derive(Debug)]
struct Meter<'v> {
    budget: Option<&'v QueryBudget>,
    raw: u64,
}

impl<'v> Meter<'v> {
    fn new(budget: Option<&'v QueryBudget>) -> Self {
        Self { budget, raw: 0 }
    }

    fn before_read(&mut self) -> Result<(), GraphError> {
        if self.raw.is_multiple_of(BUDGET_CHECK_INTERVAL)
            && let Some(budget) = self.budget
        {
            budget.check()?;
        }
        Ok(())
    }

    fn read(&mut self) {
        self.raw += 1;
    }
}

/// A sorted keyed source read by [`NewestWins`]; every advance is metered on the ticker.
trait Source {
    type Key: Ord + Copy;
    type Value;

    /// The next entry; `None` at the end or once `t` refused.
    fn advance(&mut self, t: &Ticker<'_>) -> Option<(Self::Key, Self::Value)>;
}

/// A source with its peeked head.
struct Cursor<S: Source> {
    src: S,
    head: Option<(S::Key, S::Value)>,
    primed: bool,
}

impl<S: Source> Cursor<S> {
    fn new(src: S) -> Self {
        Self {
            src,
            head: None,
            primed: false,
        }
    }

    fn peek(&mut self, t: &Ticker<'_>) -> Option<S::Key> {
        if !self.primed {
            self.head = self.src.advance(t);
            self.primed = true;
        }
        self.head.as_ref().map(|(k, _)| *k)
    }

    fn take(&mut self) -> Option<(S::Key, S::Value)> {
        self.primed = false;
        self.head.take()
    }
}

/// A k-way merge of sorted keyed sources, oldest first: per key the newest source wins.
struct NewestWins<S: Source> {
    sources: Vec<Cursor<S>>,
}

impl<S: Source> NewestWins<S> {
    /// The next merged entry. Comparing one source is one unit of `t` (its advance is another);
    /// after `t` refused the result is meaningless and the caller reports the refusal.
    fn next(&mut self, t: &Ticker<'_>) -> Option<(S::Key, S::Value)> {
        let mut best: Option<(S::Key, usize)> = None;
        for (i, c) in self.sources.iter_mut().enumerate() {
            t.count();
            if let Some(k) = c.peek(t)
                && best.is_none_or(|(b, _)| k <= b)
            {
                best = Some((k, i));
            }
        }
        let (key, winner) = best?;
        let mut value = None;
        for (i, c) in self.sources.iter_mut().enumerate() {
            if c.peek(t) == Some(key) {
                let item = c.take();
                if i == winner {
                    value = item.map(|(_, v)| v);
                }
            }
        }
        value.map(|v| (key, v))
    }
}

/// One presence source: an extent section or the prepared sediment.
enum PresenceSource<'v> {
    Extent(PresenceIter<'v>),
    Sediment(std::slice::Iter<'v, SedNode>),
}

impl Source for PresenceSource<'_> {
    type Key = u32;
    type Value = OpKind;

    fn advance(&mut self, t: &Ticker<'_>) -> Option<(u32, OpKind)> {
        match self {
            Self::Extent(it) => it.next(t),
            Self::Sediment(it) => {
                t.count();
                it.next().map(|n| (n.id, n.kind))
            }
        }
    }
}

/// Present nodes merged from presence, metered.
pub(crate) struct Nodes<'v> {
    merge: NewestWins<PresenceSource<'v>>,
    ticker: Ticker<'v>,
    meter: Meter<'v>,
    pending: Option<GraphError>,
    done: bool,
    /// The cursors' charge, released when the iterator is dropped.
    #[allow(dead_code, reason = "held for its drop")]
    lease: Option<BudgetLease>,
}

impl Iterator for Nodes<'_> {
    type Item = Result<LocalId, GraphError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        if let Some(e) = self.pending.take() {
            self.done = true;
            return Some(Err(e));
        }
        loop {
            if let Err(e) = self.meter.before_read() {
                self.done = true;
                return Some(Err(e));
            }
            let item = self.merge.next(&self.ticker);
            if let Err(e) = self.ticker.stopped() {
                self.done = true;
                return Some(Err(e));
            }
            let Some((n, kind)) = item else {
                self.done = true;
                return None;
            };
            self.meter.read();
            if kind == OpKind::Add {
                return Some(Ok(LocalId(n)));
            }
        }
    }
}

/// One source of a node's list: an extent section range or the prepared sediment. The value
/// says whether the entry comes from the sediment.
enum EdgeSource<'v> {
    Extent {
        edges: &'v Edges,
        range: Range<usize>,
    },
    Sediment(std::slice::Iter<'v, SedEdge>),
}

impl Source for EdgeSource<'_> {
    type Key = (u32, u16);
    type Value = (OpKind, Attr, bool);

    fn advance(&mut self, t: &Ticker<'_>) -> Option<(Self::Key, Self::Value)> {
        t.count();
        match self {
            Self::Extent { edges, range } => {
                let (nbr, rel, kind, attr) = edges.entry(range.next()?)?;
                Some(((nbr, rel), (kind, attr, false)))
            }
            Self::Sediment(it) => it
                .next()
                .map(|e| ((e.key.1, e.key.2), (e.kind, e.attr, true))),
        }
    }
}

/// One direction's merged list: surviving entries, optionally filtered by relation and
/// presence.
struct List<'v> {
    view: &'v View<'v>,
    rels: Option<&'v RelSelection>,
    merge: NewestWins<EdgeSource<'v>>,
}

impl List<'_> {
    /// The next surviving entry, metered on `t`.
    fn next(
        &mut self,
        t: &Ticker<'_>,
        meter: &mut Meter<'_>,
    ) -> Result<Option<(u32, u16, Attr)>, GraphError> {
        loop {
            meter.before_read()?;
            let item = self.merge.next(t);
            t.stopped()?;
            let Some(((nbr, rel), (kind, attr, fresh))) = item else {
                return Ok(None);
            };
            meter.read();
            // An extent entry whose neighbour is removed in the sediment is deleted.
            if kind != OpKind::Add || (!fresh && self.view.sediment.barrier(nbr)) {
                continue;
            }
            if let Some(rels) = self.rels
                && !(rels.contains(RelId(rel)) && self.view.contains(LocalId(nbr)))
            {
                continue;
            }
            return Ok(Some((nbr, rel, attr)));
        }
    }
}

/// Both directions merged by (neighbour, relation), outgoing first, metered.
struct Neighbours<'v> {
    out: Option<List<'v>>,
    inc: Option<List<'v>>,
    out_head: Option<(u32, u16, Attr)>,
    inc_head: Option<(u32, u16, Attr)>,
    ticker: Ticker<'v>,
    meter: Meter<'v>,
    pending: Option<GraphError>,
    done: bool,
    /// The cursors' charge, released when the iterator is dropped.
    #[allow(dead_code, reason = "held for its drop")]
    lease: Option<BudgetLease>,
}

impl Neighbours<'_> {
    /// Fill both heads from their lists; an exhausted list is dropped.
    fn fill(&mut self) -> Result<(), GraphError> {
        for (list, head) in [
            (&mut self.out, &mut self.out_head),
            (&mut self.inc, &mut self.inc_head),
        ] {
            if head.is_none()
                && let Some(l) = list
            {
                *head = l.next(&self.ticker, &mut self.meter)?;
                if head.is_none() {
                    *list = None;
                }
            }
        }
        Ok(())
    }
}
impl Iterator for Neighbours<'_> {
    type Item = Result<(LocalId, RelId, Attr), GraphError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        if let Some(e) = self.pending.take() {
            self.done = true;
            return Some(Err(e));
        }
        if let Err(e) = self.fill() {
            self.done = true;
            return Some(Err(e));
        }
        let take_out = match (&self.out_head, &self.inc_head) {
            (Some(o), Some(i)) => (o.0, o.1) <= (i.0, i.1),
            (Some(_), None) => true,
            (None, Some(_)) => false,
            (None, None) => {
                self.done = true;
                return None;
            }
        };
        let head = if take_out {
            self.out_head.take()
        } else {
            self.inc_head.take()
        };
        head.map(|(m, r, a)| Ok((LocalId(m), RelId(r), a)))
    }
}
