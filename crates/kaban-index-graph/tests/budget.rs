//! Query budgets reach the graph engine (T9 midpoint MAJ-02, MAJ-03; PC-17): decoding and
//! sediment preparation reserve before allocating and hold their charge while the data lives;
//! accessor iterators charge their cursors while alive; a cancelled or expired budget stops a
//! filtered scan within [`BUDGET_CHECK_INTERVAL`] raw reads, even when no row is produced. The
//! structural functions share that budget for their working sets and results (Step 3).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod oracle;

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use kaban_core::query_budget::QueryBudget;
use kaban_index_graph::{
    Attr, BUDGET_CHECK_INTERVAL, Base, Caps, Direction, EdgeOp, FactOp, GraphError, Layer, LocalId,
    NodeOp, OpKind, Records, RelId, RelSelection, RowCap, View, build_base, expand, seal,
    shortest_path, topo_order, wcc, work_units,
};
use oracle::{serial, view};
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
    let _serial = serial();
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
    let _serial = serial();
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
    let _serial = serial();
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
    let _serial = serial();
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

/// Scans before [`cancel_mid_scan`] gives up: an un-cancelled scan of the hub finishes in
/// milliseconds, so this bounds the wait to well under a few seconds.
const MAX_SCANS: usize = 200;
/// Wall-clock bound of [`cancel_mid_scan`].
const MAX_WAIT: Duration = Duration::from_secs(5);

