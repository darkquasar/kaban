//! Rows moved between projects keep their graph (spec 11 T11 Step 3; REC-02, INV-2): the rows
//! of project P written through `apply_batch` into a fresh project Q keep every `NodeId` and
//! edge endpoint, and the same expansion returns the same `NodeId`s in both, whatever local ids
//! each project gave them.
//!
//! Runs with the `failpoints` feature (the graph's test access lives behind it).

#![cfg(feature = "failpoints")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use std::collections::BTreeSet;
use std::path::Path;
use std::time::Duration;

use common::{Scratch, created, ctx};
use kaban_core::query_budget::QueryBudget;
use kaban_core::{IngestStore, MemoryStore, NewNode, NodeId, Op, WorkspaceId, WriteBatch};
use kaban_index_graph::{Caps, Direction, RelSelection, expand};
use kaban_store_sqlite::SqliteStore;
use kaban_store_sqlite::catalog_testing;

fn budget() -> QueryBudget {
    QueryBudget::new(u64::MAX, Some(Duration::from_secs(10)))
}

/// Nodes `(id, kind)` in rid order and edges `(src id, dst id, rel)`.
type Rows = (Vec<(String, String)>, BTreeSet<(String, String, String)>);

/// Every node and edge of the database at `db`.
fn rows(db: &Path) -> Rows {
    let conn = rusqlite::Connection::open(db).unwrap();
    let mut stmt = conn
        .prepare("SELECT id, kind FROM node ORDER BY rid")
        .unwrap();
    let nodes = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    let mut stmt = conn
        .prepare(
            "SELECT s.id, d.id, e.rel FROM edge e JOIN node s ON s.rid = e.src
             JOIN node d ON d.rid = e.dst",
        )
        .unwrap();
    let edges = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    (nodes, edges)
}

/// The `NodeId`s an outgoing depth-2 expansion from `src` reaches in `store`, and `src`'s
/// local id there.
fn reached(store: &SqliteStore, src: &NodeId) -> (BTreeSet<NodeId>, u32) {
    catalog_testing::with_main_graph(store, &budget(), |g| {
        let s = g.local(src)?.unwrap();
        let caps = Caps {
            depth: 2,
            rows: 1_500,
        };
        let r = expand(
            g.view(),
            &[s],
            &RelSelection::All,
            Direction::Out,
            caps,
            &budget(),
        )
        .unwrap();
        let ids = r
            .nodes
            .iter()
            .map(|&(n, _)| g.node_id(n).unwrap().unwrap())
            .collect();
        Ok((ids, s.0))
    })
    .unwrap()
}

#[test]
fn rows_moved_into_a_fresh_project_keep_their_ids_endpoints_and_expansions() {
    // Arrange: project P with a small graph; project Q already holding one row, so the moved
    // rows take other local ids there
    let p = Scratch::new("kaban-t11-move", "p");
    let q = Scratch::new("kaban-t11-move", "q");
    let from = SqliteStore::open(&p.db(), &p.keys()).unwrap();
    let ids: Vec<NodeId> = (0..20).map(|_| NodeId::new()).collect();
    let mut ops: Vec<Op> = ids
        .iter()
        .enumerate()
        .map(|(i, id)| Op::UpsertNode {
            id: id.clone(),
            node: NewNode {
                kind: "Fact".into(),
                text: format!("moved {i}"),
                ..Default::default()
            },
        })
        .collect();
    ops.extend((0..20).flat_map(|i| {
        [(i, (i + 1) % 20), (i, (i * 7 + 3) % 20)]
            .into_iter()
            .filter(|&(a, b)| a != b)
            .map(|(a, b)| Op::UpsertEdge {
                src: ids[a].clone(),
                dst: ids[b].clone(),
                rel: "RELATES_TO".into(),
                props: None,
            })
    }));
    from.apply_batch(WriteBatch::new(WorkspaceId::default(), ops), &ctx())
        .unwrap();
    let into = SqliteStore::open(&q.db(), &q.keys()).unwrap();
    let resident = created(&into, "Fact", "already in Q");
    let (nodes, edges) = rows(&p.db());

    // Act: P's rows through Q's batch writer
    let mut moved: Vec<Op> = nodes
        .iter()
        .map(|(id, kind)| {
            let id = NodeId::parse(id).unwrap();
            let text = from.get(&id, true).unwrap().unwrap().text;
            Op::UpsertNode {
                id,
                node: NewNode {
                    kind: kind.clone(),
                    text,
                    ..Default::default()
                },
            }
        })
        .collect();
    moved.extend(edges.iter().map(|(s, d, rel)| Op::UpsertEdge {
        src: NodeId::parse(s).unwrap(),
        dst: NodeId::parse(d).unwrap(),
        rel: rel.clone(),
        props: None,
    }));
    into.apply_batch(WriteBatch::new(WorkspaceId::default(), moved), &ctx())
        .unwrap();

    // Assert: every NodeId and endpoint kept; each expansion reaches the same NodeIds from
    // another local id
    let (q_nodes, q_edges) = rows(&q.db());
    let q_ids: Vec<_> = q_nodes
        .into_iter()
        .filter(|(id, _)| *id != resident.id)
        .collect();
    assert_eq!(q_ids, nodes);
    assert_eq!(q_edges, edges);
    for id in &ids {
        let (in_p, local_p) = reached(&from, id);
        let (in_q, local_q) = reached(&into, id);
        assert!(!in_p.is_empty(), "{id:?} reaches nothing");
        assert_eq!(in_q, in_p, "{id:?}");
        assert_ne!(local_q, local_p, "the local ids differ");
    }
}
