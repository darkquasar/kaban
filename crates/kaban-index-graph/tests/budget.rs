//! Query budgets reach the graph engine (T9 midpoint MAJ-02, MAJ-03; PC-17): decoding and
//! sediment preparation reserve before allocating and hold their charge while the data lives;
//! accessor iterators charge their cursors while alive; a cancelled, expired or memory-breached
//! budget stops every entry point before it reads, including an empty neighbour query. The
//! structural functions share that budget for their working sets and results (Step 3). That
//! every internal loop stops inside itself on cancellation is the cancellation table of the
//! crate's unit tests, which observes one call at a time.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod oracle;

use std::time::Duration;

use kaban_core::query_budget::QueryBudget;
use kaban_index_graph::{
    Attr, Base, Caps, Direction, EdgeOp, FactOp, GraphError, Layer, LocalId, NodeOp, OpKind,
    PreparedSediment, Records, RelId, RelSelection, RowCap, View, build_base, expand, seal, wcc,
};
use oracle::view;
use tantivy::directory::OwnedBytes;

/// Neighbours of the hub, all suppressed by something.
const HUB_EDGES: u32 = 2_000;

fn edge(src: u32, dst: u32, kind: OpKind) -> EdgeOp {
    EdgeOp {
        src: LocalId(src),
        rel: RelId(0),
        dst: LocalId(dst),
        kind,
        attr: Attr::default(),
    }
}

fn node(id: u32, kind: OpKind) -> NodeOp {
    NodeOp {
        node: LocalId(id),
        kind,
    }
}

fn is_memory(e: &GraphError) -> bool {
    matches!(
        e,
        GraphError::BudgetExceeded {
            limit: "memory",
            ..
        }
    )
}

#[test]
fn decoding_refuses_a_budget_smaller_than_the_records() {
    let r = Records {
        edges: (0..1000).map(|i| edge(i, i + 1, OpKind::Add)).collect(),
        ..Records::default()
    };
    let blob = r.encode();
    let budget = QueryBudget::new(1000, None);
    let got = Records::decode(&blob, &budget);
    assert!(got.as_ref().is_err_and(is_memory), "{:?}", got.map(|_| ()));
    assert_eq!(
        budget.live_bytes(),
        0,
        "nothing stays charged after a refusal"
    );
}

#[test]
fn decoded_fact_heavy_records_hold_their_charge_until_dropped() {
    let payload = vec![7u8; 1024];
    let r = Records {
        facts: (0..500)
            .map(|i| FactOp {
                node: LocalId(i),
                key: 0,
                kind: OpKind::Add,
                payload: payload.clone(),
            })
            .collect(),
        ..Records::default()
    };
    let blob = r.encode();
    let budget = QueryBudget::new(u64::MAX, None);
    let decoded = Records::decode(&blob, &budget).unwrap();
    assert_eq!(decoded.records(), &r);
    assert!(
        budget.live_bytes() >= 500 * 1024,
        "payload bytes are charged: {}",
        budget.live_bytes()
    );
    drop(decoded);
    assert_eq!(budget.live_bytes(), 0);

    // Too small for the payloads: refused part way, and everything charged is released.
    let small = QueryBudget::new(100 * 1024, None);
    let got = Records::decode(&blob, &small);
    assert!(got.as_ref().is_err_and(is_memory));
    assert_eq!(small.live_bytes(), 0);
}

/// Duplicate-heavy sediment: many records of the same edge, node and fact.
fn duplicates(n: u32) -> Vec<Records> {
    vec![Records {
        edges: (0..n).map(|_| edge(1, 2, OpKind::Add)).collect(),
        nodes: (0..n).map(|_| node(1, OpKind::Add)).collect(),
        facts: (0..n)
            .map(|_| FactOp {
                node: LocalId(1),
                key: 3,
                kind: OpKind::Add,
                payload: b"x".to_vec(),
            })
            .collect(),
    }]
}

#[test]
fn prepared_sediment_holds_its_charge_and_views_borrow_it() {
    let sediment = duplicates(10_000);
    let budget = QueryBudget::new(u64::MAX, None);
    let prepared = PreparedSediment::prepare(&sediment, &budget).unwrap();
    let live = budget.live_bytes();
    // Three input streams of 10,000 records each are copied, at least 16 bytes per record.
    assert!(live >= 3 * 10_000 * 16, "preparation is charged: {live}");
    // Views over the prepared sediment charge nothing for it.
    for _ in 0..3 {
        let v = View::new(None, &[], &prepared, &budget).unwrap();
        assert_eq!(budget.live_bytes(), live);
        assert!(v.contains(LocalId(1)));
    }
    let extended = prepared.extend(&duplicates(10), &budget).unwrap();
    assert!(budget.live_bytes() > live, "the extension is charged");
    drop(prepared);
    drop(extended);
    assert_eq!(budget.live_bytes(), 0);

    let small = QueryBudget::new(10_000 * 8, None);
    let got = PreparedSediment::prepare(&sediment, &small);
    assert!(got.as_ref().is_err_and(is_memory), "{:?}", got.map(|_| ()));
    assert_eq!(small.live_bytes(), 0);
    let prepared = PreparedSediment::prepare(&duplicates(10), &budget).unwrap();
    let got = prepared.extend(&sediment, &small);
    assert!(
        got.as_ref().is_err_and(is_memory),
        "extend: {:?}",
        got.map(|_| ())
    );
    assert_eq!(
        small.live_bytes(),
        0,
        "a refused extension releases everything"
    );
}

