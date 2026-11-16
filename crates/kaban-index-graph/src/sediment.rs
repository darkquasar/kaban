//! Prepared sediment: the net change of a run of sediment batches as sorted, barrier-applied
//! vectors that own their fact payloads and hold their budget charge (PC-17). It is built once
//! and borrowed by every [`crate::View`] over it, and extended in place of a rebuild when newer
//! batches arrive.
//!
//! Batches apply oldest first. Within a batch the node stream applies first, then the edge and
//! fact streams, so a node removal is a deletion barrier (MED-02): every edge with the node as
//! an endpoint and every fact of the node recorded before the removal is deleted, in this
//! sediment and in every extent under it, even when a later record re-adds the node. Records of
//! the removing batch itself come after the barrier and survive.
//!
//! Building sorts in place (no hidden allocation) and compacts each stream to its newest record
//! per key. Every copied record, payload chunk, sort sift, compaction and merge step is one unit
//! of work, so a cancellation or deadline stops building or extending within
//! [`crate::BUDGET_CHECK_INTERVAL`] units.

use std::ops::Range;

use kaban_core::query_budget::{BudgetLease, QueryBudget};

use crate::charge::{Charge, Ticker, sort_checked};
use crate::error::GraphError;
use crate::op::{Attr, COPY_CHUNK, OpKind, Records};

/// A record's position: (batch, index in its stream).
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

/// One fact's newest surviving change; its payload is a range of the owning arena.
#[derive(Debug, Clone)]
struct SedFact {
    key: (u32, u8),
    order: Order,
    kind: OpKind,
    payload: Range<usize>,
}

/// The prepared, sorted and barrier-applied sediment of a run of batches, owning its payloads
/// and holding the budget charge of its vectors while it lives.
///
/// Build it once with [`PreparedSediment::prepare`], borrow it from every [`crate::View`] that
/// reads the same batches (a view does no sediment work at construction), and when newer
/// batches arrive call [`PreparedSediment::extend`], which returns a new value by a linear merge
/// of the prepared state with the newly prepared batches; the old value is unchanged, so views
/// over it stay correct while they live.
#[derive(Debug)]
pub struct PreparedSediment {
    nodes: Vec<SedNode>,
    out: Vec<SedEdge>,
    inc: Vec<SedEdge>,
    facts: Vec<SedFact>,
    /// Fact payload bytes; each fact holds a range of it.
    arena: Vec<u8>,
    /// Batches applied so far: the batch index of the next extension's first batch.
    batches: usize,
    /// Whether any node is removed in the sediment.
    barriers: bool,
    /// The charge of the vectors; `None` for the empty sediment.
    _lease: Option<BudgetLease>,
}

/// The empty sediment, shared by chain-only readers.
pub(crate) static EMPTY: PreparedSediment = PreparedSediment::empty();

