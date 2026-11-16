//! Step 1 (T9): the one extent format and the merged view equal an edge-list oracle over a base,
//! three sealed layers and sediment records; every neighbour list is ascending (GRA-06); a fold
//! that omits a node drops it, its edges and its facts from the bytes (DEL-06, COD-01).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod oracle;

use kaban_index_graph::{
    Base, Direction, EdgeOp, GraphError, Layer, LocalId, NodeOp, OpKind, Records, RelId,
    RelSelection, View, build_base, fold, seal,
};
use oracle::{Oracle, Rng, random_records};
use tantivy::directory::OwnedBytes;

const IDS: u32 = 120;
const SEEDS: u64 = 16;

struct Fixture {
    base: Base,
    layers: Vec<Layer>,
    sediment: Vec<Records>,
    all: Vec<Records>,
    oracle: Oracle,
}

fn open_base(bytes: Vec<u8>) -> Base {
    Base::open(OwnedBytes::new(bytes)).expect("base opens")
}

/// A base, three layers (dense, sparse, medium) and two sediment batches from `seed`. Even
/// seeds use a small id domain (dense presence indexes), odd seeds a large one (sparse).
fn fixture(seed: u64) -> Fixture {
    let mut rng = Rng::new(seed);
    let domain = if seed % 2 == 0 { 200 } else { 20_000 };
    let base_recs = random_records(&mut rng, IDS, 400, 5);
    let layer_recs = [
        random_records(&mut rng, IDS, 150, 30),
        random_records(&mut rng, IDS, 2, 40),
        random_records(&mut rng, IDS, 60, 30),
    ];
    let sediment = vec![
        random_records(&mut rng, IDS, 40, 30),
        random_records(&mut rng, IDS, 30, 30),
    ];
    let base = open_base(build_base(&base_recs, domain));
    let mut layers: Vec<Layer> = Vec::new();
    for r in &layer_recs {
        let bytes = seal(&View::new(Some(&base), &layers, &[]), r);
        layers.push(Layer::open(OwnedBytes::new(bytes)).expect("layer opens"));
    }
    let mut all = vec![base_recs];
    all.extend(layer_recs);
    all.extend(sediment.iter().cloned());
    let mut oracle = Oracle::default();
    for r in &all {
        oracle.apply(r);
    }
    Fixture {
        base,
        layers,
        sediment,
        all,
        oracle,
    }
}

fn selections() -> Vec<RelSelection> {
    vec![
        RelSelection::All,
        RelSelection::only(vec![RelId(40_000), RelId(1)]),
        RelSelection::only(vec![RelId(7), RelId(0), RelId(7)]),
        RelSelection::only(Vec::new()),
    ]
}

/// Every probe of `view` equals `oracle`, for ids past the generated range too.
fn assert_view_matches(view: &View<'_>, oracle: &Oracle, ctx: &str) {
    assert_eq!(view.nodes().collect::<Vec<_>>(), oracle.nodes(), "{ctx}: nodes");
    for id in 0..IDS + 3 {
        let n = LocalId(id);
        assert_eq!(view.contains(n), oracle.contains(n), "{ctx}: contains {id}");
        for key in 0..5 {
            assert_eq!(view.fact(n, key), oracle.fact(n, key), "{ctx}: fact {id}/{key}");
        }
        for rels in &selections() {
            for dir in [Direction::Out, Direction::In, Direction::Both] {
                let got: Vec<_> = view.neighbours(n, rels, dir).collect();
                let want = oracle.neighbours(n, rels, dir);
                assert_eq!(got, want, "{ctx}: neighbours {id} {rels:?} {dir:?}");
                let keys: Vec<_> = got.iter().map(|&(m, r, _)| (m, r)).collect();
                if dir == Direction::Both {
                    assert!(
                        keys.windows(2).all(|w| w[0] <= w[1]),
                        "{ctx}: {id} both not ascending"
                    );
                } else {
                    assert!(
                        keys.windows(2).all(|w| w[0] < w[1]),
                        "{ctx}: {id} {dir:?} not strictly ascending"
                    );
                }
            }
        }
    }
}

#[test]
fn layered_view_equals_the_oracle_on_every_node() {
    for seed in 0..SEEDS {
        let f = fixture(seed);
        let view = View::new(Some(&f.base), &f.layers, &f.sediment);
        assert_view_matches(&view, &f.oracle, &format!("seed {seed}"));
    }
}

#[test]
fn sediment_only_view_equals_the_oracle() {
    for seed in 0..4 {
        let f = fixture(seed);
        let view = View::new(None, &[], &f.all);
        assert_view_matches(&view, &f.oracle, &format!("sediment seed {seed}"));
    }
}

