//! Steps 2 and 3 (T9): the structural functions equal a plain breadth-first oracle over the edge
//! list of base + layers + sediment (GRA-01 structural class), as sets and in their defined
//! order; multi-hop results report every node once and every edge met once (GRA-08); caps are
//! checked before any neighbour access and a hub cannot widen a capped request (GRA-04).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod oracle;

use std::collections::BTreeSet;

use kaban_index_graph::{
    Attr, Base, Caps, Direction, EdgeOp, GraphError, Layer, LocalId, NodeOp, OpKind, Reached,
    Records, RelId, RelSelection, RowCap, View, build_base, closure, common_neighbours, degree,
    expand, meet, reachable, refuse_not_planned, reverse_closure, seal, shortest_path,
    topo_order, wcc,
};
use oracle::structural::{Expected, Graph};
use oracle::{Fixture, IDS, Rng, budget, chain, view};
use tantivy::directory::OwnedBytes;

const SEEDS: u64 = 6;

fn node(id: u32) -> NodeOp {
    NodeOp {
        node: LocalId(id),
        kind: OpKind::Add,
    }
}

fn edge(src: u32, rel: u16, dst: u32) -> EdgeOp {
    EdgeOp {
        src: LocalId(src),
        rel: RelId(rel),
        dst: LocalId(dst),
        kind: OpKind::Add,
        attr: Attr::default(),
    }
}

fn caps(depth: u8, rows: u32) -> Caps {
    Caps { depth, rows }
}

fn selections() -> Vec<RelSelection> {
    vec![
        RelSelection::All,
        RelSelection::only(vec![RelId(0), RelId(40_000)]),
        RelSelection::only(vec![RelId(7)]),
    ]
}

fn cap_grid() -> Vec<Caps> {
    vec![
        caps(1, 1_000),
        caps(2, 1_000),
        caps(4, 1_000),
        caps(255, 1_000),
        caps(3, 1),
        caps(3, 7),
        caps(2, 40),
    ]
}

/// Source sets: single nodes, absent ids, duplicates and several sources.
fn source_sets(rng: &mut Rng) -> Vec<Vec<LocalId>> {
    let mut id = || LocalId(u32::try_from(rng.below(u64::from(IDS) + 5)).unwrap());
    vec![
        vec![id()],
        vec![id()],
        vec![id(), id(), id()],
        {
            let a = id();
            vec![a, id(), a]
        },
        Vec::new(),
    ]
}

/// The GRA-08 fixture: main(1)>parse(2), parse>log(3), log>parse, split over a base, a layer and
/// the sediment.
struct Gra08 {
    base: Base,
    layers: Vec<Layer>,
    sediment: Vec<Records>,
}

fn gra08() -> Gra08 {
    let base = Base::open(OwnedBytes::new(build_base(
        &Records {
            nodes: vec![node(1), node(2), node(3)],
            edges: vec![edge(1, 0, 2)],
            ..Records::default()
        },
        0,
    )))
    .unwrap();
    let layer = seal(
        &view::<Records>(Some(&base), &[], &[]),
        &Records {
            edges: vec![edge(2, 0, 3)],
            ..Records::default()
        },
    );
    Gra08 {
        base,
        layers: vec![Layer::open(OwnedBytes::new(layer)).unwrap()],
        sediment: vec![Records {
            edges: vec![edge(3, 0, 2)],
            ..Records::default()
        }],
    }
}

fn ids(v: &[(u32, u16, u32)]) -> Vec<(LocalId, RelId, LocalId)> {
    v.iter()
        .map(|&(s, r, d)| (LocalId(s), RelId(r), LocalId(d)))
        .collect()
}

fn edge_set(r: &Reached) -> BTreeSet<(LocalId, RelId, LocalId)> {
    r.edges.iter().copied().collect()
}

