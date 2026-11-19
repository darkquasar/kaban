//! The memory graph is derived state (spec 11 T11 Steps 2 and 4; INV-4, KER-08, REC-01): at
//! every stage (sediment, sealed, folded, reopened, rebuilt after `kaban.kbx` is deleted) its
//! nodes, edges, components and ranked membership equal the rows, the rebuild gives identical
//! results and local ids, and every graph extent holds only graph records.
//!
//! Runs with the `failpoints` feature (the graph's test access lives behind it).

#![cfg(feature = "failpoints")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::Duration;

use common::{Scratch, created, ctx, nid};
use kaban_core::config::{CatalogCfg, SealOps};
use kaban_core::query_budget::QueryBudget;
use kaban_core::{
    Actor, ForgetMode, IngestStore, MemoryStore, NewNode, NodeId, Op, WorkspaceId, WriteBatch,
    WriteCtx,
};
use kaban_index_graph::{
    Base, Caps, Direction, Layer, LocalId, PreparedSediment, RankParams, Records, RelSelection,
    RowCap, View, closure, degree, expand, pagerank, shortest_path, wcc, weighted_shortest_path,
};
use kaban_store_sqlite::catalog_testing::{self, MainGraph};
use kaban_store_sqlite::{SqliteStore, WorkKind};
use tantivy::directory::OwnedBytes;

const NODES: usize = 500;
const EDGES: usize = 3_000;

fn budget() -> QueryBudget {
    QueryBudget::new(u64::MAX, Some(Duration::from_secs(30)))
}

/// Seal every pending commit; fold by the default depth and delta ratio.
fn open(s: &Scratch) -> SqliteStore {
    SqliteStore::open(&s.db(), &s.keys())
        .unwrap()
        .with_catalog_config(CatalogCfg {
            seal_ops: SealOps {
                graph: 1,
                ..SealOps::default()
            },
            checkpoint_interval_ms: 60_000,
            ..CatalogCfg::default()
        })
        .unwrap()
}

/// `NODES` node ids and `EDGES` distinct, non-self `(src, dst)` index pairs, seeded.
fn fixture_pairs() -> Vec<(usize, usize)> {
    let mut seed = 0x9e37_79b9_7f4a_7c15_u64;
    let mut next = move || {
        seed = seed
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        usize::try_from(seed >> 33).unwrap() % NODES
    };
    let mut seen = BTreeSet::new();
    let mut pairs = Vec::with_capacity(EDGES);
    while pairs.len() < EDGES {
        let (a, b) = (next(), next());
        if a != b && seen.insert((a, b)) {
            pairs.push((a, b));
        }
    }
    pairs
}

fn batch(store: &SqliteStore, ops: Vec<Op>) {
    store
        .apply_batch(WriteBatch::new(WorkspaceId::default(), ops), &ctx())
        .unwrap();
}

fn edge_ops(ids: &[NodeId], pairs: &[(usize, usize)]) -> Vec<Op> {
    pairs
        .iter()
        .map(|&(a, b)| Op::UpsertEdge {
            src: ids[a].clone(),
            dst: ids[b].clone(),
            rel: "RELATES_TO".into(),
            props: None,
        })
        .collect()
}

/// Compact `store` and return the kinds of graph units that published.
fn compact(store: &SqliteStore) -> Vec<WorkKind> {
    let report = store.compact().unwrap();
    report
        .published
        .iter()
        .filter(|c| c.class == "graph_layer")
        .map(|c| c.kind)
        .collect()
}

/// The rows' truth: present local ids, purged ones, and every edge.
struct Truth {
    nodes: BTreeSet<u32>,
    purged: Vec<u32>,
    edges: BTreeSet<(u32, u32)>,
    weights: BTreeMap<(u32, u32), i64>,
}

fn truth(db: &Path) -> Truth {
    let conn = rusqlite::Connection::open(db).unwrap();
    let rids = |sql: &str| -> Vec<u32> {
        let mut stmt = conn.prepare(sql).unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    };
    let nodes = rids("SELECT rid FROM node WHERE lifecycle <> 'purged'")
        .into_iter()
        .collect();
    let purged = rids("SELECT rid FROM node WHERE lifecycle = 'purged'");
    let mut stmt = conn
        .prepare(
            "SELECT src, dst, CAST(round(coalesce(confidence, 1.0) * 1000000) AS INTEGER) FROM edge",
        )
        .unwrap();
    let rows: Vec<(u32, u32, i64)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    Truth {
        nodes,
        purged,
        edges: rows.iter().map(|&(s, d, _)| (s, d)).collect(),
        weights: rows.iter().map(|&(s, d, w)| ((s, d), w)).collect(),
    }
}

