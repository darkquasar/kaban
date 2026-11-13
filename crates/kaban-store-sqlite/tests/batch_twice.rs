//! EN-E10a idempotency (engine-plan E2.11 slice; E2.10 boundary review residual): applying the
//! same `WriteBatch` twice leaves the store in the same state. The state is a hash over the
//! rows the batch touches (nodes and edges), read back from the database, not serde equality
//! of anything in memory.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::{Scratch, ctx};
use kaban_core::{IngestStore, NewNode, NodeId, Op, WorkspaceId, WriteBatch};
use kaban_store_sqlite::SqliteStore;
use rusqlite::Connection;
use rusqlite::types::ValueRef;
use serde_json::json;
use std::hash::{DefaultHasher, Hash, Hasher};

/// The tables a batch writes, in a fixed order.
const TABLES: [&str; 2] = ["node", "edge"];

/// Every row of `table` as one line of its columns' SQL text, in `rowid` order.
fn dump(conn: &Connection, table: &str) -> Vec<String> {
    let mut stmt = conn
        .prepare(&format!("SELECT * FROM {table} ORDER BY rowid"))
        .unwrap();
    let n = stmt.column_count();
    stmt.query_map([], |r| {
        let mut line = String::new();
        for i in 0..n {
            let cell = match r.get_ref(i)? {
                ValueRef::Null => "NULL".to_owned(),
                ValueRef::Integer(x) => x.to_string(),
                ValueRef::Real(x) => x.to_string(),
                ValueRef::Text(t) => String::from_utf8_lossy(t).into_owned(),
                ValueRef::Blob(b) => format!("blob:{}", b.len()),
            };
            line.push_str(&cell);
            line.push('\u{1f}');
        }
        Ok(line)
    })
    .unwrap()
    .collect::<rusqlite::Result<Vec<_>>>()
    .unwrap()
}

/// One hash over the affected rows of every table, plus the per-table row counts for the
/// failure message.
fn state(s: &Scratch) -> (u64, Vec<(String, usize)>) {
    let conn = Connection::open(s.db()).unwrap();
    let mut h = DefaultHasher::new();
    let mut counts = Vec::new();
    for t in TABLES {
        let rows = dump(&conn, t);
        t.hash(&mut h);
        rows.hash(&mut h);
        counts.push((t.to_owned(), rows.len()));
    }
    (h.finish(), counts)
}

fn node(kind: &str, text: &str) -> NewNode {
    NewNode {
        kind: kind.into(),
        text: text.into(),
        ..Default::default()
    }
}

/// The two batches of the slice: the nodes and an edge, then producer edges over them (their
/// scoring travels in the props, as an extractor writes it).
fn batches() -> (WriteBatch, WriteBatch) {
    let ids: Vec<NodeId> = (0..4).map(|_| NodeId::new()).collect();
    let mut ops: Vec<Op> = ids
        .iter()
        .enumerate()
        .map(|(i, id)| Op::UpsertNode {
            id: id.clone(),
            node: node("Fact", &format!("Saffron ledger reconciliation\nbody {i}")),
        })
        .collect();
    ops.push(Op::UpsertEdge {
        src: ids[0].clone(),
        dst: ids[1].clone(),
        rel: "RELATES_TO".into(),
        props: None,
    });
    let nodes = WriteBatch::new(WorkspaceId::default(), ops);
    let scored = |src: &NodeId, dst: &NodeId, score: u32| Op::UpsertEdge {
        src: src.clone(),
        dst: dst.clone(),
        rel: "RELATES_TO".into(),
        props: Some(json!({"layer": "script", "method": "rule", "score": score})),
    };
    let scored = WriteBatch::new(
        WorkspaceId::default(),
        vec![
            scored(&ids[2], &ids[3], 9_000),
            scored(&ids[1], &ids[2], 6_587),
        ],
    );
    (nodes, scored)
}

#[test]
fn re_applying_the_same_batches_leaves_the_affected_rows_hash_unchanged() {
    let s = Scratch::new("e211-batch-twice", "idempotent");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let (nodes, scored) = batches();
    store.apply_batch(nodes.clone(), &ctx()).unwrap();
    store.apply_batch(scored.clone(), &ctx()).unwrap();
    let (first, counts) = state(&s);
    assert!(
        counts.iter().any(|(t, n)| t == "edge" && *n == 3),
        "the producer edges landed: {counts:?}"
    );
    store.apply_batch(nodes, &ctx()).unwrap();
    store.apply_batch(scored, &ctx()).unwrap();
    let (second, counts_after) = state(&s);
    assert_eq!(
        first, second,
        "re-applying the same batches changed the affected rows: {counts:?} -> {counts_after:?}"
    );
}

/// Negative: a different batch (two more edges) does move the hash, so the hash sees the rows
/// it claims to cover.
#[test]
fn a_different_batch_moves_the_hash() {
    let s = Scratch::new("e211-batch-twice", "moves");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let (nodes, scored) = batches();
    store.apply_batch(nodes, &ctx()).unwrap();
    let (before, _) = state(&s);
    store.apply_batch(scored, &ctx()).unwrap();
    let (after, _) = state(&s);
    assert_ne!(before, after);
}