#[test]
fn gra08_every_node_once_at_its_first_depth_and_every_edge_once() {
    let g = gra08();
    let v = view(Some(&g.base), &g.layers, &g.sediment);
    let b = budget();
    let got = expand(
        &v,
        &[LocalId(1)],
        &RelSelection::All,
        Direction::Out,
        caps(3, 100),
        &b,
    )
    .unwrap();
    assert_eq!(got.nodes, vec![(LocalId(2), 1), (LocalId(3), 2)]);
    let mut edges = got.edges.clone();
    edges.sort_unstable();
    assert_eq!(edges, ids(&[(1, 0, 2), (2, 0, 3), (3, 0, 2)]), "each edge once");
    assert!(!got.truncated);
    let via_closure = closure(&v, &[LocalId(1)], &RelSelection::All, caps(3, 100), &b).unwrap();
    assert_eq!(*via_closure, *got);

    // Backwards from parse: main and log at depth 1, and the same three edges once each.
    let back = reverse_closure(&v, &[LocalId(2)], &RelSelection::All, caps(3, 100), &b).unwrap();
    assert_eq!(back.nodes, vec![(LocalId(1), 1), (LocalId(3), 1)]);
    let mut edges = back.edges.clone();
    edges.sort_unstable();
    assert_eq!(edges, ids(&[(1, 0, 2), (2, 0, 3), (3, 0, 2)]));

    // Ignoring direction the reciprocal pair parse<>log stays two edges, each reported once.
    let both = expand(
        &v,
        &[LocalId(2)],
        &RelSelection::All,
        Direction::Both,
        caps(3, 100),
        &b,
    )
    .unwrap();
    assert_eq!(both.nodes, vec![(LocalId(1), 1), (LocalId(3), 1)]);
    assert_eq!(both.edges.len(), 3, "{:?}", both.edges);
    assert_eq!(
        edge_set(&both),
        ids(&[(1, 0, 2), (2, 0, 3), (3, 0, 2)]).into_iter().collect()
    );
}

/// `got` equals `want`: nodes exactly (order included), edges exactly as a set without a cut,
/// and as a subset with every kept node reached by a reported edge with a cut; never a
/// duplicate edge.
fn assert_reached(got: &Reached, want: &Expected, dir: Direction, ctx: &str) {
    assert_eq!(got.nodes, want.nodes, "{ctx}: nodes");
    assert_eq!(got.truncated, want.truncated, "{ctx}: truncated");
    let set = edge_set(got);
    assert_eq!(set.len(), got.edges.len(), "{ctx}: an edge reported twice");
    if want.truncated {
        assert!(set.is_subset(&want.edges), "{ctx}: edges outside the result");
        for &(n, d) in &got.nodes {
            let parent = |m: LocalId| want.depth.get(&m.0) == Some(&(d - 1));
            let linked = set.iter().any(|&(s, _, t)| match dir {
                Direction::Out => t == n && parent(s),
                Direction::In => s == n && parent(t),
                Direction::Both => (t == n && parent(s)) || (s == n && parent(t)),
            });
            assert!(linked, "{ctx}: {n:?}@{d} has no edge from depth {}", d - 1);
        }
    } else {
        assert_eq!(set, want.edges, "{ctx}: edges");
    }
}

fn graphs(f: &Fixture) -> Vec<(RelSelection, Graph)> {
    selections()
        .into_iter()
        .map(|r| {
            let g = f.oracle.graph(&r);
            (r, g)
        })
        .collect()
}

#[test]
fn expand_closure_and_reverse_closure_equal_the_oracle() {
    let b = budget();
    for seed in 0..SEEDS {
        let f = chain(seed, |r| r);
        let v = f.view();
        let mut rng = Rng::new(seed + 1000);
        let sources = source_sets(&mut rng);
        for (rels, g) in &graphs(&f) {
            for c in cap_grid() {
                for src in &sources {
                    for dir in [Direction::Out, Direction::In, Direction::Both] {
                        let ctx = format!("seed {seed} {rels:?} {c:?} {src:?} {dir:?}");
                        let got = expand(&v, src, rels, dir, c, &b).unwrap();
                        let want = g.expand(src, dir, c.depth, c.rows);
                        assert_reached(&got, &want, dir, &ctx);
                    }
                    let out = expand(&v, src, rels, Direction::Out, c, &b).unwrap();
                    assert_eq!(*closure(&v, src, rels, c, &b).unwrap(), *out);
                    let inc = expand(&v, src, rels, Direction::In, c, &b).unwrap();
                    assert_eq!(*reverse_closure(&v, src, rels, c, &b).unwrap(), *inc);
                }
            }
        }
    }
}

