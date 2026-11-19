//! The memory graph on the catalog (spec 11 T11 Step 1; GRA-02, GRA-03, KER-01): an edge is
//! traversable from every store and every process of the database once its commit returns,
//! with its row's attributes, and a row that maps to no graph attribute refuses its write by
//! name.
//!
//! Runs with the `failpoints` feature (the graph's test access lives behind it).

#![cfg(feature = "failpoints")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::{Scratch, created, ctx, nid};
use kaban_core::config::{CatalogCfg, SealOps};
use kaban_core::query_budget::QueryBudget;
use kaban_core::{
    IngestStore, MemoryStore, NewNode, NodeId, Op, StoreError, WorkspaceId, WriteBatch,
};
use kaban_index_graph::{
    Caps, Direction, LocalId, RankParams, RelSelection, RowCap, StableOrder, expand, pagerank,
};
use kaban_store_sqlite::catalog_testing;
use kaban_store_sqlite::{SqliteStore, WorkKind};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn budget() -> QueryBudget {
    QueryBudget::new(u64::MAX, Some(Duration::from_secs(10)))
}

/// The driver's text of a backend error.
fn driver_text(e: &StoreError) -> String {
    let StoreError::Backend { source } = e else {
        panic!("not a backend error: {e:?}");
    };
    source.driver().to_string()
}

const CAPS: Caps = Caps { depth: 2, rows: 50 };

#[test]
fn a_link_committed_in_one_store_is_traversable_from_another_at_once() {
    // Arrange: two stores of one database
    let s = Scratch::new("kaban-t11-main", "visible");
    let writer = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let reader = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let a = nid(&created(&writer, "Fact", "a").id);
    let b = nid(&created(&writer, "Fact", "b").id);
    let lone = nid(&created(&writer, "Fact", "isolated").id);

    // Act: the link returns in the writer; the reader loads `main` right after
    writer
        .link(&a, &b, "RELATES_TO", None, None, &ctx())
        .unwrap();
    let (reached, isolated, unused_rel) =
        catalog_testing::with_main_graph(&reader, &budget(), |g| {
            let la = g.local(&a)?.expect("a has a local id");
            let r = expand(
                g.view(),
                &[la],
                &RelSelection::All,
                Direction::Out,
                CAPS,
                &budget(),
            )
            .unwrap();
            let reached: Vec<_> = r
                .nodes
                .iter()
                .map(|&(n, _)| g.node_id(n).unwrap().unwrap())
                .collect();
            let lone = g.local(&lone)?.expect("the isolated node has a local id");
            Ok((reached, g.view().contains(lone), g.rel("SUPERSEDES")))
        })
        .unwrap();

    // Assert: b reached with no wait; the isolated node present; a declared relation that no
    // edge uses has an id
    assert_eq!(reached, vec![b.clone()]);
    assert!(isolated, "an isolated node is present");
    assert!(
        unused_rel.is_some(),
        "a declared, unused relation has an id"
    );
}

/// The database the linking role opens (set only for the child process).
const LINK_DB: &str = "KABAN_T11_LINK_DB";
/// The file naming the two nodes the linking role links, one id per line.
const LINK_NODES: &str = "link-nodes";
/// How long a role of a two-process case may run before it is killed.
const ROLE_DEADLINE: Duration = Duration::from_secs(60);

/// The linking role of the two-process case: in a child process (`KABAN_T11_LINK_DB` set) open
/// the store and link the two nodes `<dir>/link-nodes` names. A no-op elsewhere.
#[test]
#[ignore = "a role of the two-process visibility case"]
fn multi_process_link_role() {
    let Ok(db) = std::env::var(LINK_DB) else {
        return;
    };
    let db = PathBuf::from(db);
    let dir = db.parent().unwrap().to_path_buf();
    let ids = std::fs::read_to_string(dir.join(LINK_NODES)).unwrap();
    let [a, b]: [NodeId; 2] = ids.lines().map(nid).collect::<Vec<_>>().try_into().unwrap();
    let store = SqliteStore::open(&db, &dir.join("keys")).unwrap();
    store
        .link(&a, &b, "RELATES_TO", None, None, &ctx())
        .unwrap();
}

