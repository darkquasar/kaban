//! Prepared sediment as immutable sorted runs (T9 A1): any sequence of extensions answers as
//! preparing the same batches from scratch and as the oracle, at every prefix; values built
//! earlier stay correct after later extensions; the run count stays within its limit; and a
//! refused or cancelled extension leaves the value it extends usable.

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
