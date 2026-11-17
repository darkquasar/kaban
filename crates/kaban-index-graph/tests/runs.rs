//! Prepared sediment as immutable sorted runs (T9 A1): any sequence of extensions answers as
//! preparing the same batches from scratch and as the oracle, at every prefix; values built
//! earlier stay correct after later extensions; the run count stays within its limit; and a
//! refused or cancelled extension leaves the value it extends usable. Sealing a prefix of the
//! runs as a layer (T9 A1, missing decision 1: batch chronology) answers as the value unsealed,
//! and the remainder read over the extended chain still answers as the oracle.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod oracle;

use kaban_core::query_budget::QueryBudget;
use kaban_index_graph::{
    Attr, Base, Direction, EdgeOp, FactOp, GraphError, Layer, LocalId, MAX_SEDIMENT_RUNS, NodeOp,
    OpKind, PreparedSediment, Records, RelId, RelSelection, View, build_base, seal,
};
use oracle::{Oracle, Rng, budget, nodes, random_records, rows, view};
use tantivy::directory::OwnedBytes;

/// Ids the sediment of these tests draws from.
const IDS: u32 = 40;

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
            weight: i64::from(src) * 100 + i64::from(dst),
            ts: 1,
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

/// Every probe of `view` over ids `0..ids` equals `oracle`: presence, facts and neighbours in
/// each direction under two relation selections.
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

/// A small chain under the sediment: a base and two sealed layers over ids `0..IDS`.
struct Chain {
    base: Base,
    layers: Vec<Layer>,
    oracle: Oracle,
}

fn chain(seed: u64) -> Chain {
    let mut rng = Rng::new(seed + 500);
    let base_recs = random_records(&mut rng, IDS, 150, 5);
    let base = Base::open(OwnedBytes::new(build_base(&base_recs, 0)), &budget()).unwrap();
    let mut oracle = Oracle::default();
    oracle.apply(&base_recs);
    let mut layers: Vec<Layer> = Vec::new();
    for ops in [40, 3] {
        let r = random_records(&mut rng, IDS, ops, 30);
        let bytes = seal(&view::<Records>(Some(&base), &layers, &[]), &r);
        layers.push(Layer::open(OwnedBytes::new(bytes), &budget()).unwrap());
        oracle.apply(&r);
    }
    Chain {
        base,
        layers,
        oracle,
    }
}

/// A history of large and single-op batches (removals, re-adds, facts, several relations),
/// with node 5 given edges both ways and a fact, removed alone many batches later and re-added
/// with one edge after that, so the removal and the re-add fall in different runs.
fn history(seed: u64) -> Vec<Records> {
    let mut rng = Rng::new(seed);
    let mut h: Vec<Records> = (0..44)
        .map(|i| {
            let ops = match i % 8 {
                0 => 50,
                3 => 9,
                _ => 1,
            };
            random_records(&mut rng, IDS, ops, 30)
        })
        .collect();
    h.insert(
        4,
        Records {
            nodes: vec![node(5, OpKind::Add), node(6, OpKind::Add)],
            edges: vec![edge(5, 0, 6), edge(6, 0, 5), edge(5, 7, 6), edge(5, 1, 5)],
            facts: vec![fact(5, 0, b"before"), fact(6, 1, b"other")],
        },
    );
    h.insert(
        30,
        Records {
            nodes: vec![node(5, OpKind::Remove)],
            ..Records::default()
        },
    );
    h.insert(
        33,
        Records {
            nodes: vec![node(5, OpKind::Add)],
            edges: vec![edge(5, 7, 6)],
            ..Records::default()
        },
    );
    h
}

/// The oracle of the chain followed by `batches`.
fn oracle_after(chain: &Chain, batches: &[Records]) -> Oracle {
    let mut o = chain.oracle.clone();
    for r in batches {
        o.apply(r);
    }
    o
}

