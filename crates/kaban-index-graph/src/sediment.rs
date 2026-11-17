//! Prepared sediment: the pending sediment batches as a short sequence of immutable sorted
//! runs (a log-structured merge), each owning its fact payloads and holding its budget charge
//! (PC-17). It is built once, borrowed by every [`crate::View`] over the same batches, and
//! extended by adding a run instead of rebuilding.
//!
//! # Chronology and barriers
//!
//! Batches carry sequence numbers in the order they apply, oldest first, and every run covers
//! a contiguous range of them; runs are ordered oldest first and never reordered. Within a
//! batch the node stream applies first, then the edge and fact streams, so a node removal is a
//! deletion barrier (MED-02): every edge with the node as an endpoint and every fact of the
//! node recorded before the removal is deleted, in older runs and in every extent under the
//! sediment, even when a later record re-adds the node. Records of the removing batch itself
//! come after the barrier and survive. A run stores, per node, the sequence number of its last
//! removal, so the barriers survive merging.
//!
//! # Runs and merging
//!
//! A run holds four streams (nodes by id, outgoing and incoming edges by (node, neighbour,
//! relation), facts by (node, key)), each sorted and compacted to its newest surviving record
//! per key, with the run's own barriers applied. [`PreparedSediment::extend`] prepares the new
//! batches as one run, appends it and then merges by size tier, with this exact rule: let `s`
//! be the size of the new run (records over its four streams); walking from the newest older
//! run backwards, a run of size `o` joins the merge while `2 * s >= o`, and `s` grows by `o`
//! each time. The joined runs are combined by one k-way merge into one run. Afterwards each run
//! is more than twice the size of the merged run after it, so the runs shrink geometrically
//! towards the newest and there are at most about log2(records) + 1 of them.
//!
//! A run of size `o` joins a merge only together with newer runs of total size at least `o / 2`,
//! so each time a record already in a run is moved by a merge, the run it lands in is at least
//! 1.5 times as large as the one it left. Each record is therefore moved at most
//! 1 + log1.5(N) (about 1 + 1.71 log2 N) times over N appended records: O(log N) moves per
//! appended record, amortised. Compaction only removes records (an overwritten or deleted key),
//! which are never moved again.
//!
//! The run count is also capped by [`MAX_SEDIMENT_RUNS`]: while an extension would leave more,
//! the two adjacent runs with the smallest combined size are merged, so a read never merges more
//! than that many sediment sources. Only adjacent runs are ever merged, which keeps batch order.
//!
//! A merge is a k-way merge of already sorted streams through a heap of the run heads: per key
//! the newest run wins, a node keeps the newest of its removals, and an edge or fact is kept
//! only when it is not older than the last removal of its nodes. Merges never sort.
//!
//! # What sorts, and when (GRA-06)
//!
//! - Each batch is sorted exactly once, when it becomes (part of) a new run, by
//!   [`PreparedSediment::prepare`] or [`PreparedSediment::extend`]: an interruptible in-place
//!   heapsort of that batch's records only, never of what was prepared before.
//! - Queries never sort: [`crate::View::new`] over a prepared value does no sediment work, and
//!   every read merges sorted lists.
//! - Merging runs never sorts, and [`PreparedSediment::seal`] never sorts the runs: it merges
//!   them as they are. It sorts only the tombstones it derives from the chain for the removed
//!   nodes, which are not part of any run.
//! - A fresh process that rebuilds the value from stored batches prepares them (one run) or
//!   extends batch by batch; either way each stored batch is sorted once.
//!
//! # Reads
//!
//! A read looks a key up in each run, newest first, by binary search, skipping a run without a
//! search when the key lies outside the run's first and last key of that stream (each stream is
//! sorted, so these bounds cost nothing to keep) or, for barriers, outside the run's own list of
//! removed ids, which is empty for most runs. A node's edges come from the runs from its newest
//! removal on (older runs and the extents are behind its barrier), merged with the extents'
//! lists by the same heap merge; an older entry whose neighbour is removed in a newer run is
//! deleted.
//!
//! # Work
//!
//! Every copied record, payload chunk, sort sift, compaction step, merge step and heap push or
//! pop is one unit of work, so a cancellation or deadline stops building, extending, merging or
//! sealing's merge within [`crate::BUDGET_CHECK_INTERVAL`] units. Every run, merge output and run list
//! is reserved on the budget before it is built; a refused or cancelled extension returns the
//! error and leaves the value it extends unchanged.

use std::ops::Range;
use std::sync::Arc;

use kaban_core::query_budget::{BudgetLease, QueryBudget};

use crate::charge::{Charge, Phase, Ticker, sort_checked};
use crate::error::GraphError;
use crate::layer::{Sections, seal_over};
use crate::merge::{NewestWins, Slice};
use crate::op::{Attr, COPY_CHUNK, OpKind, Records};
use crate::view::View;

/// The most runs a [`PreparedSediment`] holds: an extension that would leave more merges the
/// smallest adjacent runs first, so a read never merges more than this many sediment sources.
pub const MAX_SEDIMENT_RUNS: usize = 32;

