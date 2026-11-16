//! Query budgets reach the graph engine (T9 midpoint MAJ-02, MAJ-03; PC-17): decoding and
//! sediment preparation reserve before allocating and hold their charge while the data lives;
//! accessor iterators charge their cursors while alive; a cancelled or expired budget stops a
//! filtered scan within [`BUDGET_CHECK_INTERVAL`] raw reads, even when no row is produced.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod oracle;

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use kaban_core::query_budget::QueryBudget;
use kaban_index_graph::{
    Attr, BUDGET_CHECK_INTERVAL, Base, Direction, EdgeOp, FactOp, GraphError, Layer, LocalId,
    NodeOp, OpKind, Records, RelId, RelSelection, View, build_base, seal,
};
use oracle::view;
use tantivy::directory::OwnedBytes;

/// Neighbours of the hub, all suppressed by something.
const HUB_EDGES: u32 = 20_000;

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
fn view_preparation_charges_its_sediment_while_the_view_lives() {
    let sediment = duplicates(10_000);
    let budget = QueryBudget::new(u64::MAX, None);
    let v = View::new(None, &[], &sediment, &budget).unwrap();
    let live = budget.live_bytes();
    // Three input streams of 10,000 records each are copied, at least 16 bytes per record.
    assert!(live >= 3 * 10_000 * 16, "preparation is charged: {live}");
    assert!(v.contains(LocalId(1)));
    drop(v);
    assert_eq!(budget.live_bytes(), 0);

    let small = QueryBudget::new(10_000 * 8, None);
    let got = View::new(None, &[], &sediment, &small);
    assert!(got.as_ref().is_err_and(is_memory), "{:?}", got.map(|_| ()));
    assert_eq!(small.live_bytes(), 0);
}

fn chain() -> (Base, Vec<Layer>) {
    let base = Base::open(OwnedBytes::new(build_base(
        &Records {
            nodes: (0..10).map(|n| node(n, OpKind::Add)).collect(),
            edges: (1..10).map(|n| edge(0, n, OpKind::Add)).collect(),
            ..Records::default()
        },
        10,
    )))
    .unwrap();
    let mut layers = Vec::new();
    for i in 0..3 {
        let r = Records {
            edges: vec![edge(0, i, OpKind::Remove)],
            ..Records::default()
        };
        let bytes = seal(&view::<Records>(Some(&base), &layers, &[]), &r);
        layers.push(Layer::open(OwnedBytes::new(bytes)).unwrap());
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
    let base = Base::open(OwnedBytes::new(build_base(
        &Records {
            nodes: vec![node(0, OpKind::Add)],
            edges: (1..=HUB_EDGES).map(|n| edge(0, n, OpKind::Add)).collect(),
            ..Records::default()
        },
        0,
    )))
    .unwrap();
    let tombstones = Records {
        nodes: (1..=HUB_EDGES).map(|n| node(n, OpKind::Add)).collect(),
        edges: (1..=HUB_EDGES)
            .map(|n| edge(0, n, OpKind::Remove))
            .collect(),
        ..Records::default()
    };
    let layer = seal(&view::<Records>(Some(&base), &[], &[]), &tombstones);
    (base, vec![Layer::open(OwnedBytes::new(layer)).unwrap()])
}

/// Scans before [`cancel_mid_scan`] gives up: an un-cancelled scan of the hub finishes in
/// milliseconds, so this bounds the wait to well under a few seconds.
const MAX_SCANS: usize = 200;
/// Wall-clock bound of [`cancel_mid_scan`].
const MAX_WAIT: Duration = Duration::from_secs(5);

/// Run `scan` repeatedly while another thread cancels the budget once the view has done `start`
/// raw reads; return (raw reads seen right after cancelling, raw reads at the end, rows
/// yielded, the error). Bounded: after [`MAX_SCANS`] scans or [`MAX_WAIT`] without an error the
/// canceller is released and the test fails instead of hanging.
fn cancel_mid_scan(
    v: &View<'_>,
    start: u64,
    scan: impl Fn(&QueryBudget) -> (usize, Option<GraphError>),
) -> (u64, u64, usize, GraphError) {
    let budget = QueryBudget::new(u64::MAX, None);
    let done = AtomicBool::new(false);
    let began = Instant::now();
    std::thread::scope(|s| {
        let canceller = s.spawn(|| {
            while v.raw_reads() < start && !done.load(Ordering::SeqCst) {
                if began.elapsed() > MAX_WAIT {
                    return None;
                }
                std::hint::spin_loop();
            }
            budget.cancel();
            Some(v.raw_reads())
        });
        let mut rows = 0;
        let mut err = None;
        for _ in 0..MAX_SCANS {
            let (n, e) = scan(&budget);
            rows += n;
            if e.is_some() || began.elapsed() > MAX_WAIT {
                err = e;
                break;
            }
        }
        done.store(true, Ordering::SeqCst);
        let seen = canceller.join().unwrap();
        let Some(err) = err else {
            panic!("the scan never stopped on cancellation (seen {seen:?}, {rows} rows)");
        };
        let seen = seen.expect("cancelled");
        (seen, v.raw_reads(), rows, err)
    })
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
        let before = v.raw_reads();
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
        assert_eq!(v.raw_reads(), before, "no raw read before the first check");
    }
}