#[test]
fn every_extension_equals_preparing_from_scratch_and_the_oracle() {
    let b = budget();
    for seed in 0..3 {
        let c = chain(seed);
        let h = history(seed);
        let mut rng = Rng::new(seed + 77);
        let mut values: Vec<(usize, PreparedSediment)> = Vec::new();
        let mut done = 0;
        while done < h.len() {
            let group = (1 + usize::try_from(rng.below(3)).unwrap()).min(h.len() - done);
            let batches = &h[done..done + group];
            let next = match values.last() {
                Some((_, older)) => older.extend(batches, &b).unwrap(),
                None => PreparedSediment::empty().extend(batches, &b).unwrap(),
            };
            done += group;
            let ctx = format!("seed {seed} after {done} batches");
            assert_eq!(next.batches(), done, "{ctx}: batches");
            let runs = next.runs();
            assert!(
                (1..=MAX_SEDIMENT_RUNS).contains(&runs),
                "{ctx}: {runs} runs"
            );
            let want = oracle_after(&c, &h[..done]);
            let extended = View::new(Some(&c.base), &c.layers, &next, &b).unwrap();
            assert_matches(&extended, &want, IDS + 2, &format!("{ctx}, extended"));
            let scratch = PreparedSediment::prepare(&h[..done], &b).unwrap();
            let scratch = View::new(Some(&c.base), &c.layers, &scratch, &b).unwrap();
            assert_matches(&scratch, &want, IDS + 2, &format!("{ctx}, scratch"));
            // Without the chain, the sediment alone answers as its own batches.
            let alone = View::new(None, &[], &next, &b).unwrap();
            let mut own = Oracle::default();
            for r in &h[..done] {
                own.apply(r);
            }
            assert_matches(&alone, &own, IDS + 2, &format!("{ctx}, sediment alone"));
            values.push((done, next));
        }
        // Every older value still answers as its own prefix after all later extensions.
        for (upto, value) in &values {
            let v = View::new(Some(&c.base), &c.layers, value, &b).unwrap();
            let ctx = format!("seed {seed} value of {upto} batches, re-read");
            assert_matches(&v, &oracle_after(&c, &h[..*upto]), IDS + 2, &ctx);
        }
    }
}

#[test]
fn single_op_extensions_keep_the_run_count_within_the_limit() {
    let b = budget();
    let mut rng = Rng::new(4242);
    let mut oracle = Oracle::default();
    let mut all = Vec::new();
    let mut value = PreparedSediment::empty();
    for i in 0..3000 {
        let r = random_records(&mut rng, 64, 1, 20);
        oracle.apply(&r);
        value = value.extend(std::slice::from_ref(&r), &b).unwrap();
        all.push(r);
        let runs = value.runs();
        assert!(
            (1..=MAX_SEDIMENT_RUNS).contains(&runs),
            "after {} extensions: {runs} runs",
            i + 1
        );
    }
    let v = View::new(None, &[], &value, &b).unwrap();
    assert_matches(&v, &oracle, 66, "3000 single-op extensions");
    let scratch = PreparedSediment::prepare(&all, &b).unwrap();
    let s = View::new(None, &[], &scratch, &b).unwrap();
    assert_matches(&s, &oracle, 66, "3000 batches prepared at once");
}

fn refusal(got: Result<PreparedSediment, GraphError>) -> &'static str {
    match got {
        Err(GraphError::BudgetExceeded { limit, .. }) => limit,
        other => panic!(
            "expected a budget refusal, got {:?}",
            other.map(|v| v.runs())
        ),
    }
}