/// Total length of one stream over every batch.
fn total<R: AsRef<Records>>(batches: &[R], len: impl Fn(&Records) -> usize) -> usize {
    batches
        .iter()
        .map(|r| len(r.as_ref()))
        .fold(0usize, usize::saturating_add)
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
        t.tick()?;
        let mut last = i;
        while last + 1 < v.len() && key(&v[last + 1]) == key(&v[i]) {
            t.tick()?;
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

/// Merge two sorted, key-unique runs: per key the `new` element wins; every emitted element
/// that `keep` accepts goes to `emit` with whether it came from `new`. One unit per element.
fn merge_newest<T, K: Ord>(
    old: &[T],
    new: &[T],
    key: impl Fn(&T) -> K,
    keep: impl Fn(&T) -> bool,
    mut emit: impl FnMut(&T, bool) -> Result<(), GraphError>,
    t: &Ticker<'_>,
) -> Result<(), GraphError> {
    let (mut at_old, mut at_new) = (0, 0);
    loop {
        t.tick()?;
        let (item, fresh) = match (old.get(at_old), new.get(at_new)) {
            (Some(older), Some(newer)) => match key(older).cmp(&key(newer)) {
                std::cmp::Ordering::Less => {
                    at_old += 1;
                    (older, false)
                }
                std::cmp::Ordering::Equal => {
                    at_old += 1;
                    at_new += 1;
                    (newer, true)
                }
                std::cmp::Ordering::Greater => {
                    at_new += 1;
                    (newer, true)
                }
            },
            (Some(older), None) => {
                at_old += 1;
                (older, false)
            }
            (None, Some(newer)) => {
                at_new += 1;
                (newer, true)
            }
            (None, None) => return Ok(()),
        };
        if keep(item) {
            emit(item, fresh)?;
        }
    }
}

/// The barrier batch of `id` in `nodes` (sorted by id).
fn barrier_of(nodes: &[SedNode], id: u32) -> Option<usize> {
    nodes
        .binary_search_by_key(&id, |n| n.id)
        .ok()
        .and_then(|at| nodes[at].barrier)
}

/// Whether a record of `batch` naming `ids` survives the barriers of `nodes`: it is not older
/// than the last removal of any of its nodes.
fn survives(nodes: &[SedNode], batch: usize, ids: &[u32]) -> bool {
    ids.iter()
        .all(|&id| barrier_of(nodes, id).is_none_or(|b| batch >= b))
}

/// Copy `bytes` into `arena` (reserved), one unit per [`COPY_CHUNK`]; returns its range.
fn copy_payload(
    arena: &mut Vec<u8>,
    bytes: &[u8],
    t: &Ticker<'_>,
) -> Result<Range<usize>, GraphError> {
    let start = arena.len();
    for chunk in bytes.chunks(COPY_CHUNK) {
        t.tick()?;
        arena.extend_from_slice(chunk);
    }
    Ok(start..arena.len())
}

impl PreparedSediment {
    /// No sediment: nothing is allocated or charged.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            nodes: Vec::new(),
            out: Vec::new(),
            inc: Vec::new(),
            facts: Vec::new(),
            arena: Vec::new(),
            batches: 0,
            barriers: false,
            _lease: None,
        }
    }

    /// Prepare `batches` (oldest first): copy, sort and compact their streams and apply the
    /// deletion barriers. Every vector is reserved on `budget` before it is allocated and stays
    /// charged while the result lives; the fact payloads are copied, so the result does not
    /// borrow `batches`.
    ///
    /// # Errors
    /// [`GraphError::BudgetExceeded`] when `budget` refuses a reservation, already refuses at
    /// entry or is cancelled or past its deadline while preparing; nothing stays charged.
    pub fn prepare<R: AsRef<Records>>(
        batches: &[R],
        budget: &QueryBudget,
    ) -> Result<Self, GraphError> {
        Self::prepare_from(batches, 0, budget)
    }

    /// Prepare `batches` numbered from batch index `first`.
    fn prepare_from<R: AsRef<Records>>(
        batches: &[R],
        first: usize,
        budget: &QueryBudget,
    ) -> Result<Self, GraphError> {
        let mut charge = Charge::new(budget)?;
        let t = Ticker::new(budget);

        let mut nodes: Vec<SedNode> = charge.vec(total(batches, |r| r.nodes.len()))?;
        for (b, r) in batches.iter().enumerate() {
            for (i, op) in r.as_ref().nodes.iter().enumerate() {
                t.tick()?;
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
        let mut barriers = false;
        while i < nodes.len() {
            let mut entry = nodes[i];
            let mut barrier = None;
            while i < nodes.len() && nodes[i].id == entry.id {
                t.tick()?;
                if nodes[i].kind == OpKind::Remove {
                    barrier = Some(nodes[i].order.0);
                }
                entry = nodes[i];
                i += 1;
            }
            barriers |= barrier.is_some();
            nodes[kept] = SedNode { barrier, ..entry };
            kept += 1;
        }
        nodes.truncate(kept);

        let mut out: Vec<SedEdge> = charge.vec(total(batches, |r| r.edges.len()))?;
        for (b, r) in batches.iter().enumerate() {
            for (i, e) in r.as_ref().edges.iter().enumerate() {
                t.tick()?;
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
            |e| survives(&nodes, e.order.0, &[e.key.0, e.key.1]),
            &t,
        )?;
        out.truncate(kept);
        let mut inc: Vec<SedEdge> = charge.vec(out.len())?;
        for e in &out {
            t.tick()?;
            inc.push(SedEdge {
                key: (e.key.1, e.key.0, e.key.2),
                ..*e
            });
        }
        sort_checked(&mut inc, |e| e.key, &t)?;

        let payload = total(batches, |r| {
            r.facts
                .iter()
                .map(|f| f.payload.len())
                .fold(0usize, usize::saturating_add)
        });
        let mut arena: Vec<u8> = charge.vec(payload)?;
        let mut facts: Vec<SedFact> = charge.vec(total(batches, |r| r.facts.len()))?;
        for (b, r) in batches.iter().enumerate() {
            for (i, f) in r.as_ref().facts.iter().enumerate() {
                t.tick()?;
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
            |f| survives(&nodes, f.order.0, &[f.key.0]),
            &t,
        )?;
        facts.truncate(kept);

        Ok(Self {
            nodes,
            out,
            inc,
            facts,
            arena,
            batches: first + batches.len(),
            barriers,
            _lease: Some(charge.into_lease()),
        })
    }

    /// This sediment followed by `batches` (oldest first), as a new value equal to preparing
    /// every batch from scratch (barriers and newest-wins included); `self` is unchanged, so
    /// views over it stay valid.
    ///
    /// Work: `batches` are prepared alone (sorting only them), then merged with the prepared
    /// state by one linear pass per stream, O(prepared + new) units with no re-sort of what was
    /// already prepared. The new value's vectors are reserved on `budget` and stay charged
    /// while it lives; the working copy of `batches` is released before returning.
    ///
    /// # Errors
    /// As [`PreparedSediment::prepare`]; nothing new stays charged.
    pub fn extend<R: AsRef<Records>>(
        &self,
        batches: &[R],
        budget: &QueryBudget,
    ) -> Result<Self, GraphError> {
        let new = Self::prepare_from(batches, self.batches, budget)?;
        self.merge(&new, budget)
    }

    /// `self` followed by `new` (prepared from the batches after `self`'s).
    fn merge(&self, new: &Self, budget: &QueryBudget) -> Result<Self, GraphError> {
        let mut charge = Charge::new(budget)?;
        let t = Ticker::new(budget);

        let mut nodes: Vec<SedNode> = charge.vec(self.nodes.len() + new.nodes.len())?;
        let mut barriers = false;
        merge_newest(
            &self.nodes,
            &new.nodes,
            |n| n.id,
            |_| true,
            |n, fresh| {
                let barrier = if fresh {
                    n.barrier.or_else(|| barrier_of(&self.nodes, n.id))
                } else {
                    n.barrier
                };
                barriers |= barrier.is_some();
                nodes.push(SedNode { barrier, ..*n });
                Ok(())
            },
            &t,
        )?;

        let edges = |old: &[SedEdge], fresh: &[SedEdge], charge: &mut Charge| {
            let mut merged: Vec<SedEdge> = charge.vec(old.len() + fresh.len())?;
            merge_newest(
                old,
                fresh,
                |e| e.key,
                |e| survives(&nodes, e.order.0, &[e.key.0, e.key.1]),
                |e, _| {
                    merged.push(*e);
                    Ok(())
                },
                &t,
            )?;
            Ok::<_, GraphError>(merged)
        };
        let out = edges(&self.out, &new.out, &mut charge)?;
        let inc = edges(&self.inc, &new.inc, &mut charge)?;

        let mut arena: Vec<u8> = charge.vec(self.arena.len() + new.arena.len())?;
        let mut facts: Vec<SedFact> = charge.vec(self.facts.len() + new.facts.len())?;
        merge_newest(
            &self.facts,
            &new.facts,
            |f| f.key,
            |f| survives(&nodes, f.order.0, &[f.key.0]),
            |f, fresh| {
                let from = if fresh { new } else { self };
                let bytes = from.arena.get(f.payload.clone()).unwrap_or_default();
                let payload = copy_payload(&mut arena, bytes, &t)?;
                facts.push(SedFact {
                    payload,
                    ..f.clone()
                });
                Ok(())
            },
            &t,
        )?;

        Ok(Self {
            nodes,
            out,
            inc,
            facts,
            arena,
            batches: new.batches,
            barriers,
            _lease: Some(charge.into_lease()),
        })
    }

    /// Batches applied so far.
    #[must_use]
    pub fn batches(&self) -> usize {
        self.batches
    }

    /// The newest membership change of `id` in the sediment.
    pub(crate) fn node(&self, id: u32) -> Option<OpKind> {
        self.nodes
            .binary_search_by_key(&id, |n| n.id)
            .ok()
            .map(|at| self.nodes[at].kind)
    }

    /// Whether `id` is removed somewhere in the sediment: every extent record of it is deleted.
    pub(crate) fn barrier(&self, id: u32) -> bool {
        self.barriers && barrier_of(&self.nodes, id).is_some()
    }

    /// Node changes ascending by id.
    pub(crate) fn nodes(&self) -> &[SedNode] {
        &self.nodes
    }

    /// The surviving edge changes of `n` in one direction, ascending by (neighbour, relation).
    pub(crate) fn list(&self, n: u32, out: bool) -> &[SedEdge] {
        let all = if out { &self.out } else { &self.inc };
        let lo = all.partition_point(|e| e.key.0 < n);
        let hi = all.partition_point(|e| e.key.0 <= n);
        &all[lo..hi]
    }

    /// Nodes with a surviving edge change in one direction, ascending.
    pub(crate) fn listed(&self, out: bool) -> impl Iterator<Item = u32> + '_ {
        let all = if out { &self.out } else { &self.inc };
        let mut prev = None;
        all.iter().filter_map(move |e| {
            let fresh = prev != Some(e.key.0);
            prev = Some(e.key.0);
            fresh.then_some(e.key.0)
        })
    }

    /// The surviving change of fact `key` of `n`.
    pub(crate) fn fact(&self, n: u32, key: u8) -> Option<(OpKind, &[u8])> {
        let f = &self.facts[self.facts.binary_search_by_key(&(n, key), |f| f.key).ok()?];
        Some((f.kind, self.arena.get(f.payload.clone())?))
    }

    /// Every surviving fact change in (node, key) order.
    pub(crate) fn facts(&self) -> impl Iterator<Item = ((u32, u8), OpKind, &[u8])> + '_ {
        self.facts.iter().map(|f| {
            (
                f.key,
                f.kind,
                self.arena.get(f.payload.clone()).unwrap_or_default(),
            )
        })
    }

    /// The smallest domain covering every id named here.
    pub(crate) fn domain(&self) -> u64 {
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
