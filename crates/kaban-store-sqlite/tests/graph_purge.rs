//! Purge leaves no graph extent naming the purged node (spec 11 T11 Step 5; INV-6, PC-1, wave
//! 2 item 7.8): a node with edges in the base, in a sealed layer and in the sediment is deleted
//! by a real store commit, then the catalog purges its rid and compacts; no live graph extent
//! holds an entry for it, the other nodes and edges survive, and the retired extents are
//! reclaimed.
//!
//! Runs with the `failpoints` feature (the graph's test access lives behind it).

#![cfg(feature = "failpoints")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion
#![allow(clippy::many_single_char_names)] // the fixture's nodes are named n, a, b, c and d

mod common;

use std::collections::BTreeSet;
use std::path::Path;
use std::time::Duration;

use common::{Scratch, created, ctx, nid};
use kaban_core::config::{CatalogCfg, SealOps};
use kaban_core::query_budget::QueryBudget;
use kaban_core::{ForgetMode, MemoryStore, NodeId};
use kaban_index_graph::{Base, Caps, Direction, Layer, LocalId, RelSelection, expand};
use kaban_store_sqlite::catalog_testing::{self, GraphContents};
use kaban_store_sqlite::{CompactBudget, SqliteStore, WorkKind};
use tantivy::directory::OwnedBytes;

fn budget() -> QueryBudget {
    QueryBudget::new(u64::MAX, Some(Duration::from_secs(30)))
}

/// Every pending graph commit seals; nothing folds.
fn open(s: &Scratch) -> SqliteStore {
    SqliteStore::open(&s.db(), &s.keys())
        .unwrap()
        .with_catalog_config(CatalogCfg {
            seal_ops: SealOps {
                graph: 1,
                ..SealOps::default()
            },
            fold_delta_ratio: 1e9,
            checkpoint_interval_ms: 60_000,
            ..CatalogCfg::default()
        })
        .unwrap()
}

/// The kinds of graph units a compaction published, and the extents it reclaimed.
fn compact(store: &SqliteStore) -> (Vec<WorkKind>, Vec<u64>) {
    let report = store.compact().unwrap();
    let kinds = report
        .published
        .iter()
        .filter(|c| c.class == "graph_layer")
        .map(|c| c.kind)
        .collect();
    (kinds, report.reclaim.reclaimed)
}

fn link(store: &SqliteStore, a: &NodeId, b: &NodeId) {
    store.link(a, b, "RELATES_TO", None, None, &ctx()).unwrap();
}

/// The live graph extent ids of `main`'s chain.
fn graph_chain(db: &Path) -> Vec<u64> {
    let conn = rusqlite::Connection::open(db).unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT e.id FROM cat_view_extent v JOIN cat_extent e ON e.id = v.extent_id
             WHERE v.view = 'main' AND e.class IN ('graph_layer', 'graph_base')
             ORDER BY v.position",
        )
        .unwrap();
    stmt.query_map([], |r| r.get::<_, i64>(0))
        .unwrap()
        .map(|id| u64::try_from(id.unwrap()).unwrap())
        .collect()
}

fn rid(db: &Path, id: &NodeId) -> u32 {
    let conn = rusqlite::Connection::open(db).unwrap();
    conn.query_row("SELECT rid FROM node WHERE id = ?1", [id.as_str()], |r| {
        r.get(0)
    })
    .unwrap()
}

/// Whether some extent of `contents` (the first a base, the rest layers) holds an entry for `n`.
fn extents_mentioning(contents: &GraphContents, n: LocalId) -> Vec<usize> {
    let b = budget();
    contents
        .extents
        .iter()
        .enumerate()
        .filter(|(i, bytes)| {
            let bytes = OwnedBytes::new((*bytes).clone());
            if *i == 0 {
                Base::open(bytes, &b).unwrap().mentions(n)
            } else {
                Layer::open(bytes, &b).unwrap().mentions(n)
            }
        })
        .map(|(i, _)| i)
        .collect()
}

