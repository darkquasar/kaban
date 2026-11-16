//! Sediment preparation: the net change of a view's sediment batches, as sorted vectors charged
//! to the query budget (PC-17).
//!
//! Batches apply oldest first. Within a batch the node stream applies first, then the edge and
//! fact streams, so a node removal is a deletion barrier (MED-02): every edge with the node as
//! an endpoint and every fact of the node recorded before the removal is deleted, in this
//! sediment and in every extent under it, even when a later record re-adds the node. Records of
//! the removing batch itself come after the barrier and survive.
//!
//! Preparation sorts in place (no hidden allocation) and compacts each stream to its newest
//! record per key. The vectors keep the capacity of the input streams; that capacity is what is
//! charged and it stays charged until the view is dropped.

use kaban_core::query_budget::{BudgetLease, QueryBudget};

use crate::charge::Charge;
use crate::error::GraphError;
use crate::op::{Attr, OpKind, Records};

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

/// One fact's newest surviving change.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SedFact<'a> {
    pub(crate) key: (u32, u8),
    order: Order,
    pub(crate) kind: OpKind,
    pub(crate) payload: &'a [u8],
}

/// The prepared sediment of one view.
#[derive(Debug)]
pub(crate) struct Sediment<'a> {
    nodes: Vec<SedNode>,
    out: Vec<SedEdge>,
    inc: Vec<SedEdge>,
    facts: Vec<SedFact<'a>>,
    /// Whether any node is removed in the sediment.
    barriers: bool,
    _lease: BudgetLease,
}

/// Total length of one stream over every batch.
fn total<R: AsRef<Records>>(batches: &[R], len: impl Fn(&Records) -> usize) -> usize {
    batches
        .iter()
        .map(|r| len(r.as_ref()))
        .fold(0usize, usize::saturating_add)
}

/// Keep the newest element of every run of equal keys (the slice is sorted by key, then order)
/// when `keep` accepts it; returns the kept length.
fn compact<T: Copy, K: PartialEq>(
    v: &mut [T],
    key: impl Fn(&T) -> K,
    keep: impl Fn(&T) -> bool,
) -> usize {
    let mut kept = 0;
    let mut i = 0;
    while i < v.len() {
        let mut last = i;
        while last + 1 < v.len() && key(&v[last + 1]) == key(&v[i]) {
            last += 1;
        }
        if keep(&v[last]) {
            v[kept] = v[last];
            kept += 1;
        }
        i = last + 1;
    }
    kept
}

impl<'a> Sediment<'a> {
    /// Prepare `batches` (oldest first), charging every vector to `budget` before allocating it.
    pub(crate) fn prepare<R: AsRef<Records>>(
        batches: &'a [R],
        budget: &QueryBudget,
    ) -> Result<Self, GraphError> {
        let mut charge = Charge::new(budget)?;

        let mut nodes: Vec<SedNode> = charge.vec(total(batches, |r| r.nodes.len()))?;
        for (b, r) in batches.iter().enumerate() {
            for (i, op) in r.as_ref().nodes.iter().enumerate() {
                nodes.push(SedNode {
                    id: op.node.0,
                    order: (b, i),
                    kind: op.kind,
                    barrier: None,
                });
            }
        }
        nodes.sort_unstable_by_key(|n| (n.id, n.order));
        // The newest change wins; the barrier is the last removal of the run.
        let mut kept = 0;
        let mut i = 0;
        while i < nodes.len() {
            let mut entry = nodes[i];
            let mut barrier = None;
            while i < nodes.len() && nodes[i].id == entry.id {
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
        let barriers = nodes.iter().any(|n| n.barrier.is_some());
        let barrier_of = |id: u32| -> Option<usize> {
            nodes
                .binary_search_by_key(&id, |n| n.id)
                .ok()
                .and_then(|at| nodes[at].barrier)
        };
        // A record survives when it is not older than the last removal of any of its nodes.
        let survives = |batch: usize, ids: &[u32]| {
            ids.iter()
                .all(|&id| barrier_of(id).is_none_or(|b| batch >= b))
        };

        let mut out: Vec<SedEdge> = charge.vec(total(batches, |r| r.edges.len()))?;
        for (b, r) in batches.iter().enumerate() {
            for (i, e) in r.as_ref().edges.iter().enumerate() {
                out.push(SedEdge {
                    key: (e.src.0, e.dst.0, e.rel.0),
                    order: (b, i),
                    kind: e.kind,
                    attr: e.attr,
                });
            }
        }
        out.sort_unstable_by_key(|e| (e.key, e.order));
        let kept = compact(
            &mut out,
            |e| e.key,
            |e| survives(e.order.0, &[e.key.0, e.key.1]),
        );
        out.truncate(kept);
        let mut inc: Vec<SedEdge> = charge.vec(out.len())?;
        inc.extend(out.iter().map(|e| SedEdge {
            key: (e.key.1, e.key.0, e.key.2),
            ..*e
        }));
        inc.sort_unstable_by_key(|e| e.key);

        let mut facts: Vec<SedFact<'a>> = charge.vec(total(batches, |r| r.facts.len()))?;
        for (b, r) in batches.iter().enumerate() {
            for (i, f) in r.as_ref().facts.iter().enumerate() {
                facts.push(SedFact {
                    key: (f.node.0, f.key),
                    order: (b, i),
                    kind: f.kind,
                    payload: f.payload.as_slice(),
                });
            }
        }
        facts.sort_unstable_by_key(|f| (f.key, f.order));
        let kept = compact(&mut facts, |f| f.key, |f| survives(f.order.0, &[f.key.0]));
        facts.truncate(kept);

        Ok(Self {
            nodes,
            out,
            inc,
            facts,
            barriers,
            _lease: charge.into_lease(),
        })
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
        self.barriers
            && self
                .nodes
                .binary_search_by_key(&id, |n| n.id)
                .is_ok_and(|at| self.nodes[at].barrier.is_some())
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
    pub(crate) fn fact(&self, n: u32, key: u8) -> Option<(OpKind, &'a [u8])> {
        self.facts
            .binary_search_by_key(&(n, key), |f| f.key)
            .ok()
            .map(|at| (self.facts[at].kind, self.facts[at].payload))
    }

    /// Every surviving fact change in (node, key) order.
    pub(crate) fn facts(&self) -> &[SedFact<'a>] {
        &self.facts
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
