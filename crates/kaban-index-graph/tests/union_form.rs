//! The layer union index (T9, B1): each union section is written as a sorted id list when it is
//! sparse and as a bitmap when it is dense, by the presence density rule (list when
//! touched * 30 <= domain). Both forms answer as the oracle through every chain depth, the form
//! flips exactly at the boundary, and malformed list forms are refused at open.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion
#![allow(clippy::cast_possible_truncation)] // test fixtures: small extents on a 64-bit host

mod oracle;

use std::ops::Range;

use kaban_index_graph::{
    Attr, Base, Direction, EdgeOp, GraphError, Layer, LocalId, NodeOp, OpKind, Records, RelId,
    RelSelection, View, build_base, seal,
};
use oracle::{IDS, Oracle, Rng, budget, nodes, random_records, rows, view};
use tantivy::directory::OwnedBytes;

const UNION_OUT: u32 = 5;
const UNION_IN: u32 = 6;
const LIST: u8 = 0;
const BITMAP: u8 = 1;

/// The byte range of section `tag`: a 24-byte header with the section count at offset 20,
/// 12-byte table entries (tag `u32`, length `u64`), then sections in table order.
fn section(bytes: &[u8], tag: u32) -> Range<usize> {
    let count = u32::from_le_bytes(bytes[20..24].try_into().unwrap()) as usize;
    let mut at = 24 + 12 * count;
    for i in 0..count {
        let e = 24 + 12 * i;
        let t = u32::from_le_bytes(bytes[e..e + 4].try_into().unwrap());
        let len = u64::from_le_bytes(bytes[e + 4..e + 12].try_into().unwrap()) as usize;
        if t == tag {
            return at..at + len;
        }
        at += len;
    }
    panic!("no section {tag}");
}

/// The form byte of union section `tag`.
fn form(bytes: &[u8], tag: u32) -> u8 {
    bytes[section(bytes, tag).start]
}

fn open_layer(bytes: &[u8]) -> Result<Layer, GraphError> {
    Layer::open(OwnedBytes::new(bytes.to_vec()), &budget())
}

fn node(id: u32, kind: OpKind) -> NodeOp {
    NodeOp {
        node: LocalId(id),
        kind,
    }
}

fn edge(src: u32, dst: u32, kind: OpKind) -> EdgeOp {
    EdgeOp {
        src: LocalId(src),
        rel: RelId(0),
        dst: LocalId(dst),
        kind,
        attr: Attr::default(),
    }
}

/// A base over at least `domain` ids and one layer sealed per batch, with the layer bytes and
/// the oracle after each prefix (index 0: the base alone).
struct Chain {
    base: Base,
    bytes: Vec<Vec<u8>>,
    prefixes: Vec<Oracle>,
}

impl Chain {
    fn new(base_recs: &Records, domain: u32, batches: &[Records]) -> Self {
        let base = Base::open(OwnedBytes::new(build_base(base_recs, domain)), &budget()).unwrap();
        let mut oracle = Oracle::default();
        oracle.apply(base_recs);
        let mut prefixes = vec![oracle.clone()];
        let mut bytes: Vec<Vec<u8>> = Vec::new();
        for r in batches {
            let layers: Vec<Layer> = bytes.iter().map(|b| open_layer(b).unwrap()).collect();
            bytes.push(seal(&view::<Records>(Some(&base), &layers, &[]), r));
            oracle.apply(r);
            prefixes.push(oracle.clone());
        }
        Self {
            base,
            bytes,
            prefixes,
        }
    }

    /// The opened layers of the first `depth` batches.
    fn layers(&self, depth: usize) -> Vec<Layer> {
        self.bytes[..depth]
            .iter()
            .map(|b| open_layer(b).unwrap())
            .collect()
    }
}

/// Presence and every neighbour list of `view` equal `oracle`, ids past the fixtures included.
fn assert_matches(view: &View<'_>, oracle: &Oracle, ctx: &str) {
    assert_eq!(nodes(view), oracle.nodes(), "{ctx}: nodes");
    for id in 0..IDS + 3 {
        let n = LocalId(id);
        assert_eq!(view.contains(n), oracle.contains(n), "{ctx}: contains {id}");
        for dir in [Direction::Out, Direction::In, Direction::Both] {
            assert_eq!(
                rows(view, n, &RelSelection::All, dir),
                oracle.neighbours(n, &RelSelection::All, dir),
                "{ctx}: neighbours {id} {dir:?}"
            );
        }
    }
}