/// A record's position: (batch sequence number, index in its stream).
type Order = (usize, usize);

/// One node's newest membership change, and the batch of its last removal.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SedNode {
    pub(crate) id: u32,
    order: Order,
    pub(crate) kind: OpKind,
    /// The batch of the node's last removal: a barrier for everything older.
    barrier: Option<usize>,
}

/// One edge's newest surviving change, keyed by (node, neighbour, relation).
#[derive(Debug, Clone, Copy)]
pub(crate) struct SedEdge {
    pub(crate) key: (u32, u32, u16),
    order: Order,
    pub(crate) kind: OpKind,
    pub(crate) attr: Attr,
}

/// One fact's newest surviving change; its payload is a range of the owning run's arena.
#[derive(Debug, Clone)]
pub(crate) struct SedFact {
    pub(crate) key: (u32, u8),
    order: Order,
    pub(crate) kind: OpKind,
    payload: Range<usize>,
}

/// One immutable sorted run: the net change of a contiguous range of batches, barriers
/// applied, holding the charge of its vectors while it lives.
#[derive(Debug)]
pub(crate) struct Run {
    /// The batch sequence numbers this run covers.
    span: Range<usize>,
    nodes: Vec<SedNode>,
    /// The ids removed in this run (a barrier), ascending.
    removed: Vec<u32>,
    out: Vec<SedEdge>,
    inc: Vec<SedEdge>,
    facts: Vec<SedFact>,
    /// Fact payload bytes; each fact holds a range of it.
    arena: Vec<u8>,
    _lease: BudgetLease,
}

/// The prepared sediment of a sequence of batches: immutable sorted runs, oldest first, each
/// shared by every value that contains it.
///
/// Build it once with [`PreparedSediment::prepare`], borrow it from every [`crate::View`] that
/// reads the same batches (a view does no sediment work at construction), and when newer
/// batches arrive call [`PreparedSediment::extend`], which returns a new value sharing the old
/// runs and adding one (merging runs by size tier, see the module documentation). The old value
/// is unchanged, so views over it stay correct while they live. [`PreparedSediment::seal`]
/// writes a prefix of the runs as a layer and returns the rest.
///
/// What sorts and when: each batch is sorted once, when it becomes a run; queries, run merges
/// and sealing never sort the runs (see the module documentation, "What sorts, and when").
#[derive(Debug)]
pub struct PreparedSediment {
    runs: Vec<Arc<Run>>,
    /// Batches applied so far: the sequence number of the next extension's first batch.
    batches: usize,
    /// The charge of the run list; `None` for the empty sediment.
    _lease: Option<BudgetLease>,
}

/// A layer sealed from a prefix of a [`PreparedSediment`]'s runs, and the runs left over.
#[derive(Debug)]
pub struct Sealed {
    /// The layer bytes, to open with [`crate::Layer::open`] and append to the parent's chain.
    pub layer: Vec<u8>,
    /// Batches before this sequence number are in the layer; the rest are in [`Sealed::rest`].
    pub through: usize,
    /// The runs not sealed, to read over the chain extended by the layer.
    pub rest: PreparedSediment,
}

/// The empty sediment, shared by chain-only readers.
pub(crate) static EMPTY: PreparedSediment = PreparedSediment::empty();

/// Total length of one stream over every batch, one unit of `t` per batch.
fn total<R: AsRef<Records>>(
    batches: &[R],
    len: impl Fn(&Records) -> usize,
    t: &Ticker<'_>,
) -> Result<usize, GraphError> {
    t.sum(Phase::CountBatches, batches, |r| len(r.as_ref()))
}

/// Total fact payload bytes over every batch, one unit of `t` per batch and per fact.
fn payload_bytes<R: AsRef<Records>>(batches: &[R], t: &Ticker<'_>) -> Result<usize, GraphError> {
    let mut bytes = 0usize;
    for r in t.each(Phase::SizePayload, batches) {
        let facts = &r?.as_ref().facts;
        bytes = bytes.saturating_add(t.sum(Phase::SizePayload, facts, |f| f.payload.len())?);
    }
    Ok(bytes)
}

/// Keep the newest element of every run of equal keys (the slice is sorted by key, then order)
/// when `keep` accepts it; returns the kept length.
fn compact<T: Clone, K: PartialEq>(
    v: &mut [T],
    key: impl Fn(&T) -> K,
    keep: impl Fn(&T) -> bool,
    t: &Ticker<'_>,
) -> Result<usize, GraphError> {
    let mut kept = 0;
    let mut i = 0;
    while i < v.len() {
        t.tick(Phase::Compact)?;
        let mut last = i;
        while last + 1 < v.len() && key(&v[last + 1]) == key(&v[i]) {
            t.tick(Phase::Compact)?;
            last += 1;
        }
        if keep(&v[last]) {
            v[kept] = v[last].clone();
            kept += 1;
        }
        i = last + 1;
    }
    Ok(kept)
}

