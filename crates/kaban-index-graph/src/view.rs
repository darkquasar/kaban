//! The merged view over a base, sealed layers and sediment records: the one neighbour accessor.
//!
//! Sources are read oldest first (base, layers, sediment); for every key the newest source that
//! records it decides. Every source list is sorted, so lists merge linearly with no sort.
//!
//! Query-side work is charged and checked against the caller's [`QueryBudget`] (PC-17):
//! the borrowed [`crate::PreparedSediment`] holds the charge of its vectors while it lives;
//! each [`View::neighbours`] and [`View::nodes`] iterator reserves its cursors and holds that
//! charge while it lives. Every unit of work beneath the iterator is
//! metered, not only the merged rows: preparing a cursor, comparing a source in the merge,
//! advancing a source by one entry and visiting one bitvector word are one unit each. The budget
//! is checked before the first unit and after every [`BUDGET_CHECK_INTERVAL`] units, whether or
//! not a row was yielded, so a cancellation is seen within that many units.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;

use kaban_core::query_budget::{BudgetLease, QueryBudget};

use crate::base::Base;
use crate::charge::{Charge, Phase, Ticker};
use crate::error::GraphError;
use crate::id::{Direction, LocalId, RelId, RelSelection};
use crate::layer::{Edges, Extent, Layer};
use crate::merge::{NewestWins, Source, charged};
use crate::op::{Attr, OpKind, corrupt};
use crate::presence::PresenceIter;
use crate::sediment::{EMPTY, PreparedSediment, SedEdge, SedNode};

/// Units of work between two budget checks of one metered loop (an accessor iterator, a
/// structural function's loop, decoding, validation or preparation). A unit is bounded: one
/// cursor prepared, one source compared or advanced by one entry in a merge, one bitvector word
/// visited, one record decoded or validated, 4 KiB of fact payload copied, 256 kind bytes
/// validated, one loop iteration of a structural function, one union-find step or one sift of
/// a sort (at most 2 log2(n) comparisons); apart from those fixed-size chunks none costs more
/// than a binary search over one section or prepared sediment stream, plus a presence probe
/// over the chain for a row's neighbour. A
/// cancellation or deadline is therefore seen within this many units of each live loop, and
/// [`crate::work_units`] counts them.
pub const BUDGET_CHECK_INTERVAL: u64 = 256;

/// A read view: a base, sealed layers oldest first, then a prepared sediment.
#[derive(Debug)]
pub struct View<'a> {
    base: Option<&'a Base>,
    layers: &'a [Layer],
    /// The sediment's net change, prepared by the caller and borrowed, so reads merge sorted
    /// lists and construction does no sediment work.
    sediment: &'a PreparedSediment,
    /// Whether the newest layer's union is trusted: its depth is the layer count and it was
    /// verified to cover every node touched by every layer of this chain.
    union: bool,
}

