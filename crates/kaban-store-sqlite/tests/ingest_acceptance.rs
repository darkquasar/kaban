//! EN-F35: the public batch seam cannot overwrite accepted governed content.
#![allow(clippy::unwrap_used, clippy::expect_used)] // public behavior assertions

use kaban_core::{
    Actor, AuditStore, ChangeLogStore, EdgeDiff, Governance, GovernanceStore, IngestStore,
    MemoryStore, NewEvent, NewNode, NodeId, Op, StoreError, WorkspaceId, WriteBatch, WriteCtx,
};
use kaban_store_sqlite::SqliteStore;
use serde_json::json;

fn ctx() -> WriteCtx {
    WriteCtx::new("policy", WorkspaceId::default(), Actor::Agent)
}

fn upsert(id: &NodeId, text: &str) -> Op {
    Op::UpsertNode {
        id: id.clone(),
        node: NewNode {
            kind: "Decision".into(),
            text: text.into(),
            props: Some(json!({"rationale":"measured evidence"})),
            ..Default::default()
        },
    }
}

fn batch(ops: Vec<Op>) -> WriteBatch {
    let mut batch = WriteBatch::new(WorkspaceId::default(), ops);
    batch.edge_diff = EdgeDiff::None;
    batch
}

#[test]
fn accepted_content_mutation_refuses_and_rolls_back_the_whole_batch() {
    let store = SqliteStore::open_memory().unwrap();
    let accepted = NodeId::new();
    store
        .apply_batch(batch(vec![upsert(&accepted, "original")]), &ctx())
        .unwrap();
    store
        .transition(&accepted, Governance::Accepted, NewEvent::default(), &ctx())
        .unwrap();
    let before = store.get(&accepted, true).unwrap().unwrap();
    let events = store.events(&accepted).unwrap();
    let changes = store.changes_read(0, 10_000).unwrap();
    let audits = store.audit_verify().unwrap().entries;
    let sentinel = NodeId::new();
    let result = store.apply_batch(
        batch(vec![
            upsert(&sentinel, "sentinel"),
            upsert(&accepted, "changed"),
        ]),
        &ctx(),
    );
    assert!(
        matches!(result, Err(StoreError::InvalidTransition(_))),
        "accepted governed content must refuse in-place batch mutation: {result:?}"
    );
    assert!(store.get(&sentinel, true).unwrap().is_none());
    assert_eq!(
        serde_json::to_value(store.get(&accepted, true).unwrap().unwrap()).unwrap(),
        serde_json::to_value(before).unwrap()
    );
    assert_eq!(store.events(&accepted).unwrap(), events);
    assert_eq!(store.changes_read(0, 10_000).unwrap(), changes);
    assert_eq!(store.audit_verify().unwrap().entries, audits);
}

#[test]
fn proposed_content_still_updates_and_identical_accepted_content_is_a_noop() {
    let store = SqliteStore::open_memory().unwrap();
    let id = NodeId::new();
    store
        .apply_batch(batch(vec![upsert(&id, "original")]), &ctx())
        .unwrap();
    assert_eq!(
        store
            .apply_batch(batch(vec![upsert(&id, "changed")]), &ctx())
            .unwrap()
            .updated,
        1
    );
    store
        .transition(&id, Governance::Accepted, NewEvent::default(), &ctx())
        .unwrap();
    let before = store.get(&id, true).unwrap().unwrap();
    let result = store
        .apply_batch(batch(vec![upsert(&id, "changed")]), &ctx())
        .unwrap();
    assert_eq!(result.unchanged, 1);
    assert_eq!(result.outbox_first_seq, None);
    assert_eq!(
        serde_json::to_value(store.get(&id, true).unwrap().unwrap()).unwrap(),
        serde_json::to_value(before).unwrap()
    );
}

#[test]
fn superseded_and_invalidated_governed_content_is_not_rewritten_or_revived() {
    for terminal in [Governance::Superseded, Governance::Invalidated] {
        let store = SqliteStore::open_memory().unwrap();
        let id = NodeId::new();
        store
            .apply_batch(batch(vec![upsert(&id, "original")]), &ctx())
            .unwrap();
        store
            .transition(&id, Governance::Accepted, NewEvent::default(), &ctx())
            .unwrap();
        store
            .transition(&id, terminal, NewEvent::default(), &ctx())
            .unwrap();
        let before = serde_json::to_value(store.get(&id, true).unwrap().unwrap()).unwrap();
        let events = store.events(&id).unwrap();
        let changes = store.changes_read(0, 10_000).unwrap();
        assert!(matches!(
            store.apply_batch(batch(vec![upsert(&id, "changed")]), &ctx()),
            Err(StoreError::InvalidTransition(_))
        ));
        assert_eq!(
            serde_json::to_value(store.get(&id, true).unwrap().unwrap()).unwrap(),
            before
        );
        assert_eq!(store.events(&id).unwrap(), events);
        assert_eq!(store.changes_read(0, 10_000).unwrap(), changes);
    }
}