/// Whether `id` lies within the first and last key of a sorted stream: the skip check that
/// spares a binary search in a run that cannot hold `id`.
fn spans<T>(v: &[T], id: u32, key: impl Fn(&T) -> u32) -> bool {
    match (v.first(), v.last()) {
        (Some(first), Some(last)) => key(first) <= id && id <= key(last),
        _ => false,
    }
}

/// The barrier batch of `id` in `nodes` (sorted by id) when `removed` (its removed ids) holds
/// it.
fn barrier_of(nodes: &[SedNode], removed: &[u32], id: u32) -> Option<usize> {
    if !spans(removed, id, |&r| r) || removed.binary_search(&id).is_err() {
        return None;
    }
    nodes
        .binary_search_by_key(&id, |n| n.id)
        .ok()
        .and_then(|at| nodes[at].barrier)
}

/// Whether a record of `batch` naming `ids` survives the barriers of `nodes`: it is not older
/// than the last removal of any of its nodes.
fn survives(nodes: &[SedNode], removed: &[u32], batch: usize, ids: &[u32]) -> bool {
    removed.is_empty()
        || ids
            .iter()
            .all(|&id| barrier_of(nodes, removed, id).is_none_or(|b| batch >= b))
}

/// Copy `bytes` into `arena` (reserved), one unit per [`COPY_CHUNK`]; returns its range.
fn copy_payload(
    arena: &mut Vec<u8>,
    bytes: &[u8],
    t: &Ticker<'_>,
) -> Result<Range<usize>, GraphError> {
    let start = arena.len();
    for chunk in bytes.chunks(COPY_CHUNK) {
        t.tick(Phase::Copy)?;
        arena.extend_from_slice(chunk);
    }
    Ok(start..arena.len())
}

/// The ids of `nodes` that carry a barrier, ascending, reserved on `charge`.
fn removed_ids(
    nodes: &[SedNode],
    charge: &mut Charge,
    t: &Ticker<'_>,
) -> Result<Vec<u32>, GraphError> {
    let count = t.sum(Phase::CountBarriers, nodes, |n| {
        usize::from(n.barrier.is_some())
    })?;
    let mut removed: Vec<u32> = charge.vec(count)?;
    for n in t.each(Phase::Barriers, nodes) {
        let n = n?;
        if n.barrier.is_some() {
            removed.push(n.id);
        }
    }
    Ok(removed)
}

impl Run {
    /// Prepare `batches` numbered from sequence number `first` as one run: copy, sort and
    /// compact their streams and apply their barriers. `None` when they hold no record.
    fn build<R: AsRef<Records>>(
        batches: &[R],
        first: usize,
        budget: &QueryBudget,
    ) -> Result<Option<Self>, GraphError> {
        let mut charge = Charge::new(budget)?;
        let t = Ticker::new(budget);

        let mut nodes: Vec<SedNode> = charge.vec(total(batches, |r| r.nodes.len(), &t)?)?;
        for batch in t.each(Phase::Copy, batches.iter().enumerate()) {
            let (b, r) = batch?;
            for (i, op) in r.as_ref().nodes.iter().enumerate() {
                t.tick(Phase::Copy)?;
                nodes.push(SedNode {
                    id: op.node.0,
                    order: (first + b, i),
                    kind: op.kind,
                    barrier: None,
                });
            }
        }
        sort_checked(&mut nodes, |n| (n.id, n.order), &t)?;
        // The newest change wins; the barrier is the last removal of the run.
        let mut kept = 0;
        let mut i = 0;
        while i < nodes.len() {
            let mut entry = nodes[i];
            let mut barrier = None;
            while i < nodes.len() && nodes[i].id == entry.id {
                t.tick(Phase::Compact)?;
                if nodes[i].kind == OpKind::Remove {
                    barrier = Some(nodes[i].order.0);
                }
                entry = nodes[i];
                i += 1;
            }
            nodes[kept] = SedNode { barrier, ..entry };
            kept += 1;
        }
        nodes.truncate(kept);
        let removed = removed_ids(&nodes, &mut charge, &t)?;

        let mut out: Vec<SedEdge> = charge.vec(total(batches, |r| r.edges.len(), &t)?)?;
        for batch in t.each(Phase::Copy, batches.iter().enumerate()) {
            let (b, r) = batch?;
            for (i, e) in r.as_ref().edges.iter().enumerate() {
                t.tick(Phase::Copy)?;
                out.push(SedEdge {
                    key: (e.src.0, e.dst.0, e.rel.0),
                    order: (first + b, i),
                    kind: e.kind,
                    attr: e.attr,
                });
            }
        }
        sort_checked(&mut out, |e| (e.key, e.order), &t)?;
        let kept = compact(
            &mut out,
            |e| e.key,
            |e| survives(&nodes, &removed, e.order.0, &[e.key.0, e.key.1]),
            &t,
        )?;
        out.truncate(kept);
        let mut inc: Vec<SedEdge> = charge.vec(out.len())?;
        for e in &out {
            t.tick(Phase::Transpose)?;
            inc.push(SedEdge {
                key: (e.key.1, e.key.0, e.key.2),
                ..*e
            });
        }
        sort_checked(&mut inc, |e| e.key, &t)?;

        let mut arena: Vec<u8> = charge.vec(payload_bytes(batches, &t)?)?;
        let mut facts: Vec<SedFact> = charge.vec(total(batches, |r| r.facts.len(), &t)?)?;
        for batch in t.each(Phase::Copy, batches.iter().enumerate()) {
            let (b, r) = batch?;
            for (i, f) in r.as_ref().facts.iter().enumerate() {
                t.tick(Phase::Copy)?;
                facts.push(SedFact {
                    key: (f.node.0, f.key),
                    order: (first + b, i),
                    kind: f.kind,
                    payload: copy_payload(&mut arena, &f.payload, &t)?,
                });
            }
        }
        sort_checked(&mut facts, |f| (f.key, f.order), &t)?;
        let kept = compact(
            &mut facts,
            |f| f.key,
            |f| survives(&nodes, &removed, f.order.0, &[f.key.0]),
            &t,
        )?;
        facts.truncate(kept);

        if nodes.is_empty() && out.is_empty() && facts.is_empty() {
            return Ok(None);
        }
        Ok(Some(Self {
            span: first..first + batches.len(),
            nodes,
            removed,
            out,
            inc,
            facts,
            arena,
            _lease: charge.into_lease(),
        }))
    }

