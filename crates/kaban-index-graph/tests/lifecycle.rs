//! Node removal is a deletion barrier wherever the boundaries fall (T9 midpoint MED-02): the
//! same history of batches gives the same view whether the batches sit in sediment, in sealed
//! layers or were folded into a base between a removal and a re-add. Also a multi-layer fold
//! followed by seal, and the depth-mismatch fallback of the union optimisation.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod oracle;

use kaban_index_graph::{
    Attr, Base, Direction, EdgeOp, FactOp, Layer, LocalId, NodeOp, OpKind, Records, RelId,
    RelSelection, View, build_base, fold, seal,
};
use oracle::{Oracle, Rng, nodes, random_records, rows, view};
use tantivy::directory::OwnedBytes;

fn node(id: u32, kind: OpKind) -> NodeOp {
    NodeOp {
        node: LocalId(id),
        kind,
    }
}

fn edge(src: u32, rel: u16, dst: u32, weight: i64) -> EdgeOp {
    EdgeOp {
        src: LocalId(src),
        rel: RelId(rel),
        dst: LocalId(dst),
        kind: OpKind::Add,
        attr: Attr { weight, ts: 1 },
    }
}

fn fact(id: u32, key: u8, payload: &[u8]) -> FactOp {
    FactOp {
        node: LocalId(id),
        key,
        kind: OpKind::Add,
        payload: payload.to_vec(),
    }
}

fn open_base(bytes: Vec<u8>) -> Base {
    Base::open(OwnedBytes::new(bytes)).unwrap()
}

fn open_layer(bytes: Vec<u8>) -> Layer {
    Layer::open(OwnedBytes::new(bytes)).unwrap()
}

/// Every probe of `view` over ids `0..ids` equals `oracle`.
fn assert_matches(view: &View<'_>, oracle: &Oracle, ids: u32, ctx: &str) {
    assert_eq!(nodes(view), oracle.nodes(), "{ctx}: nodes");
    let sels = [
        RelSelection::All,
        RelSelection::only(vec![RelId(0), RelId(7)]),
    ];
    for id in 0..ids {
        let n = LocalId(id);
        assert_eq!(view.contains(n), oracle.contains(n), "{ctx}: contains {id}");
        for key in 0..4 {
            assert_eq!(
                view.fact(n, key),
                oracle.fact(n, key),
                "{ctx}: fact {id}/{key}"
            );
        }
        for rels in &sels {
            for dir in [Direction::Out, Direction::In, Direction::Both] {
                assert_eq!(
                    rows(view, n, rels, dir),
                    oracle.neighbours(n, rels, dir),
                    "{ctx}: neighbours {id} {rels:?} {dir:?}"
                );
            }
        }
    }
}

/// A history over nodes 1, 2, 3: node 1 is removed (node-only) then re-added with a changed
/// fact, a re-emitted unchanged edge and without its optional fact; node 2 is removed and
/// re-added in one batch together with a new edge; then an edge into 1 is added again.
fn history() -> Vec<Records> {
    vec![
        Records {
            nodes: [1, 2, 3].map(|n| node(n, OpKind::Add)).to_vec(),
            edges: vec![
                edge(1, 0, 2, 10),
                edge(2, 1, 1, 20),
                edge(3, 0, 1, 30),
                edge(1, 7, 3, 40),
            ],
            facts: vec![fact(1, 0, b"x"), fact(1, 1, b"optional"), fact(2, 0, b"p")],
        },
        Records {
            nodes: vec![node(1, OpKind::Remove)],
            ..Records::default()
        },
        Records {
            nodes: vec![node(1, OpKind::Add)],
            edges: vec![edge(1, 7, 3, 40)],
            facts: vec![fact(1, 0, b"z")],
        },
        Records {
            nodes: vec![node(2, OpKind::Remove), node(2, OpKind::Add)],
            edges: vec![edge(2, 0, 3, 50)],
            ..Records::default()
        },
        Records {
            edges: vec![edge(3, 7, 1, 60)],
            ..Records::default()
        },
    ]
}

/// Seal `batches` one by one over `base` and `layers`.
fn sealed(base: Option<&Base>, mut layers: Vec<Layer>, batches: &[Records]) -> Vec<Layer> {
    for r in batches {
        let bytes = seal(&view::<Records>(base, &layers, &[]), r);
        layers.push(open_layer(bytes));
    }
    layers
}

/// Fold base + layers + sediment into a fresh base.
fn folded(base: Option<&Base>, layers: &[Layer], sediment: &[Records]) -> Base {
    open_base(fold(&view(base, layers, sediment), &[]))
}

