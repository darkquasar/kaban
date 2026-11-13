//! Bounded write-path plans and public behavior on invented data (OWN-120).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use common::{Scratch, created, ctx, nid};
use kaban_core::ActorId;
use kaban_core::{ForgetMode, MemoryStore, NewNode, RememberOutcome, WorkspaceId, WriteCtx};
use kaban_store_sqlite::SqliteStore;
use rusqlite::{Connection, ToSql, params};

fn plan(conn: &Connection, sql: &str, values: &[&dyn ToSql]) -> Vec<String> {
    conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
        .unwrap()
        .query_map(values, |row| row.get(3))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

#[test]
#[allow(clippy::print_stderr)] // Test-only query-plan evidence, never the MCP stdout channel.
// The claim-retirement plans went with the claim tables (T3); the duplicate lookup stays.
fn duplicate_query_uses_a_bounded_index_lookup() {
    let s = Scratch::new("s30-write-store", "plans");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    created(&store, "Fact", "plan source");
    created(&store, "Fact", "plan object");
    let conn = Connection::open(s.db()).unwrap();
    let duplicate = plan(
        &conn,
        "SELECT * FROM node WHERE lifecycle='active' AND workspace_id=?1 AND dup_key=?2 ORDER BY created_at, id",
        &[&"default", &"shared"],
    );
    eprintln!("find_dup_key: {duplicate:?}");
    assert!(
        duplicate
            .iter()
            .any(|detail| detail.contains("workspace_id=? AND dup_key=?")),
        "duplicate lookup must constrain both indexed coordinates: {duplicate:?}"
    );
}

#[test]
fn duplicate_lookup_is_nonunique_ordered_workspace_scoped_and_active_only() {
    let s = Scratch::new("s30-write-store", "duplicates");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let remember = |text: &str, workspace: &str, key: Option<&str>| {
        let write = WriteCtx::new("pv", WorkspaceId::new(workspace), kaban_core::Actor::Agent);
        match store
            .remember(
                NewNode {
                    kind: "Fact".into(),
                    text: text.into(),
                    workspace_id: Some(workspace.into()),
                    dup_key: key.map(str::to_owned),
                    ..Default::default()
                },
                &[],
                &write,
            )
            .unwrap()
        {
            RememberOutcome::Created(node) => node,
            other @ RememberOutcome::Existing { .. } => panic!("fresh fixture: {other:?}"),
        }
    };
    let a = remember("first duplicate", "default", Some("shared"));
    let b = remember("second duplicate", "default", Some("shared"));
    let archived = remember("archived duplicate", "default", Some("shared"));
    let hidden = remember("hidden duplicate", "default", Some("shared"));
    let other = remember("other tenant duplicate", "other", Some("shared"));
    remember("unkeyed note", "default", None);
    remember("different key", "default", Some("different"));
    store
        .forget(&nid(&archived.id), ForgetMode::Archive, &ctx())
        .unwrap();
    store
        .forget(&nid(&hidden.id), ForgetMode::Tombstone, &ctx())
        .unwrap();
    // Equal timestamps explicitly exercise the id tie-break without a timing assertion.
    let conn = Connection::open(s.db()).unwrap();
    SqliteStore::bind_writer(&conn, &ActorId::default()).unwrap();
    conn.execute(
        "UPDATE node SET created_at='2026-10-26T08:16:02Z' WHERE id IN (?1,?2)",
        params![a.id, b.id],
    )
    .unwrap();
    let mut expected = vec![a.id, b.id];
    expected.sort();
    let ids = |workspace| {
        store
            .find_dup_key(workspace, "shared")
            .unwrap()
            .into_iter()
            .map(|n| n.id)
            .collect::<Vec<_>>()
    };
    assert_eq!(ids("default"), expected);
    assert_eq!(ids("other"), vec![other.id]);
    assert!(store.find_dup_key("default", "missing").unwrap().is_empty());
}