/// Every prefix view of `chain` (depth 1..=3) equals its oracle, and every layer's two union
/// sections are in `want` form.
fn assert_chain(chain: &Chain, want: u8, ctx: &str) {
    for depth in 1..=chain.bytes.len() {
        let bytes = &chain.bytes[depth - 1];
        for tag in [UNION_OUT, UNION_IN] {
            assert_eq!(form(bytes, tag), want, "{ctx}: depth {depth} union {tag} form");
        }
        let layers = chain.layers(depth);
        let v = view::<Records>(Some(&chain.base), &layers, &[]);
        assert_matches(&v, &chain.prefixes[depth], &format!("{ctx}: depth {depth}"));
    }
}

#[test]
fn sparse_and_dense_layers_answer_as_the_oracle_at_depths_one_to_three() {
    for seed in 0..4 {
        let mut rng = Rng::new(seed + 7000);
        let base = random_records(&mut rng, IDS, 300, 5);
        // A few touched ids per layer in a million-id domain: sorted lists.
        let small: Vec<Records> = (0..3).map(|_| random_records(&mut rng, IDS, 3, 30)).collect();
        let sparse = Chain::new(&base, 1_000_000, &small);
        assert_chain(&sparse, LIST, &format!("sparse seed {seed}"));
        // Dozens of touched ids per layer over the fixtures' own domain: bitmaps.
        let wide: Vec<Records> = (0..3).map(|_| random_records(&mut rng, IDS, 60, 30)).collect();
        let dense = Chain::new(&base, 0, &wide);
        assert_chain(&dense, BITMAP, &format!("dense seed {seed}"));
    }
}

#[test]
fn the_union_form_flips_exactly_at_the_density_boundary() {
    // Two touched ids in each direction: list while 2 * 30 <= domain, bitmap past it.
    let base = Records {
        nodes: vec![node(0, OpKind::Add), node(1, OpKind::Add)],
        ..Records::default()
    };
    let batches = [
        Records {
            edges: vec![edge(0, 1, OpKind::Add), edge(1, 0, OpKind::Add)],
            ..Records::default()
        },
        Records {
            edges: vec![edge(0, 1, OpKind::Remove)],
            ..Records::default()
        },
    ];
    assert_chain(&Chain::new(&base, 60, &batches), LIST, "2 * 30 == 60");
    assert_chain(&Chain::new(&base, 59, &batches), BITMAP, "2 * 30 > 59");
}

/// A layer whose two union lists are [1, 2, 3] over a domain of 1,000 ids.
fn sparse_layer() -> Vec<u8> {
    let base = Records {
        nodes: (1..=3).map(|n| node(n, OpKind::Add)).collect(),
        ..Records::default()
    };
    let r = Records {
        edges: vec![
            edge(1, 2, OpKind::Add),
            edge(2, 3, OpKind::Add),
            edge(3, 1, OpKind::Add),
        ],
        ..Records::default()
    };
    Chain::new(&base, 1_000, &[r]).bytes.remove(0)
}

fn put_u32(bytes: &mut [u8], at: usize, v: u32) {
    bytes[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

#[test]
fn malformed_union_lists_are_refused() {
    let good = sparse_layer();
    assert!(open_layer(&good).is_ok(), "the intact layer opens");
    for tag in [UNION_OUT, UNION_IN] {
        let s = section(&good, tag);
        assert_eq!(good[s.start], LIST, "fixture union {tag} is a list");
        // Layout: form byte, count u64, ids u32 each.
        let ids = s.start + 9;
        let cases: [(&str, Box<dyn Fn(&mut Vec<u8>)>); 6] = [
            (
                "unsorted",
                Box::new(move |b| {
                    put_u32(b, ids, 2);
                    put_u32(b, ids + 4, 1);
                }),
            ),
            ("duplicate", Box::new(move |b| put_u32(b, ids + 4, 1))),
            ("id at the domain", Box::new(move |b| put_u32(b, ids + 8, 1_000))),
            ("id past the domain", Box::new(move |b| put_u32(b, ids + 8, u32::MAX))),
            ("unknown form byte", Box::new(move |b| b[ids - 9] = 2)),
            ("bitmap form byte on a list", Box::new(move |b| b[ids - 9] = BITMAP)),
        ];
        for (what, break_it) in &cases {
            let mut bad = good.clone();
            break_it(&mut bad);
            match open_layer(&bad) {
                Err(GraphError::Corrupt(_)) => {}
                Err(e) => panic!("union {tag} {what}: expected Corrupt, got {e:?}"),
                Ok(_) => panic!("union {tag} {what}: corrupt union opened"),
            }
        }
    }
}
