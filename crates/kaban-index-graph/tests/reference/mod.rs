//! An independent reference for the scored and set functions (T10): dense f64 code over a plain
//! edge list, written from the documented semantics, not from the crate's code. Also the seeded
//! fixture both scored test binaries use.

#![allow(dead_code)] // each test binary uses a different part of the reference
#![allow(clippy::unwrap_used)] // test code: failures are the assertion
#![allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)] // small test sizes

use std::collections::{BTreeMap, BTreeSet};

use kaban_index_graph::{
    Attr, Base, EdgeOp, GraphError, Layer, LocalId, NodeOp, OpKind, PreparedSediment, Records,
    RelId, RelSelection, StableOrder, TieRank, View, build_base, seal,
};
use tantivy::directory::OwnedBytes;

use crate::oracle::{Oracle, Rng, budget};

/// One directed edge: (source, relation, destination, attributes).
pub(crate) type Edge = (u32, u16, u32, Attr);

/// The merged graph: present nodes ascending and every edge between present nodes.
pub(crate) struct Graph {
    pub(crate) nodes: Vec<u32>,
    pub(crate) edges: Vec<Edge>,
}

impl Graph {
    /// The `rels` part of the oracle's merged state.
    pub(crate) fn of(oracle: &Oracle, rels: &RelSelection) -> Self {
        let nodes: Vec<u32> = oracle.nodes().iter().map(|n| n.0).collect();
        let mut edges = Vec::new();
        for &n in &nodes {
            for (m, r, attr) in oracle.neighbours(LocalId(n), rels, kaban_index_graph::Direction::Out)
            {
                edges.push((n, r.0, m.0, attr));
            }
        }
        Self { nodes, edges }
    }

    fn pos(&self, n: u32) -> usize {
        self.nodes.iter().position(|&m| m == n).unwrap()
    }

    /// Power iteration over the dense transition matrix. `seeds` empty: `PageRank` (uniform
    /// teleport); otherwise personalised to the present seeds. Each edge is one link; dangling
    /// mass goes to every node uniformly; start from the teleport vector; stop after
    /// `iterations` or once the L1 change is below `tolerance`.
    pub(crate) fn rank(&self, seeds: &[u32], damping: f64, iterations: u32, tol: f64) -> Vec<f64> {
        let n = self.nodes.len();
        if n == 0 {
            return Vec::new();
        }
        let mut links = vec![vec![0.0f64; n]; n]; // links[i][j]: edges i -> j
        let mut outdeg = vec![0.0f64; n];
        for &(s, _, d, _) in &self.edges {
            links[self.pos(s)][self.pos(d)] += 1.0;
            outdeg[self.pos(s)] += 1.0;
        }
        let mut teleport = vec![0.0f64; n];
        if seeds.is_empty() {
            teleport.fill(1.0 / n as f64);
        } else {
            let present: BTreeSet<u32> =
                seeds.iter().copied().filter(|s| self.nodes.contains(s)).collect();
            if present.is_empty() {
                return vec![0.0; n];
            }
            for s in &present {
                teleport[self.pos(*s)] = 1.0 / present.len() as f64;
            }
        }
        let mut r = teleport.clone();
        for _ in 0..iterations {
            let dangling: f64 = (0..n).filter(|&i| outdeg[i] == 0.0).map(|i| r[i]).sum();
            let mut next = vec![0.0f64; n];
            for j in 0..n {
                let mut inflow = 0.0;
                for i in 0..n {
                    if links[i][j] > 0.0 {
                        inflow += links[i][j] * r[i] / outdeg[i];
                    }
                }
                next[j] = (1.0 - damping) * teleport[j] + damping * (inflow + dangling / n as f64);
            }
            let change: f64 = (0..n).map(|i| (next[i] - r[i]).abs()).sum();
            r = next;
            if change < tol {
                break;
            }
        }
        r
    }

