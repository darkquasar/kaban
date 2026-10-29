//! `kaban://documents` pagination and the `index_status` aggregates over a 1,000-document store
//! (docs-ingest-plan.md §16 item 3 (c); DI2.0): a page is read as a page through the store role
//! (id order, `offset` skipped, `limit` capped by the service maximum), the totals come from the
//! status aggregates, and a page past the end is empty while the totals stay.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

use kaban_core::{Actor, IngestStore, NewNode, Op, WorkspaceId, WriteBatch, WriteCtx};
use kaban_ingest::{documents_resource, index_status};
use kaban_store_sqlite::SqliteStore;
use serde_json::json;
use std::sync::Arc;

const N: usize = 1_000;

/// The service maximum for one page (`IngestStore::list_nodes` clamps there too).
const PAGE_CAP: usize = 500;

fn ws() -> WorkspaceId {
    WorkspaceId::new("paging")
}

/// `N` Document nodes in one batch, written in an order that is not the path order (ids are
/// time-ordered, so id order and path order differ): every tenth is `failed` (with an error)
/// and every tenth is `blocked` (two blocked chunks, one rule hit); `i % 3` chunk ids each.
fn seed() -> Arc<SqliteStore> {
    let store = Arc::new(SqliteStore::open_memory().unwrap());
    let ops = (0..N)
        .map(|slot| {
            let i = (slot * 7919) % N; // a permutation of 0..N (7919 is prime)
            let status = match i % 10 {
                7 => "failed",
                3 => "blocked",
                _ => "ok",
            };
            let mut props = json!({
                "source_id": format!("src-{i}"),
                "path": format!("docs/{i:04}.md"),
                "status": status,
                "title": format!("Doc {i}"),
                "lang": "en",
                "chunk_ids": (0..i % 3).map(|c| format!("chunk-{i}-{c}")).collect::<Vec<_>>(),
            });
            if status == "failed" {
                props["error"] = json!(format!("boom {i}"));
            }
            if status == "blocked" {
                props["blocked_chunks"] = json!(2);
                props["rule_ids"] = json!({"aws-access-key-id": 1});
            }
            Op::UpsertNode {
                id: kaban_core::NodeId::new(),
                node: NewNode {
                    kind: "Document".into(),
                    props: Some(props),
                    ..NewNode::default()
                },
            }
        })
        .collect();
    let ctx = WriteCtx::new("policy-v0", ws(), Actor::Agent);
    store.apply_batch(WriteBatch::new(ws(), ops), &ctx).unwrap();
    store
}

fn paths(docs: &[kaban_api::DocumentSummary]) -> Vec<&str> {
    docs.iter().map(|d| d.path.as_str()).collect()
}

/// `offset=990&limit=10` is the last page: ten rows, in the store's id order (the same ten the
/// role's page method returns), nothing more after it.
#[test]
fn the_last_page_is_the_stores_own_page_in_id_order() {
    let store = seed();

    let body = documents_resource(store.as_ref(), &ws(), 990, 10).unwrap();

    assert_eq!(
        (body.offset, body.limit, body.more),
        (990, 10, false),
        "{body:?}"
    );
    assert_eq!(body.documents.len(), 10, "{body:?}");
    let expected: Vec<String> = store
        .list_nodes("Document", ws().as_str(), 990, 10)
        .unwrap()
        .iter()
        .map(|n| n.props["path"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        paths(&body.documents),
        expected,
        "the page is the role's page, in its order"
    );
    let first = &body.documents[0];
    assert_eq!(first.lang.as_deref(), Some("en"));
    assert!(
        first.title.as_deref().unwrap().starts_with("Doc "),
        "{first:?}"
    );
}

/// A page past the end is empty, says nothing follows, and still carries the totals; the first
/// page says more follows.
#[test]
fn a_page_past_the_end_is_empty_and_keeps_the_totals() {
    let store = seed();

    let past = documents_resource(store.as_ref(), &ws(), N, 10).unwrap();
    let first = documents_resource(store.as_ref(), &ws(), 0, 10).unwrap();

    assert!(past.documents.is_empty(), "{past:?}");
    assert!(!past.more);
    let total: u32 = past.status.documents_by_status.values().sum();
    assert_eq!(total, u32::try_from(N).unwrap(), "{:?}", past.status);
    assert_eq!(first.documents.len(), 10);
    assert!(first.more, "{first:?}");
}

/// `limit` is capped at the service maximum (500), reported as capped, and a page of that size
/// still leaves more.
#[test]
fn limit_is_capped_at_the_service_maximum() {
    let store = seed();

    let body = documents_resource(store.as_ref(), &ws(), 0, 9_999).unwrap();

    assert_eq!(body.limit, PAGE_CAP, "{body:?}");
    assert_eq!(body.documents.len(), PAGE_CAP);
    assert!(body.more);
}

/// The status aggregates over the 1,000 documents: counts by status, chunk sums, blocked chunks,
/// rule hits and the failed list with its errors, the same through `index_status` and through
/// the resource.
#[test]
fn the_status_aggregates_cover_every_document() {
    let store = seed();

    let status = index_status(store.as_ref(), &ws()).unwrap();
    let body = documents_resource(store.as_ref(), &ws(), 0, 1).unwrap();

    assert_eq!(
        status.documents_by_status,
        [("ok", 800u32), ("failed", 100), ("blocked", 100)]
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v))
            .collect()
    );
    // sum of i % 3 over 0..1000
    assert_eq!(status.chunks, 999);
    assert_eq!(status.blocked_chunks, 200);
    assert_eq!(status.rule_ids.get("aws-access-key-id"), Some(&100));
    let mut failed: Vec<(String, String)> = status
        .failed
        .iter()
        .map(|f| (f.path.clone(), f.error.clone()))
        .collect();
    failed.sort();
    let expected: Vec<(String, String)> = (0..N)
        .filter(|i| i % 10 == 7)
        .map(|i| (format!("docs/{i:04}.md"), format!("boom {i}")))
        .collect();
    assert_eq!(failed, expected);
    assert_eq!(status.last_run, None);
    assert_eq!(body.status, status);
}