/// Weakly connected components of the truth: node -> lowest member.
fn components(t: &Truth) -> BTreeMap<u32, u32> {
    let mut root: BTreeMap<u32, u32> = t.nodes.iter().map(|&n| (n, n)).collect();
    fn find(root: &mut BTreeMap<u32, u32>, n: u32) -> u32 {
        let p = root[&n];
        if p == n {
            return n;
        }
        let r = find(root, p);
        root.insert(n, r);
        r
    }
    for &(a, b) in &t.edges {
        let (ra, rb) = (find(&mut root, a), find(&mut root, b));
        root.insert(ra.max(rb), ra.min(rb));
    }
    let nodes: Vec<u32> = t.nodes.iter().copied().collect();
    nodes.into_iter().map(|n| (n, find(&mut root, n))).collect()
}

/// The graph at `stage` equals the rows: contains, nodes, every edge and its weight, WCC and
/// PageRank membership.
fn assert_matches_rows(store: &SqliteStore, db: &Path, stage: &str) {
    let t = truth(db);
    let comps = components(&t);
    let n = u32::try_from(t.nodes.len()).unwrap();
    catalog_testing::with_main_graph(store, &budget(), |g| {
        let (v, b, all) = (g.view(), budget(), RelSelection::All);
        let listed: BTreeSet<u32> = v.nodes(&b).map(|n| n.unwrap().0).collect();
        assert_eq!(listed, t.nodes, "{stage}: nodes");
        assert!(t.nodes.iter().all(|&n| v.contains(LocalId(n))), "{stage}");
        assert!(t.purged.iter().all(|&n| !v.contains(LocalId(n))), "{stage}");
        let mut weights = BTreeMap::new();
        for &s in &t.nodes {
            for e in v.neighbours(LocalId(s), &all, Direction::Out, &b) {
                let (d, _, attr) = e.unwrap();
                weights.insert((s, d.0), attr.weight);
            }
        }
        assert_eq!(weights, t.weights, "{stage}: edges and weights");
        let w = wcc(v, &all, RowCap(n), &b).unwrap();
        assert!(!w.truncated, "{stage}");
        let got: BTreeMap<u32, u32> = w.rows.iter().map(|&(a, c)| (a.0, c.0)).collect();
        assert_eq!(got, comps, "{stage}: components");
        let pr = pagerank(v, &all, RankParams::default(), g.order(), RowCap(n), &b).unwrap();
        let ranked: BTreeSet<u32> = pr.rows.iter().map(|&(a, _)| a.0).collect();
        assert_eq!(ranked, t.nodes, "{stage}: ranked membership");
        Ok(())
    })
    .unwrap();
}

/// The recorded query results (expand, closure, degree, shortest and weighted shortest paths
/// from fixed sources) and every present node's local id.
fn record(store: &SqliteStore, probes: &[NodeId]) -> (Vec<String>, BTreeMap<String, u32>) {
    catalog_testing::with_main_graph(store, &budget(), |g| {
        let (v, b, all) = (g.view(), budget(), RelSelection::All);
        let caps = Caps {
            depth: 3,
            rows: 1_500,
        };
        let local = |id: &NodeId| g.local(id).unwrap().unwrap();
        let mut out = Vec::new();
        for (i, p) in probes.iter().enumerate() {
            let (s, d) = (local(p), local(&probes[(i + 1) % probes.len()]));
            let reached = |r: &kaban_index_graph::Reached| format!("{r:?}");
            out.push(reached(&expand(v, &[s], &all, Direction::Both, caps, &b).unwrap()));
            out.push(reached(&closure(v, &[s], &all, caps, &b).unwrap()));
            out.push(format!("{:?}", degree(v, s, &all, Direction::Both, &b)));
            let path = shortest_path(v, s, d, &all, caps, &b).map(|p| p.map(|c| c.into_parts().0));
            out.push(format!("{path:?}"));
            let weighted = weighted_shortest_path(v, s, d, &all, caps, g.order(), &b)
                .map(|p| p.map(|c| c.into_parts().0));
            out.push(format!("{weighted:?}"));
        }
        Ok((out, locals(g)))
    })
    .unwrap()
}

