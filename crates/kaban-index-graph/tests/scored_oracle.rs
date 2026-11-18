//! Step 1 (T10): the scored and set functions equal an independent dense reference on a
//! 200-node seeded fixture with weights and timestamps, some only in sediment records
//! (GRA-01 scored class, GRA-03): top-k order equal, scores within 1e-6 relative. Whole-graph
//! functions take a cap and refuse or flag a cut result (GRA-04).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::many_single_char_names
)] // small test sizes, short names in oracle loops

mod oracle;
mod reference;

use std::collections::{BTreeMap, BTreeSet};

use kaban_index_graph::{
    Caps, GraphError, LocalId, RankParams, RelSelection, RowCap, adamic_adar, clustering, jaccard,
    pagerank, ppr, time_window_expand, triangle_count, weighted_shortest_path,
};
use oracle::{Rng, budget};
use reference::{Graph, IDS, Order, scored, selections};

const SEEDS: u64 = 3;
const TOLERANCE: f64 = 1e-6;

fn close(got: f64, want: f64) -> bool {
    (got - want).abs() <= TOLERANCE * want.abs().max(f64::MIN_POSITIVE)
}

fn assert_ranked(what: &str, got: &[(LocalId, f64)], want: &[(u32, f64)]) {
    let ids: Vec<u32> = got.iter().map(|r| r.0.0).collect();
    let want_ids: Vec<u32> = want.iter().map(|r| r.0).collect();
    assert_eq!(ids, want_ids, "{what}: top-k order");
    for (g, w) in got.iter().zip(want) {
        assert!(
            close(g.1, w.1),
            "{what}: node {} score {} vs {}",
            w.0,
            g.1,
            w.1
        );
    }
}

fn params() -> Vec<RankParams> {
    vec![
        RankParams::default(),
        RankParams {
            damping: 0.5,
            iterations: 200,
            tolerance: 1e-12,
        },
    ]
}

#[test]
fn pagerank_and_ppr_equal_the_reference_in_top_k_order() {
    let order = Order::reversed(IDS);
    for seed in 0..SEEDS {
        let f = scored(seed);
        let v = f.view();
        let mut rng = Rng::new(seed + 100);
        for rels in selections() {
            let g = Graph::of(&f.oracle, &rels);
            for p in params() {
                for cap in [1u32, 10, 50, 1_500] {
                    let what = format!("seed {seed} {rels:?} {p:?} cap {cap}");
                    let scores = g.rank(&[], p.damping, p.iterations, p.tolerance);
                    let want = g.top(&scores, &order, cap as usize);
                    let got = pagerank(&v, &rels, p, &order, RowCap(cap), &budget()).unwrap();
                    assert_ranked(&format!("pagerank {what}"), &got.rows, &want);
                    assert_eq!(got.truncated, g.nodes.len() > cap as usize, "{what}");

                    let seeds: Vec<u32> =
                        (0..3).map(|_| rng.below(u64::from(IDS)) as u32).collect();
                    let scores = g.rank(&seeds, p.damping, p.iterations, p.tolerance);
                    let want = g.top(&scores, &order, cap as usize);
                    let ids: Vec<LocalId> = seeds.iter().map(|&s| LocalId(s)).collect();
                    let got = ppr(&v, &ids, &rels, p, &order, RowCap(cap), &budget()).unwrap();
                    assert_ranked(&format!("ppr {seeds:?} {what}"), &got.rows, &want);
                }
            }
        }
    }
}

#[test]
fn jaccard_adamic_adar_and_clustering_equal_the_reference() {
    for seed in 0..SEEDS {
        let f = scored(seed);
        let v = f.view();
        let mut rng = Rng::new(seed + 200);
        for rels in selections() {
            let g = Graph::of(&f.oracle, &rels);
            for _ in 0..60 {
                let a = rng.below(u64::from(IDS) + 3) as u32;
                let b = rng.below(u64::from(IDS) + 3) as u32;
                let what = format!("seed {seed} {rels:?} a {a} b {b}");
                let got = jaccard(&v, LocalId(a), LocalId(b), &rels, &budget()).unwrap();
                assert!(close(got, g.jaccard(a, b)), "jaccard {what}: {got}");
                let got = adamic_adar(&v, LocalId(a), LocalId(b), &rels, &budget()).unwrap();
                assert!(close(got, g.adamic_adar(a, b)), "adamic_adar {what}: {got}");
                let got = clustering(&v, LocalId(a), &rels, &budget()).unwrap();
                assert!(close(got, g.clustering(a)), "clustering {what}: {got}");
            }
        }
    }
}