/// Pairs of ids, absent ones and equal ones included.
fn pairs(rng: &mut Rng, n: usize) -> Vec<(u32, u32)> {
    let mut id = || u32::try_from(rng.below(u64::from(IDS) + 3)).unwrap();
    let mut v: Vec<(u32, u32)> = (0..n).map(|_| (id(), id())).collect();
    let same = id();
    v.push((same, same));
    v
}

#[test]
fn reachable_and_shortest_path_equal_the_oracle() {
    let b = budget();
    for seed in 0..SEEDS {
        let f = chain(seed, |r| r);
        let v = f.view();
        let mut rng = Rng::new(seed + 2000);
        for (rels, g) in &graphs(&f) {
            for c in cap_grid() {
                for (s, d) in pairs(&mut rng, 25) {
                    let ctx = format!("seed {seed} {rels:?} {c:?} {s}->{d}");
                    let (src, dst) = (LocalId(s), LocalId(d));
                    let got = reachable(&v, src, dst, rels, c, &b).unwrap();
                    assert_eq!(got, g.reachable(s, d, c.depth, c.rows), "{ctx}: reachable");
                    let path = shortest_path(&v, src, dst, rels, c, &b).unwrap();
                    let want = g.shortest_path(s, d, c.depth, c.rows);
                    assert_eq!(path.as_ref().map(|p| p.value().clone()), want, "{ctx}: path");
                }
            }
        }
    }
}

#[test]
fn shortest_path_breaks_ties_by_the_lowest_id() {
    // 1>5>9 and 1>3>9 and 1>7>9: three paths of two hops; 3 is the lowest middle node. A longer
    // path through 2 does not win.
    let r = Records {
        nodes: [1, 2, 3, 5, 7, 8, 9].map(node).to_vec(),
        edges: vec![
            edge(1, 0, 5),
            edge(1, 0, 7),
            edge(1, 0, 3),
            edge(5, 0, 9),
            edge(3, 0, 9),
            edge(7, 0, 9),
            edge(1, 0, 2),
            edge(2, 0, 8),
            edge(8, 0, 9),
        ],
        ..Records::default()
    };
    let rs = [r];
    let v = view(None, &[], &rs);
    let b = budget();
    let path = shortest_path(
        &v,
        LocalId(1),
        LocalId(9),
        &RelSelection::All,
        caps(4, 100),
        &b,
    )
    .unwrap()
    .unwrap();
    assert_eq!(*path, vec![LocalId(1), LocalId(3), LocalId(9)]);
    assert!(path.bytes() > 0, "the returned path is charged");
}

#[test]
fn degree_and_common_neighbours_equal_the_oracle() {
    let b = budget();
    for seed in 0..SEEDS {
        let f = chain(seed, |r| r);
        let v = f.view();
        let mut rng = Rng::new(seed + 3000);
        for (rels, g) in &graphs(&f) {
            for id in 0..IDS + 3 {
                for dir in [Direction::Out, Direction::In, Direction::Both] {
                    let got = degree(&v, LocalId(id), rels, dir, &b).unwrap();
                    let want = u32::try_from(g.degree(id, dir)).unwrap();
                    assert_eq!(got, want, "seed {seed} {rels:?} degree {id} {dir:?}");
                }
            }
            for (a, c) in pairs(&mut rng, 40) {
                let got = common_neighbours(&v, LocalId(a), LocalId(c), rels, &b).unwrap();
                assert_eq!(
                    *got,
                    g.common(a, c),
                    "seed {seed} {rels:?} common {a} {c}"
                );
            }
        }
    }
}

#[test]
fn meet_equals_the_oracle() {
    let b = budget();
    for seed in 0..SEEDS {
        let f = chain(seed, |r| r);
        let v = f.view();
        let mut rng = Rng::new(seed + 4000);
        for (rels, g) in &graphs(&f) {
            for c in cap_grid() {
                for (x, y) in pairs(&mut rng, 15) {
                    let got = meet(&v, LocalId(x), LocalId(y), rels, c, &b).unwrap();
                    assert_eq!(
                        *got,
                        g.meet(x, y, c.depth, c.rows),
                        "seed {seed} {rels:?} {c:?} meet {x} {y}"
                    );
                }
            }
        }
    }
}