#[test]
fn a_seal_split_between_a_purged_nodes_edge_and_its_removal_leaves_no_entry_for_it() {
    // Arrange: n has 300 edges in the base; then one pending row adds n -> x (small) and the
    // next removes n and its edges (large); the purge rewrites the chain without n
    let s = Scratch::new("kaban-t11-purge", "split");
    let db = s.db();
    let store = open(&s);
    let [n, x] = ["n", "x"].map(|t| nid(&created(&store, "Fact", t).id));
    let others: Vec<NodeId> = (0..300)
        .map(|i| NodeId::parse(&created(&store, "Fact", &format!("m{i}")).id).unwrap())
        .collect();
    for m in &others {
        link(&store, &n, m);
    }
    compact(&store);
    link(&store, &n, &x);
    store.forget(&n, ForgetMode::Purge, &ctx()).unwrap();
    let rid_n = LocalId(rid(&db, &n));
    let purged = catalog_testing::purge(&store, &[i64::from(rid_n.0)]).unwrap();
    assert!(purged.skipped.is_empty(), "{:?}", purged.skipped);
    let pending = catalog_testing::graph_contents(&store).unwrap().sediment.len();
    assert_eq!(pending, 2, "the edge row and the removal row are pending");

    // Act: compact under the smallest budget (in 1 KiB steps) that seals anything, so the
    // halved unit holds the edge row alone; then seal the rest
    let mut split = None;
    for kib in 1..=4096_u64 {
        let report = catalog_testing::compact(
            &store,
            CompactBudget {
                memory_bytes: kib << 10,
                deadline: Duration::from_secs(30),
            },
        )
        .unwrap();
        if let Some(c) = report
            .published
            .iter()
            .find(|c| c.class == "graph_layer" && c.kind == WorkKind::Seal)
        {
            split = Some(c.sediment_rows);
            break;
        }
    }
    assert_eq!(split, Some(1), "a seal of the edge row alone");
    compact(&store);

    // Assert: every row sealed, and no live graph extent holds an entry for n
    let after = catalog_testing::graph_contents(&store).unwrap();
    assert!(after.sediment.is_empty(), "every pending row sealed");
    assert_eq!(
        extents_mentioning(&after, rid_n),
        Vec::<usize>::new(),
        "no graph extent holds an entry for the purged n"
    );
}

#[test]
fn a_purged_node_leaves_no_entry_in_any_graph_extent_and_its_old_extents_are_reclaimed() {
    // Arrange: n's edges in the base (n -> a, d -> n), in a sealed layer (n -> b) and in the
    // sediment (n -> c); edges without n in each too
    let s = Scratch::new("kaban-t11-purge", "purge");
    let db = s.db();
    let store = open(&s);
    let [n, a, b, c, d] = ["n", "a", "b", "c", "d"].map(|t| nid(&created(&store, "Fact", t).id));
    link(&store, &n, &a);
    link(&store, &d, &n);
    link(&store, &a, &d);
    assert_eq!(compact(&store).0, [WorkKind::Seal], "the base");
    link(&store, &n, &b);
    link(&store, &a, &b);
    assert_eq!(compact(&store).0, [WorkKind::Seal], "a sealed layer");
    link(&store, &n, &c);
    link(&store, &c, &d);
    let rid_n = LocalId(rid(&db, &n));
    let old = graph_chain(&db);
    let before = catalog_testing::graph_contents(&store).unwrap();
    assert_eq!(before.extents.len(), 2, "a base and one layer");
    assert_eq!(
        extents_mentioning(&before, rid_n),
        [0, 1],
        "n is in both extents"
    );
    assert!(!before.sediment.is_empty(), "n's last edge is pending");

    // Act: delete n's rows in a real commit, purge its rid, compact
    store.forget(&n, ForgetMode::Purge, &ctx()).unwrap();
    let purged = catalog_testing::purge(&store, &[i64::from(rid_n.0)]).unwrap();
    let (sealed, compact_reclaimed) = compact(&store);
    catalog_testing::checkpoint_tick(&store).unwrap();
    let late = catalog_testing::reclaim_retired(&store).unwrap();

    // Assert: no live graph extent names n; the rest survives; the old extents are gone
    assert!(purged.skipped.is_empty(), "{:?}", purged.skipped);
    assert_eq!(sealed, [WorkKind::Seal], "the pending rows seal");
    let after = catalog_testing::graph_contents(&store).unwrap();
    assert!(after.sediment.is_empty(), "every pending row sealed");
    assert_eq!(
        extents_mentioning(&after, rid_n),
        Vec::<usize>::new(),
        "no graph extent holds an entry for n"
    );
    let reached = catalog_testing::with_main_graph(&store, &budget(), |g| {
        assert!(!g.view().contains(rid_n), "n is absent");
        let from = [a.clone(), c.clone()].map(|x| g.local(&x).unwrap().unwrap());
        let r = expand(
            g.view(),
            &from,
            &RelSelection::All,
            Direction::Out,
            Caps { depth: 1, rows: 50 },
            &budget(),
        )
        .unwrap();
        Ok(r.nodes
            .iter()
            .map(|&(m, _)| g.node_id(m).unwrap().unwrap())
            .collect::<BTreeSet<_>>())
    })
    .unwrap();
    assert_eq!(
        reached,
        BTreeSet::from([b, d]),
        "a -> b, a -> d, c -> d survive"
    );
    let reclaimed: BTreeSet<u64> = purged
        .reclaim
        .reclaimed
        .iter()
        .chain(&compact_reclaimed)
        .chain(&late.reclaimed)
        .copied()
        .collect();
    assert!(
        old.iter().all(|e| reclaimed.contains(e)),
        "the retired extents {old:?} are reclaimed ({reclaimed:?})"
    );
}
