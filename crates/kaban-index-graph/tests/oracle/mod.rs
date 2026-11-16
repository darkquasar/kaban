//! A plain edge-list oracle: records applied in order to sets and maps, read back naively.

#![allow(dead_code)] // each test binary uses a different part of the oracle
#![allow(clippy::unwrap_used)] // test code: failures are the assertion

use std::collections::{BTreeMap, BTreeSet};

use kaban_core::query_budget::QueryBudget;
use kaban_index_graph::{
    Attr, Base, Direction, EdgeOp, FactOp, Layer, LocalId, NodeOp, OpKind, Records, RelId,
    RelSelection, View,
};

/// The merged state of base, layer and sediment records applied in order.
#[derive(Debug, Clone, Default)]
pub(crate) struct Oracle {
    nodes: BTreeSet<u32>,
    edges: BTreeMap<(u32, u16, u32), Attr>,
    facts: BTreeMap<(u32, u8), Vec<u8>>,
}

impl Oracle {
    /// Apply `r`: first the node stream, where a removal deletes every edge touching the node
    /// and every fact of the node so far (the deletion barrier), then edges and facts in order;
    /// the newest record wins.
    pub(crate) fn apply(&mut self, r: &Records) {
        for n in &r.nodes {
            match n.kind {
                OpKind::Add => {
                    self.nodes.insert(n.node.0);
                }
                OpKind::Remove => {
                    self.nodes.remove(&n.node.0);
                    self.edges
                        .retain(|&(s, _, d), _| s != n.node.0 && d != n.node.0);
                    self.facts.retain(|&(m, _), _| m != n.node.0);
                }
            }
        }
        for e in &r.edges {
            let key = (e.src.0, e.rel.0, e.dst.0);
            match e.kind {
                OpKind::Add => {
                    self.edges.insert(key, e.attr);
                }
                OpKind::Remove => {
                    self.edges.remove(&key);
                }
            }
        }
        for f in &r.facts {
            let key = (f.node.0, f.key);
            match f.kind {
                OpKind::Add => {
                    self.facts.insert(key, f.payload.clone());
                }
                OpKind::Remove => {
                    self.facts.remove(&key);
                }
            }
        }
    }

    /// Drop `n`, its edges and its facts (what a fold with `omit: [n]` keeps).
    pub(crate) fn purge(&mut self, n: LocalId) {
        self.nodes.remove(&n.0);
        self.edges.retain(|&(s, _, d), _| s != n.0 && d != n.0);
        self.facts.retain(|&(m, _), _| m != n.0);
    }

    pub(crate) fn contains(&self, n: LocalId) -> bool {
        self.nodes.contains(&n.0)
    }

    pub(crate) fn nodes(&self) -> Vec<LocalId> {
        self.nodes.iter().map(|&n| LocalId(n)).collect()
    }

    pub(crate) fn fact(&self, n: LocalId, key: u8) -> Option<&[u8]> {
        if !self.contains(n) {
            return None;
        }
        self.facts.get(&(n.0, key)).map(Vec::as_slice)
    }

    /// Neighbours of a present `n` whose other endpoint is present, ordered by (neighbour,
    /// relation, outgoing first).
    pub(crate) fn neighbours(
        &self,
        n: LocalId,
        rels: &RelSelection,
        dir: Direction,
    ) -> Vec<(LocalId, RelId, Attr)> {
        if !self.contains(n) {
            return Vec::new();
        }
        let out = matches!(dir, Direction::Out | Direction::Both);
        let inc = matches!(dir, Direction::In | Direction::Both);
        let mut rows: Vec<(u32, u16, u8, Attr)> = Vec::new();
        for (&(s, r, d), &attr) in &self.edges {
            if !selected(rels, r) {
                continue;
            }
            if out && s == n.0 && self.nodes.contains(&d) {
                rows.push((d, r, 0, attr));
            }
            if inc && d == n.0 && self.nodes.contains(&s) {
                rows.push((s, r, 1, attr));
            }
        }
        rows.sort_by_key(|&(m, r, side, _)| (m, r, side));
        rows.into_iter()
            .map(|(m, r, _, attr)| (LocalId(m), RelId(r), attr))
            .collect()
    }
}