impl<'a> View<'a> {
    /// A view over `base`, `layers` (oldest first) and a prepared `sediment`, which the view
    /// borrows: construction copies and sorts nothing, so one [`PreparedSediment`] serves every
    /// query over the same batches. Removals in the sediment are deletion barriers over the
    /// extents (see [`crate::NodeOp`]).
    ///
    /// When the newest layer's depth equals the number of layers, its union bitvectors may let
    /// reads skip the layers for untouched nodes. The union is trusted only after this call
    /// verifies that it covers every touched node (outgoing and incoming) of every supplied
    /// layer, so a union missing a bit is refused rather than read as absence: O(touched ids of
    /// the layers), metered like every other preparation step. Depth equality alone proves
    /// neither coverage nor ancestry; a chain whose depth does not match reads every layer.
    ///
    /// # Errors
    /// [`GraphError::BudgetExceeded`] when `budget` already refuses at entry or is cancelled or
    /// past its deadline during the union check; [`GraphError::Corrupt`] when the newest union
    /// misses a node touched in the chain. Nothing stays charged.
    pub fn new(
        base: Option<&'a Base>,
        layers: &'a [Layer],
        sediment: &'a PreparedSediment,
        budget: &QueryBudget,
    ) -> Result<View<'a>, GraphError> {
        budget.check()?;
        let union = match layers.last() {
            Some(last) if usize::try_from(last.extent.depth).ok() == Some(layers.len()) => {
                verify_union(last, layers, &Ticker::new(budget))?;
                true
            }
            _ => false,
        };
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
            sediment: &EMPTY,
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
    /// its first unit of work and after every [`BUDGET_CHECK_INTERVAL`] units (see the module
    /// documentation). After it yields an error it ends.
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
    /// each extent's and run's cursor is one unit of `t`. The extents and the runs older than
    /// the newest run that removes `n` are behind its barrier and are not read.
    fn list<'v>(
        &'v self,
        n: u32,
        out: bool,
        rels: Option<&'v RelSelection>,
        mut charge: Option<&mut Charge>,
        t: &Ticker<'_>,
    ) -> Result<List<'v>, GraphError> {
        let runs = self.sediment.sorted_runs();
        let cut = self.sediment.cut(n);
        let mut sources: Vec<EdgeSource<'v>> =
            charged(charge.as_deref_mut(), self.layers.len() + 1 + runs.len())?;
        if cut.is_none() {
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
                t.tick(Phase::Cursor)?;
                let edges = e.edges(out);
                if let Some(range) = edges.list(n) {
                    sources.push(EdgeSource::Extent { edges, range });
                }
            }
        }
        for (i, run) in runs.iter().enumerate().skip(cut.unwrap_or(0)) {
            t.tick(Phase::Cursor)?;
            let list = run.list(n, out);
            if !list.is_empty() {
                sources.push(EdgeSource::Run {
                    entries: list.iter(),
                    from: i + 1,
                });
            }
        }
        Ok(List {
            view: self,
            rels,
            merge: NewestWins::new(sources, charge)?,
        })
    }

    /// The surviving edges of `n` in one direction, presence not applied (writer paths).
    pub(crate) fn alive(&self, n: u32, out: bool) -> Vec<(u32, u16, Attr)> {
        let mut rows = Vec::new();
        let t = Ticker::unmetered();
        let Ok(mut list) = self.list(n, out, None, None, &t) else {
            return rows;
        };
        while let Ok(Some(row)) = list.next(&t) {
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
        for run in self.sediment.sorted_runs() {
            set.extend(run.edges(out).iter().map(|e| e.key.0));
        }
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
            merge: NewestWins::empty(),
            ticker: Ticker::maybe(budget),
            pending: None,
            done: false,
            lease: None,
        };
        let t = &it.ticker;
        let runs = self.sediment.sorted_runs();
        let prepared = budget.map(Charge::new).transpose().and_then(|mut charge| {
            let mut sources: Vec<PresenceSource<'v>> =
                charged(charge.as_mut(), self.layers.len() + 1 + runs.len())?;
            for e in self.extents() {
                t.tick(Phase::Cursor)?;
                sources.push(PresenceSource::Extent(e.presence.cursor()));
            }
            for run in runs {
                t.tick(Phase::Cursor)?;
                sources.push(PresenceSource::Sediment(run.nodes().iter()));
            }
            let merge = NewestWins::new(sources, charge.as_mut())?;
            Ok((merge, charge.map(Charge::into_lease)))
        });
        match prepared {
            Ok((merge, lease)) => {
                it.merge = merge;
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
            None => self.extents().rev().find_map(|e| e.facts.get(n.0, key))?,
        };
        (kind == OpKind::Add).then_some(payload)
    }

    /// Every surviving fact, presence not applied (writer paths). Runs apply oldest first: a
    /// run's removals delete the older facts of their nodes before its own facts apply.
    pub(crate) fn alive_facts(&self) -> BTreeMap<(u32, u8), &'a [u8]> {
        let mut map = BTreeMap::new();
        for e in self.extents() {
            map.extend(e.facts.iter().map(|(k, kind, p)| (k, (kind, p))));
        }
        for run in self.sediment.sorted_runs() {
            if !run.removed().is_empty() {
                map.retain(|&(n, _), _| !run.removes(n));
            }
            map.extend(
                run.facts()
                    .iter()
                    .map(|f| (f.key, (f.kind, run.payload(f)))),
            );
        }
        map.into_iter()
            .filter(|(_, (kind, _))| *kind == OpKind::Add)
            .map(|(k, (_, p))| (k, p))
            .collect()
    }

    /// The keys of the surviving facts of `n` (writer paths).
    pub(crate) fn alive_fact_keys(&self, n: u32) -> Vec<u8> {
        let mut map: BTreeMap<u8, OpKind> = BTreeMap::new();
        let cut = self.sediment.cut(n);
        if cut.is_none() {
            for e in self.extents() {
                map.extend(e.facts.of_node(n).map(|((_, k), kind, _)| (k, kind)));
            }
        }
        for run in self.sediment.sorted_runs().iter().skip(cut.unwrap_or(0)) {
            map.extend(run.facts_of(n).iter().map(|f| (f.key.1, f.kind)));
        }
        map.into_iter()
            .filter(|&(_, kind)| kind == OpKind::Add)
            .map(|(k, _)| k)
            .collect()
    }
}

