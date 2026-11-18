//! Step 2 (T10): every scored and set function returns bit-identical output across 20 runs and
//! when run on 1 and on 4 threads at once (GRA-07). The crate has no thread setting of its own:
//! every function is sequential with a fixed reduction order, so the threads are concurrent
//! callers over one shared view. Ties break by the public stable order before the cap cuts:
//! equal-score nodes in reverse local-id order are cut by the cap and equal-cost paths are
//! chosen by that order, and the selection equals a `NodeId` oracle after the records are
//! copied into a project with different rids, sediment nodes included.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod oracle;
mod reference;

use std::collections::BTreeMap;

use kaban_index_graph::{
    Attr, Caps, EdgeOp, LocalId, NodeOp, OpKind, RankParams, Records, RelId, RelSelection, RowCap,
    View, adamic_adar, clustering, jaccard, pagerank, ppr, time_window_expand, triangle_count,
    weighted_shortest_path,
};
use oracle::budget;
use reference::{Fixture, IDS, Order, scored};

const RUNS: usize = 20;
const THREADS: usize = 4;

/// Every output of every function over `v`, as exact bits.
fn outputs(v: &View<'_>, order: &Order) -> Vec<u64> {
    let b = budget();
    let rels = RelSelection::All;
    let p = RankParams::default();
    let caps = Caps {
        depth: 4,
        rows: 1_500,
    };
    let mut out = Vec::new();
    let mut ranked = |rows: &[(LocalId, f64)]| {
        for &(n, s) in rows {
            out.push(u64::from(n.0));
            out.push(s.to_bits());
        }
    };
    ranked(&pagerank(v, &rels, p, order, RowCap(1_500), &b).unwrap().rows);
    ranked(&ppr(v, &[LocalId(1), LocalId(2)], &rels, p, order, RowCap(1_500), &b).unwrap().rows);
    for a in (0..IDS).step_by(7) {
        let (x, y) = (LocalId(a), LocalId((a * 13 + 5) % IDS));
        out.push(jaccard(v, x, y, &rels, &b).unwrap().to_bits());
        out.push(adamic_adar(v, x, y, &rels, &b).unwrap().to_bits());
        out.push(clustering(v, x, &rels, &b).unwrap().to_bits());
        if let Some(path) = weighted_shortest_path(v, x, y, &rels, caps, order, &b).unwrap() {
            out.push(path.0.cast_unsigned());
            out.extend(path.1.iter().map(|n| u64::from(n.0)));
        }
        let reached = time_window_expand(v, &[x], &rels, (0, 600_000), caps, &b).unwrap();
        out.extend(reached.nodes.iter().map(|&(n, d)| u64::from(n.0) << 8 | u64::from(d)));
        out.push(u64::MAX);
    }
    out.push(triangle_count(v, &rels, RowCap(1_500), &b).unwrap());
    out
}

#[test]
fn outputs_are_bit_identical_across_runs_and_threads() {
    let f = scored(7);
    let v = f.view();
    let order = Order::reversed(IDS);
    let first = outputs(&v, &order);
    assert!(first.len() > 500, "the outputs cover the fixture");
    for run in 1..RUNS {
        assert_eq!(outputs(&v, &order), first, "run {run}");
    }
    let threaded: Vec<Vec<u64>> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..THREADS)
            .map(|_| s.spawn(|| outputs(&v, &order)))
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    for (t, got) in threaded.iter().enumerate() {
        assert_eq!(got, &first, "thread {t} of {THREADS}");
    }
}

fn add(n: u32) -> NodeOp {
    NodeOp {
        node: LocalId(n),
        kind: OpKind::Add,
    }
}

fn link(src: u32, rel: u16, dst: u32, weight: i64) -> EdgeOp {
    EdgeOp {
        src: LocalId(src),
        rel: RelId(rel),
        dst: LocalId(dst),
        kind: OpKind::Add,
        attr: Attr { weight, ts: 0 },
    }
}