/// Whether `rels` selects `rel`, by a plain linear scan (independent of the crate's search).
pub(crate) fn selected(rels: &RelSelection, rel: u16) -> bool {
    match rels {
        RelSelection::All => true,
        RelSelection::Only(ids) => ids.iter().any(|id| id.0 == rel),
    }
}

/// A budget that never refuses.
pub(crate) fn budget() -> QueryBudget {
    QueryBudget::new(u64::MAX, None)
}

/// A view that must build under an unbounded budget.
pub(crate) fn view<'a, R: AsRef<Records>>(
    base: Option<&'a Base>,
    layers: &'a [Layer],
    sediment: &'a [R],
) -> View<'a> {
    View::new(base, layers, sediment, &budget()).unwrap()
}

/// Every row of one accessor call, under an unbounded budget.
pub(crate) fn rows(
    view: &View<'_>,
    n: LocalId,
    rels: &RelSelection,
    dir: Direction,
) -> Vec<(LocalId, RelId, Attr)> {
    let b = budget();
    view.neighbours(n, rels, dir, &b)
        .collect::<Result<_, _>>()
        .unwrap()
}

/// Every present node, under an unbounded budget.
pub(crate) fn nodes(view: &View<'_>) -> Vec<LocalId> {
    let b = budget();
    view.nodes(&b).collect::<Result<_, _>>().unwrap()
}

/// splitmix64: a seeded, dependency-free generator for test fixtures.
#[derive(Debug, Clone)]
pub(crate) struct Rng(u64);

impl Rng {
    pub(crate) fn new(seed: u64) -> Self {
        Self(seed)
    }

    pub(crate) fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `0..n` (n > 0).
    pub(crate) fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }

    pub(crate) fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }
}

/// Relation ids used by fixtures, including high values so membership is not a bit shift.
pub(crate) const RELS: [u16; 4] = [0, 1, 7, 40_000];

/// Random records over ids `0..ids`: `ops` edge ops, about a third as many node and fact ops.
/// `remove_percent` of ops are removes.
pub(crate) fn random_records(rng: &mut Rng, ids: u32, ops: usize, remove_percent: u64) -> Records {
    let id = |rng: &mut Rng| LocalId(u32::try_from(rng.below(u64::from(ids))).unwrap_or(0));
    let kind = |rng: &mut Rng| {
        if rng.chance(remove_percent) {
            OpKind::Remove
        } else {
            OpKind::Add
        }
    };
    let mut r = Records::default();
    for _ in 0..ops {
        let rel = RELS[usize::try_from(rng.below(4)).unwrap_or(0)];
        let attr = Attr {
            weight: i64::try_from(rng.below(5_000_000)).unwrap_or(0) - 1_000_000,
            ts: i64::try_from(rng.below(1 << 40)).unwrap_or(0),
        };
        r.edges.push(EdgeOp {
            src: id(rng),
            rel: RelId(rel),
            dst: id(rng),
            kind: kind(rng),
            attr,
        });
    }
    for _ in 0..ops.div_ceil(3) {
        r.nodes.push(NodeOp {
            node: id(rng),
            kind: kind(rng),
        });
    }
    for _ in 0..ops.div_ceil(3) {
        let kind = kind(rng);
        let len = if kind == OpKind::Add { rng.below(9) } else { 0 };
        let payload = (0..len)
            .map(|_| u8::try_from(rng.below(256)).unwrap_or(0))
            .collect();
        r.facts.push(FactOp {
            node: id(rng),
            key: u8::try_from(rng.below(4)).unwrap_or(0),
            kind,
            payload,
        });
    }
    r
}