#[test]
fn a_refused_or_cancelled_extension_leaves_the_value_usable() {
    let b = budget();
    let c = chain(9);
    let h = history(9);
    // A value of several runs: one large batch, then single batches.
    let mut value = PreparedSediment::prepare(&h[..1], &b).unwrap();
    for r in &h[1..6] {
        value = value.extend(std::slice::from_ref(r), &b).unwrap();
    }
    let want = oracle_after(&c, &h[..6]);
    let live = b.live_bytes();

    // A batch large enough to merge with every run: building its own run fits the small
    // budget, the merge that follows does not.
    let big: Vec<Records> = vec![random_records(&mut Rng::new(31), IDS, 400, 20)];
    let alone = QueryBudget::new(u64::MAX, None);
    let run_bytes = {
        let _run = PreparedSediment::prepare(&big, &alone).unwrap();
        alone.live_bytes()
    };
    let small = QueryBudget::new(run_bytes + run_bytes / 2, None);
    assert_eq!(refusal(value.extend(&big, &small)), "memory");
    assert_eq!(
        small.live_bytes(),
        0,
        "a refused extension releases everything"
    );

    let cancelled = QueryBudget::new(u64::MAX, None);
    cancelled.cancel();
    assert_eq!(refusal(value.extend(&big, &cancelled)), "cancelled");
    assert_eq!(cancelled.live_bytes(), 0);

    // The value is untouched: same charge, same answers, and it still extends.
    assert_eq!(b.live_bytes(), live);
    let v = View::new(Some(&c.base), &c.layers, &value, &b).unwrap();
    assert_matches(&v, &want, IDS + 2, "after the refusals");
    let next = value.extend(&big, &b).unwrap();
    let v = View::new(Some(&c.base), &c.layers, &next, &b).unwrap();
    let mut want = want;
    want.apply(&big[0]);
    assert_matches(&v, &want, IDS + 2, "extended after the refusals");
}

/// Build a value from `h` by extensions of one to three batches.
fn extended(h: &[Records], seed: u64) -> PreparedSediment {
    let b = budget();
    let mut rng = Rng::new(seed + 31);
    let mut value = PreparedSediment::empty();
    let mut done = 0;
    while done < h.len() {
        let group = (1 + usize::try_from(rng.below(3)).unwrap()).min(h.len() - done);
        value = value.extend(&h[done..done + group], &b).unwrap();
        done += group;
    }
    value
}

/// The chain's layers followed by `layer`.
fn with_layer(c: &Chain, layer: Vec<u8>) -> Vec<Layer> {
    let mut layers = c.layers.clone();
    layers.push(Layer::open(OwnedBytes::new(layer), &budget()).unwrap());
    layers
}

#[test]
fn sealing_a_prepared_value_answers_as_the_value_unsealed() {
    let b = budget();
    for seed in 0..3 {
        let c = chain(seed);
        let h = history(seed);
        let value = extended(&h, seed);
        assert!(
            value.runs() > 1,
            "seed {seed}: the fixture holds several runs"
        );
        let want = oracle_after(&c, &h);
        let unsealed = View::new(Some(&c.base), &c.layers, &value, &b).unwrap();
        assert_matches(&unsealed, &want, IDS + 2, &format!("seed {seed} unsealed"));
        let sealed = value.seal(&unsealed, h.len(), &b).unwrap();
        assert_eq!(sealed.through, h.len(), "seed {seed}: everything is sealed");
        assert_eq!(sealed.rest.runs(), 0, "seed {seed}: nothing is left");
        let layers = with_layer(&c, sealed.layer);
        let v = View::new(Some(&c.base), &layers, &sealed.rest, &b).unwrap();
        assert_matches(&v, &want, IDS + 2, &format!("seed {seed} sealed"));
    }
}

#[test]
fn both_sealing_entries_write_the_same_layer_for_one_batch() {
    let b = budget();
    for seed in 0..3 {
        let c = chain(seed);
        let parent = View::new(Some(&c.base), &c.layers, &EMPTY, &b).unwrap();
        for (i, r) in history(seed).iter().enumerate() {
            let prepared = PreparedSediment::prepare(std::slice::from_ref(r), &b).unwrap();
            let sealed = prepared.seal(&parent, 1, &b).unwrap();
            assert_eq!(sealed.through, 1, "seed {seed} batch {i}");
            assert_eq!(sealed.layer, seal(&parent, r), "seed {seed} batch {i}");
        }
    }
}

