//! `node_anchor` maintained on write (engine-spec EN-F10; engine-plan E2.2; CG-JOIN v1 §5):
//! a remembered `CodeRef` and a scoped `Decision` produce the rows the 0004 backfill derives,
//! and a prefix-less glob produces the universal `""` row (E1.7a RE2-2). Invented data.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::{Scratch, ctx};
use kaban_core::{MemoryStore, NewNode, RememberOutcome};
use kaban_store_sqlite::SqliteStore;
use rusqlite::{Connection, OpenFlags};
use serde_json::json;

type Row = (String, String, Option<String>, Option<String>);

fn anchors(db: &std::path::Path, id: &str) -> Vec<Row> {
    let conn = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    conn.prepare(
        "SELECT a.anchor_kind, a.key, a.repo, a.snapshot_scope FROM node_anchor a JOIN node n ON n.rid = a.rid
         WHERE n.id = ?1 ORDER BY a.anchor_kind, a.key",
    )
    .unwrap()
    .query_map([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
    .unwrap()
    .collect::<rusqlite::Result<_>>()
    .unwrap()
}

fn created(store: &SqliteStore, n: NewNode) -> String {
    match store.remember(n, &[], &ctx()).unwrap() {
        RememberOutcome::Created(n) => n.id,
        RememberOutcome::Existing { node, .. } => panic!("fresh store: {node:?}"),
    }
}

#[test]
fn a_remembered_coderef_and_a_scoped_decision_produce_the_backfill_rows() {
    let s = Scratch::new("e22-anchors", "rows");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let scope40 = "0123456789ABCDEF0123456789abcdef01234567";
    let coderef = created(
        &store,
        NewNode {
            kind: "CodeRef".into(),
            text: "repo src/api/recall.rs recall".into(),
            repo: Some("repo".into()),
            props: Some(json!({
                "relative_path": "src/api/recall.rs", "name_path": "recall",
                "symbol": "crate::api::recall", "snapshot_commit": scope40,
            })),
            ..Default::default()
        },
    );
    let scope = Some(scope40.to_ascii_lowercase());
    assert_eq!(
        anchors(&s.db(), &coderef),
        vec![
            (
                "path".into(),
                "src/api/recall.rs".into(),
                Some("repo".into()),
                scope.clone()
            ),
            (
                "symbol".into(),
                "crate::api::recall".into(),
                Some("repo".into()),
                scope
            ),
        ]
    );
    let decision = created(
        &store,
        NewNode {
            kind: "Decision".into(),
            text: "every Rust file follows the budget".into(),
            repo: Some("repo".into()),
            path_scope: vec!["src/api/**".into(), "**/*.rs".into()],
            props: Some(json!({"decision": "budget", "rationale": "fixture"})),
            ..Default::default()
        },
    );
    assert_eq!(
        anchors(&s.db(), &decision),
        vec![
            ("path".into(), String::new(), Some("repo".into()), None),
            ("path".into(), "src/api/".into(), Some("repo".into()), None),
        ],
        "one prefix row per glob; the universal row for the prefix-less glob"
    );
    // a plain fact anchors nothing
    let fact = created(
        &store,
        NewNode {
            kind: "Fact".into(),
            text: "a fact without pointers".into(),
            ..Default::default()
        },
    );
    assert_eq!(anchors(&s.db(), &fact).len(), 0);
}