/// Run `scan` repeatedly while another thread cancels the budget once [`work_units`] reaches
/// `start`; return (units seen right after cancelling, units at the end, rows yielded, the
/// error). Bounded: after [`MAX_SCANS`] scans or [`MAX_WAIT`] without an error the
/// canceller is released and the test fails instead of hanging.
fn cancel_mid_scan(
    start: u64,
    scan: impl Fn(&QueryBudget) -> (usize, Option<GraphError>),
) -> (u64, u64, usize, GraphError) {
    let budget = QueryBudget::new(u64::MAX, None);
    let done = AtomicBool::new(false);
    let began = Instant::now();
    std::thread::scope(|s| {
        let canceller = s.spawn(|| {
            while work_units() < start && !done.load(Ordering::SeqCst) {
                if began.elapsed() > MAX_WAIT {
                    return None;
                }
                std::hint::spin_loop();
            }
            budget.cancel();
            Some(work_units())
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
        assert_eq!(
            budget.live_bytes(),
            0,
            "every lease is released on the failure path"
        );
        let Some(err) = err else {
            panic!("the scan never stopped on cancellation (seen {seen:?}, {rows} rows)");
        };
        let seen = seen.expect("cancelled");
        (seen, work_units(), rows, err)
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
    let _serial = serial();
    let (base, layers) = hub();
    let v = view::<Records>(Some(&base), &layers, &[]);
    let cancelled = QueryBudget::new(u64::MAX, None);
    cancelled.cancel();
    let expired = QueryBudget::new(u64::MAX, Some(Duration::ZERO));
    std::thread::sleep(Duration::from_millis(2));
    for (budget, limit) in [(&cancelled, "cancelled"), (&expired, "time")] {
        let before = work_units();
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
        assert_eq!(work_units(), before, "no raw read before the first check");
    }
}

#[test]
fn cancellation_stops_an_all_filtered_neighbour_scan_within_the_interval() {
    let _serial = serial();
    let (base, layers) = hub();
    let v = view::<Records>(Some(&base), &layers, &[]);
    let selections = [
        ("tombstones", RelSelection::All),
        ("unselected relation", RelSelection::only(vec![RelId(9)])),
    ];
    for (name, rels) in &selections {
        // Sanity: a full scan reads every entry and yields nothing.
        let free = QueryBudget::new(u64::MAX, None);
        let before = work_units();
        let (rows, err) = drain(v.neighbours(LocalId(0), rels, Direction::Out, &free));
        assert_eq!((rows, err), (0, None), "{name}");
        assert!(
            work_units() - before >= u64::from(HUB_EDGES),
            "{name}: every entry is a raw read"
        );

        let start = work_units() + 3 * BUDGET_CHECK_INTERVAL;
        let (seen, end, rows, err) = cancel_mid_scan(start, |b| {
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
    let before = work_units();
    let empty = RelSelection::only(Vec::new());
    let free = QueryBudget::new(u64::MAX, None);
    assert_eq!(
        drain(v.neighbours(LocalId(0), &empty, Direction::Both, &free)),
        (0, None)
    );
    assert_eq!(work_units(), before);
}

#[test]
fn cancellation_stops_a_scan_over_removed_endpoints_within_the_interval() {
    let _serial = serial();
    // The hub's edges survive, but every endpoint is absent (never added).
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
    let v = view::<Records>(Some(&base), &[], &[]);
    let start = 3 * BUDGET_CHECK_INTERVAL;
    let (seen, end, rows, _) = cancel_mid_scan(start, |b| {
        drain(v.neighbours(LocalId(0), &RelSelection::All, Direction::Both, b))
    });
    assert_eq!(rows, 0);
    assert!(end <= seen + BUDGET_CHECK_INTERVAL, "{} past", end - seen);
}

#[test]
fn cancellation_stops_a_scan_over_removed_nodes_within_the_interval() {
    let _serial = serial();
    let (base, layers) = hub();
    // Remove every node in a newer layer: presence is tombstone-heavy.
    let all = Records {
        nodes: (0..=HUB_EDGES).map(|n| node(n, OpKind::Remove)).collect(),
        ..Records::default()
    };
    let layer = seal(&view::<Records>(Some(&base), &layers, &[]), &all);
    let mut layers = layers;
    layers.push(Layer::open(OwnedBytes::new(layer), &oracle::budget()).unwrap());
    let v = view::<Records>(Some(&base), &layers, &[]);
    let free = QueryBudget::new(u64::MAX, None);
    assert_eq!(drain(v.nodes(&free)), (0, None));
    let start = work_units() + 3 * BUDGET_CHECK_INTERVAL;
    let (seen, end, rows, _) = cancel_mid_scan(start, |b| drain(v.nodes(b)));
    assert_eq!(rows, 0);
    assert!(end <= seen + BUDGET_CHECK_INTERVAL, "{} past", end - seen);
}

/// END-MAJ-02: a dense presence index whose population sits in a high-id cluster leaves a long
/// run of empty bitvector words before the first entry. Each word visited is a unit of work.
#[test]
fn cancellation_stops_a_presence_scan_inside_a_run_of_empty_words() {
    const DOMAIN: u32 = 1 << 21;
    const CLUSTER: u32 = 80_000; // 80,000 * 30 > 2^21: the presence index is a bitvector
    let _serial = serial();
    let base = Base::open(
        OwnedBytes::new(build_base(
            &Records {
                nodes: (DOMAIN - CLUSTER..DOMAIN)
                    .map(|n| node(n, OpKind::Add))
                    .collect(),
                ..Records::default()
            },
            DOMAIN,
        )),
        &oracle::budget(),
    )
    .unwrap();
    let v = view::<Records>(Some(&base), &[], &[]);
    let free = QueryBudget::new(u64::MAX, None);
    let before = work_units();
    assert_eq!(drain(v.nodes(&free)), (CLUSTER as usize, None));
    let empty_words = u64::from((DOMAIN - CLUSTER) / 64);
    assert!(
        work_units() - before >= empty_words,
        "every bitvector word visited is metered: {} units",
        work_units() - before
    );

    let start = work_units() + 3 * BUDGET_CHECK_INTERVAL;
    let (seen, end, _, err) = cancel_mid_scan(start, |b| drain(v.nodes(b)));
    assert!(
        matches!(
            err,
            GraphError::BudgetExceeded {
                limit: "cancelled",
                ..
            }
        ),
        "{err:?}"
    );
    assert!(
        end <= seen + BUDGET_CHECK_INTERVAL,
        "stopped {} units after the cancellation was seen",
        end - seen
    );
}

/// END-MAJ-02: one merged key of a list over many overlapping sources compares and advances
/// every source; those are units of work, not one raw read.
#[test]
fn cancellation_stops_a_merge_over_many_overlapping_sources() {
    const LAYERS: usize = 300;
    const KEYS: u32 = 1_000;
    let _serial = serial();
    let base = Base::open(
        OwnedBytes::new(build_base(
            &Records {
                nodes: (0..=KEYS).map(|n| node(n, OpKind::Add)).collect(),
                ..Records::default()
            },
            0,
        )),
        &oracle::budget(),
    )
    .unwrap();
    // Every layer tombstones the same edges of node 0: each merged key is in every layer.
    let tombstones = Records {
        edges: (1..=KEYS).map(|n| edge(0, n, OpKind::Remove)).collect(),
        ..Records::default()
    };
    let mut layers: Vec<Layer> = Vec::new();
    for _ in 0..LAYERS {
        let bytes = seal(&view::<Records>(Some(&base), &layers, &[]), &tombstones);
        layers.push(Layer::open(OwnedBytes::new(bytes), &oracle::budget()).unwrap());
    }
    let v = view::<Records>(Some(&base), &layers, &[]);
    let all = RelSelection::All;
    let free = QueryBudget::new(u64::MAX, None);
    let before = work_units();
    assert_eq!(
        drain(v.neighbours(LocalId(0), &all, Direction::Out, &free)),
        (0, None)
    );
    assert!(
        work_units() - before >= u64::from(KEYS) * LAYERS as u64,
        "every source advance is metered: {} units",
        work_units() - before
    );

    let start = work_units() + 3 * BUDGET_CHECK_INTERVAL;
    let (seen, end, rows, err) = cancel_mid_scan(start, |b| {
        drain(v.neighbours(LocalId(0), &all, Direction::Out, b))
    });
    assert_eq!(rows, 0);
    assert!(
        matches!(
            err,
            GraphError::BudgetExceeded {
                limit: "cancelled",
                ..
            }
        ),
        "{err:?}"
    );
    assert!(
        end <= seen + BUDGET_CHECK_INTERVAL,
        "stopped {} units after the cancellation was seen",
        end - seen
    );
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
    let _serial = serial();
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
    let _serial = serial();
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
fn cancellation_stops_a_low_output_traversal_within_the_interval() {
    let _serial = serial();
    // The hub's 20,000 edges all filter out: expand and wcc read them all and produce nothing.
    let (base, layers) = hub();
    let v = view::<Records>(Some(&base), &layers, &[]);
    let all = RelSelection::All;
    let free = QueryBudget::new(u64::MAX, None);
    let none = expand(&v, &[LocalId(0)], &all, Direction::Out, CAPS, &free).unwrap();
    assert_eq!(none.nodes.len(), 0);
    drop(none);
    let start = work_units() + 3 * BUDGET_CHECK_INTERVAL;
    let (seen, end, rows, err) = cancel_mid_scan(start, |b| {
        match expand(&v, &[LocalId(0)], &all, Direction::Out, CAPS, b) {
            Ok(r) => (r.nodes.len(), None),
            Err(e) => (0, Some(e)),
        }
    });
    assert_eq!(rows, 0);
    assert!(
        matches!(
            err,
            GraphError::BudgetExceeded {
                limit: "cancelled",
                ..
            }
        ),
        "{err:?}"
    );
    // Two metered loops are live (the search's and the accessor's): each sees the
    // cancellation within the interval.
    assert!(
        end <= seen + 2 * BUDGET_CHECK_INTERVAL,
        "expand: {} past",
        end - seen
    );

    let start = work_units() + 3 * BUDGET_CHECK_INTERVAL;
    let (seen, end, _, err) = cancel_mid_scan(start, |b| match wcc(&v, &all, RowCap(10), b) {
        Ok(r) => (r.len(), None),
        Err(e) => (0, Some(e)),
    });
    assert!(
        matches!(
            err,
            GraphError::BudgetExceeded {
                limit: "cancelled",
                ..
            }
        ),
        "{err:?}"
    );
    assert!(
        end <= seen + 2 * BUDGET_CHECK_INTERVAL,
        "wcc: {} past",
        end - seen
    );
}

#[test]
fn an_expired_budget_stops_a_traversal_before_it_reads() {
    let _serial = serial();
    let rs = wide(100);
    let v = view(None, &[], &rs);
    let expired = QueryBudget::new(u64::MAX, Some(Duration::ZERO));
    std::thread::sleep(Duration::from_millis(2));
    let before = work_units();
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
    assert_eq!(
        work_units(),
        before,
        "no unit of work before the first check"
    );
}

// ---- Unchecked phases (END-MAJ-03) -------------------------------------------------------

/// Input size of the phase tests: large enough that each phase is many check intervals long.
const PHASE: u32 = 50_000;

fn is_cancelled(e: &GraphError) -> bool {
    matches!(
        e,
        GraphError::BudgetExceeded {
            limit: "cancelled",
            ..
        }
    )
}

/// Run `call` once unbudgeted and return the units it metered, then cancel a second run once
/// `from` units past the start plus half of the rest are done, and require the refusal within
/// `loops` check intervals (one per metered loop live at that point).
fn cancel_in_phase<T>(
    what: &str,
    from: u64,
    loops: u64,
    call: impl Fn(&QueryBudget) -> Result<T, GraphError>,
) -> u64 {
    let free = QueryBudget::new(u64::MAX, None);
    let before = work_units();
    assert!(call(&free).is_ok(), "{what}: the free run succeeds");
    let total = work_units() - before;
    assert!(
        total > from,
        "{what}: {total} units, phase starts at {from}"
    );
    let start = work_units() + from + (total - from) / 2;
    let (seen, end, _, err) = cancel_mid_scan(start, |b| match call(b) {
        Ok(_) => (1, None),
        Err(e) => (0, Some(e)),
    });
    assert!(is_cancelled(&err), "{what}: {err:?}");
    assert!(
        end <= seen + loops * BUDGET_CHECK_INTERVAL,
        "{what}: stopped {} units after the cancellation was seen",
        end - seen
    );
    total
}

#[test]
fn a_recorded_memory_breach_stops_decoding_opening_and_view_preparation() {
    let _serial = serial();
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
    let got = View::new::<Records>(None, &[], &[], &budget);
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
    let got = Layer::open(OwnedBytes::new(layer), &budget);
    assert!(
        got.as_ref().is_err_and(is_memory),
        "layer: {:?}",
        got.map(|_| ())
    );
    assert_eq!(budget.live_bytes(), 0);
}

/// Many edges in descending order, so preparation has to sort them.
fn descending(n: u32) -> Records {
    Records {
        nodes: (0..=n).rev().map(|m| node(m, OpKind::Add)).collect(),
        edges: (1..=n).rev().map(|m| edge(m, m - 1, OpKind::Add)).collect(),
        ..Records::default()
    }
}

#[test]
fn cancellation_stops_decoding_within_the_interval() {
    let _serial = serial();
    let blob = descending(PHASE).encode();
    let total = cancel_in_phase("decode", 0, 1, |b| Records::decode(&blob, b));
    assert!(
        total >= u64::from(2 * PHASE),
        "every record is metered: {total}"
    );
}

#[test]
fn cancellation_stops_view_preparation_and_its_sorts_within_the_interval() {
    let _serial = serial();
    let sediment = vec![descending(PHASE)];
    let total = cancel_in_phase("view", 0, 1, |b| {
        View::new(None, &[], &sediment, b).map(|_| ())
    });
    // Copying, sorting and compacting the node and both edge streams.
    assert!(
        total >= u64::from(6 * PHASE),
        "preparation is metered: {total}"
    );
}

#[test]
fn cancellation_stops_extent_validation_within_the_interval() {
    let _serial = serial();
    let bytes = OwnedBytes::new(build_base(&descending(PHASE), 0));
    let total = cancel_in_phase("open", 0, 1, |b| Base::open(bytes.clone(), b).map(|_| ()));
    // Presence, both edge sections: every id and entry validated.
    assert!(
        total >= u64::from(3 * PHASE),
        "validation is metered: {total}"
    );
}

#[test]
fn cancellation_stops_whole_graph_functions_over_an_empty_relation() {
    let _serial = serial();
    let sediment = vec![descending(PHASE)];
    let v = view(None, &[], &sediment);
    let none = RelSelection::only(Vec::new());
    let free = QueryBudget::new(u64::MAX, None);
    let before = work_units();
    assert_eq!(drain(v.nodes(&free)), (PHASE as usize + 1, None));
    let listing = work_units() - before;
    let total = cancel_in_phase("wcc", listing, 2, |b| wcc(&v, &none, RowCap(10), b));
    // The forest initialisation and the node loop, which reads no edge.
    assert!(
        total - listing >= u64::from(2 * PHASE),
        "wcc's own loops are metered: {}",
        total - listing
    );

    // Sources in descending order: the search sorts them, then in-degrees, heap and release.
    let src: Vec<LocalId> = (0..=PHASE).rev().map(LocalId).collect();
    let caps = Caps {
        depth: 1,
        rows: 10 * PHASE,
    };
    let total = cancel_in_phase("topo_order", 0, 2, |b| topo_order(&v, &src, &none, caps, b));
    assert!(
        total >= u64::from(5 * PHASE),
        "topo_order's loops and sort are metered: {total}"
    );
}

#[test]
fn cancellation_stops_shortest_path_initialisation_within_the_interval() {
    let _serial = serial();
    // 0 fans out to 1..=n, each of which reaches n + 1.
    let n = PHASE;
    let sediment = vec![Records {
        nodes: (0..=n + 1).map(|m| node(m, OpKind::Add)).collect(),
        edges: (1..=n)
            .flat_map(|m| [edge(0, m, OpKind::Add), edge(m, n + 1, OpKind::Add)])
            .collect(),
        ..Records::default()
    }];
    let v = view(None, &[], &sediment);
    let caps = Caps {
        depth: 3,
        rows: 10 * n,
    };
    cancel_in_phase("shortest_path", 0, 2, |b| {
        shortest_path(&v, LocalId(0), LocalId(n + 1), &RelSelection::All, caps, b)
    });
}

/// Nodes 0..=d in a chain whose union-find forest is a path d -> d-1 -> ... -> 0 once the
/// helpers are processed (each helper `h_k` links k-1 then k, in descending k); `extra` adds one
/// last node with an edge to d, whose find walks the whole path.
fn deep_forest(d: u32, extra: bool) -> Vec<Records> {
    let helper = |k: u32| d + 1 + (d - k); // h_d < h_(d-1) < ... < h_1
    let mut r = Records {
        nodes: (0..=d)
            .chain((1..=d).map(helper))
            .map(|m| node(m, OpKind::Add))
            .collect(),
        edges: (1..=d)
            .flat_map(|k| {
                [
                    edge(helper(k), k - 1, OpKind::Add),
                    edge(helper(k), k, OpKind::Add),
                ]
            })
            .collect(),
        ..Records::default()
    };
    if extra {
        let z = 2 * d + 1;
        r.nodes.push(node(z, OpKind::Add));
        r.edges.push(edge(z, d, OpKind::Add));
    }
    vec![r]
}

#[test]
fn cancellation_stops_a_long_union_find_walk_within_the_interval() {
    let _serial = serial();
    let d = PHASE;
    let all = RelSelection::All;
    let free = QueryBudget::new(u64::MAX, None);
    let without = deep_forest(d, false);
    let v = view(None, &[], &without);
    let before = work_units();
    wcc(&v, &all, RowCap(1), &free).unwrap();
    let base = work_units() - before;
    let with = deep_forest(d, true);
    let v = view(None, &[], &with);
    let components = wcc(&v, &all, RowCap(u32::MAX), &free).unwrap();
    assert!(
        components.iter().all(|&(_, c)| c == LocalId(0)),
        "one component"
    );
    drop(components);
    let before = work_units();
    wcc(&v, &all, RowCap(1), &free).unwrap();
    let total = work_units() - before;
    assert!(
        total - base >= u64::from(d / 2),
        "the walk of the last find is metered: {} units",
        total - base
    );
    // Cancel inside the last walk: it is the last quarter of the work.
    cancel_in_phase("wcc walk", total - u64::from(d / 4) * 2, 2, |b| {
        wcc(&v, &all, RowCap(1), b)
    });
}