    /// The first `cap` (node, score) pairs by descending score, then ascending rank.
    pub(crate) fn top(&self, scores: &[f64], order: &Order, cap: usize) -> Vec<(u32, f64)> {
        let mut rows: Vec<(u32, f64)> = self.nodes.iter().copied().zip(scores.iter().copied()).collect();
        rows.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap()
                .then(order.of(a.0).cmp(&order.of(b.0)))
        });
        rows.truncate(cap);
        rows
    }

    /// The distinct neighbours of `n` in either direction (itself only through a self-loop).
    pub(crate) fn around(&self, n: u32) -> BTreeSet<u32> {
        let mut out = BTreeSet::new();
        for &(s, _, d, _) in &self.edges {
            if s == n {
                out.insert(d);
            }
            if d == n {
                out.insert(s);
            }
        }
        out
    }

    pub(crate) fn jaccard(&self, a: u32, b: u32) -> f64 {
        let (na, nb) = (self.around(a), self.around(b));
        let union = na.union(&nb).count();
        if union == 0 {
            return 0.0;
        }
        na.intersection(&nb).count() as f64 / union as f64
    }

    /// Sum over shared neighbours z of 1 / ln |N(z)|; a z with fewer than two neighbours adds 0.
    pub(crate) fn adamic_adar(&self, a: u32, b: u32) -> f64 {
        let (na, nb) = (self.around(a), self.around(b));
        na.intersection(&nb)
            .map(|&z| {
                let k = self.around(z).len();
                if k < 2 { 0.0 } else { 1.0 / (k as f64).ln() }
            })
            .sum()
    }

    /// Links among the other neighbours of `n` over the possible pairs.
    pub(crate) fn clustering(&self, n: u32) -> f64 {
        let mut nb = self.around(n);
        nb.remove(&n);
        let k = nb.len();
        if k < 2 {
            return 0.0;
        }
        let nb: Vec<u32> = nb.into_iter().collect();
        let mut links = 0usize;
        for i in 0..k {
            let ni = self.around(nb[i]);
            for &w in &nb[i + 1..] {
                if ni.contains(&w) {
                    links += 1;
                }
            }
        }
        2.0 * links as f64 / (k * (k - 1)) as f64
    }

    /// Unordered triples of distinct, pairwise adjacent nodes.
    pub(crate) fn triangles(&self) -> u64 {
        let around: BTreeMap<u32, BTreeSet<u32>> =
            self.nodes.iter().map(|&n| (n, self.around(n))).collect();
        let mut count = 0;
        for (i, &a) in self.nodes.iter().enumerate() {
            for (j, &b) in self.nodes.iter().enumerate().skip(i + 1) {
                if !around[&a].contains(&b) {
                    continue;
                }
                for &c in &self.nodes[j + 1..] {
                    if around[&a].contains(&c) && around[&b].contains(&c) {
                        count += 1;
                    }
                }
            }
        }
        count
    }

    /// The cheapest path by (cost, hops), relaxed to a fixed point (Bellman-Ford); among the
    /// optimal predecessors of a node the lowest-ranked is its parent.
    pub(crate) fn cheapest(&self, src: u32, dst: u32, order: &Order) -> Option<(i64, Vec<u32>)> {
        if !self.nodes.contains(&src) || !self.nodes.contains(&dst) {
            return None;
        }
        let mut best: BTreeMap<u32, (i64, u32)> = BTreeMap::new();
        best.insert(src, (0, 0));
        loop {
            let mut changed = false;
            for &(s, _, d, attr) in &self.edges {
                let Some(&(c, h)) = best.get(&s) else { continue };
                let cand = (c + attr.weight, h + 1);
                if best.get(&d).is_none_or(|&old| cand < old) {
                    best.insert(d, cand);
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        let &(cost, _) = best.get(&dst)?;
        let mut path = vec![dst];
        let mut at = dst;
        while at != src {
            let (c, h) = best[&at];
            let parent = self
                .edges
                .iter()
                .filter(|&&(s, _, d, attr)| {
                    d == at && best.get(&s) == Some(&(c - attr.weight, h - 1))
                })
                .map(|&(s, ..)| s)
                .min_by_key(|&s| order.of(s))
                .unwrap();
            path.push(parent);
            at = parent;
        }
        path.reverse();
        Some((cost, path))
    }

    /// Breadth-first search from the present `src` along edges with `lo <= ts <= hi` to
    /// `depth` hops: reached non-source nodes with their first depth, and every window edge
    /// leaving a node reached below the depth bound (sources at depth 0).
    pub(crate) fn window(
        &self,
        src: &[u32],
        (lo, hi): (i64, i64),
        depth: u8,
    ) -> (BTreeMap<u32, u8>, BTreeSet<(u32, u16, u32)>) {
        let mut seen: BTreeMap<u32, u8> = src
            .iter()
            .copied()
            .filter(|s| self.nodes.contains(s))
            .map(|s| (s, 0))
            .collect();
        let mut edges = BTreeSet::new();
        for d in 0..depth {
            let level: Vec<u32> = seen.iter().filter(|&(_, &x)| x == d).map(|(&n, _)| n).collect();
            for n in level {
                for &(s, r, m, attr) in &self.edges {
                    if s == n && lo <= attr.ts && attr.ts <= hi {
                        edges.insert((s, r, m));
                        seen.entry(m).or_insert(d + 1);
                    }
                }
            }
        }
        for s in src {
            if seen.get(s) == Some(&0) {
                seen.remove(s);
            }
        }
        (seen, edges)
    }
}

/// A stable order given as one rank per local id.
pub(crate) struct Order(pub(crate) BTreeMap<u32, u32>);

impl Order {
    /// Ranks that reverse local-id order over `0..ids`.
    pub(crate) fn reversed(ids: u32) -> Self {
        Self((0..ids).map(|i| (i, ids - i)).collect())
    }

    pub(crate) fn of(&self, n: u32) -> u32 {
        self.0[&n]
    }
}

impl StableOrder for Order {
    fn rank(&self, n: LocalId) -> Result<TieRank, GraphError> {
        self.0
            .get(&n.0)
            .map(|&r| TieRank(r))
            .ok_or(GraphError::Corrupt("no rank".to_owned()))
    }
}

/// Ids the scored fixture draws from.
pub(crate) const IDS: u32 = 200;

/// A base, two sealed layers and two sediment batches, kept with the records they came from.
pub(crate) struct Fixture {
    pub(crate) base: Base,
    pub(crate) layers: Vec<Layer>,
    pub(crate) sediment: &'static PreparedSediment,
    pub(crate) batches: Vec<Records>,
    pub(crate) oracle: Oracle,
}

impl Fixture {
    /// Build the chain from `batches`: the first is the base, the next two are sealed layers,
    /// the rest are the sediment. The prepared sediment is leaked so views can borrow it.
    pub(crate) fn build(batches: Vec<Records>) -> Self {
        let base = Base::open(OwnedBytes::new(build_base(&batches[0], 256)), &budget()).unwrap();
        let mut layers: Vec<Layer> = Vec::new();
        for r in &batches[1..3] {
            let empty = PreparedSediment::prepare::<Records>(&[], &budget()).unwrap();
            let bytes = seal(&View::new(Some(&base), &layers, &empty, &budget()).unwrap(), r);
            layers.push(Layer::open(OwnedBytes::new(bytes), &budget()).unwrap());
        }
        let sediment: &'static PreparedSediment =
            Box::leak(Box::new(PreparedSediment::prepare(&batches[3..], &budget()).unwrap()));
        let mut oracle = Oracle::default();
        for r in &batches {
            oracle.apply(r);
        }
        Self {
            base,
            layers,
            sediment,
            batches,
            oracle,
        }
    }

    pub(crate) fn view(&self) -> View<'_> {
        View::new(Some(&self.base), &self.layers, self.sediment, &budget()).unwrap()
    }

    /// The same records with every id `i` renamed to `rename(i)`: another project's rids.
    pub(crate) fn renamed(&self, rename: impl Fn(u32) -> u32) -> Self {
        let id = |n: LocalId| LocalId(rename(n.0));
        let batches = self
            .batches
            .iter()
            .map(|r| Records {
                nodes: r
                    .nodes
                    .iter()
                    .map(|n| NodeOp {
                        node: id(n.node),
                        kind: n.kind,
                    })
                    .collect(),
                edges: r
                    .edges
                    .iter()
                    .map(|e| EdgeOp {
                        src: id(e.src),
                        dst: id(e.dst),
                        ..*e
                    })
                    .collect(),
                facts: Vec::new(),
            })
            .collect();
        Self::build(batches)
    }
}

const RELS: [u16; 4] = [0, 1, 7, 40_000];

fn edges(rng: &mut Rng, r: &mut Records, count: usize, remove_percent: u64) {
    for _ in 0..count {
        let id = |rng: &mut Rng| LocalId(rng.below(u64::from(IDS)) as u32);
        let weight = if rng.chance(5) {
            0
        } else {
            rng.below(1_000_000) as i64
        };
        r.edges.push(EdgeOp {
            src: id(rng),
            rel: RelId(RELS[rng.below(4) as usize]),
            dst: id(rng),
            kind: if rng.chance(remove_percent) {
                OpKind::Remove
            } else {
                OpKind::Add
            },
            attr: Attr {
                weight,
                ts: rng.below(1_000_000) as i64,
            },
        });
    }
}

fn nodes(r: &mut Records, ids: impl IntoIterator<Item = u32>, kind: OpKind) {
    r.nodes
        .extend(ids.into_iter().map(|n| NodeOp { node: LocalId(n), kind }));
}

/// The 200-node seeded fixture: nodes 0..190 in the base, 190..200 only in the sediment;
/// weights (some zero) and timestamps on every edge; edges added, removed and re-weighted in
/// the layers and the sediment, a few nodes removed (deletion barriers) and re-added.
pub(crate) fn scored(seed: u64) -> Fixture {
    let mut rng = Rng::new(seed);
    let mut base = Records::default();
    nodes(&mut base, 0..190, OpKind::Add);
    edges(&mut rng, &mut base, 700, 0);
    let mut first = Records::default();
    edges(&mut rng, &mut first, 150, 20);
    nodes(&mut first, [3, 17], OpKind::Remove);
    let mut second = Records::default();
    nodes(&mut second, [3], OpKind::Add);
    edges(&mut rng, &mut second, 60, 20);
    let mut sed1 = Records::default();
    nodes(&mut sed1, 190..200, OpKind::Add);
    edges(&mut rng, &mut sed1, 120, 0);
    let mut sed2 = Records::default();
    edges(&mut rng, &mut sed2, 40, 30);
    Fixture::build(vec![base, first, second, sed1, sed2])
}

/// The relation selections every scored test runs over.
pub(crate) fn selections() -> Vec<RelSelection> {
    vec![
        RelSelection::All,
        RelSelection::only(vec![RelId(0), RelId(40_000)]),
        RelSelection::only(vec![RelId(7)]),
    ]
}