fn locals(g: &MainGraph<'_>) -> BTreeMap<String, u32> {
    let b = budget();
    g.view()
        .nodes(&b)
        .map(|n| {
            let n = n.unwrap();
            (g.node_id(n).unwrap().unwrap().as_str().to_owned(), n.0)
        })
        .collect()
}

/// REC-01: every graph extent and pending row holds only local ids (presence included), rel
/// ids, ops and mapped attributes: no facts, every rel id declared, every weight non-negative;
/// chain plus sediment present exactly the rows' nodes.
fn assert_extents_hold_only_graph_records(store: &SqliteStore, db: &Path, stage: &str) {
    let t = truth(db);
    let b = budget();
    let rels = catalog_testing::with_main_graph(store, &b, |g| {
        let names = rel_names(db);
        Ok(names.iter().filter_map(|n| g.rel(n)).count())
    })
    .unwrap();
    let (extents, sediment) = ok(catalog_testing::graph_contents(store));
    assert!(!extents.is_empty(), "{stage}: a chain");
    let base = Base::open(OwnedBytes::new(extents[0].clone()), &b).unwrap();
    let layers: Vec<Layer> = extents[1..]
        .iter()
        .map(|e| Layer::open(OwnedBytes::new(e.clone()), &b).unwrap())
        .collect();
    let decoded: Vec<_> = sediment
        .iter()
        .map(|p| Records::decode(p, &b).unwrap())
        .collect();
    for r in &decoded {
        let r = r.records();
        assert!(r.facts.is_empty(), "{stage}: a fact in the sediment");
        for e in &r.edges {
            assert!(usize::from(e.rel.0) < rels && e.attr.weight >= 0, "{stage}: {e:?}");
        }
    }
    let empty = PreparedSediment::empty();
    let one = [base.clone()];
    let mut checked: Vec<View<'_>> = vec![View::new(Some(&one[0]), &[], &empty, &b).unwrap()];
    for l in &layers {
        checked.push(View::new(None, std::slice::from_ref(l), &empty, &b).unwrap());
    }
    for v in &checked {
        for n in v.nodes(&b) {
            let n = n.unwrap();
            assert!((0..=u8::MAX).all(|k| v.fact(n, k).is_none()), "{stage}: a fact");
            for e in v.neighbours(n, &RelSelection::All, Direction::Both, &b) {
                let (_, rel, attr) = e.unwrap();
                assert!(usize::from(rel.0) < rels && attr.weight >= 0, "{stage}");
            }
        }
    }
    let prepared = PreparedSediment::prepare(&decoded, &b).unwrap();
    let whole = View::new(Some(&base), &layers, &prepared, &b).unwrap();
    let present: BTreeSet<u32> = whole.nodes(&b).map(|n| n.unwrap().0).collect();
    assert_eq!(present, t.nodes, "{stage}: presence equals node truth");
}

/// `r`'s value, or a panic naming the driver error behind it.
fn ok<T>(r: kaban_core::Result<T>) -> T {
    match r {
        Ok(v) => v,
        Err(kaban_core::StoreError::Backend { source }) => panic!("{}", source.driver()),
        Err(e) => panic!("{e:?}"),
    }
}

