//! Corruption is not absence (T9 midpoint MAJ-01, MED-01): extent bytes that keep their lengths
//! but break an invariant (rank samples, kind bytes, interior offsets, id order, domain, padding
//! bits, section count and order) are refused at open, never read as a partial or stale answer
//! and never a panic. Also the format edges: density boundary, rank samples past 512 bits,
//! empty extents and the `u32::MAX` id.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion
#![allow(clippy::cast_possible_truncation)] // test fixtures: small extents on a 64-bit host

mod oracle;

use std::ops::Range;

use kaban_index_graph::{
    Attr, Base, Direction, EdgeOp, FactOp, GraphError, Layer, LocalId, NodeOp, OpKind, Records,
    RelId, RelSelection, View, build_base, seal,
};
use oracle::{nodes, rows, view};
use tantivy::directory::OwnedBytes;

const PRESENCE: u32 = 1;
const OUT: u32 = 2;
const FACTS: u32 = 4;
const UNION_OUT: u32 = 5;

fn node(id: u32, kind: OpKind) -> NodeOp {
    NodeOp {
        node: LocalId(id),
        kind,
    }
}

fn edge(src: u32, rel: u16, dst: u32) -> EdgeOp {
    EdgeOp {
        src: LocalId(src),
        rel: RelId(rel),
        dst: LocalId(dst),
        kind: OpKind::Add,
        attr: Attr {
            weight: i64::from(src),
            ts: i64::from(dst),
        },
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

fn open_base(bytes: &[u8]) -> Result<Base, GraphError> {
    Base::open(OwnedBytes::new(bytes.to_vec()), &oracle::budget())
}

fn open_layer(bytes: &[u8]) -> Result<Layer, GraphError> {
    Layer::open(OwnedBytes::new(bytes.to_vec()), &oracle::budget())
}

fn assert_corrupt<T>(r: Result<T, GraphError>, what: &str) {
    match r {
        Err(GraphError::Corrupt(_)) => {}
        Err(e) => panic!("{what}: expected Corrupt, got {e:?}"),
        Ok(_) => panic!("{what}: corrupt bytes opened"),
    }
}

/// The byte range of section `tag`, following the documented layout: a 24-byte header with the
/// section count at offset 20, 12-byte table entries (tag `u32`, length `u64`), then sections in
/// table order.
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

fn put_u32(bytes: &mut [u8], at: usize, v: u32) {
    bytes[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

fn put_u64(bytes: &mut [u8], at: usize, v: u64) {
    bytes[at..at + 8].copy_from_slice(&v.to_le_bytes());
}

fn get_u64(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
}

/// Offsets inside a sorted-mode id index followed by columns: (ids, end of ids).
fn sorted_ids(bytes: &[u8], start: usize) -> (usize, usize) {
    assert_eq!(bytes[start], 0, "fixture index is sorted");
    let count = get_u64(bytes, start + 1) as usize;
    (start + 9, start + 9 + 4 * count)
}

/// A small sparse base: nodes 1, 2, 3, 5; edges 1>2, 1>3, 1>5 (rel 1), 2>1; facts on 1 and 2.
fn sparse() -> Records {
    Records {
        nodes: [1, 2, 3, 5].map(|n| node(n, OpKind::Add)).to_vec(),
        edges: vec![edge(1, 0, 2), edge(1, 0, 3), edge(1, 1, 5), edge(2, 0, 1)],
        facts: vec![fact(1, 0, b"a"), fact(1, 1, b"bb"), fact(2, 0, b"c")],
    }
}

/// Edge-section positions of [`sparse`]'s outgoing section: offsets, neighbours, kinds.
struct EdgeAt {
    offsets: usize,
    nbrs: usize,
    kinds: usize,
}

fn out_layout(bytes: &[u8]) -> EdgeAt {
    let s = section(bytes, OUT);
    let (_, ids_end) = sorted_ids(bytes, s.start);
    let touched = 2;
    let entries = 4;
    let offsets = ids_end;
    let nbrs = offsets + 8 * (touched + 1);
    EdgeAt {
        offsets,
        nbrs,
        kinds: nbrs + 4 * entries + 2 * entries,
    }
}

/// Fact-section positions: (nodes, keys, kinds, offsets) for 3 facts.
fn fact_layout(bytes: &[u8]) -> (usize, usize, usize, usize) {
    let s = section(bytes, FACTS);
    let count = get_u64(bytes, s.start) as usize;
    assert_eq!(count, 3);
    let nodes = s.start + 8;
    let keys = nodes + 4 * count;
    let kinds = keys + count;
    (nodes, keys, kinds, kinds + count)
}

#[test]
fn a_well_formed_sparse_base_opens_and_reads() {
    let bytes = build_base(&sparse(), 1000);
    let base = open_base(&bytes).unwrap();
    let v = view::<Records>(Some(&base), &[], &[]);
    assert_eq!(nodes(&v), [1, 2, 3, 5].map(LocalId).to_vec());
    assert_eq!(
        rows(&v, LocalId(1), &RelSelection::All, Direction::Out).len(),
        3
    );
    assert_eq!(v.fact(LocalId(1), 1), Some(&b"bb"[..]));
}

#[test]
fn broken_invariants_with_unchanged_lengths_are_corrupt() {
    let good = build_base(&sparse(), 1000);
    let mut cases: Vec<(&str, Vec<u8>)> = Vec::new();
    let mut case = |name, f: &dyn Fn(&mut Vec<u8>)| {
        let mut b = good.clone();
        f(&mut b);
        assert_ne!(b, good, "{name} mutates");
        cases.push((name, b));
    };
    let p = section(&good, PRESENCE);
    let (ids, kinds) = sorted_ids(&good, p.start);
    case("presence kind byte", &|b| b[kinds + 1] = 2);
    case("presence ids unsorted", &|b| {
        put_u32(b, ids + 4, 3);
        put_u32(b, ids + 8, 2);
    });
    case("presence id duplicate", &|b| put_u32(b, ids + 4, 1));
    case("presence id past domain", &|b| put_u32(b, ids + 12, 1000));
    let e = out_layout(&good);
    case("edge kind byte", &|b| b[e.kinds] = 2);
    case("edge list unsorted", &|b| {
        put_u32(b, e.nbrs, 3);
        put_u32(b, e.nbrs + 4, 2);
    });
    case("edge list duplicate", &|b| put_u32(b, e.nbrs + 4, 2));
    case("edge neighbour past domain", &|b| {
        put_u32(b, e.nbrs + 8, 1000);
    });
    case("edge offsets decrease", &|b| put_u64(b, e.offsets + 8, 5));
    case("edge offsets do not start at zero", &|b| {
        put_u64(b, e.offsets, 1);
    });
    let (fnodes, fkeys, fkinds, foffs) = fact_layout(&good);
    case("fact kind byte", &|b| b[fkinds] = 2);
    case("fact key duplicate", &|b| b[fkeys + 1] = 0);
    case("fact nodes unsorted", &|b| put_u32(b, fnodes + 8, 0));
    case("fact node past domain", &|b| put_u32(b, fnodes + 8, 1000));
    case("fact offsets decrease", &|b| put_u64(b, foffs + 8, 4));
    case("section count", &|b| put_u32(b, 20, 5));
    case("huge section count", &|b| put_u32(b, 20, u32::MAX));
    case("duplicate section", &|b| put_u32(b, 24 + 12, PRESENCE));
    case("reordered sections", &|b| {
        // Swap the outgoing and incoming table entries (their sections keep their bytes, so
        // the lengths still fill the file).
        let (o, i) = (24 + 12, 24 + 24);
        let (ot, it) = (b[o..o + 12].to_vec(), b[i..i + 12].to_vec());
        b[o..o + 12].copy_from_slice(&it);
        b[i..i + 12].copy_from_slice(&ot);
    });
    for (name, bytes) in cases {
        assert_corrupt(open_base(&bytes), name);
    }
}

#[test]
fn a_newer_tombstone_with_a_bad_kind_byte_is_corrupt_not_skipped() {
    let base = open_base(&build_base(&sparse(), 1000)).unwrap();
    let removal = Records {
        nodes: vec![node(1, OpKind::Remove)],
        facts: vec![FactOp {
            node: LocalId(2),
            key: 0,
            kind: OpKind::Remove,
            payload: Vec::new(),
        }],
        ..Records::default()
    };
    let good = seal(&view::<Records>(Some(&base), &[], &[]), &removal);
    let layer = open_layer(&good).unwrap();
    let v = view::<Records>(Some(&base), std::slice::from_ref(&layer), &[]);
    assert!(!v.contains(LocalId(1)));
    assert_eq!(v.fact(LocalId(2), 0), None);

    let p = section(&good, PRESENCE);
    let (_, kinds) = sorted_ids(&good, p.start);
    let mut bad = good.clone();
    bad[kinds] = 2;
    assert_corrupt(open_layer(&bad), "layer presence tombstone kind");

    let s = section(&good, FACTS);
    let count = get_u64(&good, s.start) as usize;
    let fkinds = s.start + 8 + 5 * count;
    let mut bad = good.clone();
    let at = (fkinds..fkinds + count)
        .find(|&i| good[i] == 1)
        .expect("a fact tombstone");
    bad[at] = 2;
    assert_corrupt(open_layer(&bad), "layer fact tombstone kind");

    let u = section(&good, UNION_OUT);
    let mut bad = good.clone();
    // Domain 1000: bits 1000..1024 of the last word are padding.
    let last = u.end - 8;
    let word = get_u64(&bad, last) | 1 << 50;
    put_u64(&mut bad, last, word);
    assert_corrupt(open_layer(&bad), "union padding bit");
    assert_corrupt(open_base(&good), "a layer is not a base");
}

/// A dense base (bitvector presence): nodes 0 and 1 over domain 40, plus a dense layer that
/// removes node 1.
fn dense_pair() -> (Vec<u8>, Vec<u8>) {
    let base = build_base(
        &Records {
            nodes: vec![node(0, OpKind::Add), node(1, OpKind::Add)],
            ..Records::default()
        },
        40,
    );
    let opened = open_base(&base).unwrap();
    let layer = seal(
        &view::<Records>(Some(&opened), &[], &[]),
        &Records {
            nodes: vec![node(0, OpKind::Add), node(1, OpKind::Remove)],
            ..Records::default()
        },
    );
    (base, layer)
}

/// Positions in a bit-mode presence section: (count, words, samples, kinds).
fn bit_presence(bytes: &[u8]) -> (usize, usize, usize, usize) {
    let s = section(bytes, PRESENCE);
    assert_eq!(bytes[s.start], 1, "fixture presence is a bitvector");
    let count = s.start + 1;
    let nbits = get_u64(bytes, s.start + 9);
    let words = s.start + 17;
    let nwords = nbits.div_ceil(64) as usize;
    let samples = words + 8 * nwords;
    (count, words, samples, samples + 8 * nwords.div_ceil(8))
}

#[test]
fn a_corrupt_rank_sample_is_refused_without_panic() {
    let (base, layer) = dense_pair();
    for (name, good) in [("base", &base), ("layer", &layer)] {
        let (_, _, samples, _) = bit_presence(good);
        for sample in [u64::MAX, u64::MAX - 1, 1] {
            let mut bad = good.clone();
            put_u64(&mut bad, samples, sample);
            let what = format!("{name} rank sample {sample}");
            if name == "base" {
                assert_corrupt(open_base(&bad), &what);
            } else {
                assert_corrupt(open_layer(&bad), &what);
            }
        }
    }
    // A valid pair still reads: node 0 present, node 1 removed by the newer layer.
    let b = open_base(&base).unwrap();
    let l = open_layer(&layer).unwrap();
    let v = view::<Records>(Some(&b), std::slice::from_ref(&l), &[]);
    assert!(v.contains(LocalId(0)));
    assert!(!v.contains(LocalId(1)));
}

#[test]
fn bitvector_padding_and_count_mismatch_are_corrupt() {
    let (base, _) = dense_pair();
    let (count, words, _, _) = bit_presence(&base);
    let mut bad = base.clone();
    let w = get_u64(&bad, words) | 1 << 50; // domain 40: bit 50 is padding
    put_u64(&mut bad, words, w);
    assert_corrupt(open_base(&bad), "presence padding bit");
    // A set bit inside the domain whose popcount no longer matches the count.
    let mut bad = base.clone();
    let w = get_u64(&bad, words) | 1 << 7;
    put_u64(&mut bad, words, w);
    assert_corrupt(open_base(&bad), "presence popcount");
    let mut bad = base.clone();
    put_u64(&mut bad, count, 3);
    assert_corrupt(open_base(&bad), "presence count");
}

/// Ids hit across the 63/64, 511/512 and 1023/1024 word and sample boundaries.
const EDGES_AT: [u32; 8] = [63, 64, 511, 512, 1023, 1024, 1535, 1536];

#[test]
fn dense_ranks_are_exact_across_word_and_sample_boundaries() {
    let domain = 2000;
    // Every node present and every node with one outgoing edge: dense presence and edge
    // indexes with four rank samples.
    let base_recs = Records {
        nodes: (0..domain).map(|n| node(n, OpKind::Add)).collect(),
        edges: (0..domain).map(|n| edge(n, 0, (n + 1) % domain)).collect(),
        facts: EDGES_AT
            .iter()
            .map(|&n| fact(n, 0, &n.to_le_bytes()))
            .collect(),
    };
    let base_bytes = build_base(&base_recs, domain);
    bit_presence(&base_bytes);
    let base = open_base(&base_bytes).unwrap();
    // A dense layer removing every third node, so a wrong rank reads a wrong kind.
    let removal = Records {
        nodes: (0..domain)
            .map(|n| {
                node(
                    n,
                    if n % 3 == 0 {
                        OpKind::Remove
                    } else {
                        OpKind::Add
                    },
                )
            })
            .collect(),
        ..Records::default()
    };
    let layer_bytes = seal(&view::<Records>(Some(&base), &[], &[]), &removal);
    bit_presence(&layer_bytes);
    let layer = open_layer(&layer_bytes).unwrap();
    let b = view::<Records>(Some(&base), &[], &[]);
    for n in 0..domain {
        assert!(b.contains(LocalId(n)), "base contains {n}");
        let got = rows(&b, LocalId(n), &RelSelection::All, Direction::Out);
        assert_eq!(got.len(), 1, "base out {n}");
        assert_eq!(got[0].0, LocalId((n + 1) % domain), "base out {n}");
    }
    for &n in &EDGES_AT {
        assert_eq!(
            b.fact(LocalId(n), 0),
            Some(&n.to_le_bytes()[..]),
            "fact {n}"
        );
    }
    let layers = [layer];
    let v = view::<Records>(Some(&base), &layers, &[]);
    let want: Vec<LocalId> = (0..domain).filter(|n| n % 3 != 0).map(LocalId).collect();
    assert_eq!(nodes(&v), want);
    for n in 0..domain {
        assert_eq!(v.contains(LocalId(n)), n % 3 != 0, "layered contains {n}");
    }
}

/// The index mode byte of the presence section.
fn presence_mode(bytes: &[u8]) -> u8 {
    bytes[section(bytes, PRESENCE).start]
}

#[test]
fn density_threshold_is_strict_at_equality() {
    let two = Records {
        nodes: vec![node(0, OpKind::Add), node(1, OpKind::Add)],
        ..Records::default()
    };
    // touched * 30 == domain keeps the sorted array; one id less of domain picks the bitvector.
    let at = build_base(&two, 60);
    let past = build_base(&two, 59);
    assert_eq!(presence_mode(&at), 0, "2 * 30 == 60 is sparse");
    assert_eq!(presence_mode(&past), 1, "2 * 30 > 59 is dense");
    for bytes in [at, past] {
        let base = open_base(&bytes).unwrap();
        let v = view::<Records>(Some(&base), &[], &[]);
        assert_eq!(nodes(&v), vec![LocalId(0), LocalId(1)]);
        assert!(!v.contains(LocalId(2)));
    }
}

#[test]
fn empty_base_layer_and_view_open_and_read_nothing() {
    let base = open_base(&build_base(&Records::default(), 0)).unwrap();
    let layer = open_layer(&seal(
        &view::<Records>(Some(&base), &[], &[]),
        &Records::default(),
    ))
    .unwrap();
    let layers = [layer];
    for v in [
        view::<Records>(None, &[], &[]),
        view::<Records>(Some(&base), &[], &[]),
        view::<Records>(Some(&base), &layers, &[]),
    ] {
        assert_eq!(nodes(&v), Vec::<LocalId>::new());
        for n in [0, 1, u32::MAX] {
            assert!(!v.contains(LocalId(n)));
            assert_eq!(
                rows(&v, LocalId(n), &RelSelection::All, Direction::Both),
                vec![]
            );
            assert_eq!(v.fact(LocalId(n), 0), None);
        }
    }
}

#[test]
fn the_largest_id_round_trips_through_a_base() {
    let max = u32::MAX;
    let recs = Records {
        nodes: vec![node(0, OpKind::Add), node(max, OpKind::Add)],
        edges: vec![edge(0, 9, max), edge(max, 9, 0)],
        facts: vec![fact(max, 255, b"top")],
    };
    let base = open_base(&build_base(&recs, 0)).unwrap();
    let v = view::<Records>(Some(&base), &[], &[]);
    assert_eq!(nodes(&v), vec![LocalId(0), LocalId(max)]);
    let out = rows(&v, LocalId(0), &RelSelection::All, Direction::Out);
    assert_eq!(
        out.iter().map(|r| r.0).collect::<Vec<_>>(),
        vec![LocalId(max)]
    );
    let both = rows(&v, LocalId(max), &RelSelection::All, Direction::Both);
    assert_eq!(
        both.iter().map(|r| r.0).collect::<Vec<_>>(),
        vec![LocalId(0); 2]
    );
    assert_eq!(v.fact(LocalId(max), 255), Some(&b"top"[..]));
}

const UNION_IN: u32 = 6;

/// Clear bit `id` of the union bitvector section `tag` of layer `bytes` (lengths unchanged).
fn clear_union_bit(bytes: &mut [u8], tag: u32, id: u32) {
    let words = section(bytes, tag).start + 8; // after the bit count
    let at = words + 8 * (id / 64) as usize;
    let word = get_u64(bytes, at) & !(1u64 << (id % 64));
    put_u64(bytes, at, word);
}

/// The view over `base` and `layers`, or the answer it gives: a view that prepares must read
/// node `n`'s list in `dir` as the oracle does (no edge); a view that refuses must say Corrupt.
fn assert_safe(base: &Base, layers: &[Layer], n: u32, dir: Direction, what: &str) {
    match View::new::<Records>(Some(base), layers, &[], &oracle::budget()) {
        Err(GraphError::Corrupt(_)) => {}
        Err(e) => panic!("{what}: expected Corrupt, got {e:?}"),
        Ok(v) => assert_eq!(
            rows(&v, LocalId(n), &RelSelection::All, dir),
            Vec::new(),
            "{what}: a missing union bit turned a tombstone into a stale edge"
        ),
    }
}

/// END-MED-01: a union bitvector that misses a touched node of its own layer or of an ancestor
/// layer is never trusted.
#[test]
fn a_union_missing_a_touched_node_is_never_trusted() {
    let base = open_base(&build_base(
        &Records {
            nodes: vec![
                node(1, OpKind::Add),
                node(2, OpKind::Add),
                node(3, OpKind::Add),
            ],
            edges: vec![edge(1, 0, 2)],
            ..Records::default()
        },
        0,
    ))
    .unwrap();
    // Layer 1 removes 1 > 2; layer 2 adds an unrelated edge 3 > 3.
    let remove = Records {
        edges: vec![EdgeOp {
            kind: OpKind::Remove,
            ..edge(1, 0, 2)
        }],
        ..Records::default()
    };
    let l1 = seal(&view::<Records>(Some(&base), &[], &[]), &remove);
    let first = [open_layer(&l1).unwrap()];
    let l2 = seal(
        &view::<Records>(Some(&base), &first, &[]),
        &Records {
            edges: vec![edge(3, 0, 3)],
            ..Records::default()
        },
    );
    // Sanity: the intact chains hide the removed edge.
    for layers in [
        vec![open_layer(&l1).unwrap()],
        vec![open_layer(&l1).unwrap(), open_layer(&l2).unwrap()],
    ] {
        let v = view::<Records>(Some(&base), &layers, &[]);
        assert!(rows(&v, LocalId(1), &RelSelection::All, Direction::Out).is_empty());
        assert!(rows(&v, LocalId(2), &RelSelection::All, Direction::In).is_empty());
    }
    for (tag, n, dir) in [(UNION_OUT, 1, Direction::Out), (UNION_IN, 2, Direction::In)] {
        // The current layer's own change.
        let mut bad = l1.clone();
        clear_union_bit(&mut bad, tag, n);
        match open_layer(&bad) {
            Err(GraphError::Corrupt(_)) => {}
            Err(e) => panic!("own change {tag}: expected Corrupt, got {e:?}"),
            Ok(layer) => assert_safe(&base, &[layer], n, dir, &format!("own change {tag}")),
        }
        // An ancestor layer's change, missing from the newest union.
        let mut bad = l2.clone();
        clear_union_bit(&mut bad, tag, n);
        match open_layer(&bad) {
            Err(GraphError::Corrupt(_)) => {}
            Err(e) => panic!("ancestor change {tag}: expected Corrupt, got {e:?}"),
            Ok(layer) => assert_safe(
                &base,
                &[open_layer(&l1).unwrap(), layer],
                n,
                dir,
                &format!("ancestor change {tag}"),
            ),
        }
    }
}
