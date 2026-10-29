//! `document_index_summary` (docs-ingest DI2.0 (c), DI2.5): the per-document counters are summed
//! over the active rows of the kind inside the workspace without decoding a node; a row without
//! the optional `table_unknown_chunks` key counts zero.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

use kaban_core::{Actor, IngestStore, NewNode, NodeId, Op, WorkspaceId, WriteBatch, WriteCtx};
use kaban_store_sqlite::SqliteStore;
use serde_json::json;

fn document(text: &str, props: serde_json::Value) -> Op {
    Op::UpsertNode {
        id: NodeId::new(),
        node: NewNode {
            kind: "Document".into(),
            text: text.into(),
            props: Some(props),
            ..Default::default()
        },
    }
}

/// DI2.5: a document with `table_unknown_chunks: 2` and one without the key sum to 2 beside the
/// other counters; an empty workspace sums to zero.
#[test]
fn table_unknown_chunks_sums_the_documents_that_carry_the_key() {
    // Arrange
    let store = SqliteStore::open_memory().unwrap();
    let ws = WorkspaceId::default();
    let ctx = WriteCtx::new("v1", ws.clone(), Actor::Agent);
    let batch = WriteBatch::new(
        ws.clone(),
        vec![
            document(
                "docs/rows.pdf",
                json!({"path": "docs/rows.pdf", "status": "ok", "chunk_ids": ["a", "b"],
                       "blocked_chunks": 1, "table_unknown_chunks": 2}),
            ),
            document(
                "docs/plain.md",
                json!({"path": "docs/plain.md", "status": "ok", "chunk_ids": ["c"]}),
            ),
        ],
    );

    // Act
    store.apply_batch(batch, &ctx).unwrap();
    let summary = store
        .document_index_summary("Document", ws.as_str())
        .unwrap();
    let empty = store.document_index_summary("Document", "other").unwrap();

    // Assert
    assert_eq!(summary.table_unknown_chunks, 2, "{summary:?}");
    assert_eq!(summary.blocked_chunks, 1);
    assert_eq!(summary.chunks, 3);
    assert_eq!(summary.by_status.get("ok"), Some(&2));
    assert_eq!(empty.table_unknown_chunks, 0, "{empty:?}");
}