/// A spawned role process, killed and reaped however the test ends.
struct Supervised(std::process::Child);

impl Drop for Supervised {
    fn drop(&mut self) {
        // a role that already exited makes both calls no-ops
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Run the ignored test `role` of this binary in a child process with `env` set, its stderr
/// kept in `dir`, and wait at most `deadline` for it (the store suites' two-process helper).
fn supervise(
    role: &str,
    env: (&str, &std::ffi::OsStr),
    dir: &Path,
    deadline: Duration,
) -> Result<ExitStatus, String> {
    let log = dir.join(format!("{role}.stderr"));
    let stderr = std::fs::File::create(&log).map_err(|e| format!("{role}: {e}"))?;
    let child = Command::new(std::env::current_exe().map_err(|e| e.to_string())?)
        .args(["--exact", role, "--ignored", "--nocapture"])
        .env(env.0, env.1)
        .stdout(Stdio::null())
        .stderr(stderr)
        .spawn()
        .map_err(|e| format!("{role}: spawn: {e}"))?;
    let mut child = Supervised(child);
    let started = Instant::now();
    loop {
        if let Some(status) = child.0.try_wait().map_err(|e| format!("{role}: {e}"))? {
            return Ok(status);
        }
        if started.elapsed() >= deadline {
            let _ = child.0.kill();
            let reaped = child.0.wait();
            let tail = std::fs::read_to_string(&log).unwrap_or_default();
            return Err(format!(
                "role {role} still ran after {deadline:?}: killed, reaped ({reaped:?}); stderr:\n{tail}"
            ));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_link_committed_by_another_process_is_traversable_here_once_it_exits() {
    // Arrange: process A's store with two nodes, its graph already loaded (and cached)
    let s = Scratch::new("kaban-t11-main", "process");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let [a, b] = ["a", "b"].map(|t| nid(&created(&store, "Fact", t).id));
    assert_eq!(out_of(&store, &a), Vec::<NodeId>::new(), "no edge yet");
    std::fs::write(
        s.0.join(LINK_NODES),
        format!("{}\n{}", a.as_str(), b.as_str()),
    )
    .unwrap();

    // Act: process B links a -> b and exits
    let status = supervise(
        "multi_process_link_role",
        (LINK_DB, s.db().as_os_str()),
        &s.0,
        ROLE_DEADLINE,
    )
    .unwrap();

    // Assert: A traverses the edge at once, through its cached graph
    assert!(status.success(), "the linking process failed: {status}");
    assert_eq!(out_of(&store, &a), vec![b]);
}

#[test]
fn an_edge_carries_its_rows_weight_and_time_and_unmappable_values_refuse_the_write() {
    // Arrange
    let s = Scratch::new("kaban-t11-main", "attrs");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let a = nid(&created(&store, "Fact", "a").id);
    let b = nid(&created(&store, "Fact", "b").id);
    store
        .link(&a, &b, "RELATES_TO", Some(0.25), None, &ctx())
        .unwrap();
    catalog_testing::corrupt(
        &store,
        "UPDATE edge SET created_at = '2024-02-29T12:00:00.123+02:00'",
    )
    .unwrap();

    // Act
    let attrs = catalog_testing::with_main_graph(&store, &budget(), |g| {
        let la = g.local(&a)?.unwrap();
        let rels = RelSelection::All;
        let b2 = budget();
        Ok(g.view()
            .neighbours(la, &rels, Direction::Out, &b2)
            .map(|n| n.unwrap().2)
            .collect::<Vec<_>>())
    })
    .unwrap();
    let negative = catalog_testing::corrupt(&store, "UPDATE edge SET confidence = -0.5");
    let untimed = catalog_testing::corrupt(&store, "UPDATE edge SET created_at = 'yesterday'");

    // Assert: weight in millionths, time in epoch milliseconds; both refusals name the
    // attribute and roll their write back
    assert_eq!(attrs.len(), 1);
    assert_eq!((attrs[0].weight, attrs[0].ts), (250_000, 1_709_200_800_123));
    let negative = driver_text(&negative.expect_err("a negative weight was admitted"));
    assert!(
        negative.contains("graph_weight_refused") && negative.contains("confidence"),
        "{negative}"
    );
    let untimed = driver_text(&untimed.expect_err("an unparsable time was admitted"));
    assert!(
        untimed.contains("graph_timestamp_refused") && untimed.contains("created_at"),
        "{untimed}"
    );
}

/// The `NodeId`s one hop out of `src` in `store`'s main graph.
fn out_of(store: &SqliteStore, src: &NodeId) -> Vec<NodeId> {
    selected(store, src, &[RelSelection::All]).remove(0)
}

#[test]
fn a_database_recreated_at_the_same_path_loads_its_own_graph_not_the_cached_one() {
    // Arrange: a store links a -> b and loads its graph (the process caches it); its database
    // and container are deleted
    let s = Scratch::new("kaban-t11-main", "recreated");
    {
        let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
        let [a, b, _c] = ["a", "b", "c"].map(|t| nid(&created(&store, "Fact", t).id));
        store
            .link(&a, &b, "RELATES_TO", None, None, &ctx())
            .unwrap();
        assert_eq!(out_of(&store, &a), vec![b]);
    }
    std::fs::remove_dir_all(&s.0).unwrap();
    std::fs::create_dir_all(&s.0).unwrap();

    // Act: a new database at the same path, the same steps (so the same generations, rids
    // and sediment seqs), but a -> c
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let [a, _b, c] = ["a", "b", "c"].map(|t| nid(&created(&store, "Fact", t).id));
    store
        .link(&a, &c, "RELATES_TO", None, None, &ctx())
        .unwrap();
    let reached = out_of(&store, &a);

    // Assert: the new database's graph, not the cached one of the old
    assert_eq!(reached, vec![c], "the recreated database's own edge a -> c");
}

#[test]
fn a_load_under_a_ceiling_below_what_building_its_relation_dictionary_holds_is_refused() {
    // Arrange: 2000 more relations of 200-byte names; `text` the bytes of every relation name
    let s = Scratch::new("kaban-t11-main", "rels-budget");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    catalog_testing::corrupt(
        &store,
        "WITH RECURSIVE i(k) AS (SELECT 0 UNION ALL SELECT k + 1 FROM i WHERE k + 1 < 2000)
         INSERT INTO schema_type(ns, kind, name, created_at)
         SELECT 'user', 'rel', printf('L%0199d', k), '2024-01-01T00:00:00Z' FROM i",
    )
    .unwrap();
    let text: i64 = rusqlite::Connection::open(s.db())
        .unwrap()
        .query_row(
            "SELECT sum(length(name)) FROM schema_type WHERE kind = 'rel'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let text = u64::try_from(text).unwrap();

    // Act: a load whose whole budget is twice the names' bytes (the names kept plus the set
    // that deduplicates them while the dictionary is built)
    let tight = QueryBudget::new(2 * text, Some(Duration::from_secs(10)));
    let loaded = catalog_testing::with_main_graph(&store, &tight, |g| Ok(g.rel("RELATES_TO")));

    // Assert: refused by its budget, before the dictionary is built
    assert!(
        matches!(loaded, Err(StoreError::BudgetExceeded(_))),
        "a load under {} bytes for {text} bytes of relation names: {loaded:?}",
        2 * text
    );
}

#[test]
fn an_idle_cached_graph_is_dropped_by_the_next_write_without_another_load() {
    // Arrange: a load leaves main's graph in the process cache
    let s = Scratch::new("kaban-t11-main", "idle");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let [a, b] = ["a", "b"].map(|t| nid(&created(&store, "Fact", t).id));
    store
        .link(&a, &b, "RELATES_TO", None, None, &ctx())
        .unwrap();
    assert_eq!(out_of(&store, &a), vec![b]);
    assert!(
        catalog_testing::graph_cache_holds(&store).unwrap(),
        "cached"
    );

    // Act: 61 s of idleness on the cache's clock, then one unrelated write and no load
    catalog_testing::advance_graph_cache_clock(Duration::from_secs(61));
    created(&store, "Fact", "unrelated");

    // Assert: the idle entry (and the pin locks its bytes hold) is gone
    assert!(
        !catalog_testing::graph_cache_holds(&store).unwrap(),
        "an entry idle past 60 s survived a write"
    );
}

#[test]
fn a_chain_of_code_class_extents_loads_when_the_code_classes_are_asked_for() {
    // Arrange: main's chain holds one code graph base (nodes 1 and 2, edge 1 -> 2) and no
    // memory graph extent
    use kaban_index_graph::{Attr, EdgeOp, NodeOp, OpKind, Records, RelId, build_base};
    let s = Scratch::new("kaban-t11-main", "code-chain");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let code = Records {
        nodes: [1, 2]
            .map(|n| NodeOp {
                node: LocalId(n),
                kind: OpKind::Add,
            })
            .to_vec(),
        edges: vec![EdgeOp {
            src: LocalId(1),
            rel: RelId(0),
            dst: LocalId(2),
            kind: OpKind::Add,
            attr: Attr::default(),
        }],
        ..Records::default()
    };
    catalog_testing::publish_code_base(&store, &build_base(&code, 0)).unwrap();

    // Act: load main's graph for the code classes
    let (present, reached) = catalog_testing::with_main_code_graph(&store, &budget(), |g| {
        let r = expand(
            g.view(),
            &[LocalId(1)],
            &RelSelection::All,
            Direction::Out,
            CAPS,
            &budget(),
        )
        .unwrap();
        Ok((
            g.view().contains(LocalId(1)),
            r.nodes.iter().map(|&(n, _)| n).collect::<Vec<_>>(),
        ))
    })
    .unwrap();

    // Assert: the code extents are the loaded graph
    assert!(present, "the code base's node 1 is present");
    assert_eq!(reached, vec![LocalId(2)], "the code base's edge 1 -> 2");
}

#[test]
fn a_time_whose_offset_hours_or_minutes_are_out_of_range_refuses_the_write() {
    // Arrange
    let s = Scratch::new("kaban-t11-main", "offset");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let a = nid(&created(&store, "Fact", "a").id);
    let b = nid(&created(&store, "Fact", "b").id);
    store
        .link(&a, &b, "RELATES_TO", None, None, &ctx())
        .unwrap();

    // Act: offsets past 23 hours or 59 minutes, then the largest valid one
    let refused = ["+99:99", "+24:00", "-23:60"].map(|offset| {
        catalog_testing::corrupt(
            &store,
            &format!("UPDATE edge SET created_at = '2024-01-01T00:00:00{offset}'"),
        )
    });
    let largest = catalog_testing::corrupt(
        &store,
        "UPDATE edge SET created_at = '2024-01-01T00:00:00+23:59'",
    );

    // Assert: each out-of-range offset is refused by name; +23:59 is a time
    for r in refused {
        let text = driver_text(&r.expect_err("an out-of-range offset was admitted"));
        assert!(text.contains("graph_timestamp_refused"), "{text}");
    }
    largest.unwrap();
}

/// Declare `n` more relations `{prefix}{i}` in one commit of `store`.
fn declare(store: &SqliteStore, prefix: &str, n: u32) {
    catalog_testing::corrupt(
        store,
        &format!(
            "WITH RECURSIVE i(k) AS (SELECT 0 UNION ALL SELECT k + 1 FROM i WHERE k + 1 < {n})
             INSERT INTO schema_type(ns, kind, name, created_at)
             SELECT 'user', 'rel', '{prefix}' || k, '2024-01-01T00:00:00Z' FROM i"
        ),
    )
    .unwrap();
}

/// Write the edge `src -> dst` of relation `rel` as a raw row in one commit of `store`.
fn raw_edge(store: &SqliteStore, src: &NodeId, dst: &NodeId, rel: &str) -> kaban_core::Result<()> {
    catalog_testing::corrupt(
        store,
        &format!(
            "INSERT INTO edge(id, src, dst, rel, created_at) VALUES ('{rel}-{d}',
               (SELECT rid FROM node WHERE id = '{s}'), (SELECT rid FROM node WHERE id = '{d}'),
               '{rel}', '2024-01-01T00:00:00Z')",
            s = src.as_str(),
            d = dst.as_str()
        ),
    )
}

/// The declared relation names in declaration order.
fn rel_names(db: &std::path::Path) -> Vec<String> {
    let conn = rusqlite::Connection::open(db).unwrap();
    let mut stmt = conn
        .prepare("SELECT name FROM schema_type WHERE kind = 'rel' ORDER BY rowid")
        .unwrap();
    stmt.query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

/// For each selection, the `NodeId`s reached in one hop from `src`.
fn selected(store: &SqliteStore, src: &NodeId, selections: &[RelSelection]) -> Vec<Vec<NodeId>> {
    catalog_testing::with_main_graph(store, &budget(), |g| {
        let s = g.local(src)?.unwrap();
        let caps = Caps { depth: 1, rows: 50 };
        Ok(selections
            .iter()
            .map(|sel| {
                let r = expand(g.view(), &[s], sel, Direction::Out, caps, &budget()).unwrap();
                r.nodes
                    .iter()
                    .map(|&(n, _)| g.node_id(n).unwrap().unwrap())
                    .collect()
            })
            .collect())
    })
    .unwrap()
}

#[test]
fn relation_ids_cover_every_u16_without_aliasing_survive_a_rebuild_and_refuse_when_exhausted() {
    // Arrange: 70 more relations (65+ in all), edges of the relations with ids 0, 63 and 64
    let dir = Scratch::new("kaban-t11-main", "rels");
    let store = SqliteStore::open(&dir.db(), &dir.keys()).unwrap();
    let [hub, zero, r63, r64, last] =
        ["a", "b", "c", "d", "e"].map(|t| nid(&created(&store, "Fact", t).id));
    declare(&store, "R", 70);
    let names = rel_names(&dir.db());
    let by_id = |id: u16| -> String {
        catalog_testing::with_main_graph(&store, &budget(), |g| {
            Ok(names
                .iter()
                .find(|n| g.rel(n) == Some(kaban_index_graph::RelId(id)))
                .cloned())
        })
        .unwrap()
        .expect("a relation with this id")
    };
    for (id, dst) in [(0, &zero), (63, &r63), (64, &r64)] {
        raw_edge(&store, &hub, dst, &by_id(id)).unwrap();
    }
    let rel = kaban_index_graph::RelId;
    let selections = [
        RelSelection::All,
        RelSelection::only(vec![]),
        RelSelection::only(vec![rel(0)]),
        RelSelection::only(vec![rel(63)]),
        RelSelection::only(vec![rel(64)]),
        RelSelection::only(vec![rel(u16::MAX)]),
    ];
    let expected = vec![
        vec![zero.clone(), r63.clone(), r64.clone()],
        vec![],
        vec![zero.clone()],
        vec![r63.clone()],
        vec![r64.clone()],
        vec![],
    ];

    // Act, Assert: each selection reaches its own relation's edge only, before and after a
    // rebuild
    assert_eq!(selected(&store, &hub, &selections), expected);
    catalog_testing::rebuild_graph(&store).unwrap();
    assert_eq!(selected(&store, &hub, &selections), expected, "rebuilt");

    // Arrange: relations up to and past the last id
    let have = u32::try_from(rel_names(&dir.db()).len()).unwrap();
    declare(&store, "X", 65_537 - have);
    let names = rel_names(&dir.db());
    assert_eq!(names.len(), 65_537);

    // Act: an edge of the relation with the last id, then one of the relation past it
    raw_edge(&store, &hub, &last, &names[65_535]).unwrap();
    let refused = raw_edge(&store, &hub, &last, &names[65_536]);

    // Assert: the last id serves; the relation past it is refused by name, and every earlier
    // result stands
    let refused = driver_text(&refused.expect_err("a relation past the last id was admitted"));
    assert!(
        refused.contains("graph_relation_ids_exhausted"),
        "{refused}"
    );
    let mut after = expected;
    after[0].push(last.clone());
    after[5] = vec![last.clone()];
    assert_eq!(selected(&store, &hub, &selections), after);
}

#[test]
fn graph_rows_seal_once_the_node_and_edge_operations_they_hold_reach_seal_ops() {
    // Arrange: a graph seal is due at 3 operations; one commit stages 2 node adds
    let s = Scratch::new("kaban-t11-main", "seal-due");
    let store = SqliteStore::open(&s.db(), &s.keys())
        .unwrap()
        .with_catalog_config(CatalogCfg {
            seal_ops: SealOps {
                graph: 3,
                ..SealOps::default()
            },
            checkpoint_interval_ms: 60_000,
            ..CatalogCfg::default()
        })
        .unwrap();
    let nodes = |n: usize| {
        let ops = (0..n)
            .map(|i| Op::UpsertNode {
                id: NodeId::new(),
                node: NewNode {
                    kind: "Fact".into(),
                    text: format!("node {i}"),
                    ..Default::default()
                },
            })
            .collect();
        store
            .apply_batch(WriteBatch::new(WorkspaceId::default(), ops), &ctx())
            .unwrap();
    };
    let graph_seals = || {
        let report = store.compact().unwrap();
        report
            .published
            .iter()
            .filter(|c| c.class == "graph_layer" && c.kind == WorkKind::Seal)
            .count()
    };
    nodes(2);

    // Act: compact below the threshold, then after one more single-node commit
    let below = graph_seals();
    nodes(1);
    let reached = graph_seals();

    // Assert: two operations in one row are not due; three in two rows are
    assert_eq!(below, 0, "2 operations do not reach seal_ops 3");
    assert_eq!(reached, 1, "3 operations reach seal_ops 3");
}

/// A scored query over `store`'s main graph under `b`: its tie order (the `Arc` it was handed,
/// and the bytes asking for it reserved on `b`), then a `PageRank` over it.
fn ranked(store: &SqliteStore, b: &QueryBudget) -> kaban_core::Result<(Arc<dyn StableOrder>, u64)> {
    catalog_testing::with_main_graph(store, b, |g| {
        let before = b.live_bytes();
        let order = g.order()?;
        let reserved = b.live_bytes() - before;
        let params = RankParams::default();
        pagerank(g.view(), &RelSelection::All, params, &*order, RowCap(10), b).unwrap();
        Ok((order, reserved))
    })
}

#[test]
fn scored_queries_at_one_generation_share_one_tie_order_and_a_commit_prepares_a_new_one() {
    // Arrange: a -> b
    let s = Scratch::new("kaban-t11-main", "order-once");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let [a, b] = ["a", "b"].map(|t| nid(&created(&store, "Fact", t).id));
    store
        .link(&a, &b, "RELATES_TO", None, None, &ctx())
        .unwrap();

    // Act: two scored queries at one generation, a commit, a third
    let (first, first_reserved) = ranked(&store, &budget()).unwrap();
    let (second, second_reserved) = ranked(&store, &budget()).unwrap();
    created(&store, "Fact", "c");
    let (third, third_reserved) = ranked(&store, &budget()).unwrap();

    // Assert: the second query reuses the first one's order and reserves nothing for it; the
    // third, at the commit's generation, has an order of its own
    assert!(
        Arc::ptr_eq(&first, &second),
        "the second scored query at one generation prepared the tie order again"
    );
    assert_eq!(second_reserved, 0, "bytes the reused order reserved");
    assert!(
        !Arc::ptr_eq(&second, &third),
        "a query after a commit reused the older generation's tie order"
    );
    assert!(first_reserved > 0, "the first preparation is charged");
    assert!(
        third_reserved > 0,
        "the new generation's preparation is charged"
    );
}

/// Present nodes in one commit of `store`: rids `0..n` (each sorting as its rid).
fn many_nodes(store: &SqliteStore, n: u32) {
    catalog_testing::corrupt(
        store,
        &format!(
            "WITH RECURSIVE i(k) AS (SELECT 0 UNION ALL SELECT k + 1 FROM i WHERE k + 1 < {n})
             INSERT INTO node(rid, id, kind, text, created_at, updated_at, workspace_id,
                              content_hash)
             SELECT k, printf('00000000-0000-7000-8000-%012x', k), 'Fact', 'node ' || k,
                    '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z', 'default', printf('%064x', k)
             FROM i"
        ),
    )
    .unwrap();
}

#[test]
fn an_expand_reserves_no_tie_order_and_a_pagerank_under_the_same_ceiling_is_refused() {
    // Arrange: 20000 present nodes; a first load leaves main's graph in the process cache
    const NODES: u32 = 20_000;
    let s = Scratch::new("kaban-t11-main", "order-ceiling");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    many_nodes(&store, NODES);
    catalog_testing::with_main_graph(&store, &budget(), |g| Ok(g.view().contains(LocalId(0))))
        .unwrap();

    // Act: an expand, then a PageRank, each under a ceiling of half the order's id pairs
    let ceiling = u64::from(NODES) * 4;
    let tight = || QueryBudget::new(ceiling, Some(Duration::from_secs(10)));
    let b = tight();
    let expanded = catalog_testing::with_main_graph(&store, &b, |g| {
        let r = expand(
            g.view(),
            &[LocalId(0)],
            &RelSelection::All,
            Direction::Out,
            CAPS,
            &b,
        );
        Ok(r.unwrap().nodes.len())
    });
    let pagerank = ranked(&store, &tight()).map(|_| ());

    // Assert: the expand prepares no order; the PageRank's preparation is refused
    assert!(
        expanded.is_ok(),
        "an expand under {ceiling} bytes for {NODES} nodes: {expanded:?}"
    );
    assert!(
        matches!(pagerank, Err(StoreError::BudgetExceeded(_))),
        "a PageRank under {ceiling} bytes for {NODES} nodes: {pagerank:?}"
    );
}

#[test]
fn a_query_pinned_before_a_commit_gets_its_own_generations_tie_order_after_it() {
    // Arrange: two stores of one database; a present
    let s = Scratch::new("kaban-t11-main", "order-isolation");
    let reader = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let writer = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let a = nid(&created(&writer, "Fact", "a").id);

    // Act: inside a query pinned at g, the writer commits c and runs a scored query at the new
    // generation (preparing its order); only then the pinned query asks for its order
    let (c_at_g, a_ranked, c_ranked, c_ranked_newer) =
        catalog_testing::with_main_graph(&reader, &budget(), |g| {
            let c = nid(&created(&writer, "Fact", "c").id);
            let (c_local, c_ranked_newer) =
                catalog_testing::with_main_graph(&writer, &budget(), |h| {
                    let c_local = h.local(&c)?.expect("c is present after its commit");
                    Ok((c_local, h.order()?.rank(c_local).is_ok()))
                })?;
            let order = g.order()?;
            let a_local = g.local(&a)?.expect("a is present at g");
            Ok((
                g.local(&c)?,
                order.rank(a_local).is_ok(),
                order.rank(c_local).is_ok(),
                c_ranked_newer,
            ))
        })
        .unwrap();

    // Assert: the newer generation ranks c; the pinned query's order is g's (a, not c)
    assert_eq!(c_at_g, None, "c is not present at g");
    assert!(c_ranked_newer, "the newer generation's order ranks c");
    assert!(a_ranked, "g's order ranks a");
    assert!(
        !c_ranked,
        "the query pinned at g got a newer generation's tie order (it ranks c)"
    );
}