#[test]
fn cancellation_stops_an_all_filtered_neighbour_scan_within_the_interval() {
    let (base, layers) = hub();
    let v = view::<Records>(Some(&base), &layers, &[]);
    let selections = [
        ("tombstones", RelSelection::All),
        ("unselected relation", RelSelection::only(vec![RelId(9)])),
    ];
    for (name, rels) in &selections {
        // Sanity: a full scan reads every entry and yields nothing.
        let free = QueryBudget::new(u64::MAX, None);
        let before = v.raw_reads();
        let (rows, err) = drain(v.neighbours(LocalId(0), rels, Direction::Out, &free));
        assert_eq!((rows, err), (0, None), "{name}");
        assert!(
            v.raw_reads() - before >= u64::from(HUB_EDGES),
            "{name}: every entry is a raw read"
        );

        let start = v.raw_reads() + 3 * BUDGET_CHECK_INTERVAL;
        let (seen, end, rows, err) = cancel_mid_scan(&v, start, |b| {
            drain(v.neighbours(LocalId(0), rels, Direction::Out, b))
        });
        assert_eq!(rows, 0, "{name}");
        assert!(
            matches!(
                err,
                GraphError::BudgetExceeded {
                    limit: "cancelled",
                    ..
                }
            ),
            "{name}: {err:?}"
        );
        assert!(
            end <= seen + BUDGET_CHECK_INTERVAL,
            "{name}: stopped {} raw reads after the cancellation was seen",
            end - seen
        );
    }
    // An empty selection reads nothing at all.
    let before = v.raw_reads();
    let empty = RelSelection::only(Vec::new());
    let free = QueryBudget::new(u64::MAX, None);
    assert_eq!(
        drain(v.neighbours(LocalId(0), &empty, Direction::Both, &free)),
        (0, None)
    );
    assert_eq!(v.raw_reads(), before);
}

#[test]
fn cancellation_stops_a_scan_over_removed_endpoints_within_the_interval() {
    // The hub's edges survive, but every endpoint is absent (never added).
    let base = Base::open(OwnedBytes::new(build_base(
        &Records {
            nodes: vec![node(0, OpKind::Add)],
            edges: (1..=HUB_EDGES).map(|n| edge(0, n, OpKind::Add)).collect(),
            ..Records::default()
        },
        0,
    )))
    .unwrap();
    let v = view::<Records>(Some(&base), &[], &[]);
    let start = 3 * BUDGET_CHECK_INTERVAL;
    let (seen, end, rows, _) = cancel_mid_scan(&v, start, |b| {
        drain(v.neighbours(LocalId(0), &RelSelection::All, Direction::Both, b))
    });
    assert_eq!(rows, 0);
    assert!(end <= seen + BUDGET_CHECK_INTERVAL, "{} past", end - seen);
}

#[test]
fn cancellation_stops_a_scan_over_removed_nodes_within_the_interval() {
    let (base, layers) = hub();
    // Remove every node in a newer layer: presence is tombstone-heavy.
    let all = Records {
        nodes: (0..=HUB_EDGES).map(|n| node(n, OpKind::Remove)).collect(),
        ..Records::default()
    };
    let layer = seal(&view::<Records>(Some(&base), &layers, &[]), &all);
    let mut layers = layers;
    layers.push(Layer::open(OwnedBytes::new(layer)).unwrap());
    let v = view::<Records>(Some(&base), &layers, &[]);
    let free = QueryBudget::new(u64::MAX, None);
    assert_eq!(drain(v.nodes(&free)), (0, None));
    let start = v.raw_reads() + 3 * BUDGET_CHECK_INTERVAL;
    let (seen, end, rows, _) = cancel_mid_scan(&v, start, |b| drain(v.nodes(b)));
    assert_eq!(rows, 0);
    assert!(end <= seen + BUDGET_CHECK_INTERVAL, "{} past", end - seen);
}