/// Refuse a newest-layer union that misses a node touched (in either direction) by any layer of
/// `layers`, one unit of `t` per touched id or index word visited.
fn verify_union(last: &Layer, layers: &[Layer], t: &Ticker<'_>) -> Result<(), GraphError> {
    let Some((out, inc)) = last.extent.union.as_ref() else {
        return Err(corrupt("layer union"));
    };
    for layer in layers {
        for (union, edges) in [(out, &layer.extent.out), (inc, &layer.extent.inc)] {
            let mut ids = edges.touched_cursor();
            while let Some(n) = ids.next(t, Phase::UnionCoverage) {
                if !union.contains(n) {
                    return Err(corrupt("layer union coverage"));
                }
            }
            t.stopped()?;
        }
    }
    Ok(())
}

/// One presence source: an extent section or a sediment run.
enum PresenceSource<'v> {
    Extent(PresenceIter<'v>),
    Sediment(std::slice::Iter<'v, SedNode>),
}

impl Source for PresenceSource<'_> {
    type Key = u32;
    type Value = OpKind;

    fn advance(&mut self, t: &Ticker<'_>, phase: Phase) -> Option<(u32, OpKind)> {
        match self {
            Self::Extent(it) => it.next(t, phase),
            Self::Sediment(it) => {
                if !t.step(phase) {
                    return None;
                }
                it.next().map(|n| (n.id, n.kind))
            }
        }
    }
}

/// Present nodes merged from presence, metered.
pub(crate) struct Nodes<'v> {
    merge: NewestWins<PresenceSource<'v>>,
    ticker: Ticker<'v>,
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
            let item = self.merge.next(&self.ticker);
            if let Err(e) = self.ticker.stopped() {
                self.done = true;
                return Some(Err(e));
            }
            let Some((n, kind, _)) = item else {
                self.done = true;
                return None;
            };
            if kind == OpKind::Add {
                return Some(Ok(LocalId(n)));
            }
        }
    }
}

/// One source of a node's list: an extent section range or a sediment run. The value carries
/// the first run newer than the entry (0 for an extent entry): a removal of the neighbour in
/// that run or a later one deletes the entry.
enum EdgeSource<'v> {
    Extent {
        edges: &'v Edges,
        range: Range<usize>,
    },
    Run {
        entries: std::slice::Iter<'v, SedEdge>,
        from: usize,
    },
}

impl Source for EdgeSource<'_> {
    type Key = (u32, u16);
    type Value = (OpKind, Attr, usize);

    fn advance(&mut self, t: &Ticker<'_>, phase: Phase) -> Option<(Self::Key, Self::Value)> {
        if !t.step(phase) {
            return None;
        }
        match self {
            Self::Extent { edges, range } => {
                let (nbr, rel, kind, attr) = edges.entry(range.next()?)?;
                Some(((nbr, rel), (kind, attr, 0)))
            }
            Self::Run { entries, from } => entries
                .next()
                .map(|e| ((e.key.1, e.key.2), (e.kind, e.attr, *from))),
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
    fn next(&mut self, t: &Ticker<'_>) -> Result<Option<(u32, u16, Attr)>, GraphError> {
        loop {
            let item = self.merge.next(t);
            t.stopped()?;
            let Some(((nbr, rel), (kind, attr, from), _)) = item else {
                return Ok(None);
            };
            // An entry whose neighbour is removed in a newer run is deleted.
            if kind != OpKind::Add || self.view.sediment.removed_since(nbr, from) {
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
                *head = l.next(&self.ticker)?;
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