    /// Merge adjacent `runs` (oldest first, at least one) into one run by a k-way merge of their
    /// sorted streams: per key the newest run wins, a node keeps its newest removal, and an
    /// edge or fact older than the last removal of one of its nodes is dropped. No sorting.
    pub(crate) fn merge(runs: &[Arc<Run>], budget: &QueryBudget) -> Result<Self, GraphError> {
        let mut charge = Charge::new(budget)?;
        let t = Ticker::new(budget);
        let sum = |len: fn(&Run) -> usize| runs.iter().map(|r| len(r)).sum::<usize>();
        let span = match (runs.first(), runs.last()) {
            (Some(first), Some(last)) => first.span.start..last.span.end,
            _ => 0..0,
        };

        let mut nodes: Vec<SedNode> = charge.vec(sum(|r| r.nodes.len()))?;
        let mut sources = charge.vec(runs.len())?;
        sources.extend(
            runs.iter()
                .map(|r| Slice::new(&r.nodes, |n: &SedNode| n.id)),
        );
        let mut merge = NewestWins::new(sources, Some(&mut charge), &t)?;
        loop {
            let mut barrier = None;
            let next = merge.next_with(&t, |_, n| barrier = n.barrier.or(barrier));
            t.stopped()?;
            let Some((_, n, _)) = next else { break };
            nodes.push(SedNode { barrier, ..*n });
        }
        drop(merge);
        let removed = removed_ids(&nodes, &mut charge, &t)?;

        let mut edges = |side: fn(&Run) -> &[SedEdge]| {
            let mut merged: Vec<SedEdge> = charge.vec(runs.iter().map(|r| side(r).len()).sum())?;
            let mut sources = charge.vec(runs.len())?;
            sources.extend(
                runs.iter()
                    .map(|r| Slice::new(side(r), |e: &SedEdge| e.key)),
            );
            let mut merge = NewestWins::new(sources, Some(&mut charge), &t)?;
            loop {
                let next = merge.next(&t);
                t.stopped()?;
                let Some((_, e, _)) = next else { break };
                t.tick(Phase::Merge)?;
                if survives(&nodes, &removed, e.order.0, &[e.key.0, e.key.1]) {
                    merged.push(*e);
                }
            }
            Ok::<_, GraphError>(merged)
        };
        let out = edges(|r| &r.out)?;
        let inc = edges(|r| &r.inc)?;

        let mut arena: Vec<u8> = charge.vec(sum(|r| r.arena.len()))?;
        let mut facts: Vec<SedFact> = charge.vec(sum(|r| r.facts.len()))?;
        let mut sources = charge.vec(runs.len())?;
        sources.extend(
            runs.iter()
                .map(|r| Slice::new(&r.facts, |f: &SedFact| f.key)),
        );
        let mut merge = NewestWins::new(sources, Some(&mut charge), &t)?;
        loop {
            let next = merge.next(&t);
            t.stopped()?;
            let Some((_, f, from)) = next else { break };
            t.tick(Phase::Merge)?;
            if survives(&nodes, &removed, f.order.0, &[f.key.0]) {
                let payload = copy_payload(&mut arena, runs[from].payload(f), &t)?;
                facts.push(SedFact {
                    payload,
                    ..f.clone()
                });
            }
        }
        drop(merge);

        Ok(Self {
            span,
            nodes,
            removed,
            out,
            inc,
            facts,
            arena,
            _lease: charge.into_lease(),
        })
    }