/// The view after `history()` split at every kind of boundary.
fn arrangements(h: &[Records], check: &dyn Fn(&View<'_>, &str)) {
    // Everything in sediment.
    check(&view(None, &[], h), "all sediment");
    let b0 = open_base(build_base(&h[0], 0));
    check(&view(Some(&b0), &[], &h[1..]), "base + sediment");
    let layers = sealed(Some(&b0), Vec::new(), &h[1..]);
    check(&view::<Records>(Some(&b0), &layers, &[]), "base + layers");
    let two = sealed(Some(&b0), Vec::new(), &h[1..3]);
    check(
        &view(Some(&b0), &two, &h[3..]),
        "base + 2 layers + sediment",
    );
    // A fold between the removal and the re-add, over a layer or over sediment.
    let l1 = sealed(Some(&b0), Vec::new(), &h[1..2]);
    let f = folded(Some(&b0), &l1, &[]);
    check(
        &view(Some(&f), &[], &h[2..]),
        "fold(layer removal) + sediment",
    );
    let f = folded(Some(&b0), &[], &h[1..2]);
    let rest = sealed(Some(&f), Vec::new(), &h[2..]);
    check(
        &view::<Records>(Some(&f), &rest, &[]),
        "fold(sediment removal) + layers",
    );
    // A fold of several layers, then seal on the folded base.
    let f = folded(Some(&b0), &two, &[]);
    let rest = sealed(Some(&f), Vec::new(), &h[3..4]);
    check(
        &view(Some(&f), &rest, &h[4..]),
        "fold(2 layers) + seal + sediment",
    );
}

#[test]
fn removal_then_readd_never_shows_older_edges_or_facts() {
    let h = history();
    let mut oracle = Oracle::default();
    for r in &h {
        oracle.apply(r);
    }
    // The expected state, stated plainly.
    assert_eq!(oracle.nodes(), vec![LocalId(1), LocalId(2), LocalId(3)]);
    assert_eq!(oracle.fact(LocalId(1), 0), Some(&b"z"[..]));
    assert_eq!(oracle.fact(LocalId(1), 1), None);
    assert_eq!(oracle.fact(LocalId(2), 0), None);
    let out1: Vec<_> = oracle
        .neighbours(LocalId(1), &RelSelection::All, Direction::Both)
        .into_iter()
        .map(|(m, r, _)| (m.0, r.0))
        .collect();
    assert_eq!(
        out1,
        vec![(3, 7), (3, 7)],
        "1>3 re-emitted and 3>1 added later only"
    );
    arrangements(&h, &|v, ctx| assert_matches(v, &oracle, 5, ctx));
}

#[test]
fn folding_at_any_boundary_keeps_the_view() {
    for seed in 0..8 {
        let mut rng = Rng::new(seed);
        let ids = 40;
        let batches: Vec<Records> = (0..6)
            .map(|i| random_records(&mut rng, ids, if i == 0 { 200 } else { 40 }, 35))
            .collect();
        let mut oracle = Oracle::default();
        for r in &batches {
            oracle.apply(r);
        }
        let b0 = open_base(build_base(&batches[0], 0));
        let ctx = format!("seed {seed} sediment");
        assert_matches(&view(Some(&b0), &[], &batches[1..]), &oracle, ids, &ctx);
        for k in 1..batches.len() {
            // Seal up to k, fold, then the rest as layers and sediment.
            let layers = sealed(Some(&b0), Vec::new(), &batches[1..k]);
            let f = folded(Some(&b0), &layers, &[]);
            let mid = usize::midpoint(k, batches.len());
            let rest = sealed(Some(&f), Vec::new(), &batches[k..mid]);
            let v = view(Some(&f), &rest, &batches[mid..]);
            assert_matches(&v, &oracle, ids, &format!("seed {seed} fold at {k}"));
        }
    }
}

#[test]
fn a_depth_mismatch_disables_the_union_shortcut() {
    // Chain A: base, then a layer touching node 1. Chain B: base, then two layers that never
    // touch node 1. A view over [A1, B1, B2] has a newest-layer depth (2) unequal to its
    // layer count (3), so node 1's list must still be read from A1.
    let base_recs = Records {
        nodes: (0..4).map(|n| node(n, OpKind::Add)).collect(),
        edges: vec![edge(0, 0, 3, 1)],
        ..Records::default()
    };
    let base = open_base(build_base(&base_recs, 0));
    let a = vec![Records {
        edges: vec![edge(1, 0, 2, 5)],
        ..Records::default()
    }];
    let b = vec![
        Records {
            edges: vec![edge(2, 0, 3, 6)],
            ..Records::default()
        },
        Records {
            edges: vec![edge(3, 0, 0, 7)],
            ..Records::default()
        },
    ];
    let la = sealed(Some(&base), Vec::new(), &a);
    let lb = sealed(Some(&base), Vec::new(), &b);
    let mixed: Vec<Layer> = la.into_iter().chain(lb).collect();
    let mut oracle = Oracle::default();
    for r in std::iter::once(&base_recs).chain(&a).chain(&b) {
        oracle.apply(r);
    }
    let v = view::<Records>(Some(&base), &mixed, &[]);
    assert_eq!(
        rows(&v, LocalId(1), &RelSelection::All, Direction::Out).len(),
        1,
        "node 1's edge from the mismatched layer is read"
    );
    assert_matches(&v, &oracle, 4, "depth mismatch");
}