#[test]
fn every_prefix_of_the_chain_equals_the_oracle() {
    let f = fixture(3);
    let mut oracle = Oracle::default();
    oracle.apply(&f.all[0]);
    assert_view_matches(&View::new(Some(&f.base), &[], &[]), &oracle, "base only");
    for depth in 1..=f.layers.len() {
        oracle.apply(&f.all[depth]);
        let view = View::new(Some(&f.base), &f.layers[..depth], &[]);
        assert_view_matches(&view, &oracle, &format!("depth {depth}"));
    }
}

/// The present node with the most edges in the oracle.
fn busiest(oracle: &Oracle) -> LocalId {
    oracle
        .nodes()
        .into_iter()
        .max_by_key(|&n| oracle.neighbours(n, &RelSelection::All, Direction::Both).len())
        .expect("fixture has present nodes")
}

#[test]
fn fold_omitting_a_node_drops_it_its_edges_and_its_facts() {
    for seed in 0..SEEDS {
        let f = fixture(seed);
        let view = View::new(Some(&f.base), &f.layers, &f.sediment);
        let n = busiest(&f.oracle);
        assert!(
            !f.oracle
                .neighbours(n, &RelSelection::All, Direction::Both)
                .is_empty(),
            "seed {seed}: the omitted node has edges"
        );
        let folded = open_base(fold(&view, &[n]));
        let mut purged = f.oracle.clone();
        purged.purge(n);
        let ctx = format!("fold seed {seed}");
        assert_view_matches(&View::new(Some(&folded), &[], &[]), &purged, &ctx);

        // Re-adding the node must not resurrect anything: the bytes no longer hold its edges
        // or facts, they were not merely hidden by presence.
        let readd = [Records {
            nodes: vec![NodeOp {
                node: n,
                kind: OpKind::Add,
            }],
            ..Records::default()
        }];
        purged.apply(&readd[0]);
        let ctx = format!("fold+readd seed {seed}");
        assert_view_matches(&View::new(Some(&folded), &[], &readd), &purged, &ctx);
    }
}

#[test]
fn records_round_trip_through_the_sediment_blob() {
    for seed in 0..SEEDS {
        let mut rng = Rng::new(seed);
        let r = random_records(&mut rng, IDS, 50, 30);
        assert_eq!(Records::decode(&r.encode()), Ok(r), "seed {seed}");
    }
    let empty = Records::default();
    assert_eq!(Records::decode(&empty.encode()), Ok(empty));
}

#[test]
fn an_edge_op_encodes_in_27_bytes() {
    let one = Records {
        edges: vec![EdgeOp {
            src: LocalId(u32::MAX),
            rel: RelId(u16::MAX),
            dst: LocalId(0),
            kind: OpKind::Remove,
            attr: kaban_index_graph::Attr {
                weight: i64::MIN,
                ts: i64::MAX,
            },
        }],
        ..Records::default()
    };
    let empty = Records::default().encode().len();
    assert_eq!(one.encode().len() - empty, 27);
}

fn is_corrupt<T: std::fmt::Debug>(r: &Result<T, GraphError>) -> bool {
    matches!(r, Err(GraphError::Corrupt(_)))
}

#[test]
fn truncated_or_padded_bytes_are_corrupt() {
    let f = fixture(2);
    let blob = f.all[1].encode();
    for len in 0..blob.len() {
        assert!(is_corrupt(&Records::decode(&blob[..len])), "records prefix {len}");
    }
    let mut padded = blob.clone();
    padded.push(0);
    assert!(is_corrupt(&Records::decode(&padded)), "records trailing byte");

    let base_bytes = build_base(&f.all[0], 200);
    let layer_bytes = seal(&View::new(Some(&f.base), &[], &[]), &f.all[1]);
    for (name, bytes) in [("base", &base_bytes), ("layer", &layer_bytes)] {
        assert!(bytes.len() > 64, "{name} has content");
        for len in (0..bytes.len()).step_by(7).chain([bytes.len() - 1]) {
            let cut = OwnedBytes::new(bytes[..len].to_vec());
            let opened = if name == "base" {
                Base::open(cut).map(|_| ())
            } else {
                Layer::open(cut).map(|_| ())
            };
            assert!(is_corrupt(&opened), "{name} prefix {len}");
        }
        let mut padded = bytes.clone();
        padded.push(0);
        let opened = if name == "base" {
            Base::open(OwnedBytes::new(padded)).map(|_| ())
        } else {
            Layer::open(OwnedBytes::new(padded)).map(|_| ())
        };
        assert!(is_corrupt(&opened), "{name} trailing byte");
    }
    assert!(
        is_corrupt(&Layer::open(OwnedBytes::new(base_bytes)).map(|_| ())),
        "a base is not a layer"
    );
    assert!(
        is_corrupt(&Base::open(OwnedBytes::new(layer_bytes)).map(|_| ())),
        "a layer is not a base"
    );
}