    /// This run written as a layer over the chain of `parent`: its rows as they are, its
    /// removals as barriers over the chain.
    fn seal(&self, parent: &View<'_>) -> Vec<u8> {
        let rows = |side: &[SedEdge]| side.iter().map(|e| (e.key, e.kind, e.attr)).collect();
        let own = Sections {
            nodes: self.nodes.iter().map(|n| (n.id, n.kind)).collect(),
            out: rows(&self.out),
            inc: rows(&self.inc),
            facts: self
                .facts
                .iter()
                .map(|f| (f.key, f.kind, self.payload(f)))
                .collect(),
        };
        seal_over(parent, &self.removed, own)
    }

    /// Records over the four streams: the size the merge rule compares.
    fn size(&self) -> usize {
        self.nodes.len() + self.out.len() + self.inc.len() + self.facts.len()
    }

    /// The newest change of `id` in this run.
    pub(crate) fn node(&self, id: u32) -> Option<&SedNode> {
        if !spans(&self.nodes, id, |n| n.id) {
            return None;
        }
        let at = self.nodes.binary_search_by_key(&id, |n| n.id).ok()?;
        self.nodes.get(at)
    }

    /// Whether `id` is removed in this run: older records of it are deleted.
    pub(crate) fn removes(&self, id: u32) -> bool {
        spans(&self.removed, id, |&r| r) && self.removed.binary_search(&id).is_ok()
    }

    /// The ids removed in this run, ascending.
    pub(crate) fn removed(&self) -> &[u32] {
        &self.removed
    }

    /// Node changes ascending by id.
    pub(crate) fn nodes(&self) -> &[SedNode] {
        &self.nodes
    }

    /// Edge changes of one direction, ascending by (node, neighbour, relation).
    pub(crate) fn edges(&self, out: bool) -> &[SedEdge] {
        if out { &self.out } else { &self.inc }
    }

    /// The edge changes of `n` in one direction, ascending by (neighbour, relation).
    pub(crate) fn list(&self, n: u32, out: bool) -> &[SedEdge] {
        let all = self.edges(out);
        if !spans(all, n, |e| e.key.0) {
            return &[];
        }
        let lo = all.partition_point(|e| e.key.0 < n);
        let hi = all.partition_point(|e| e.key.0 <= n);
        &all[lo..hi]
    }

    /// Fact changes ascending by (node, key).
    pub(crate) fn facts(&self) -> &[SedFact] {
        &self.facts
    }

    /// The fact changes of `n`, ascending by key.
    pub(crate) fn facts_of(&self, n: u32) -> &[SedFact] {
        if !spans(&self.facts, n, |f| f.key.0) {
            return &[];
        }
        let lo = self.facts.partition_point(|f| f.key.0 < n);
        let hi = self.facts.partition_point(|f| f.key.0 <= n);
        &self.facts[lo..hi]
    }

    /// The change of fact `key` of `n` in this run.
    pub(crate) fn fact(&self, n: u32, key: u8) -> Option<&SedFact> {
        if !spans(&self.facts, n, |f| f.key.0) {
            return None;
        }
        let at = self.facts.binary_search_by_key(&(n, key), |f| f.key).ok()?;
        self.facts.get(at)
    }

    /// The payload of `f`, a fact of this run.
    pub(crate) fn payload(&self, f: &SedFact) -> &[u8] {
        self.arena.get(f.payload.clone()).unwrap_or_default()
    }

    /// The smallest domain covering every id named here.
    fn domain(&self) -> u64 {
        let top = [
            self.nodes.last().map(|n| n.id),
            self.out.last().map(|e| e.key.0),
            self.inc.last().map(|e| e.key.0),
            self.facts.last().map(|f| f.key.0),
        ];
        top.into_iter()
            .flatten()
            .max()
            .map_or(0, |id| u64::from(id) + 1)
    }
}

/// Where the size-tier rule starts merging `sizes` (oldest first, the newest last): the first
/// index of the suffix that merges, or the last index when nothing merges.
fn tier_start(sizes: &[usize]) -> usize {
    let Some(&newest) = sizes.last() else {
        return 0;
    };
    let mut start = sizes.len() - 1;
    let mut merged = newest;
    while start > 0 && merged.saturating_mul(2) >= sizes[start - 1] {
        start -= 1;
        merged = merged.saturating_add(sizes[start]);
    }
    start
}