#[test]
fn sealing_a_prefix_leaves_a_remainder_that_extends_over_the_new_chain() {
    let b = budget();
    for seed in 0..3 {
        let c = chain(seed);
        let h = history(seed);
        let (now, later) = h.split_at(38);
        let value = extended(now, seed);
        let unsealed = View::new(Some(&c.base), &c.layers, &value, &b).unwrap();
        for upto in [0, 1, 5, 17, 30, 37, 38] {
            let ctx = format!("seed {seed} sealed up to {upto}");
            let sealed = value.seal(&unsealed, upto, &b).unwrap();
            assert!(sealed.through <= upto, "{ctx}: through {}", sealed.through);
            assert_eq!(sealed.rest.batches(), now.len(), "{ctx}: batches");
            // The layer holds exactly the batches before `through`.
            let layers = with_layer(&c, sealed.layer);
            let only = View::new(Some(&c.base), &layers, &EMPTY, &b).unwrap();
            let sealed_part = oracle_after(&c, &now[..sealed.through]);
            assert_matches(&only, &sealed_part, IDS + 2, &format!("{ctx}, layer alone"));
            let v = View::new(Some(&c.base), &layers, &sealed.rest, &b).unwrap();
            assert_matches(&v, &oracle_after(&c, now), IDS + 2, &ctx);
            // The remainder keeps extending over the new chain.
            let rest = sealed.rest.extend(later, &b).unwrap();
            let v = View::new(Some(&c.base), &layers, &rest, &b).unwrap();
            assert_matches(
                &v,
                &oracle_after(&c, &h),
                IDS + 2,
                &format!("{ctx}, extended"),
            );
        }
    }
}

static EMPTY: PreparedSediment = PreparedSediment::empty();

#[test]
fn a_removal_and_readd_split_across_runs_is_sealed_without_resurrection() {
    let b = budget();
    let base_recs = Records {
        nodes: (1..=4).map(|n| node(n, OpKind::Add)).collect(),
        edges: vec![edge(1, 0, 2), edge(2, 0, 1), edge(3, 7, 1)],
        facts: vec![fact(1, 0, b"base")],
    };
    let base = Base::open(OwnedBytes::new(build_base(&base_recs, 0)), &b).unwrap();
    // Run 1: a large batch adding more of node 1. Run 2: node 1 removed among unrelated edges.
    // Run 3: node 1 re-added with one edge and a fact. The sizes keep them separate runs.
    let filler = |from: u32, n: u32| (from..from + n).map(|i| edge(20 + i, 0, 21 + i)).collect();
    let mut first = Records {
        nodes: vec![node(1, OpKind::Add)],
        edges: vec![edge(1, 7, 3), edge(4, 0, 1)],
        facts: vec![fact(1, 1, b"older")],
    };
    first.edges.extend::<Vec<_>>(filler(0, 30));
    let removal = Records {
        nodes: vec![node(1, OpKind::Remove)],
        edges: filler(40, 6),
        ..Records::default()
    };
    let readd = Records {
        nodes: vec![node(1, OpKind::Add)],
        edges: vec![edge(1, 7, 2)],
        facts: vec![fact(1, 0, b"new")],
    };
    let h = [first, removal, readd];
    let mut value = PreparedSediment::prepare(&h[..1], &b).unwrap();
    for r in &h[1..] {
        value = value.extend(std::slice::from_ref(r), &b).unwrap();
    }
    assert_eq!(
        value.runs(),
        3,
        "the removal and the re-add sit in different runs"
    );
    let mut oracle = Oracle::default();
    oracle.apply(&base_recs);
    for r in &h {
        oracle.apply(r);
    }
    let parent = View::new(Some(&base), &[], &value, &b).unwrap();
    for upto in 0..=3 {
        let ctx = format!("sealed up to {upto}");
        let sealed = value.seal(&parent, upto, &b).unwrap();
        assert_eq!(sealed.through, upto, "{ctx}: runs end at every batch");
        let layers = [Layer::open(OwnedBytes::new(sealed.layer), &b).unwrap()];
        let v = View::new(Some(&base), &layers, &sealed.rest, &b).unwrap();
        assert_matches(&v, &oracle, 72, &ctx);
        assert_eq!(
            v.fact(LocalId(1), 1),
            None,
            "{ctx}: the older fact stays deleted"
        );
        let both: Vec<(u32, u16)> = rows(&v, LocalId(1), &RelSelection::All, Direction::Both)
            .into_iter()
            .map(|(m, r, _)| (m.0, r.0))
            .collect();
        assert_eq!(both, vec![(2, 7)], "{ctx}: only the re-added edge");
    }
}
