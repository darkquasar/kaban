//! Ordering across merges (GRA-06, PC-2): neighbour lists merged from base, layers and sediment
//! are ordered by (neighbour, relation); equal-neighbour runs of several relations, self-loops
//! and reciprocal edges keep their identity; traversal results are ordered by (depth, local id)
//! whatever order the parents found them in; set functions collapse equal-neighbour runs.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod oracle;

use kaban_index_graph::{
    Attr, Base, Caps, Direction, EdgeOp, Layer, LocalId, NodeOp, OpKind, Records, RelId,
    RelSelection, build_base, common_neighbours, degree, expand, seal,
};
use oracle::{Oracle, budget, rows, view};
use tantivy::directory::OwnedBytes;

fn node(id: u32) -> NodeOp {
    NodeOp {
        node: LocalId(id),
        kind: OpKind::Add,
    }
}

fn edge(src: u32, rel: u16, dst: u32, kind: OpKind) -> EdgeOp {
    EdgeOp {
        src: LocalId(src),
        rel: RelId(rel),
        dst: LocalId(dst),
        kind,
        attr: Attr {
            weight: i64::from(src) * 1000 + i64::from(dst),
            ts: i64::from(rel),
        },
    }
}

fn add(src: u32, rel: u16, dst: u32) -> EdgeOp {
    edge(src, rel, dst, OpKind::Add)
}

/// Node 1's edges spread over a base, a layer and the sediment, with equal-neighbour runs of
/// several relations and a removal in the sediment.
fn spread() -> (Base, Vec<Layer>, Vec<Records>, Oracle) {
    let base_recs = Records {
        nodes: (0..12).map(node).collect(),
        edges: vec![add(1, 0, 5), add(1, 1, 9), add(1, 7, 9), add(4, 0, 1)],
        ..Records::default()
    };
    let layer_recs = Records {
        edges: vec![add(1, 0, 2), add(1, 7, 5), add(5, 0, 1)],
        ..Records::default()
    };
    let sediment = vec![Records {
        edges: vec![
            add(1, 1, 5),
            add(1, 0, 3),
            edge(1, 1, 9, OpKind::Remove),
            add(1, 0, 1),
        ],
        ..Records::default()
    }];
    let base = Base::open(
        OwnedBytes::new(build_base(&base_recs, 0)),
        &oracle::budget(),
    )
    .unwrap();
    let layer = seal(&view::<Records>(Some(&base), &[], &[]), &layer_recs);
    let mut oracle = Oracle::default();
    for r in [&base_recs, &layer_recs, &sediment[0]] {
        oracle.apply(r);
    }
    (
        base,
        vec![Layer::open(OwnedBytes::new(layer), &oracle::budget()).unwrap()],
        sediment,
        oracle,
    )
}

fn keys(rows: &[(LocalId, RelId, Attr)]) -> Vec<(u32, u16)> {
    rows.iter().map(|&(m, r, _)| (m.0, r.0)).collect()
}

#[test]
fn merged_lists_are_ordered_by_neighbour_then_relation() {
    let (base, layers, sediment, oracle) = spread();
    let v = view(Some(&base), &layers, &sediment);
    let all = RelSelection::All;
    let out = rows(&v, LocalId(1), &all, Direction::Out);
    assert_eq!(
        keys(&out),
        vec![(1, 0), (2, 0), (3, 0), (5, 0), (5, 1), (5, 7), (9, 7)],
        "a run of three relations to 5 stays in relation order; 1>9 r1 removed"
    );
    assert_eq!(out, oracle.neighbours(LocalId(1), &all, Direction::Out));
    // Both: the self-loop appears once per direction, outgoing first; the reciprocal pair
    // 1>5 / 5>1 keeps both edges.
    let both = rows(&v, LocalId(1), &all, Direction::Both);
    assert_eq!(
        keys(&both),
        vec![
            (1, 0),
            (1, 0),
            (2, 0),
            (3, 0),
            (4, 0),
            (5, 0),
            (5, 0),
            (5, 1),
            (5, 7),
            (9, 7)
        ]
    );
    assert_eq!(both, oracle.neighbours(LocalId(1), &all, Direction::Both));
}