/// Edges forced forward (src < dst) so the graph is acyclic.
fn forward(mut r: Records) -> Records {
    for e in &mut r.edges {
        if e.src > e.dst {
            std::mem::swap(&mut e.src, &mut e.dst);
        }
        if e.src == e.dst {
            e.dst = LocalId(e.dst.0 + 1);
        }
    }
    r
}

fn check_topo(f: &Fixture, ctx: &str, b: &kaban_core::query_budget::QueryBudget) -> (u32, u32) {
    let v = f.view();
    let mut rng = Rng::new(7);
    let (mut ordered, mut cycles) = (0, 0);
    for (rels, g) in &graphs(f) {
        for c in cap_grid() {
            for src in source_sets(&mut rng) {
                let ctx = format!("{ctx} {rels:?} {c:?} {src:?}");
                let got = topo_order(&v, &src, rels, c, b);
                let Some(set) = g.topo_set(&src, c.depth, c.rows) else {
                    assert!(
                        matches!(got, Err(GraphError::CapExceeded { limit: "rows", .. })),
                        "{ctx}: a cut set is refused: {got:?}"
                    );
                    continue;
                };
                if g.has_cycle(&set) {
                    let Err(GraphError::Cycle { node }) = got else {
                        panic!("{ctx}: a cycle is refused: {got:?}");
                    };
                    assert!(g.on_cycle(&set, node.0), "{ctx}: {node:?} is on a cycle");
                    cycles += 1;
                } else {
                    let got = got.unwrap();
                    assert!(g.is_topo_order(&set, &got), "{ctx}: {:?}", *got);
                    ordered += 1;
                }
            }
        }
    }
    (ordered, cycles)
}

#[test]
fn topo_order_is_valid_on_acyclic_views_and_names_a_cycle_otherwise() {
    let b = budget();
    let mut totals = (0, 0);
    for seed in 0..SEEDS {
        let dag = chain(seed, forward);
        let (o, c) = check_topo(&dag, &format!("dag seed {seed}"), &b);
        assert_eq!(c, 0, "dag seed {seed}: forward edges make no cycle");
        totals.0 += o;
        let cyclic = chain(seed, |r| r);
        let (o, c) = check_topo(&cyclic, &format!("seed {seed}"), &b);
        totals.0 += o;
        totals.1 += c;
    }
    assert!(totals.0 > 50 && totals.1 > 50, "both outcomes exercised: {totals:?}");
}

#[test]
fn a_self_loop_is_a_cycle() {
    let rs = [Records {
        nodes: vec![node(1), node(2)],
        edges: vec![edge(1, 0, 2), edge(2, 0, 2)],
        ..Records::default()
    }];
    let v = view(None, &[], &rs);
    let got = topo_order(&v, &[LocalId(1)], &RelSelection::All, caps(3, 10), &budget());
    assert_eq!(got.map(|o| o.value().clone()), Err(GraphError::Cycle { node: LocalId(2) }));
}

#[test]
fn wcc_equals_the_oracle_with_the_lowest_member_as_component() {
    let b = budget();
    for seed in 0..SEEDS {
        let f = chain(seed, |r| r);
        let v = f.view();
        for (rels, g) in &graphs(&f) {
            for cap in [1, 10, 10_000] {
                let got = wcc(&v, rels, RowCap(cap), &b).unwrap();
                assert_eq!(*got, g.wcc(cap), "seed {seed} {rels:?} cap {cap}");
            }
        }
    }
}

#[test]
fn unplanned_functions_are_refused_by_name() {
    for name in [
        "betweenness",
        "louvain",
        "all_paths",
        "k_core",
        "articulation_points",
        "label_propagation",
    ] {
        assert_eq!(
            refuse_not_planned(name),
            GraphError::NotPlanned { function: name },
            "{name}"
        );
    }
    // Any other name is not one of the refused functions: it is reported as unknown.
    for name in ["", "Louvain", "pagerank", "betweenness "] {
        assert_eq!(
            refuse_not_planned(name),
            GraphError::NotPlanned { function: "unknown" },
            "{name:?}"
        );
    }
}

// ---- Step 3: caps --------------------------------------------------------------------------