#[test]
fn triangle_count_equals_the_reference_and_a_short_cap_refuses() {
    for seed in 0..SEEDS {
        let f = scored(seed);
        let v = f.view();
        for rels in selections() {
            let g = Graph::of(&f.oracle, &rels);
            let got = triangle_count(&v, &rels, RowCap(1_500), &budget()).unwrap();
            assert_eq!(got, g.triangles(), "seed {seed} {rels:?}");
            let short = u32::try_from(g.nodes.len() - 1).unwrap();
            assert_eq!(
                triangle_count(&v, &rels, RowCap(short), &budget()),
                Err(GraphError::CapExceeded {
                    limit: "rows",
                    max: u64::from(short),
                }),
                "a count over part of the graph is refused"
            );
        }
    }
}

#[test]
fn weighted_shortest_path_equals_the_reference() {
    let order = Order::reversed(IDS);
    let caps = Caps {
        depth: 255,
        rows: 1_500,
    };
    let mut found = 0;
    for seed in 0..SEEDS {
        let f = scored(seed);
        let v = f.view();
        let mut rng = Rng::new(seed + 300);
        for rels in selections() {
            let g = Graph::of(&f.oracle, &rels);
            for _ in 0..40 {
                let src = rng.below(u64::from(IDS)) as u32;
                let dst = rng.below(u64::from(IDS)) as u32;
                let want = g.cheapest(src, dst, &order);
                let got = weighted_shortest_path(
                    &v,
                    LocalId(src),
                    LocalId(dst),
                    &rels,
                    caps,
                    &order,
                    &budget(),
                )
                .unwrap();
                let got = got.map(|c| {
                    let (cost, path) = c.into_parts().0;
                    (cost, path.iter().map(|n| n.0).collect::<Vec<_>>())
                });
                found += usize::from(want.is_some());
                assert_eq!(got, want, "seed {seed} {rels:?} {src} -> {dst}");
            }
        }
    }
    assert!(found > 50, "the fixture finds paths ({found})");
}

#[test]
fn time_window_expand_equals_the_reference() {
    let mut nonempty = 0;
    for seed in 0..SEEDS {
        let f = scored(seed);
        let v = f.view();
        let mut rng = Rng::new(seed + 400);
        for rels in selections() {
            let g = Graph::of(&f.oracle, &rels);
            for _ in 0..20 {
                let src: Vec<u32> = (0..2).map(|_| rng.below(u64::from(IDS)) as u32).collect();
                let lo = rng.below(800_000) as i64;
                let window = (lo, lo + 300_000);
                let depth = 1 + rng.below(4) as u8;
                let (want_nodes, want_edges) = g.window(&src, window, depth);
                let ids: Vec<LocalId> = src.iter().map(|&s| LocalId(s)).collect();
                let caps = Caps { depth, rows: 1_500 };
                let got = time_window_expand(&v, &ids, &rels, window, caps, &budget()).unwrap();
                let nodes: BTreeMap<u32, u8> = got.nodes.iter().map(|&(n, d)| (n.0, d)).collect();
                let edges: BTreeSet<(u32, u16, u32)> =
                    got.edges.iter().map(|&(s, r, d)| (s.0, r.0, d.0)).collect();
                let what = format!("seed {seed} {rels:?} {src:?} {window:?} depth {depth}");
                assert_eq!(nodes, want_nodes, "nodes {what}");
                assert_eq!(edges, want_edges, "edges {what}");
                assert_eq!(got.edges.len(), edges.len(), "each edge once {what}");
                assert!(!got.truncated, "{what}");
                nonempty += usize::from(!nodes.is_empty());
            }
        }
    }
    assert!(nonempty > 20, "the windows reach nodes ({nonempty})");
}