#[test]
fn expand_ignoring_direction_reports_self_loops_once_and_keeps_reciprocal_edges() {
    let (base, layers, sediment, _) = spread();
    let v = view(Some(&base), &layers, &sediment);
    let b = budget();
    let caps = Caps {
        depth: 2,
        rows: 100,
    };
    let got = expand(
        &v,
        &[LocalId(1)],
        &RelSelection::All,
        Direction::Both,
        caps,
        &b,
    )
    .unwrap();
    let nodes: Vec<(u32, u8)> = got.nodes.iter().map(|&(n, d)| (n.0, d)).collect();
    assert_eq!(nodes, vec![(2, 1), (3, 1), (4, 1), (5, 1), (9, 1)]);
    let mut edges: Vec<(u32, u16, u32)> =
        got.edges.iter().map(|&(s, r, d)| (s.0, r.0, d.0)).collect();
    let reported = edges.len();
    edges.sort_unstable();
    edges.dedup();
    assert_eq!(reported, edges.len(), "no edge twice: {edges:?}");
    assert_eq!(
        edges,
        vec![
            (1, 0, 1),
            (1, 0, 2),
            (1, 0, 3),
            (1, 0, 5),
            (1, 1, 5),
            (1, 7, 5),
            (1, 7, 9),
            (4, 0, 1),
            (5, 0, 1),
        ]
    );
}

#[test]
fn traversal_results_are_ordered_by_depth_then_id_not_by_discovery() {
    // 0>10, 0>20; 10>40, 20>30: 40 is found before 30, yet 30 comes first at depth 2.
    let rs = [Records {
        nodes: [0, 10, 20, 30, 40].map(node).to_vec(),
        edges: vec![add(0, 0, 20), add(0, 0, 10), add(10, 0, 40), add(20, 0, 30)],
        ..Records::default()
    }];
    let v = view(None, &[], &rs);
    let caps = Caps {
        depth: 3,
        rows: 100,
    };
    let got = expand(
        &v,
        &[LocalId(0)],
        &RelSelection::All,
        Direction::Out,
        caps,
        &budget(),
    )
    .unwrap();
    let nodes: Vec<(u32, u8)> = got.nodes.iter().map(|&(n, d)| (n.0, d)).collect();
    assert_eq!(nodes, vec![(10, 1), (20, 1), (30, 2), (40, 2)]);
    // Cut inside depth 2: the lowest id wins, not the first found.
    let cut = Caps { depth: 3, rows: 3 };
    let got = expand(
        &v,
        &[LocalId(0)],
        &RelSelection::All,
        Direction::Out,
        cut,
        &budget(),
    )
    .unwrap();
    let nodes: Vec<(u32, u8)> = got.nodes.iter().map(|&(n, d)| (n.0, d)).collect();
    assert_eq!(nodes, vec![(10, 1), (20, 1), (30, 2)]);
    assert!(got.truncated);
}

#[test]
fn set_functions_collapse_equal_neighbour_runs() {
    let (base, layers, sediment, _) = spread();
    let v = view(Some(&base), &layers, &sediment);
    let b = budget();
    let all = RelSelection::All;
    // 1 reaches 5 by three relations and is reached from 5; 4's and 5's only neighbour is 1,
    // and 1 is a neighbour of itself through its self-loop.
    let shared = common_neighbours(&v, LocalId(1), LocalId(4), &all, Direction::Both, &b).unwrap();
    assert_eq!(*shared, vec![LocalId(1)]);
    let shared = common_neighbours(&v, LocalId(1), LocalId(5), &all, Direction::Both, &b).unwrap();
    assert_eq!(
        *shared,
        vec![LocalId(1)],
        "1 once despite five edges between 1 and 5"
    );
    let shared = common_neighbours(&v, LocalId(1), LocalId(1), &all, Direction::Both, &b).unwrap();
    assert_eq!(
        *shared,
        [1, 2, 3, 4, 5, 9].map(LocalId).to_vec(),
        "each neighbour once, ascending"
    );
    // Degree counts edges, not neighbours: the self-loop counts once per direction.
    assert_eq!(degree(&v, LocalId(1), &all, Direction::Out, &b).unwrap(), 7);
    assert_eq!(degree(&v, LocalId(1), &all, Direction::In, &b).unwrap(), 3);
    assert_eq!(
        degree(&v, LocalId(1), &all, Direction::Both, &b).unwrap(),
        10
    );
    let r1 = RelSelection::only(vec![RelId(1)]);
    assert_eq!(degree(&v, LocalId(1), &r1, Direction::Out, &b).unwrap(), 1);
}
