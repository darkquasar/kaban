//! The memory graph on the catalog (spec 11 T11 Step 1; GRA-02, GRA-03, KER-01): an edge is
//! traversable from every store of the database once its commit returns, with its row's
//! attributes, and a row that maps to no graph attribute refuses its write by name.
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
use kaban_index_graph::{Caps, Direction, RelSelection, expand};
use kaban_store_sqlite::catalog_testing;
use kaban_store_sqlite::{SqliteStore, WorkKind};
use std::time::Duration;

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
    use kaban_index_graph::{Attr, EdgeOp, LocalId, NodeOp, OpKind, Records, RelId, build_base};
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