impl PreparedSediment {
    /// No sediment: nothing is allocated or charged.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            runs: Vec::new(),
            batches: 0,
            _lease: None,
        }
    }

    /// Prepare `batches` (oldest first) as one run: copy, sort and compact their streams and
    /// apply the deletion barriers. Every vector is reserved on `budget` before it is
    /// allocated and stays charged while the result lives; the fact payloads are copied, so the
    /// result does not borrow `batches`.
    ///
    /// # Errors
    /// [`GraphError::BudgetExceeded`] when `budget` refuses a reservation, already refuses at
    /// entry or is cancelled or past its deadline while preparing; nothing stays charged.
    pub fn prepare<R: AsRef<Records>>(
        batches: &[R],
        budget: &QueryBudget,
    ) -> Result<Self, GraphError> {
        Self::empty().extend(batches, budget)
    }

    /// This sediment followed by `batches` (oldest first), as a new value that answers as
    /// preparing every batch from scratch (barriers and newest-wins included); `self` is
    /// unchanged, so views over it stay valid.
    ///
    /// Work: `batches` are prepared as one new run (sorting only them), which shares `self`'s
    /// runs and then merges with the newest of them by the size-tier rule and the run limit
    /// (see the module documentation): O(log N) record moves per appended record, amortised,
    /// over N records. The new run, every merge output and the run list are reserved on
    /// `budget` before they are built and stay charged while the new value lives; runs shared
    /// with `self` keep their own charge.
    ///
    /// # Errors
    /// As [`PreparedSediment::prepare`]; nothing new stays charged and `self` is unchanged.
    pub fn extend<R: AsRef<Records>>(
        &self,
        batches: &[R],
        budget: &QueryBudget,
    ) -> Result<Self, GraphError> {
        self.extend_within(batches, budget, MAX_SEDIMENT_RUNS)
    }

    /// [`PreparedSediment::extend`] under a run limit of `limit` (at least one).
    fn extend_within<R: AsRef<Records>>(
        &self,
        batches: &[R],
        budget: &QueryBudget,
        limit: usize,
    ) -> Result<Self, GraphError> {
        budget.check()?;
        let fresh = Run::build(batches, self.batches, budget)?;
        let mut charge = Charge::new(budget)?;
        let mut runs: Vec<Arc<Run>> = charge.vec(self.runs.len() + usize::from(fresh.is_some()))?;
        runs.extend(self.runs.iter().cloned());
        if let Some(fresh) = fresh {
            runs.push(Arc::new(fresh));
            let mut sizes: Vec<usize> = charge.vec(runs.len())?;
            sizes.extend(runs.iter().map(|r| r.size()));
            let start = tier_start(&sizes);
            if start + 1 < runs.len() {
                let merged = Run::merge(&runs[start..], budget)?;
                runs.truncate(start);
                runs.push(Arc::new(merged));
            }
        }
        while runs.len() > limit.max(1) {
            let at = (0..runs.len() - 1)
                .min_by_key(|&i| runs[i].size() + runs[i + 1].size())
                .unwrap_or(0);
            let merged = Run::merge(&runs[at..at + 2], budget)?;
            runs[at] = Arc::new(merged);
            runs.remove(at + 1);
        }
        Ok(Self {
            runs,
            batches: self.batches + batches.len(),
            _lease: Some(charge.into_lease()),
        })
    }

    /// Seal the longest prefix of runs that ends at or before batch `upto` as one layer over
    /// the chain of `parent` (its base and layers; its sediment is ignored, as by
    /// [`crate::seal`]), and return the layer with the runs left over.
    ///
    /// The sealed runs are combined by one k-way merge of their sorted streams, as an
    /// extension's merge does (batch order, newest record per key, each node's newest removal),
    /// and written as the layer's sections without sorting them. Every node removed in the
    /// sealed runs is a barrier over the chain: the layer gets a tombstone for each of its
    /// chain edges (both directions, presence not applied) and chain facts, derived from the
    /// chain only, under the runs' own records; those tombstones are the only rows sorted. The
    /// unions take the density-chosen form, as [`crate::seal`] writes them.
    ///
    /// [`Sealed::through`] is the largest sequence number at most `upto` such that every batch
    /// before it is in the layer: `upto` itself when a run ends there or no record lies between,
    /// otherwise the first batch of the run that straddles `upto`. The leftover runs keep their
    /// sequence numbers and are read, and extended, over the chain with the new layer appended:
    /// their removals still delete the older records now in the layer. `self` is unchanged.
    ///
    /// The merge of the sealed runs and the leftover run list are reserved on `budget`,
    /// metered and cancellable; the merge's charge is released when this returns. The layer
    /// bytes and the section rows copied into them are writer memory, unbudgeted like
    /// [`crate::seal`].
    ///
    /// # Errors
    /// [`GraphError::BudgetExceeded`] when `budget` refuses a reservation, already refuses at
    /// entry or is cancelled or past its deadline while merging; nothing stays charged.
    pub fn seal(
        &self,
        parent: &View<'_>,
        upto: usize,
        budget: &QueryBudget,
    ) -> Result<Sealed, GraphError> {
        budget.check()?;
        let count = self.runs.iter().take_while(|r| r.span.end <= upto).count();
        let next = self.runs.get(count).map_or(self.batches, |r| r.span.start);
        let through = upto.min(next);
        let layer = match &self.runs[..count] {
            [] => seal_over(parent, &[], Sections::default()),
            [one] => one.seal(parent),
            many => Run::merge(many, budget)?.seal(parent),
        };
        let mut charge = Charge::new(budget)?;
        let mut rest: Vec<Arc<Run>> = charge.vec(self.runs.len() - count)?;
        rest.extend(self.runs[count..].iter().cloned());
        Ok(Sealed {
            layer,
            through,
            rest: Self {
                runs: rest,
                batches: self.batches,
                _lease: Some(charge.into_lease()),
            },
        })
    }

    /// Batches applied so far.
    #[must_use]
    pub fn batches(&self) -> usize {
        self.batches
    }

    /// The number of immutable sorted runs held: 0 for a sediment without records, otherwise
    /// between 1 and [`MAX_SEDIMENT_RUNS`].
    #[must_use]
    pub fn runs(&self) -> usize {
        self.runs.len()
    }

    /// The runs, oldest first.
    pub(crate) fn sorted_runs(&self) -> &[Arc<Run>] {
        &self.runs
    }

    /// The newest membership change of `id` in the sediment.
    pub(crate) fn node(&self, id: u32) -> Option<OpKind> {
        self.runs
            .iter()
            .rev()
            .find_map(|r| r.node(id))
            .map(|n| n.kind)
    }

    /// The index of the newest run that removes `id`: older runs and every extent hold no
    /// surviving record of it.
    pub(crate) fn cut(&self, id: u32) -> Option<usize> {
        self.runs.iter().rposition(|r| r.removes(id))
    }

    /// Whether `id` is removed in run `from` or a newer one: records of it older than run
    /// `from` (from extents when `from` is 0) are deleted.
    pub(crate) fn removed_since(&self, id: u32, from: usize) -> bool {
        self.runs
            .get(from..)
            .is_some_and(|newer| newer.iter().any(|r| r.removes(id)))
    }

    /// The sediment's change of fact `key` of `n`, newest run first; a removal of `n` reads as
    /// a removed fact (it deletes every older value).
    pub(crate) fn fact(&self, n: u32, key: u8) -> Option<(OpKind, &[u8])> {
        for r in self.runs.iter().rev() {
            if let Some(f) = r.fact(n, key) {
                return Some((f.kind, r.payload(f)));
            }
            if r.removes(n) {
                return Some((OpKind::Remove, &[]));
            }
        }
        None
    }

    /// The smallest domain covering every id named here.
    pub(crate) fn domain(&self) -> u64 {
        self.runs.iter().map(|r| r.domain()).max().unwrap_or(0)
    }
}
#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)] // test code: failures are the assertion

    use super::*;
    use crate::id::{Direction, LocalId, RelId, RelSelection};
    use crate::op::{EdgeOp, FactOp, NodeOp};

    /// Batch `i`: one or two edges, a node change (every fifth a removal) and a fact change.
    fn batch(i: u32) -> Records {
        let id = |k: u32| LocalId(k % 9);
        let mut r = Records {
            edges: vec![EdgeOp {
                src: id(i),
                rel: RelId(u16::try_from(i % 3).unwrap()),
                dst: id(i * 4 + 1),
                kind: if i % 6 == 4 {
                    OpKind::Remove
                } else {
                    OpKind::Add
                },
                attr: Attr {
                    weight: i64::from(i),
                    ts: 0,
                },
            }],
            nodes: vec![NodeOp {
                node: id(i * 7),
                kind: if i % 5 == 3 {
                    OpKind::Remove
                } else {
                    OpKind::Add
                },
            }],
            facts: vec![FactOp {
                node: id(i * 2),
                key: u8::try_from(i % 2).unwrap(),
                kind: OpKind::Add,
                payload: i.to_le_bytes().to_vec(),
            }],
        };
        if i.is_multiple_of(4) {
            r.edges.push(EdgeOp {
                src: id(i + 2),
                rel: RelId(0),
                dst: id(i),
                kind: OpKind::Add,
                attr: Attr::default(),
            });
        }
        r
    }

    type Answer = (bool, Vec<Option<Vec<u8>>>, Vec<(u32, u16, Attr)>);

    /// Everything a view over `s` answers about ids `0..10`.
    fn answers(s: &PreparedSediment) -> Vec<Answer> {
        let b = QueryBudget::new(u64::MAX, None);
        let v = View::new(None, &[], s, &b).unwrap();
        (0..10)
            .map(|n| {
                let n = LocalId(n);
                let facts = (0..2).map(|k| v.fact(n, k).map(<[u8]>::to_vec)).collect();
                let rows = v
                    .neighbours(n, &RelSelection::All, Direction::Both, &b)
                    .map(|row| row.map(|(m, r, a)| (m.0, r.0, a)))
                    .collect::<Result<_, _>>()
                    .unwrap();
                (v.contains(n), facts, rows)
            })
            .collect()
    }

    /// The charge of a vector's allocation, as [`Charge::vec`] reserves it.
    fn vec_charge<T>(v: &[T], capacity: usize) -> u64 {
        let _ = v;
        if capacity == 0 || size_of::<T>() == 0 {
            return 0;
        }
        let bytes = u64::try_from(capacity * size_of::<T>()).unwrap();
        QueryBudget::allocation_charge(bytes, align_of::<T>() as u64).unwrap()
    }

    /// Everything one run owns: its six vectors and the shared allocation of its `Arc` (the two
    /// reference counts and the run itself).
    fn run_charge(r: &Run) -> u64 {
        let arc = std::alloc::Layout::new::<[usize; 2]>()
            .extend(std::alloc::Layout::new::<Run>())
            .unwrap()
            .0
            .pad_to_align();
        vec_charge(&r.nodes, r.nodes.capacity())
            + vec_charge(&r.removed, r.removed.capacity())
            + vec_charge(&r.out, r.out.capacity())
            + vec_charge(&r.inc, r.inc.capacity())
            + vec_charge(&r.facts, r.facts.capacity())
            + vec_charge(&r.arena, r.arena.capacity())
            + QueryBudget::allocation_charge(arc.size() as u64, arc.align() as u64).unwrap()
    }

    /// The charge of a value's run list.
    fn list_charge(s: &PreparedSediment) -> u64 {
        vec_charge(&s.runs, s.runs.capacity())
    }

    fn records(ids: std::ops::Range<u32>) -> Records {
        Records {
            nodes: ids
                .clone()
                .map(|n| NodeOp {
                    node: LocalId(n),
                    kind: OpKind::Add,
                })
                .collect(),
            edges: ids
                .clone()
                .map(|n| EdgeOp {
                    src: LocalId(n),
                    rel: RelId(0),
                    dst: LocalId(n + 1),
                    kind: OpKind::Add,
                    attr: Attr::default(),
                })
                .collect(),
            facts: ids
                .map(|n| FactOp {
                    node: LocalId(n),
                    key: 0,
                    kind: OpKind::Add,
                    payload: vec![7; 40],
                })
                .collect(),
        }
    }

    /// POST-MED-02: a run's charge is exactly what it retains, its shared allocation included.
    #[test]
    fn a_tiny_runs_charge_equals_what_it_retains() {
        let b = QueryBudget::new(u64::MAX, None);
        let value = PreparedSediment::prepare(&[records(0..1)], &b).unwrap();
        assert_eq!(value.runs.len(), 1);
        assert_eq!(
            b.live_bytes(),
            list_charge(&value) + run_charge(&value.runs[0])
        );
        drop(value);
        assert_eq!(b.live_bytes(), 0);
    }

    /// POST-MED-02: a run shared by two values on two budgets stays charged once, to the budget
    /// that built it, until its last holder drops, in either drop order.
    #[test]
    fn a_shared_run_is_charged_once_until_its_last_holder_drops() {
        for older_first in [true, false] {
            let (b1, b2) = (
                QueryBudget::new(u64::MAX, None),
                QueryBudget::new(u64::MAX, None),
            );
            let older = PreparedSediment::prepare(&[records(0..20)], &b1).unwrap();
            // A small newer run stays apart: no merge.
            let newer = older.extend(&[records(100..101)], &b2).unwrap();
            assert_eq!(newer.runs.len(), 2, "the runs stay unmerged");
            assert!(Arc::ptr_eq(&older.runs[0], &newer.runs[0]), "shared");
            let shared = run_charge(&older.runs[0]);
            assert_eq!(b1.live_bytes(), list_charge(&older) + shared);
            assert_eq!(
                b2.live_bytes(),
                list_charge(&newer) + run_charge(&newer.runs[1])
            );
            if older_first {
                drop(older);
                assert_eq!(b1.live_bytes(), shared, "the shared run stays charged");
                drop(newer);
            } else {
                drop(newer);
                assert_eq!(b2.live_bytes(), 0);
                drop(older);
            }
            assert_eq!((b1.live_bytes(), b2.live_bytes()), (0, 0), "no leak");
        }
    }

    /// POST-MED-02: a merged value keeps no charge for the merge's scratch or the size
    /// selection once they are freed.
    #[test]
    fn a_merged_values_charge_equals_what_it_retains() {
        let b1 = QueryBudget::new(u64::MAX, None);
        let older = PreparedSediment::prepare(&[records(0..20)], &b1).unwrap();
        let b2 = QueryBudget::new(u64::MAX, None);
        let merged = older.extend(&[records(20..40)], &b2).unwrap();
        assert_eq!(merged.runs.len(), 1, "the equal-sized runs merge");
        assert_eq!(
            b2.live_bytes(),
            list_charge(&merged) + run_charge(&merged.runs[0])
        );
        drop(merged);
        assert_eq!(b2.live_bytes(), 0);
    }

    #[test]
    fn a_run_limit_merges_adjacent_runs_and_keeps_every_answer() {
        let b = QueryBudget::new(u64::MAX, None);
        for limit in [1, 2, 3] {
            let mut value = PreparedSediment::empty();
            let mut all = Vec::new();
            for i in 0..40 {
                let r = batch(i);
                value = value
                    .extend_within(std::slice::from_ref(&r), &b, limit)
                    .unwrap();
                all.push(r);
                let runs = value.runs();
                assert!((1..=limit).contains(&runs), "limit {limit}: {runs} runs");
                let scratch = PreparedSediment::prepare(&all, &b).unwrap();
                assert_eq!(
                    answers(&value),
                    answers(&scratch),
                    "limit {limit}, batch {i}"
                );
            }
        }
    }
}