fn chain() -> (Base, Vec<Layer>) {
    let base = Base::open(
        OwnedBytes::new(build_base(
            &Records {
                nodes: (0..10).map(|n| node(n, OpKind::Add)).collect(),
                edges: (1..10).map(|n| edge(0, n, OpKind::Add)).collect(),
                ..Records::default()
            },
            10,
        )),
        &oracle::budget(),
    )
    .unwrap();
    let mut layers = Vec::new();
    for i in 0..3 {
        let r = Records {
            edges: vec![edge(0, i, OpKind::Remove)],
            ..Records::default()
        };
        let bytes = seal(&view::<Records>(Some(&base), &layers, &[]), &r);
        layers.push(Layer::open(OwnedBytes::new(bytes), &oracle::budget()).unwrap());
    }
    (base, layers)
}

#[test]
fn simultaneous_iterators_hold_their_cursor_charges() {
    let (base, layers) = chain();
    let v = view::<Records>(Some(&base), &layers, &[]);
    let budget = QueryBudget::new(u64::MAX, None);
    let all = RelSelection::All;
    let mut a = v.neighbours(LocalId(0), &all, Direction::Both, &budget);
    let one = budget.live_bytes();
    assert!(one > 0, "the neighbour cursors are charged");
    let mut b = v.nodes(&budget);
    let two = budget.live_bytes();
    assert!(two > one, "the presence cursors are charged too");
    assert!(a.next().unwrap().is_ok());
    assert!(b.next().unwrap().is_ok());
    drop(a);
    assert_eq!(budget.live_bytes(), two - one);
    drop(b);
    assert_eq!(budget.live_bytes(), 0);

    let none = QueryBudget::new(0, None);
    let first = v.neighbours(LocalId(0), &all, Direction::Out, &none).next();
    assert!(first.is_some_and(|r| r.is_err_and(|e| is_memory(&e))));
    let first = v.nodes(&none).next();
    assert!(first.is_some_and(|r| r.is_err_and(|e| is_memory(&e))));
}

/// A present hub (node 0) with `HUB_EDGES` outgoing edges that all filter out: the base holds
/// them with absent endpoints; a layer re-adds the endpoints but tombstones every edge.
fn hub() -> (Base, Vec<Layer>) {
    let base = Base::open(
        OwnedBytes::new(build_base(
            &Records {
                nodes: vec![node(0, OpKind::Add)],
                edges: (1..=HUB_EDGES).map(|n| edge(0, n, OpKind::Add)).collect(),
                ..Records::default()
            },
            0,
        )),
        &oracle::budget(),
    )
    .unwrap();
    let tombstones = Records {
        nodes: (1..=HUB_EDGES).map(|n| node(n, OpKind::Add)).collect(),
        edges: (1..=HUB_EDGES)
            .map(|n| edge(0, n, OpKind::Remove))
            .collect(),
        ..Records::default()
    };
    let layer = seal(&view::<Records>(Some(&base), &[], &[]), &tombstones);
    (
        base,
        vec![Layer::open(OwnedBytes::new(layer), &oracle::budget()).unwrap()],
    )
}

fn drain<T>(it: impl Iterator<Item = Result<T, GraphError>>) -> (usize, Option<GraphError>) {
    let mut rows = 0;
    for item in it {
        match item {
            Ok(_) => rows += 1,
            Err(e) => return (rows, Some(e)),
        }
    }
    (rows, None)
}

#[test]
fn a_cancelled_or_expired_budget_stops_before_the_first_raw_read() {
    let (base, layers) = hub();
    let v = view::<Records>(Some(&base), &layers, &[]);
    let cancelled = QueryBudget::new(u64::MAX, None);
    cancelled.cancel();
    let expired = QueryBudget::new(u64::MAX, Some(Duration::ZERO));
    std::thread::sleep(Duration::from_millis(2));
    for (budget, limit) in [(&cancelled, "cancelled"), (&expired, "time")] {
        let (rows, err) =
            drain(v.neighbours(LocalId(0), &RelSelection::All, Direction::Out, budget));
        assert_eq!(rows, 0);
        assert!(
            matches!(err, Some(GraphError::BudgetExceeded { limit: l, .. }) if l == limit),
            "{err:?}"
        );
        let (rows, err) = drain(v.nodes(budget));
        assert_eq!(rows, 0);
        assert!(err.is_some());
    }
}