fn rel_names(db: &Path) -> Vec<String> {
    let conn = rusqlite::Connection::open(db).unwrap();
    let mut stmt = conn
        .prepare("SELECT name FROM schema_type WHERE kind = 'rel'")
        .unwrap();
    stmt.query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

#[test]
fn the_graph_equals_the_rows_at_every_stage_and_a_rebuild_after_losing_the_container_changes_nothing()
 {
    // Arrange: 500 nodes and 3,000 edges, sealed, folded and sealed again, sediment pending
    let s = Scratch::new("kaban-t11-rebuild", "stages");
    let db = s.db();
    let store = open(&s);
    let ids: Vec<NodeId> = (0..NODES).map(|_| NodeId::new()).collect();
    let pairs = fixture_pairs();
    let mut ops: Vec<Op> = ids
        .iter()
        .enumerate()
        .map(|(i, id)| Op::UpsertNode {
            id: id.clone(),
            node: NewNode {
                kind: "Fact".into(),
                text: format!("node {i}"),
                ..Default::default()
            },
        })
        .collect();
    ops.extend(edge_ops(&ids, &pairs[..1_000]));
    batch(&store, ops);
    assert_matches_rows(&store, &db, "sediment");
    assert_eq!(compact(&store), [WorkKind::Seal]);
    assert_matches_rows(&store, &db, "sealed");
    batch(&store, edge_ops(&ids, &pairs[1_000..2_000]));
    assert_eq!(compact(&store), [WorkKind::Seal]);
    assert_eq!(compact(&store), [WorkKind::Fold], "the heavy layer folds");
    assert_matches_rows(&store, &db, "folded");
    batch(&store, edge_ops(&ids, &pairs[2_000..2_700]));
    assert_eq!(compact(&store), [WorkKind::Seal]);
    batch(&store, edge_ops(&ids, &pairs[2_700..]));
    catalog_testing::corrupt(
        &store,
        "UPDATE edge SET confidence = ((rowid * 7) % 10 + 1) / 10.0 WHERE rowid % 3 = 0",
    )
    .unwrap();
    // isolated insertion, last-edge deletion, purged middle and highest ids, archived and
    // restored, a foreign workspace
    let isolated: Vec<NodeId> = (0..3)
        .map(|i| nid(&created(&store, "Fact", &format!("isolated {i}")).id))
        .collect();
    let (x, y) = (
        nid(&created(&store, "Fact", "x").id),
        nid(&created(&store, "Fact", "y").id),
    );
    store
        .link(&x, &y, "RELATES_TO", None, None, &ctx())
        .unwrap();
    catalog_testing::corrupt(
        &store,
        &format!(
            "DELETE FROM edge WHERE src = (SELECT rid FROM node WHERE id = '{}')",
            x.as_str()
        ),
    )
    .unwrap();
    let highest = nid(&created(&store, "Fact", "highest").id);
    for purged in [&ids[250], &highest] {
        store.forget(purged, ForgetMode::Purge, &ctx()).unwrap();
    }
    store.forget(&ids[10], ForgetMode::Archive, &ctx()).unwrap();
    store.forget(&ids[11], ForgetMode::Archive, &ctx()).unwrap();
    catalog_testing::corrupt(
        &store,
        &format!(
            "UPDATE node SET lifecycle = 'active' WHERE id = '{}'",
            ids[11].as_str()
        ),
    )
    .unwrap();
    let foreign: Vec<NodeId> = (0..2).map(|_| NodeId::new()).collect();
    let mut ops: Vec<Op> = foreign
        .iter()
        .enumerate()
        .map(|(i, id)| Op::UpsertNode {
            id: id.clone(),
            node: NewNode {
                kind: "Fact".into(),
                text: format!("foreign {i}"),
                ..Default::default()
            },
        })
        .collect();
    ops.extend(edge_ops(&foreign, &[(0, 1)]));
    let other = WorkspaceId::new("other");
    store
        .apply_batch(
            WriteBatch::new(other.clone(), ops),
            &WriteCtx::new(other, Actor::Agent),
        )
        .unwrap();
    let (_, pending) = catalog_testing::graph_contents(&store).unwrap();
    assert!(!pending.is_empty(), "the sediment is not empty");
    assert_matches_rows(&store, &db, "final");
    assert_extents_hold_only_graph_records(&store, &db, "final");
    let probes = [
        ids[0].clone(),
        ids[1].clone(),
        ids[10].clone(),
        ids[300].clone(),
        isolated[0].clone(),
        x.clone(),
        y.clone(),
        foreign[0].clone(),
    ];
    let recorded = record(&store, &probes);

    // Act: reopen; then lose the container, reopen and rebuild
    drop(store);
    let store = open(&s);
    assert_matches_rows(&store, &db, "reopened");
    let reopened = record(&store, &probes);
    drop(store);
    let kbx = db.with_file_name("kaban.kbx");
    assert!(kbx.exists(), "{kbx:?}");
    std::fs::remove_file(&kbx).unwrap();
    let store = open(&s);
    catalog_testing::rebuild_graph(&store).unwrap();

    // Assert: identical results and local ids; the rebuilt graph equals the rows, holds only
    // graph records, and the catalog verifies clean
    assert_eq!(reopened, recorded, "reopened");
    assert_matches_rows(&store, &db, "rebuilt");
    assert_extents_hold_only_graph_records(&store, &db, "rebuilt");
    let rebuilt = record(&store, &probes);
    assert_eq!(rebuilt.1, recorded.1, "local ids");
    assert_eq!(rebuilt.0, recorded.0, "results");
    let verify = store.verify_catalog().unwrap();
    assert_eq!(verify.inconsistencies(), 0, "{verify:?}");
}