#[test]
fn caps_and_parameters_out_of_range_are_refused() {
    let f = scored(0);
    let v = f.view();
    let order = Order::reversed(IDS);
    let all = RelSelection::All;
    let b = budget();
    let p = RankParams::default();
    let missing = Err(GraphError::CapMissing { limit: "rows" });
    assert_eq!(
        pagerank(&v, &all, p, &order, RowCap(0), &b).map(|r| r.rows.len()),
        missing
    );
    assert_eq!(
        ppr(&v, &[LocalId(1)], &all, p, &order, RowCap(0), &b).map(|r| r.rows.len()),
        missing
    );
    assert_eq!(
        triangle_count(&v, &all, RowCap(0), &b),
        missing.map(|_: usize| 0)
    );
    for (bad, name) in [
        (RankParams { damping: 1.5, ..p }, "damping"),
        (
            RankParams {
                damping: f64::NAN,
                ..p
            },
            "damping",
        ),
        (RankParams { iterations: 0, ..p }, "iterations"),
        (
            RankParams {
                tolerance: -1.0,
                ..p
            },
            "tolerance",
        ),
    ] {
        assert_eq!(
            pagerank(&v, &all, bad, &order, RowCap(5), &b).map(|r| r.rows.len()),
            Err(GraphError::InvalidParam { name })
        );
    }
    let caps = Caps { depth: 3, rows: 50 };
    assert_eq!(
        time_window_expand(&v, &[LocalId(1)], &all, (10, 9), caps, &b).map(|r| r.nodes.len()),
        Err(GraphError::InvalidParam { name: "window" })
    );
    for bad in [Caps { depth: 0, rows: 5 }, Caps { depth: 5, rows: 0 }] {
        assert!(matches!(
            weighted_shortest_path(&v, LocalId(1), LocalId(2), &all, bad, &order, &b),
            Err(GraphError::CapMissing { .. })
        ));
    }
}

#[test]
fn a_negative_weight_and_a_cut_weighted_search_are_refused() {
    let order = Order::reversed(IDS);
    let all = RelSelection::All;
    let wide = Caps {
        depth: 255,
        rows: 1_500,
    };
    // A chain 0 -> 1 -> ... -> 9, weight 1 each, the last edge negative.
    let mut base = kaban_index_graph::Records::default();
    for i in 0..10u32 {
        base.nodes.push(kaban_index_graph::NodeOp {
            node: LocalId(i),
            kind: kaban_index_graph::OpKind::Add,
        });
    }
    for i in 0..9u32 {
        base.edges.push(kaban_index_graph::EdgeOp {
            src: LocalId(i),
            rel: kaban_index_graph::RelId(0),
            dst: LocalId(i + 1),
            kind: kaban_index_graph::OpKind::Add,
            attr: kaban_index_graph::Attr {
                weight: if i == 8 { -1 } else { 1 },
                ts: 0,
            },
        });
    }
    let empty = kaban_index_graph::Records::default();
    let f = reference::Fixture::build(vec![base, empty.clone(), empty.clone(), empty]);
    let v = f.view();
    let b = budget();
    let path = |dst: u32, caps: Caps| {
        weighted_shortest_path(&v, LocalId(0), LocalId(dst), &all, caps, &order, &b)
            .map(|p| p.map(|c| c.into_parts().0))
    };
    assert_eq!(path(5, wide), Ok(Some((5, (0..=5).map(LocalId).collect()))));
    assert_eq!(
        path(9, wide),
        Err(GraphError::NegativeWeight {
            src: LocalId(8),
            rel: kaban_index_graph::RelId(0),
            dst: LocalId(9),
        })
    );
    // Settling 0..=3 is all four rows allow: node 5 lies past them and is not read as absent.
    assert_eq!(
        path(
            5,
            Caps {
                depth: 255,
                rows: 4
            }
        ),
        Err(GraphError::CapExceeded {
            limit: "rows",
            max: 4
        })
    );
    // The cheapest path has five hops: a depth cap of four refuses rather than cut it.
    assert_eq!(
        path(
            5,
            Caps {
                depth: 4,
                rows: 1_500
            }
        ),
        Err(GraphError::CapExceeded {
            limit: "depth",
            max: 4
        })
    );
}