// ---- Structural functions (Step 3) ---------------------------------------------------------

const CAPS: Caps = Caps {
    depth: 4,
    rows: 100_000,
};

/// A present chain 0 > 1 > ... > n with a fan of `n` extra edges from 0.
fn wide(n: u32) -> Vec<Records> {
    vec![Records {
        nodes: (0..=2 * n).map(|m| node(m, OpKind::Add)).collect(),
        edges: (1..=n)
            .map(|m| edge(m - 1, m, OpKind::Add))
            .chain((n + 1..=2 * n).map(|m| edge(0, m, OpKind::Add)))
            .collect(),
        ..Records::default()
    }]
}

#[test]
fn a_memory_limit_below_the_working_set_refuses_and_releases() {
    let rs = wide(5_000);
    let v = view(None, &[], &rs);
    let all = RelSelection::All;
    let free = QueryBudget::new(u64::MAX, None);
    let full = expand(&v, &[LocalId(0)], &all, Direction::Out, CAPS, &free).unwrap();
    assert!(full.nodes.len() > 5_000);
    drop(full);
    let small = QueryBudget::new(16 * 1024, None);
    let got = expand(&v, &[LocalId(0)], &all, Direction::Out, CAPS, &small);
    assert!(got.as_ref().is_err_and(is_memory), "{:?}", got.map(|_| ()));
    assert_eq!(
        small.live_bytes(),
        0,
        "nothing stays charged after a refusal"
    );
    let got = wcc(&v, &all, RowCap(100_000), &small);
    assert!(got.as_ref().is_err_and(is_memory), "{:?}", got.map(|_| ()));
    assert_eq!(small.live_bytes(), 0);
}

#[test]
fn a_recorded_memory_breach_stops_a_later_structural_call() {
    let rs = wide(10);
    let v = view(None, &[], &rs);
    let budget = QueryBudget::new(1024, None);
    assert!(budget.reserve(4096).is_err(), "records a memory breach");
    let got = expand(
        &v,
        &[LocalId(0)],
        &RelSelection::All,
        Direction::Out,
        CAPS,
        &budget,
    );
    assert!(got.as_ref().is_err_and(is_memory), "{:?}", got.map(|_| ()));
    assert_eq!(budget.live_bytes(), 0);
}

#[test]
fn an_expired_budget_stops_a_traversal_before_it_reads() {
    let rs = wide(100);
    let v = view(None, &[], &rs);
    let expired = QueryBudget::new(u64::MAX, Some(Duration::ZERO));
    std::thread::sleep(Duration::from_millis(2));
    let got = expand(
        &v,
        &[LocalId(0)],
        &RelSelection::All,
        Direction::Out,
        CAPS,
        &expired,
    );
    assert!(matches!(
        got,
        Err(GraphError::BudgetExceeded { limit: "time", .. })
    ));
}

#[test]
fn a_recorded_memory_breach_stops_decoding_opening_and_view_preparation() {
    let blob = Records::default().encode();
    let base = build_base(&Records::default(), 0);
    let layer = seal(&view::<Records>(None, &[], &[]), &Records::default());
    let budget = QueryBudget::new(1024, None);
    assert!(budget.reserve(4096).is_err(), "records a memory breach");
    let got = Records::decode(&blob, &budget);
    assert!(
        got.as_ref().is_err_and(is_memory),
        "decode: {:?}",
        got.map(|_| ())
    );
    let empty = PreparedSediment::empty();
    let got = View::new(None, &[], &empty, &budget);
    assert!(
        got.as_ref().is_err_and(is_memory),
        "view: {:?}",
        got.map(|_| ())
    );
    let got = Base::open(OwnedBytes::new(base), &budget);
    assert!(
        got.as_ref().is_err_and(is_memory),
        "base: {:?}",
        got.map(|_| ())
    );
    let got = PreparedSediment::prepare::<Records>(&[], &budget);
    assert!(
        got.as_ref().is_err_and(is_memory),
        "prepare: {:?}",
        got.map(|_| ())
    );
    let got = PreparedSediment::empty().extend::<Records>(&[], &budget);
    assert!(
        got.as_ref().is_err_and(is_memory),
        "extend: {:?}",
        got.map(|_| ())
    );
    let got = Layer::open(OwnedBytes::new(layer), &budget);
    assert!(
        got.as_ref().is_err_and(is_memory),
        "layer: {:?}",
        got.map(|_| ())
    );
    assert_eq!(budget.live_bytes(), 0);
}