/// Along relation 0 hub 0 links to leaves 1..=8 (leaves 7 and 8 and their links only in the
/// sediment), so the leaves tie exactly; along relation 1 source 10 reaches 14 through 11, 12
/// and 13 at equal cost.
fn ties() -> Fixture {
    let base = Records {
        nodes: [0, 1, 2, 3, 4, 5, 6, 10, 11, 12, 13, 14].map(add).to_vec(),
        edges: vec![
            link(0, 0, 1, 1),
            link(0, 0, 2, 1),
            link(0, 0, 3, 1),
            link(10, 1, 11, 2),
            link(11, 1, 14, 3),
        ],
        ..Records::default()
    };
    let layer = Records {
        edges: vec![
            link(0, 0, 4, 1),
            link(0, 0, 5, 1),
            link(10, 1, 12, 2),
            link(12, 1, 14, 3),
        ],
        ..Records::default()
    };
    let layer2 = Records {
        edges: vec![link(0, 0, 6, 1)],
        ..Records::default()
    };
    let sediment = Records {
        nodes: vec![add(7), add(8)],
        edges: vec![
            link(0, 0, 7, 1),
            link(0, 0, 8, 1),
            link(10, 1, 13, 2),
            link(13, 1, 14, 3),
        ],
        ..Records::default()
    };
    Fixture::build(vec![base, layer, layer2, sediment])
}

/// The selections the tie fixture's functions make, as `NodeId` names (`name` maps a local id
/// to the public id it stands for).
fn selection(f: &Fixture, order: &Order, name: &dyn Fn(u32) -> u32) -> Vec<u32> {
    let v = f.view();
    let b = budget();
    let rels = RelSelection::only(vec![RelId(0)]);
    let p = RankParams::default();
    let mut out = Vec::new();
    let top = pagerank(&v, &rels, p, order, RowCap(3), &b).unwrap();
    assert!(top.truncated);
    out.extend(top.rows.iter().map(|r| name(r.0.0)));
    let local = |id: u32| f.oracle.nodes().iter().map(|n| n.0).find(|&n| name(n) == id).unwrap();
    let top = ppr(&v, &[LocalId(local(0))], &rels, p, order, RowCap(3), &b).unwrap();
    out.extend(top.rows.iter().map(|r| name(r.0.0)));
    let (src, dst) = (local(10), local(14));
    let rels = RelSelection::All;
    let caps = Caps {
        depth: 4,
        rows: 100,
    };
    let path = weighted_shortest_path(&v, LocalId(src), LocalId(dst), &rels, caps, order, &b)
        .unwrap()
        .unwrap();
    assert_eq!(path.0, 5);
    out.extend(path.1.iter().map(|n| name(n.0)));
    out
}

#[test]
fn ties_break_by_the_stable_order_before_the_cap_and_survive_new_rids() {
    // NodeId order is the reverse of local-id order here: NodeId rank of node i is 100 - i.
    let f = ties();
    let order = Order((0..20).map(|i| (i, 100 - i)).collect());
    let got = selection(&f, &order, &|n| n);
    // PageRank: the three lowest NodeId ranks among the eight tied leaves (8, 7, 6; two are
    // sediment nodes). PPR from the hub: the hub, then two of the tied leaves. Then the path
    // through the lowest-ranked of the equal-cost middle nodes.
    assert_eq!(got, vec![8, 7, 6, 0, 8, 7, 10, 13, 14]);

    // The same records in another project: rid i becomes 50 + (i * 7) % 20, so local-id order
    // differs; the stable order still ranks each node by its NodeId.
    let rid = |i: u32| 50 + (i * 7) % 20;
    let copy = f.renamed(rid);
    let back: BTreeMap<u32, u32> = (0..20).map(|i| (rid(i), i)).collect();
    let copied_order = Order((0..20).map(|i| (rid(i), 100 - i)).collect());
    let again = selection(&copy, &copied_order, &|n| back[&n]);
    assert_eq!(again, got, "the selection follows NodeIds, not rids");
}
