//! The CG-JOIN v1 §5 anchor lifecycle slices (SO-2, SO-13; engine-plan E2.2; v4 CG5.3 reruns
//! these names) through the public API: `remember`, `apply_batch` (the `IngestStore` role
//! trait), `forget` and the M5 import (`SyncStore::import_record`). The rows are read back
//! on a second read-only connection. Invented data.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::Scratch;
use kaban_api::{ApiConfig, ForgetRequest, KabanApi, RememberRequest};
use kaban_core::{
    Actor, ForgetMode, Governance, ImportedRecord, IngestStore, Lifecycle, MemoryStore, NewNode,
    NodeId, Op, SyncStore, WorkspaceId, WriteBatch, WriteCtx,
};
use kaban_policy::Policy;
use kaban_store_sqlite::SqliteStore;
use rusqlite::{Connection, OpenFlags};
use serde_json::{Value, json};
use std::sync::Arc;

/// `(rowid, anchor_kind, key, repo, snapshot_scope)` of one `node_anchor` row.
type AnchorRow = (i64, String, String, Option<String>, Option<String>);

const SCOPE40: &str = "0123456789ABCDEF0123456789abcdef01234567";

struct Fixture {
    scratch: Scratch,
    store: Arc<SqliteStore>,
    api: KabanApi,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let scratch = Scratch::new("e22-anchor-lifecycle", name);
        let store = Arc::new(SqliteStore::open(&scratch.db(), &scratch.keys()).unwrap());
        let api = KabanApi::with_config(
            store.clone(),
            Policy::empty(),
            store.signer(),
            None,
            ApiConfig {
                workspace: WorkspaceId::default(),
                repo_root: None,
                repo: Some("repo".into()),
                record_signer_identity: false,
                engine: Arc::default(),
            },
        );
        Self {
            scratch,
            store,
            api,
        }
    }
    fn remember_coderef(&self, text: &str, symbol: &str) -> String {
        self.api
            .remember(RememberRequest {
                kind: "CodeRef".into(),
                text: text.into(),
                props: Some(coderef_props(symbol)),
                ..Default::default()
            })
            .unwrap()
            .id
    }
    /// `(rowid, anchor_kind, key, repo, snapshot_scope)` of the node's rows.
    fn anchors(&self, id: &str) -> Vec<AnchorRow> {
        let conn = Connection::open_with_flags(self.scratch.db(), OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
        conn.prepare(
            "SELECT a.rowid, a.anchor_kind, a.key, a.repo, a.snapshot_scope FROM node_anchor a
             JOIN node n ON n.rid = a.rid WHERE n.id = ?1 ORDER BY a.anchor_kind, a.key",
        )
        .unwrap()
        .query_map([id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
    }
    fn foreign_key_violations(&self) -> i64 {
        let conn = Connection::open_with_flags(self.scratch.db(), OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
        conn.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| {
            r.get(0)
        })
        .unwrap()
    }
}

fn coderef_props(symbol: &str) -> Value {
    json!({
        "repo": "repo", "commit": SCOPE40, "tool": "fixture",
        "relative_path": "src/api/recall.rs", "name_path": "recall", "kind": "Function", "line": 7,
        "symbol": symbol, "snapshot_commit": SCOPE40,
    })
}

/// A `remember` that changes a `CodeRef`'s `symbol` (the batch write path under its id) deletes
/// the old `symbol` row and writes the new one in the same transaction; the `path` row is
/// untouched (same rowid, same bytes).
#[test]
fn anchor_update_rewrites_key() {
    let f = Fixture::new("update");
    let id = f.remember_coderef("repo src/api/recall.rs recall", "crate::api::recall");
    let before = f.anchors(&id);
    assert_eq!(before.len(), 2);
    let path_row = before.iter().find(|r| r.1 == "path").unwrap().clone();
    let node = f
        .store
        .get(&NodeId::parse(&id).unwrap(), false)
        .unwrap()
        .unwrap();
    f.store
        .apply_batch(
            WriteBatch::new(
                WorkspaceId::default(),
                vec![Op::UpsertNode {
                    id: NodeId::parse(&id).unwrap(),
                    node: NewNode {
                        kind: "CodeRef".into(),
                        text: node.text.clone(),
                        repo: node.repo.clone(),
                        props: Some(coderef_props("crate::api::recall_v2")),
                        ..Default::default()
                    },
                }],
            ),
            &WriteCtx::new("v1", WorkspaceId::default(), Actor::Agent),
        )
        .unwrap();
    let after = f.anchors(&id);
    assert_eq!(after.len(), 2);
    let symbols: Vec<&str> = after
        .iter()
        .filter(|r| r.1 == "symbol")
        .map(|r| r.2.as_str())
        .collect();
    assert_eq!(
        symbols,
        vec!["crate::api::recall_v2"],
        "old symbol row gone, new one written"
    );
    assert_eq!(
        after.iter().find(|r| r.1 == "path").unwrap(),
        &path_row,
        "the path row is untouched"
    );
}

/// `forget` (tombstone) and `purge` remove every anchor row of the node;
/// `PRAGMA foreign_key_check` stays empty.
#[test]
fn anchor_removed_on_forget_and_purge() {
    let f = Fixture::new("forget-purge");
    let a = f.remember_coderef("repo src/a.rs a", "crate::a");
    let b = f.remember_coderef("repo src/b.rs b", "crate::b");
    assert_eq!((f.anchors(&a).len(), f.anchors(&b).len()), (2, 2));
    let r = f
        .api
        .forget(ForgetRequest {
            id: a.clone(),
            mode: Some(ForgetMode::Tombstone),
        })
        .unwrap();
    assert_eq!(r.lifecycle, Lifecycle::Tombstoned);
    assert!(
        f.anchors(&a).is_empty(),
        "a tombstoned pointer never joins a snapshot"
    );
    assert_eq!(f.anchors(&b).len(), 2, "the other node is untouched");
    let r = f
        .api
        .forget(ForgetRequest {
            id: b.clone(),
            mode: Some(ForgetMode::Purge),
        })
        .unwrap();
    assert_eq!(r.lifecycle, Lifecycle::Purged);
    assert_eq!(f.anchors(&b).len(), 0, "a purged node has no anchor rows");
    assert_eq!(f.foreign_key_violations(), 0);
}

/// A `CodeRef` arriving through the M5 import path gets the same rows as one from `remember`,
/// byte for byte (apart from the node it hangs on).
#[test]
fn anchor_imported_coderef_rows() {
    let f = Fixture::new("import");
    let remembered = f.remember_coderef("repo src/api/recall.rs recall", "crate::api::recall");
    let imported = NodeId::new();
    let now = kaban_core::now_iso();
    f.store
        .import_record(
            &ImportedRecord {
                id: imported.clone(),
                kind: "CodeRef".into(),
                text: "repo src/api/recall.rs recall (imported)".into(),
                summary: None,
                tags: vec![],
                confidence: None,
                importance: None,
                lifecycle: Lifecycle::Active,
                governance: Governance::None,
                path_scope: vec![],
                props: coderef_props("crate::api::recall"),
                rev: 1,
                created_at: now.clone(),
                updated_at: now,
                valid_from: None,
                valid_to: None,
                accepted_commit: None,
                agent_id: None,
                session_id: None,
                repo: Some("repo".into()),
                commit_sha: None,
                dup_key: None,
                event: None,
            },
            &WriteCtx::new("v1", WorkspaceId::default(), Actor::Agent),
        )
        .unwrap();
    let strip = |rows: Vec<AnchorRow>| {
        rows.into_iter()
            .map(|(_, k, key, repo, scope)| (k, key, repo, scope))
            .collect::<Vec<_>>()
    };
    let a = strip(f.anchors(&remembered));
    let b = strip(f.anchors(imported.as_str()));
    assert_eq!(a.len(), 2);
    assert_eq!(a, b, "import and remember derive identical rows");
}

/// SO-2: a `CodeRef` with `symbol` and `snapshot_commit` yields two rows with the same
/// 40-hex lowercase `snapshot_scope` and the same `repo`.
#[test]
fn anchor_path_scope_preserved_on_both_rows() {
    let f = Fixture::new("scope");
    let id = f.remember_coderef("repo src/api/recall.rs recall", "crate::api::recall");
    let rows = f.anchors(&id);
    assert_eq!(rows.len(), 2);
    let scope = Some(SCOPE40.to_ascii_lowercase());
    for (_, kind, key, repo, snapshot_scope) in &rows {
        assert_eq!(repo.as_deref(), Some("repo"), "{kind} {key}");
        assert_eq!(snapshot_scope, &scope, "{kind} {key}");
    }
    assert_eq!(rows[0].1, "path");
    assert_eq!(rows[0].2, "src/api/recall.rs");
    assert_eq!(rows[1].1, "symbol");
    assert_eq!(rows[1].2, "crate::api::recall");
}
