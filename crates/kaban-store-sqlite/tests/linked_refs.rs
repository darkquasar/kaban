//! `GraphStore::linked_refs` (core-spec F19; M3 review R3-10): the `CodeRef` pointers of a list
//! of owners in one bounded query. A pointer is the target of an `ABOUT` edge from the owner or
//! the source of an `EVIDENCE_FOR` edge into it; it must be an active `CodeRef` of the owner's
//! workspace. Rows come back in owner order, then edge order, each pointer once, at most `limit`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::{created, ctx, nid};
use kaban_core::ActorId;
use kaban_core::{
    Actor, ForgetMode, MemoryStore, NewNode, NodeId, RememberOutcome, WorkspaceId, WriteCtx,
};
use kaban_store_sqlite::SqliteStore;

fn link(store: &SqliteStore, src: &str, dst: &str, rel: &str) {
    store
        .link(&nid(src), &nid(dst), rel, None, None, &ctx())
        .unwrap();
}

fn paths(nodes: &[kaban_core::Node]) -> Vec<&str> {
    nodes.iter().map(|n| n.text.as_str()).collect()
}

#[test]
fn pointers_of_several_owners_come_in_owner_then_edge_order_each_once_and_bounded() {
    let store = SqliteStore::open_memory().unwrap();
    let a = created(&store, "Decision", "owner a");
    let b = created(&store, "Fact", "owner b");
    let lonely = created(&store, "Fact", "owner without pointers");
    let p1 = created(&store, "CodeRef", "src/p1.rs");
    let p2 = created(&store, "CodeRef", "src/p2.rs");
    let p3 = created(&store, "CodeRef", "src/p3.rs");
    let shared = created(&store, "CodeRef", "src/shared.rs");
    let entity = created(&store, "Entity", "an entity, not a pointer");
    let gone = created(&store, "CodeRef", "src/gone.rs");
    // a: ABOUT p1, EVIDENCE_FOR from shared, ABOUT entity (wrong kind), ABOUT gone (tombstoned)
    link(&store, &a.id, &p1.id, "ABOUT");
    link(&store, &shared.id, &a.id, "EVIDENCE_FOR");
    link(&store, &a.id, &entity.id, "ABOUT");
    link(&store, &a.id, &gone.id, "ABOUT");
    store
        .forget(&nid(&gone.id), ForgetMode::Tombstone, &ctx())
        .unwrap();
    // b: ABOUT p2 and p3 (p3 linked first), ABOUT shared again, RELATES_TO p1 (wrong rel)
    link(&store, &b.id, &p3.id, "ABOUT");
    link(&store, &b.id, &p2.id, "ABOUT");
    link(&store, &b.id, &shared.id, "ABOUT");
    link(&store, &b.id, &p1.id, "RELATES_TO");

    let owners: Vec<NodeId> = [&b.id, &lonely.id, &a.id]
        .iter()
        .map(|id| nid(id))
        .collect();
    let refs = store.linked_refs(&owners, 64).unwrap();
    assert_eq!(
        paths(&refs),
        vec!["src/p3.rs", "src/p2.rs", "src/shared.rs", "src/p1.rs"],
        "b's pointers in edge order, then a's (shared already seen); entity, gone and the \
         RELATES_TO neighbour excluded"
    );

    // bounded: the limit cuts the row stream in that order
    let refs = store.linked_refs(&owners, 2).unwrap();
    assert_eq!(paths(&refs), vec!["src/p3.rs", "src/p2.rs"]);

    // no owners, or owners without pointers: nothing
    assert!(store.linked_refs(&[], 64).unwrap().is_empty());
    assert!(
        store
            .linked_refs(&[nid(&lonely.id)], 64)
            .unwrap()
            .is_empty()
    );
    assert!(
        store
            .linked_refs(&[nid("01a17d72-2e84-7000-8000-000000000000")], 64)
            .unwrap()
            .is_empty()
    );
}

/// S6: a pointer of another workspace never surfaces, even through a legacy edge row written
/// before cross-workspace links were refused (R3-3).
#[test]
fn a_pointer_in_another_workspace_is_not_a_pointer_of_the_owner() {
    let s = common::Scratch::new("kaban-linked-refs", "ws");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let owner = created(&store, "Fact", "owner in the default workspace");
    let other_ctx = WriteCtx::new("pv", WorkspaceId::new("other"), Actor::Agent);
    let foreign = match store
        .remember(
            NewNode {
                kind: "CodeRef".into(),
                text: "src/foreign.rs".into(),
                workspace_id: Some("other".into()),
                ..Default::default()
            },
            &[],
            &other_ctx,
        )
        .unwrap()
    {
        RememberOutcome::Created(n) => n,
        RememberOutcome::Existing { node, .. } => panic!("fresh store: {node:?}"),
    };
    // the write path refuses the edge (R3-3); write the legacy row raw
    {
        let conn = rusqlite::Connection::open(s.db()).unwrap();
        SqliteStore::bind_writer(&conn, &ActorId::default()).unwrap();
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        conn.execute(
            "INSERT INTO edge(id, src, dst, rel, created_at, workspace_id) \
             SELECT 'legacy-edge', s.rid, d.rid, 'ABOUT', '2026-10-26T08:16:02Z', s.workspace_id \
             FROM node s, node d WHERE s.id = ?1 AND d.id = ?2",
            [&owner.id, &foreign.id],
        )
        .unwrap();
    }
    let refs = store.linked_refs(&[nid(&owner.id)], 64).unwrap();
    assert!(refs.is_empty(), "{refs:?}");
}