/// A present hub (node 0) with `n` outgoing edges to present nodes 1..=n.
fn hub_view_records(n: u32) -> Vec<Records> {
    vec![Records {
        nodes: (0..=n).map(node).collect(),
        edges: (1..=n).map(|m| edge(0, 0, m)).collect(),
        ..Records::default()
    }]
}

fn is_missing(r: &Result<impl std::fmt::Debug, GraphError>, limit: &str) -> bool {
    matches!(r, Err(GraphError::CapMissing { limit: l }) if *l == limit)
}

#[test]
fn a_missing_cap_is_refused_before_any_neighbour_access() {
    let rs = hub_view_records(50);
    let v = view(None, &[], &rs);
    let b = budget();
    let all = RelSelection::All;
    let src = [LocalId(0)];
    let (a, z) = (LocalId(0), LocalId(7));
    for (bad, limit) in [(caps(0, 10), "depth"), (caps(3, 0), "rows"), (caps(0, 0), "depth")] {
        let before = v.raw_reads();
        assert!(is_missing(&expand(&v, &src, &all, Direction::Both, bad, &b), limit));
        assert!(is_missing(&closure(&v, &src, &all, bad, &b), limit));
        assert!(is_missing(&reverse_closure(&v, &src, &all, bad, &b), limit));
        assert!(is_missing(&reachable(&v, a, z, &all, bad, &b), limit));
        assert!(is_missing(&shortest_path(&v, a, z, &all, bad, &b), limit));
        assert!(is_missing(&meet(&v, a, z, &all, bad, &b), limit));
        assert!(is_missing(&topo_order(&v, &src, &all, bad, &b), limit));
        assert_eq!(v.raw_reads(), before, "{bad:?}: no raw read");
    }
    let before = v.raw_reads();
    assert!(is_missing(&wcc(&v, &all, RowCap(0), &b), "rows"));
    assert_eq!(v.raw_reads(), before, "RowCap(0): no raw read");
    // Sanity: the same calls with caps do read.
    expand(&v, &src, &all, Direction::Out, caps(1, 10), &b).unwrap();
    assert!(v.raw_reads() > before);
}

#[test]
fn a_hub_cannot_widen_a_capped_expand() {
    let rs = hub_view_records(10_000);
    let v = view(None, &[], &rs);
    let b = budget();
    let got = expand(
        &v,
        &[LocalId(0)],
        &RelSelection::All,
        Direction::Out,
        caps(2, 200),
        &b,
    )
    .unwrap();
    assert_eq!(got.nodes.len(), 200);
    assert!(got.truncated);
    let want: Vec<(LocalId, u8)> = (1..=200).map(|m| (LocalId(m), 1)).collect();
    assert_eq!(got.nodes, want, "the lowest ids at the first depth");
    assert_eq!(got.edges.len(), 200, "one edge per kept node");
    assert!(
        v.raw_reads() < 1_000,
        "the hub list is not read past the cut: {}",
        v.raw_reads()
    );
    // Exactly the cap without more nodes is not a cut.
    let rs = hub_view_records(200);
    let v = view(None, &[], &rs);
    let got = expand(&v, &[LocalId(0)], &RelSelection::All, Direction::Out, caps(2, 200), &b)
        .unwrap();
    assert_eq!(got.nodes.len(), 200);
    assert!(!got.truncated);
}

#[test]
fn structural_functions_hold_no_budget_after_returning_unless_the_result_lives() {
    let f = chain(2, |r| r);
    let v: View<'_> = f.view();
    let b = kaban_core::query_budget::QueryBudget::new(u64::MAX, None);
    let r = expand(&v, &[LocalId(3)], &RelSelection::All, Direction::Both, caps(3, 50), &b)
        .unwrap();
    assert!(!r.nodes.is_empty());
    assert_eq!(b.live_bytes(), r.bytes(), "only the result stays charged");
    assert!(r.bytes() > 0);
    drop(r);
    assert_eq!(b.live_bytes(), 0);
    let w = wcc(&v, &RelSelection::All, RowCap(1000), &b).unwrap();
    assert_eq!(b.live_bytes(), w.bytes());
    let (rows, hold) = w.into_parts();
    assert!(!rows.is_empty());
    drop(rows);
    assert!(b.live_bytes() > 0, "the hold keeps the charge");
    drop(hold);
    assert_eq!(b.live_bytes(), 0);
}
