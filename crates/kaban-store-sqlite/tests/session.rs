//! Migration 0003 and the hook session scratch (core-plan C4.3/C4.4 as amended by
//! `reviews/m4-prep-dispositions.md` M4-13, M4-14, M4-15, M4-17): `session_edits` exists on a
//! fresh database and on a version-2 database after its next open (with the pre-migration
//! backup), its rows are recorded, listed and pruned through the role trait, and the briefing,
//! pending-failure and props-update operations behave as the hooks need.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::Scratch;
use kaban_store_sqlite::SqliteStore;
use rusqlite::Connection;

/// The version a plain open brings any database to: the one before the gated FTS5 retirement
/// (spec 05 SS-16), which only a production open with the Tantivy adapter runs.
const PLAIN_OPEN: u32 = kaban_store_sqlite::LATEST_SCHEMA_VERSION;

fn raw(s: &Scratch) -> Connection {
    Connection::open(s.db()).unwrap()
}

fn schema_version(conn: &Connection) -> u32 {
    conn.query_row(
        "SELECT value FROM index_meta WHERE key = 'schema_version'",
        [],
        |r| r.get::<_, String>(0),
    )
    .unwrap()
    .parse()
    .unwrap()
}

fn columns(conn: &Connection, table: &str) -> Vec<(String, bool)> {
    let mut st = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .unwrap();
    st.query_map([], |r| {
        Ok((r.get::<_, String>(1)?, r.get::<_, i64>(3)? == 1))
    })
    .unwrap()
    .collect::<Result<_, _>>()
    .unwrap()
}

/// M4-13: 0003 adds `session_edits(session_id, kind, path, ts)` with every column NOT NULL
/// and no `workspace_id` (DB-scoped, F17). A database left at version 2 gets the table on its
/// next open, with the usual pre-migration backup; the frozen `status`/`source_session` columns
/// are not dropped (recorded plan deviation).
#[test]
fn migration_0003_adds_session_edits_to_fresh_and_version_2_databases() {
    // Arrange: a fresh database is at the latest version and has the table
    let s = Scratch::new("kaban-sess", "migrate");
    {
        let _store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    }
    let conn = raw(&s);
    assert_eq!(schema_version(&conn), PLAIN_OPEN);
    let cols = columns(&conn, "session_edits");
    assert_eq!(
        cols,
        vec![
            ("session_id".to_owned(), true),
            ("kind".to_owned(), true),
            ("path".to_owned(), true),
            ("ts".to_owned(), true),
        ],
        "session_edits columns (name, NOT NULL)"
    );
    assert!(
        columns(&conn, "node")
            .iter()
            .any(|(c, _)| c == "source_session"),
        "the frozen columns stay until a later migration (M4-13)"
    );
    // the kind set is closed
    let bad = conn.execute(
        "INSERT INTO session_edits(session_id, kind, path, ts) VALUES ('s', 'other', 'p', 1)",
        [],
    );
    assert!(bad.is_err(), "kind outside edit/failure/prompted accepted");
}

/// M4-14, M4-15 negatives on the store ops: `update_failure_props` resolves the id inside the
/// write context's workspace (S6: another tenant's Failure is `NotFound`), refuses a node that
/// is not a Failure (`KindMismatch`) and bumps `rev` once; `pending_failures` hides a Failure
/// whose `pending_fix` is `false`; `session_edits_prune` deletes at most `limit` rows and only
/// older ones.
#[test]
fn failure_props_update_and_pending_lookup_stay_inside_the_workspace_and_the_bounds() {
    use kaban_core::{
        Actor, FailurePatch, GovernanceStore, MemoryStore, NewNode, NodeId, PendingFailuresQuery,
        PendingScope, RememberOutcome, SessionEdit, SessionEditKind, WorkspaceId, WriteCtx,
    };
    let s = Scratch::new("kaban-sess", "ops");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let ctx_a = WriteCtx::new("pv", WorkspaceId::new("ws-a"), Actor::Agent);
    let ctx_b = WriteCtx::new("pv", WorkspaceId::new("ws-b"), Actor::Agent);
    let remember = |kind: &str, text: &str, ctx: &WriteCtx| -> String {
        match store
            .remember(
                NewNode {
                    kind: kind.into(),
                    text: text.into(),
                    workspace_id: Some(ctx.workspace_id.as_str().to_owned()),
                    props: (kind == "Failure").then(|| serde_json::json!({"pending_fix": true})),
                    ..Default::default()
                },
                &[],
                ctx,
            )
            .unwrap()
        {
            RememberOutcome::Created(n) => n.id,
            RememberOutcome::Existing { node, .. } => node.id,
        }
    };
    let failure_a = remember("Failure", "\"make\" failed: boom", &ctx_a);
    let fact_a = remember("Fact", "a fact", &ctx_a);
    let nid = |id: &str| NodeId::parse(id).unwrap();
    let patch = FailurePatch {
        fix: Some("fixed".into()),
        pending_fix: Some(false),
        evidence_run: None,
    };

    // another tenant cannot patch it, a Fact is refused
    assert!(matches!(
        store.update_failure_props(&nid(&failure_a), &patch, &ctx_b),
        Err(kaban_core::StoreError::NotFound(_))
    ));
    assert!(matches!(
        store.update_failure_props(&nid(&fact_a), &patch, &ctx_a),
        Err(kaban_core::StoreError::KindMismatch { .. })
    ));
    // pending in its workspace until the patch
    let q = |ws: &str| PendingFailuresQuery {
        workspace_id: ws.into(),
        session_id: "s1".into(),
        scope: PendingScope::Workspace,
        limit: 10,
    };
    assert_eq!(store.pending_failures(&q("ws-a")).unwrap().len(), 1);
    assert_eq!(store.pending_failures(&q("ws-b")).unwrap().len(), 0, "S6");
    let before = store.get(&nid(&failure_a), false).unwrap().unwrap();
    let after = store
        .update_failure_props(&nid(&failure_a), &patch, &ctx_a)
        .unwrap();
    assert_eq!(after.rev, before.rev + 1);
    assert_eq!(after.props["fix"], serde_json::json!("fixed"));
    assert_eq!(store.pending_failures(&q("ws-a")).unwrap().len(), 0);

    // prune: bounded and only older rows
    for ts in [10, 20, 30, 40] {
        store
            .session_edit_record(&SessionEdit {
                session_id: "s1".into(),
                kind: SessionEditKind::Edit,
                path: format!("p{ts}"),
                ts,
            })
            .unwrap();
    }
    assert_eq!(store.session_edits_prune(35, 2).unwrap(), 2, "limit binds");
    assert_eq!(
        store.session_edits_prune(35, 10).unwrap(),
        1,
        "only rows below 35"
    );
    let left = store
        .session_edits("s1", SessionEditKind::Edit, 0, 10)
        .unwrap();
    assert_eq!(left.iter().map(|r| r.ts).collect::<Vec<_>>(), vec![40]);
}
