//! `apply_batch` (core-plan C2.6; core-spec F16 store side, F17, N5; MD-23, D-16, D-17, WD-1..WD-7):
//! one ingestion batch is one transaction; unchanged nodes write nothing; edges are diffed per
//! upserted node; tombstones are idempotent.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use kaban_core::ActorId;
use kaban_core::{
    Actor, AuditStore, BatchAssertion, BatchSubject, ChangeLogStore, ClaimClass, EdgeDiff, Fixed,
    GovernanceStore, IngestStore, Layer, MemoryStore, Method, NewNode, NodeId, Op, Rank,
    StoreError, StoreScope, WorkspaceId, WriteBatch, WriteCtx,
};
use kaban_store_sqlite::SqliteStore;

fn ctx() -> WriteCtx {
    WriteCtx::new("v1", WorkspaceId::default(), Actor::Agent)
}

fn node(kind: &str, text: &str) -> NewNode {
    NewNode {
        kind: kind.into(),
        text: text.into(),
        ..Default::default()
    }
}

fn batch(ops: Vec<Op>) -> WriteBatch {
    WriteBatch::new(WorkspaceId::default(), ops)
}

fn upsert(id: &NodeId, kind: &str, text: &str) -> Op {
    Op::UpsertNode {
        id: id.clone(),
        node: node(kind, text),
    }
}

fn edge(src: &NodeId, dst: &NodeId, rel: &str) -> Op {
    Op::UpsertEdge {
        src: src.clone(),
        dst: dst.clone(),
        rel: rel.into(),
        props: None,
    }
}

fn changes_count(store: &SqliteStore) -> usize {
    store.changes_read(0, 10_000).unwrap().len()
}

/// Plan slice 1: a batch with one failing op (an unknown rel) rolls back entirely: the valid
/// node before it is not stored, no changes row and no audit row is left behind (D-17).
#[test]
fn a_batch_with_one_failing_op_rolls_back_entirely() {
    let store = SqliteStore::open_memory().unwrap();
    let doc = NodeId::new();
    let other = NodeId::new();
    let changes_before = changes_count(&store);
    let before_audit = store.audit_verify().unwrap().entries;
    let mut ctx = ctx();
    ctx.audit.push(kaban_core::AuditEntry {
        op: kaban_core::AuditOp::Redact,
        tool: "ingest".into(),
        field: "text".into(),
        workspace_id: WorkspaceId::default(),
        policy_version: "v1".into(),
        input_hmac: "stub".into(),
        rules: vec!["old-password".into()],
        node_id: None,
    });
    let err = store
        .apply_batch(
            batch(vec![
                upsert(&doc, "Fact", "a valid node before the failing op"),
                upsert(&other, "Fact", "another valid node"),
                edge(&doc, &other, "NO_SUCH_REL"),
            ]),
            &ctx,
        )
        .expect_err("unknown rel fails the batch");
    assert!(
        matches!(err, StoreError::UnknownRel(ref r) if r == "NO_SUCH_REL"),
        "{err:?}"
    );
    assert!(
        store.get(&doc, true).unwrap().is_none(),
        "the node written before the failing op was rolled back"
    );
    assert!(store.get(&other, true).unwrap().is_none());
    assert_eq!(
        changes_count(&store),
        changes_before,
        "no changes row survives the rollback"
    );
    assert_eq!(
        store.audit_verify().unwrap().entries,
        before_audit,
        "the batch's audit rows are in the same transaction and rolled back with it"
    );
    // the same batch without the bad edge lands, with its audit row (D-17)
    let report = store
        .apply_batch(
            batch(vec![
                upsert(&doc, "Fact", "a valid node before the failing op"),
                upsert(&other, "Fact", "another valid node"),
                edge(&doc, &other, "RELATES_TO"),
            ]),
            &ctx,
        )
        .unwrap();
    assert_eq!(
        (report.inserted, report.updated, report.unchanged),
        (2, 0, 0)
    );
    assert_eq!(store.audit_verify().unwrap().entries, before_audit + 1);
    assert_eq!(store.get(&doc, false).unwrap().unwrap().id, doc.as_str());
    // 2 node inserts + 1 edge insert = 3 changes rows, reported as the batch's range
    assert_eq!(changes_count(&store), changes_before + 3);
    let (first, last) = (
        report.outbox_first_seq.unwrap(),
        report.outbox_last_seq.unwrap(),
    );
    assert_eq!(last - first, 2, "{report:?}");
    assert_eq!(
        store.changes_read(first - 1, 10).unwrap().len(),
        3,
        "the reported range is exactly the batch's rows"
    );
}

/// Plan slice 2 (N5 idempotent re-runs): re-applying a batch whose node content is unchanged
/// writes no row (same `rev`) and no changes row; the report says `unchanged`.
#[test]
fn an_unchanged_node_produces_no_changes_row() {
    let store = SqliteStore::open_memory().unwrap();
    let doc = NodeId::new();
    let first = store
        .apply_batch(
            batch(vec![upsert(&doc, "Fact", "the same text twice")]),
            &ctx(),
        )
        .unwrap();
    assert_eq!(first.inserted, 1);
    let rev_before = store.get(&doc, false).unwrap().unwrap().rev;
    let changes_before = changes_count(&store);
    let second = store
        .apply_batch(
            batch(vec![upsert(&doc, "Fact", "the same text twice")]),
            &ctx(),
        )
        .unwrap();
    assert_eq!(
        (second.inserted, second.updated, second.unchanged),
        (0, 0, 1),
        "{second:?}"
    );
    assert_eq!(second.outbox_first_seq, None, "{second:?}");
    assert_eq!(second.outbox_last_seq, None, "{second:?}");
    assert_eq!(
        changes_count(&store),
        changes_before,
        "no changes row for an unchanged node"
    );
    assert_eq!(
        store.get(&doc, false).unwrap().unwrap().rev,
        rev_before,
        "row not written"
    );
    // a real text change is an update with one changes row
    let third = store
        .apply_batch(
            batch(vec![upsert(&doc, "Fact", "a different text")]),
            &ctx(),
        )
        .unwrap();
    assert_eq!((third.updated, third.unchanged), (1, 0), "{third:?}");
    assert_eq!(changes_count(&store), changes_before + 1);
    assert_eq!(store.get(&doc, false).unwrap().unwrap().rev, rev_before + 1);
}

/// WD-7: "unchanged" covers every persisted content field, not the text alone. docs-ingest
/// Documents carry their state in props with an often empty text, so a props-only change must
/// be an `updated` write (and the new props must be readable afterwards).
#[test]
fn a_props_only_change_of_a_batch_node_is_updated_not_unchanged() {
    let store = SqliteStore::open_memory().unwrap();
    let doc = NodeId::new();
    let with_props = |status: &str| Op::UpsertNode {
        id: doc.clone(),
        node: NewNode {
            kind: "Fact".into(),
            text: String::new(),
            props: Some(serde_json::json!({"status": status, "pipeline_version": 3})),
            ..Default::default()
        },
    };
    store
        .apply_batch(batch(vec![with_props("blocked")]), &ctx())
        .unwrap();
    let again = store
        .apply_batch(batch(vec![with_props("blocked")]), &ctx())
        .unwrap();
    assert_eq!(again.unchanged, 1, "{again:?}");
    let changed = store
        .apply_batch(batch(vec![with_props("done")]), &ctx())
        .unwrap();
    assert_eq!((changed.updated, changed.unchanged), (1, 0), "{changed:?}");
    assert_eq!(
        store.get(&doc, false).unwrap().unwrap().props["status"],
        serde_json::json!("done")
    );
    // tags and summary are content too
    let tagged = Op::UpsertNode {
        id: doc.clone(),
        node: NewNode {
            kind: "Fact".into(),
            text: String::new(),
            summary: Some("now with a summary".into()),
            tags: vec!["b".into(), "a".into()],
            props: Some(serde_json::json!({"status": "done", "pipeline_version": 3})),
            ..Default::default()
        },
    };
    let r = store
        .apply_batch(batch(vec![tagged.clone()]), &ctx())
        .unwrap();
    assert_eq!(r.updated, 1, "{r:?}");
    let r = store.apply_batch(batch(vec![tagged]), &ctx()).unwrap();
    assert_eq!(r.unchanged, 1, "{r:?}");
}

/// Apply `op` twice (the second time identical) and return the two reports' `(updated,
/// unchanged)` pairs: a change must read `(1, 0)` then `(0, 1)`.
fn update_then_same(store: &SqliteStore, op: &Op) -> [(u32, u32); 2] {
    let first = store.apply_batch(batch(vec![op.clone()]), &ctx()).unwrap();
    let second = store.apply_batch(batch(vec![op.clone()]), &ctx()).unwrap();
    [
        (first.updated, first.unchanged),
        (second.updated, second.unchanged),
    ]
}

/// E2 interim review RE4-14 (EN-E3): a `repo`-only change of a batch node is an update that
/// persists and refreshes its anchors (the `CodeRef`'s `path` row carries the new repo); the
/// same repo again is `unchanged`.
#[test]
fn a_repo_only_change_is_updated_and_refreshes_anchors() {
    let s = common::Scratch::new("e23-batch", "repo-only");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let id = NodeId::new();
    let coderef = |repo: &str| Op::UpsertNode {
        id: id.clone(),
        node: NewNode {
            kind: "CodeRef".into(),
            text: "src/api/recall.rs recall".into(),
            repo: Some(repo.into()),
            props: Some(
                serde_json::json!({"relative_path": "src/api/recall.rs", "name_path": "recall"}),
            ),
            ..Default::default()
        },
    };
    let anchor_repos = || -> Vec<Option<String>> {
        let conn = rusqlite::Connection::open_with_flags(
            s.db(),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        conn.prepare(
            "SELECT a.repo FROM node_anchor a JOIN node n ON n.rid = a.rid WHERE n.id = ?1 AND a.anchor_kind = 'path' AND a.key = 'src/api/recall.rs'",
        )
        .unwrap()
        .query_map([id.as_str()], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
    };
    let r = store
        .apply_batch(batch(vec![coderef("repo-a")]), &ctx())
        .unwrap();
    assert_eq!(r.inserted, 1, "{r:?}");
    assert_eq!(anchor_repos(), vec![Some("repo-a".to_owned())]);
    assert_eq!(
        update_then_same(&store, &coderef("repo-b")),
        [(1, 0), (0, 1)],
        "a repo-only change is an update, the same repo again is unchanged"
    );
    assert_eq!(
        store.get(&id, false).unwrap().unwrap().repo.as_deref(),
        Some("repo-b")
    );
    assert_eq!(anchor_repos(), vec![Some("repo-b".to_owned())]);
}

/// RE4-14 (EN-E3): a `dup_key`-only change of a batch node is an update that persists; the
/// same key again is `unchanged`.
#[test]
fn a_dup_key_only_change_is_updated() {
    let store = SqliteStore::open_memory().unwrap();
    let id = NodeId::new();
    let keyed = |dup_key: &str| Op::UpsertNode {
        id: id.clone(),
        node: NewNode {
            kind: "Fact".into(),
            text: "keyed note".into(),
            dup_key: Some(dup_key.into()),
            ..Default::default()
        },
    };
    store
        .apply_batch(batch(vec![keyed("budget-1")]), &ctx())
        .unwrap();
    assert_eq!(
        update_then_same(&store, &keyed("budget-2")),
        [(1, 0), (0, 1)]
    );
    assert_eq!(
        store.get(&id, false).unwrap().unwrap().dup_key.as_deref(),
        Some("budget-2")
    );
}

/// RE4-14 (EN-E3): a `store_scope`-only change of a batch node is an update that persists;
/// the same scope again is `unchanged`.
#[test]
fn a_store_scope_only_change_is_updated() {
    let store = SqliteStore::open_memory().unwrap();
    let id = NodeId::new();
    let scoped = |store_scope: StoreScope| Op::UpsertNode {
        id: id.clone(),
        node: NewNode {
            kind: "Fact".into(),
            text: "scoped note".into(),
            store_scope,
            ..Default::default()
        },
    };
    store
        .apply_batch(batch(vec![scoped(StoreScope::Project)]), &ctx())
        .unwrap();
    assert_eq!(
        update_then_same(&store, &scoped(StoreScope::Personal)),
        [(1, 0), (0, 1)]
    );
    assert_eq!(
        store.get(&id, false).unwrap().unwrap().store_scope,
        StoreScope::Personal
    );
}

/// D-16: the exact-duplicate index is a `remember` feature. Two batch nodes with identical text
/// in one batch both land (their hashes mix in the id), and a `remember` of the same text dedups
/// against another `remember`, never against a batch node.
#[test]
fn identical_text_batch_nodes_coexist_and_remember_dedup_is_unchanged() {
    let store = SqliteStore::open_memory().unwrap();
    let (a, b) = (NodeId::new(), NodeId::new());
    let report = store
        .apply_batch(
            batch(vec![
                upsert(&a, "Fact", "the very same chunk text"),
                upsert(&b, "Fact", "the very same chunk text"),
            ]),
            &ctx(),
        )
        .unwrap();
    assert_eq!(report.inserted, 2, "{report:?}");
    assert!(store.get(&a, false).unwrap().is_some());
    assert!(store.get(&b, false).unwrap().is_some());
    // `remember` of the same text: created once (not matched to a batch node), then `Existing`
    // against that `remember` node
    let first = store
        .remember(node("Fact", "the very same chunk text"), &[], &ctx())
        .unwrap();
    let kaban_core::RememberOutcome::Created(created) = first else {
        panic!("a remember never dedups against a batch node: {first:?}");
    };
    assert!(created.id != a.as_str() && created.id != b.as_str());
    let second = store
        .remember(node("Fact", "the very same chunk text"), &[], &ctx())
        .unwrap();
    match second {
        kaban_core::RememberOutcome::Existing { node, .. } => assert_eq!(node.id, created.id),
        other @ kaban_core::RememberOutcome::Created(_) => {
            panic!("remember dedup against remember is unchanged: {other:?}")
        }
    }
}

fn out_neighbours(store: &SqliteStore, from: &NodeId) -> Vec<String> {
    let r = store
        .traverse(&kaban_core::TraverseQuery {
            start_ids: vec![from.clone()],
            rels: vec![],
            direction: kaban_core::TraverseDirection::Out,
            max_depth: 1,
            node_kinds: vec![],
            governance: vec![],
            limit: 0,
            workspace_id: None,
            ..Default::default()
        })
        .unwrap();
    let mut ids: Vec<String> = r.edges.into_iter().map(|e| e.dst).collect();
    ids.sort();
    ids
}

/// MD-23 under EN-E4: a producer states the full outgoing edge set of each node it upserts
/// (`Owned`, the ingest pipeline's rule). An outgoing edge of its own the batch no longer
/// names is deleted (with its `delete` changes row); incoming edges from nodes outside the
/// batch are untouched; an unchanged edge writes nothing. The `WriteBatch::new` default
/// (`EdgeDiff::None`, OWN-83 (4)) names no producer and deletes nothing: a re-upsert under it
/// keeps every edge.
#[test]
fn outgoing_edges_absent_from_the_batch_are_deleted_and_incoming_edges_are_kept() {
    let store = SqliteStore::open_memory().unwrap();
    let (doc, s1, s2, other) = (NodeId::new(), NodeId::new(), NodeId::new(), NodeId::new());
    let ingest = "kaban-ingest";
    store
        .apply_batch(
            owned_batch(
                vec![
                    upsert(&doc, "Fact", "document"),
                    upsert(&s1, "Fact", "section one"),
                    upsert(&s2, "Fact", "section two"),
                    upsert(&other, "Fact", "an unrelated node pointing at the document"),
                    owned_edge(&doc, &s1, "RELATES_TO", ingest),
                    owned_edge(&doc, &s2, "RELATES_TO", ingest),
                    owned_edge(&other, &doc, "RELATES_TO", ingest),
                ],
                ingest,
            ),
            &ctx(),
        )
        .unwrap();
    let mut expect = vec![s1.to_string(), s2.to_string()];
    expect.sort();
    assert_eq!(out_neighbours(&store, &doc), expect);
    // the default rule names no producer and deletes nothing of the document
    store
        .apply_batch(batch(vec![upsert(&doc, "Fact", "document")]), &ctx())
        .unwrap();
    assert_eq!(
        out_neighbours(&store, &doc),
        expect,
        "a re-upsert under the default rule identifies no producer: both edges kept"
    );
    let changes_before = changes_count(&store);
    // the producer re-ingests the document with section two gone; `other` is not in this batch
    let report = store
        .apply_batch(
            owned_batch(
                vec![
                    upsert(&doc, "Fact", "document"),
                    upsert(&s1, "Fact", "section one"),
                    owned_edge(&doc, &s1, "RELATES_TO", ingest),
                ],
                ingest,
            ),
            &ctx(),
        )
        .unwrap();
    assert_eq!(report.unchanged, 2, "{report:?}");
    assert_eq!(
        out_neighbours(&store, &doc),
        vec![s1.to_string()],
        "doc -> s2 deleted"
    );
    assert_eq!(
        out_neighbours(&store, &other),
        vec![doc.to_string()],
        "incoming edge other -> doc untouched"
    );
    // exactly one changes row: the edge delete (the unchanged nodes and edge write nothing)
    let rows = store.changes_read(0, 10_000).unwrap();
    assert_eq!(rows.len(), changes_before + 1, "{rows:?}");
    let last = rows.last().unwrap();
    assert_eq!(
        (last.entity, last.op),
        (kaban_core::ChangeEntity::Edge, kaban_core::ChangeOp::Delete)
    );
    assert_eq!(report.outbox_first_seq, Some(last.seq));
    assert_eq!(report.outbox_last_seq, Some(last.seq));
}

/// `Op::Tombstone` is the `ForgetMode::Tombstone` path: lifecycle only, edges kept, no event;
/// repeating it is a no-op that is not counted (WD-3); an unknown id fails the whole batch.
#[test]
fn tombstone_op_hides_the_node_is_idempotent_and_fails_on_an_unknown_id() {
    let store = SqliteStore::open_memory().unwrap();
    let (doc, chunk) = (NodeId::new(), NodeId::new());
    store
        .apply_batch(
            batch(vec![
                upsert(&doc, "Fact", "document to retire"),
                upsert(&chunk, "Fact", "its chunk"),
                edge(&doc, &chunk, "RELATES_TO"),
            ]),
            &ctx(),
        )
        .unwrap();
    let tomb = |id: &NodeId| Op::Tombstone {
        id: id.clone(),
        reason: "source file deleted".into(),
    };
    let changes_before = changes_count(&store);
    let report = store.apply_batch(batch(vec![tomb(&doc)]), &ctx()).unwrap();
    assert_eq!(report.tombstoned, 1, "{report:?}");
    assert!(
        store.get(&doc, false).unwrap().is_none(),
        "hidden from default reads"
    );
    let hidden = store.get(&doc, true).unwrap().unwrap();
    assert_eq!(hidden.lifecycle, kaban_core::Lifecycle::Tombstoned);
    assert!(store.events(&doc).unwrap().is_empty(), "no event (D1)");
    // exactly one changes row (the node update): the edge was kept, no edge delete row
    let rows = store.changes_read(0, 10_000).unwrap();
    assert_eq!(rows.len(), changes_before + 1, "{rows:?}");
    assert_eq!(
        (rows.last().unwrap().entity, rows.last().unwrap().op),
        (kaban_core::ChangeEntity::Node, kaban_core::ChangeOp::Update)
    );
    // idempotent re-run (N5): nothing counted, nothing written
    let changes_before = changes_count(&store);
    let again = store.apply_batch(batch(vec![tomb(&doc)]), &ctx()).unwrap();
    assert_eq!(again.tombstoned, 0, "{again:?}");
    assert_eq!(changes_count(&store), changes_before);
    // an unknown id fails the batch and rolls back the valid op before it
    let unknown = NodeId::new();
    let err = store
        .apply_batch(batch(vec![tomb(&chunk), tomb(&unknown)]), &ctx())
        .expect_err("unknown id");
    assert!(matches!(err, StoreError::NotFound(_)), "{err:?}");
    assert!(
        store.get(&chunk, false).unwrap().is_some(),
        "the chunk tombstone before the failing op was rolled back"
    );
}

/// WD-1, WD-2, WD-5: an archived row is updated in place and stays hidden from default reads
/// (archived is reachable by id); a purged id, a different stored kind and an id owned by
/// another workspace each fail the whole batch (`NotFound` for purged and other-workspace: no
/// existence leak, F17; `KindMismatch` for the kind).
#[test]
fn upsert_on_archived_purged_other_kind_and_other_workspace_rows() {
    let store = SqliteStore::open_memory().unwrap();
    let (archived, purged, typed, foreign) =
        (NodeId::new(), NodeId::new(), NodeId::new(), NodeId::new());
    store
        .apply_batch(
            batch(vec![
                upsert(&archived, "Fact", "to be archived"),
                upsert(&purged, "Fact", "to be purged"),
                upsert(&typed, "Fact", "a fact"),
            ]),
            &ctx(),
        )
        .unwrap();
    // the seeding context is bound to the same tenant as its batch (R2-1)
    store
        .apply_batch(
            WriteBatch::new(
                WorkspaceId::new("other-tenant"),
                vec![upsert(&foreign, "Fact", "owned by another workspace")],
            ),
            &WriteCtx::new("v1", WorkspaceId::new("other-tenant"), Actor::Agent),
        )
        .unwrap();
    store
        .forget(&archived, kaban_core::ForgetMode::Archive, &ctx())
        .unwrap();
    store
        .forget(&purged, kaban_core::ForgetMode::Purge, &ctx())
        .unwrap();

    // WD-1: archived row updated in place, lifecycle untouched
    let r = store
        .apply_batch(
            batch(vec![upsert(&archived, "Fact", "archived but re-ingested")]),
            &ctx(),
        )
        .unwrap();
    assert_eq!(r.updated, 1, "{r:?}");
    let n = store.get(&archived, false).unwrap().unwrap();
    assert_eq!(n.lifecycle, kaban_core::Lifecycle::Archived);
    assert_eq!(n.text, "archived but re-ingested");

    // WD-1: a purged id never reappears
    let err = store
        .apply_batch(
            batch(vec![
                upsert(&typed, "Fact", "changed before the failing op"),
                upsert(&purged, "Fact", "ghost"),
            ]),
            &ctx(),
        )
        .expect_err("purged id");
    assert!(
        matches!(err, StoreError::NotFound(ref id) if id == purged.as_str()),
        "{err:?}"
    );
    assert_eq!(
        store.get(&typed, false).unwrap().unwrap().text,
        "a fact",
        "rolled back"
    );
    assert_eq!(store.get(&purged, true).unwrap().unwrap().text, "");

    // WD-2: kind is immutable per id
    let err = store
        .apply_batch(
            batch(vec![upsert(&typed, "Idea", "same id, other kind")]),
            &ctx(),
        )
        .expect_err("kind mismatch");
    assert!(
        matches!(err, StoreError::KindMismatch { ref id, ref stored, ref requested }
            if id == typed.as_str() && stored == "Fact" && requested == "Idea"),
        "{err:?}"
    );
    assert_eq!(store.get(&typed, false).unwrap().unwrap().kind, "Fact");

    // WD-5: another workspace's row is NotFound to this batch; the node field is ignored
    let err = store
        .apply_batch(
            batch(vec![upsert(&foreign, "Fact", "cross-tenant write")]),
            &ctx(),
        )
        .expect_err("other workspace");
    assert!(
        matches!(err, StoreError::NotFound(ref id) if id == foreign.as_str()),
        "{err:?}"
    );
    assert_eq!(
        store.get(&foreign, false).unwrap().unwrap().text,
        "owned by another workspace"
    );
    let own = NodeId::new();
    store
        .apply_batch(
            batch(vec![Op::UpsertNode {
                id: own.clone(),
                node: NewNode {
                    workspace_id: Some("ignored-field".into()),
                    ..node("Fact", "the batch workspace wins")
                },
            }]),
            &ctx(),
        )
        .unwrap();
    assert_eq!(
        store.get(&own, false).unwrap().unwrap().workspace_id,
        "default"
    );
}

// ---- M2 review R2-1 (C2.10): tenant checks on every op of a batch ----

/// A context bound to `ws` carrying one audit entry, so a rollback can be asserted on the
/// audit chain as well as on rows and `ChangeLog`.
fn ctx_with_audit(ws: &str) -> WriteCtx {
    let mut ctx = WriteCtx::new("v1", WorkspaceId::new(ws), Actor::Agent);
    ctx.audit.push(kaban_core::AuditEntry {
        op: kaban_core::AuditOp::Redact,
        tool: "ingest".into(),
        field: "text".into(),
        workspace_id: WorkspaceId::new(ws),
        policy_version: "v1".into(),
        input_hmac: "stub".into(),
        rules: vec!["old-password".into()],
        node_id: None,
    });
    ctx
}

/// Everything a rolled-back batch must leave untouched.
struct Footprint {
    changes: usize,
    audit: u64,
    events: usize,
}

fn footprint(store: &SqliteStore, watched: &NodeId) -> Footprint {
    Footprint {
        changes: changes_count(store),
        audit: store.audit_verify().unwrap().entries,
        events: store.events(watched).unwrap().len(),
    }
}

fn assert_rolled_back(store: &SqliteStore, watched: &NodeId, before: &Footprint, what: &str) {
    let after = footprint(store, watched);
    assert_eq!(
        after.changes, before.changes,
        "{what}: changes rows survived"
    );
    assert_eq!(after.audit, before.audit, "{what}: audit rows survived");
    assert_eq!(after.events, before.events, "{what}: events survived");
}

/// Two tenants: `theirs` is an active node of `other-tenant`; `mine` and `mine2` belong to the
/// default workspace. Returns the store and the three ids.
fn two_tenants() -> (SqliteStore, NodeId, NodeId, NodeId) {
    let store = SqliteStore::open_memory().unwrap();
    let (theirs, mine, mine2) = (NodeId::new(), NodeId::new(), NodeId::new());
    store
        .apply_batch(
            WriteBatch::new(
                WorkspaceId::new("other-tenant"),
                vec![upsert(&theirs, "Fact", "a note owned by another tenant")],
            ),
            &WriteCtx::new("v1", WorkspaceId::new("other-tenant"), Actor::Agent),
        )
        .unwrap();
    store
        .apply_batch(
            batch(vec![
                upsert(&mine, "Fact", "my own note"),
                upsert(&mine2, "Fact", "my second note"),
            ]),
            &ctx(),
        )
        .unwrap();
    (store, theirs, mine, mine2)
}

/// R2-1: a batch of workspace A cannot tombstone a node of workspace B: the foreign id is
/// `NotFound` (no existence leak), the node stays active and the whole batch (a valid upsert
/// before the op, the audit entry, every changes row) rolls back.
#[test]
fn a_foreign_tombstone_is_not_found_and_rolls_the_batch_back() {
    let (store, theirs, mine, _) = two_tenants();
    let before = footprint(&store, &theirs);
    let err = store
        .apply_batch(
            batch(vec![
                upsert(&mine, "Fact", "changed before the foreign op"),
                Op::Tombstone {
                    id: theirs.clone(),
                    reason: "cross-tenant".into(),
                },
            ]),
            &ctx_with_audit("default"),
        )
        .expect_err("a foreign tombstone must fail the batch");
    assert!(
        matches!(err, StoreError::NotFound(ref id) if id == theirs.as_str()),
        "{err:?}"
    );
    let n = store.get(&theirs, true).unwrap().unwrap();
    assert_eq!(
        n.lifecycle,
        kaban_core::Lifecycle::Active,
        "the other tenant's node is untouched"
    );
    assert_eq!(
        store.get(&mine, false).unwrap().unwrap().text,
        "my own note",
        "the valid upsert before the foreign op rolled back"
    );
    assert_rolled_back(&store, &theirs, &before, "foreign tombstone");
}

/// R2-1: an edge whose source lives in another workspace is `NotFound` for that id; no edge
/// is created and the batch rolls back entirely.
#[test]
fn a_foreign_edge_source_is_not_found_and_rolls_the_batch_back() {
    let (store, theirs, mine, _) = two_tenants();
    let before = footprint(&store, &theirs);
    let err = store
        .apply_batch(
            batch(vec![
                upsert(&mine, "Fact", "changed before the foreign edge"),
                edge(&theirs, &mine, "RELATES_TO"),
            ]),
            &ctx_with_audit("default"),
        )
        .expect_err("a foreign source must fail the batch");
    assert!(
        matches!(err, StoreError::NotFound(ref id) if id == theirs.as_str()),
        "{err:?}"
    );
    assert!(
        out_neighbours(&store, &theirs).is_empty(),
        "no edge created"
    );
    assert_eq!(
        store.get(&mine, false).unwrap().unwrap().text,
        "my own note"
    );
    assert_rolled_back(&store, &theirs, &before, "foreign src");
}

/// R2-1: an edge whose target lives in another workspace is `NotFound` for that id; no edge
/// is created and the batch rolls back entirely.
#[test]
fn a_foreign_edge_target_is_not_found_and_rolls_the_batch_back() {
    let (store, theirs, mine, _) = two_tenants();
    let before = footprint(&store, &theirs);
    let err = store
        .apply_batch(
            batch(vec![
                upsert(&mine, "Fact", "changed before the foreign edge"),
                edge(&mine, &theirs, "RELATES_TO"),
            ]),
            &ctx_with_audit("default"),
        )
        .expect_err("a foreign target must fail the batch");
    assert!(
        matches!(err, StoreError::NotFound(ref id) if id == theirs.as_str()),
        "{err:?}"
    );
    assert!(out_neighbours(&store, &mine).is_empty(), "no edge created");
    assert_eq!(
        store.get(&mine, false).unwrap().unwrap().text,
        "my own note"
    );
    assert_rolled_back(&store, &theirs, &before, "foreign dst");
}

/// R2-1: a `SUPERSEDES` edge to another tenant's node has no side effect on it: the target
/// stays active with no event, the edge does not exist, the batch rolls back.
#[test]
fn supersedes_to_a_foreign_node_has_no_side_effect_and_rolls_the_batch_back() {
    let (store, theirs, mine, mine2) = two_tenants();
    let before = footprint(&store, &theirs);
    let err = store
        .apply_batch(
            batch(vec![
                upsert(&mine2, "Fact", "changed before the foreign supersede"),
                edge(&mine, &theirs, "SUPERSEDES"),
            ]),
            &ctx_with_audit("default"),
        )
        .expect_err("a foreign SUPERSEDES target must fail the batch");
    assert!(
        matches!(err, StoreError::NotFound(ref id) if id == theirs.as_str()),
        "{err:?}"
    );
    let n = store.get(&theirs, true).unwrap().unwrap();
    assert_eq!(
        (n.lifecycle, n.rev),
        (kaban_core::Lifecycle::Active, 1),
        "the foreign target was neither archived nor bumped"
    );
    assert!(out_neighbours(&store, &mine).is_empty(), "no edge created");
    assert_eq!(
        store.get(&mine2, false).unwrap().unwrap().text,
        "my second note"
    );
    assert_rolled_back(&store, &theirs, &before, "foreign SUPERSEDES");
}

/// R2-1: a batch whose workspace differs from the write context's is refused before any write
/// (the audit rows would otherwise be attributed to a workspace other than the mutated rows):
/// `WorkspaceMismatch`, nothing stored, no audit or changes row.
#[test]
fn a_batch_whose_workspace_differs_from_the_context_is_refused_before_any_write() {
    let (store, theirs, mine, _) = two_tenants();
    let before = footprint(&store, &theirs);
    let fresh = NodeId::new();
    let err = store
        .apply_batch(
            batch(vec![
                upsert(&fresh, "Fact", "a new node"),
                upsert(&mine, "Fact", "changed under the wrong context"),
            ]),
            &ctx_with_audit("other-tenant"),
        )
        .expect_err("ctx workspace must match the batch workspace");
    assert!(
        matches!(err, StoreError::WorkspaceMismatch { ref ctx, ref batch }
            if ctx == "other-tenant" && batch == "default"),
        "{err:?}"
    );
    assert!(store.get(&fresh, true).unwrap().is_none(), "nothing stored");
    assert_eq!(
        store.get(&mine, false).unwrap().unwrap().text,
        "my own note"
    );
    assert_rolled_back(&store, &theirs, &before, "ctx mismatch");
}

// ---- DI1.8 (RD2-6): `WriteBatch.edge_diff` (engine-spec EN-E4, docs-ingest-spec D5 [A-3]) ----

fn owned_node(kind: &str, text: &str, owner: &str) -> NewNode {
    NewNode {
        kind: kind.into(),
        text: text.into(),
        props: Some(serde_json::json!({ "owner": owner })),
        ..Default::default()
    }
}

fn owned_edge(src: &NodeId, dst: &NodeId, rel: &str, owner: &str) -> Op {
    Op::UpsertEdge {
        src: src.clone(),
        dst: dst.clone(),
        rel: rel.into(),
        props: Some(serde_json::json!({ "owner": owner })),
    }
}

fn owned_batch(ops: Vec<Op>, owner: &str) -> WriteBatch {
    let mut b = WriteBatch::new(WorkspaceId::default(), ops);
    b.edge_diff = EdgeDiff::Owned {
        owner: kaban_core::Owner::parse(owner).unwrap(),
    };
    b
}

/// RD2-6: under `Owned{"kaban-ingest"}` a document edit (the batch names every node again
/// but not the human `link` edge nor the `remember`-created `EVIDENCE_FOR` edge from an
/// unchanged chunk) keeps both foreign edges and still deletes the pipeline's own stale edge.
#[test]
fn owned_edge_diff_keeps_foreign_edges_across_a_document_edit() {
    let store = SqliteStore::open_memory().unwrap();
    let (doc, c1, c2, human_target, record) = (
        NodeId::new(),
        NodeId::new(),
        NodeId::new(),
        NodeId::new(),
        NodeId::new(),
    );
    let owner = "kaban-ingest";
    store
        .apply_batch(
            owned_batch(
                vec![
                    upsert(&doc, "Document", "document"),
                    upsert(&c1, "Chunk", "chunk one"),
                    upsert(&c2, "Chunk", "chunk two"),
                    owned_edge(&c1, &doc, "PART_OF", owner),
                    owned_edge(&c2, &doc, "PART_OF", owner),
                    owned_edge(&c1, &c2, "NEXT", owner),
                ],
                owner,
            ),
            &ctx(),
        )
        .unwrap();
    // a human `link` from the unchanged chunk and a `remember`-style evidence edge
    store
        .apply_batch(
            batch(vec![
                upsert(&human_target, "Fact", "target"),
                upsert(&record, "Decision", "a decision"),
            ]),
            &ctx(),
        )
        .unwrap();
    store
        .link(&c1, &human_target, "RELATES_TO", None, None, &ctx())
        .unwrap();
    store
        .link(
            &c1,
            &record,
            "EVIDENCE_FOR",
            None,
            Some(serde_json::json!({ "owner": "kaban-parse" })),
            &ctx(),
        )
        .unwrap();
    // the edit: chunk two is gone, chunk one is unchanged and re-named
    let report = store
        .apply_batch(
            owned_batch(
                vec![
                    upsert(&doc, "Document", "document v2"),
                    upsert(&c1, "Chunk", "chunk one"),
                    owned_edge(&c1, &doc, "PART_OF", owner),
                ],
                owner,
            ),
            &ctx(),
        )
        .unwrap();
    assert_eq!(report.unchanged, 1, "{report:?}");
    let mut expect = vec![
        doc.to_string(),
        human_target.to_string(),
        record.to_string(),
    ];
    expect.sort();
    assert_eq!(
        out_neighbours(&store, &c1),
        expect,
        "the owned NEXT edge is gone, the human and parse edges stay"
    );
}

/// RD2-6: a shared Tag node (or a placeholder Document) upserted by a second producer's batch
/// never loses an outgoing edge another producer wrote; `EdgeDiff::None` deletes nothing even
/// for the batch's own owner.
#[test]
fn shared_upserts_never_erase_another_producers_edges_and_none_deletes_nothing() {
    let store = SqliteStore::open_memory().unwrap();
    let (tag, a, b) = (NodeId::new(), NodeId::new(), NodeId::new());
    store
        .apply_batch(
            owned_batch(
                vec![
                    upsert(&tag, "Tag", "shared"),
                    upsert(&a, "Fact", "a"),
                    owned_edge(&tag, &a, "RELATES_TO", "kaban-parse"),
                ],
                "kaban-parse",
            ),
            &ctx(),
        )
        .unwrap();
    // the ingest pipeline names the Tag again (TAGGED is incoming, so only outgoing edges of
    // `tag` are at stake) without naming the parse edge
    store
        .apply_batch(
            owned_batch(
                vec![
                    upsert(&tag, "Tag", "shared"),
                    upsert(&b, "Chunk", "b"),
                    owned_edge(&b, &tag, "TAGGED", "kaban-ingest"),
                ],
                "kaban-ingest",
            ),
            &ctx(),
        )
        .unwrap();
    assert_eq!(out_neighbours(&store, &tag), vec![a.to_string()]);
    let mut none = WriteBatch::new(WorkspaceId::default(), vec![upsert(&tag, "Tag", "shared")]);
    none.edge_diff = EdgeDiff::None;
    store.apply_batch(none, &ctx()).unwrap();
    assert_eq!(out_neighbours(&store, &tag), vec![a.to_string()]);
    // a human batch under the default rule (EN-E4, E2.5a; OWN-83 (4)): the parse layer's edge
    // is not its to delete either
    store
        .apply_batch(batch(vec![upsert(&tag, "Tag", "shared")]), &ctx())
        .unwrap();
    assert_eq!(
        out_neighbours(&store, &tag),
        vec![a.to_string()],
        "a human batch of another layer keeps the parse edge"
    );
}

// ---- DI1.8 (RD2-5): `Op::Reactivate`, the ingestion reconciliation transition ----

fn reactivate(id: &NodeId) -> Op {
    Op::Reactivate { id: id.clone() }
}

fn lifecycle_of(store: &SqliteStore, id: &NodeId) -> kaban_core::Lifecycle {
    store.get(id, true).unwrap().unwrap().lifecycle
}

/// RD2-5: an ingest-owned Chunk the pipeline tombstoned comes back `active` through
/// `Op::Reactivate` (rev bumped, an changes row, a `reactivated` event); repeating the op on an
/// active node is a no-op; a purged id is `NotFound`.
#[test]
fn reactivate_restores_an_ingest_owned_tombstoned_chunk() {
    let store = SqliteStore::open_memory().unwrap();
    let chunk = NodeId::new();
    let owned = |text: &str| Op::UpsertNode {
        id: chunk.clone(),
        node: owned_node("Chunk", text, kaban_core::INGEST_OWNER),
    };
    store.apply_batch(batch(vec![owned("c")]), &ctx()).unwrap();
    let rev_before = store.get(&chunk, true).unwrap().unwrap().rev;
    store
        .apply_batch(
            batch(vec![Op::Tombstone {
                id: chunk.clone(),
                reason: "gone".into(),
            }]),
            &ctx(),
        )
        .unwrap();
    assert_eq!(
        lifecycle_of(&store, &chunk),
        kaban_core::Lifecycle::Tombstoned
    );
    let changes_before = changes_count(&store);
    let report = store
        .apply_batch(batch(vec![reactivate(&chunk), owned("c")]), &ctx())
        .unwrap();
    assert_eq!(report.reactivated, 1, "{report:?}");
    assert_eq!(
        report.unchanged, 1,
        "the content upsert is unchanged: {report:?}"
    );
    let n = store.get(&chunk, false).unwrap().expect("visible again");
    assert_eq!(n.lifecycle, kaban_core::Lifecycle::Active);
    assert!(
        n.rev > rev_before + 1,
        "rev bumped by tombstone and reactivate: {}",
        n.rev
    );
    assert!(
        changes_count(&store) > changes_before,
        "changes row for the reactivation"
    );
    let events = store.events(&chunk).unwrap();
    assert!(
        events.iter().any(|e| e.kind == "reactivated"),
        "audited as a `reactivated` event: {events:?}"
    );
    // idempotent on an active node
    let again = store
        .apply_batch(batch(vec![reactivate(&chunk)]), &ctx())
        .unwrap();
    assert_eq!(again.reactivated, 0);
    assert_eq!(again.unchanged, 1);
    assert_eq!(
        report.outcomes,
        vec![
            kaban_core::OpOutcome {
                id: chunk.clone(),
                node_kind: "Chunk".into(),
                outcome: kaban_core::OutcomeKind::Reactivated
            },
            kaban_core::OpOutcome {
                id: chunk.clone(),
                node_kind: "Chunk".into(),
                outcome: kaban_core::OutcomeKind::Unchanged
            }
        ]
    );
    store
        .forget(&chunk, kaban_core::ForgetMode::Purge, &ctx())
        .unwrap();
    let err = store
        .apply_batch(batch(vec![reactivate(&chunk)]), &ctx())
        .unwrap_err();
    assert!(matches!(err, StoreError::NotFound(_)), "{err:?}");
}

/// RD2-5: every other `Tombstoned -> Active` attempt is refused with a fixed class (core F5):
/// a human-tombstoned Decision (governed kind), an ingest-owned governed kind, and a Chunk
/// without the ingest owner; the refusing batch rolls back entirely.
#[test]
fn reactivate_refuses_governed_kinds_and_foreign_owners() {
    let store = SqliteStore::open_memory().unwrap();
    let cases = [
        ("Decision", None, "human-tombstoned Decision"),
        (
            "Decision",
            Some(kaban_core::INGEST_OWNER),
            "ingest-owned governed kind",
        ),
        ("Chunk", None, "chunk without owner"),
        ("Chunk", Some("kaban-parse"), "chunk of another producer"),
    ];
    for (kind, owner, what) in cases {
        let id = NodeId::new();
        let n = match owner {
            Some(o) => owned_node(kind, "t", o),
            None => node(kind, "t"),
        };
        store
            .apply_batch(
                batch(vec![Op::UpsertNode {
                    id: id.clone(),
                    node: n,
                }]),
                &ctx(),
            )
            .unwrap();
        store
            .forget(&id, kaban_core::ForgetMode::Tombstone, &ctx())
            .unwrap();
        let marker = NodeId::new();
        let err = store
            .apply_batch(
                batch(vec![upsert(&marker, "Fact", "m"), reactivate(&id)]),
                &ctx(),
            )
            .unwrap_err();
        assert!(
            matches!(
                err,
                StoreError::InvalidTransition(kaban_core::InvalidTransition::Reactivate { .. })
            ),
            "{what}: {err:?}"
        );
        assert_eq!(
            lifecycle_of(&store, &id),
            kaban_core::Lifecycle::Tombstoned,
            "{what}"
        );
        assert!(
            store.get(&marker, true).unwrap().is_none(),
            "{what}: rolled back"
        );
    }
}

/// E2.2 slice (EN-E4, typed owners): an `Owned{KabanEmbed}` batch leaves the `kaban-ingest`
/// edges and the human `link` edges of an upserted node intact and deletes only its own
/// unnamed edges; `INGEST_OWNER` is the wire form of `Owner::KabanIngest`.
#[test]
fn an_embed_owned_batch_deletes_only_its_own_unnamed_edges() {
    use kaban_core::{INGEST_OWNER, Owner};
    assert_eq!(INGEST_OWNER, Owner::KabanIngest.as_str());
    assert_eq!(
        serde_json::to_value(EdgeDiff::Owned {
            owner: Owner::KabanIngest
        })
        .unwrap()["owner"],
        serde_json::json!("kaban-ingest"),
        "the JSON wire form is unchanged"
    );
    let store = SqliteStore::open_memory().unwrap();
    let (c1, doc, other, vec_) = (NodeId::new(), NodeId::new(), NodeId::new(), NodeId::new());
    let ingest = "kaban-ingest";
    store
        .apply_batch(
            owned_batch(
                vec![
                    upsert(&doc, "Document", "a document"),
                    upsert(&c1, "Chunk", "chunk one"),
                    upsert(&other, "Fact", "a fact"),
                    upsert(&vec_, "Fact", "a vector neighbour"),
                    owned_edge(&c1, &doc, "PART_OF", ingest),
                ],
                ingest,
            ),
            &ctx(),
        )
        .unwrap();
    store
        .link(&c1, &other, "RELATES_TO", None, None, &ctx())
        .unwrap();
    let embed = |ops| {
        let mut b = batch(ops);
        b.edge_diff = EdgeDiff::Owned {
            owner: Owner::KabanEmbed,
        };
        b
    };
    // the embed layer names a neighbour edge of the chunk, then re-upserts the chunk without it
    store
        .apply_batch(
            embed(vec![
                upsert(&c1, "Chunk", "chunk one (embedded)"),
                owned_edge(&c1, &vec_, "RELATES_TO", "kaban-embed"),
            ]),
            &ctx(),
        )
        .unwrap();
    let mut out = out_neighbours(&store, &c1);
    out.sort();
    let mut expected = vec![doc.to_string(), other.to_string(), vec_.to_string()];
    expected.sort();
    assert_eq!(out, expected);
    store
        .apply_batch(
            embed(vec![upsert(&c1, "Chunk", "chunk one (re-embedded)")]),
            &ctx(),
        )
        .unwrap();
    let mut out = out_neighbours(&store, &c1);
    out.sort();
    let mut expected = vec![doc.to_string(), other.to_string()];
    expected.sort();
    assert_eq!(
        out, expected,
        "the ingest PART_OF and the human link survive; the embed edge went"
    );
}

/// E2.5a (EN-E4 "a layer never deletes an edge it did not create"), moved by E2.5e (RW2-3): a
/// chunk written by a producer under the default rule carries a human `link` (projected,
/// `layer = human`) and the reconciler's projection of a layer's `MENTIONS` claim
/// (`props.owner = kaban-reconcile`, `layer` set); the producer re-upserts the chunk naming
/// no edge at all: both foreign rows survive, and so does the first batch's `PART_OF`,
/// tagged `human` like the link and so no identified producer's to delete (E2.5a deleted it).
#[test]
fn complete_diff_keeps_edges_of_other_layers() {
    let store = SqliteStore::open_memory().unwrap();
    let (doc, chunk, human_target, entity) =
        (NodeId::new(), NodeId::new(), NodeId::new(), NodeId::new());
    // the producer's first batch: the `WriteBatch::new` default (`None`, OWN-83 (4))
    store
        .apply_batch(
            batch(vec![
                upsert(&doc, "Document", "a document"),
                upsert(&chunk, "Chunk", "a chunk"),
                upsert(&human_target, "Fact", "what the human linked"),
                upsert(&entity, "Fact", "what a layer found in the chunk"),
                edge(&chunk, &doc, "PART_OF"),
            ]),
            &ctx(),
        )
        .unwrap();
    // a human judgement on the chunk
    store
        .link(&chunk, &human_target, "RELATES_TO", None, None, &ctx())
        .unwrap();
    // a layer's `MENTIONS` claim, projected by the pass as the reconciler's own row
    let mut claimed = owned_batch(vec![upsert(&chunk, "Chunk", "a chunk")], "kaban-parse");
    claimed.assertions = vec![edge_claim(
        &chunk,
        "MENTIONS",
        &entity,
        Layer::Gliner,
        Method::Gliner,
        9000,
        "l1",
    )];
    let report = store.apply_batch(claimed, &ctx()).unwrap();
    assert_eq!(report.assertions_inserted, 1, "{report:?}");
    let owners: std::collections::BTreeMap<String, serde_json::Value> = out_edges(&store, &chunk)
        .into_iter()
        .map(|(dst, props)| (dst, props["owner"].clone()))
        .collect();
    assert_eq!(owners.len(), 3, "{owners:?}");
    assert_eq!(owners[&doc.to_string()], serde_json::json!("human"));
    assert_eq!(
        owners[&human_target.to_string()],
        serde_json::json!("human")
    );
    assert_eq!(
        owners[&entity.to_string()],
        serde_json::json!("kaban-reconcile")
    );
    // the producer re-ingests the chunk and names none of its edges
    store
        .apply_batch(
            batch(vec![upsert(&chunk, "Chunk", "a chunk, re-ingested")]),
            &ctx(),
        )
        .unwrap();
    let mut expect = vec![
        doc.to_string(),
        entity.to_string(),
        human_target.to_string(),
    ];
    expect.sort();
    assert_eq!(
        out_neighbours(&store, &chunk),
        expect,
        "the human link and the reconciler's MENTIONS survive a default-rule re-upsert, and so does the first batch's PART_OF: tagged `human` like the link, it is no identified producer's (E2.5e, RW2-3)"
    );
}

/// Every outgoing edge of `from` as stored, serialised (id, endpoints, rel, confidence,
/// `created_at`, props), in edge id order: the byte-identity witness of RW2-3.
fn out_edge_rows(store: &SqliteStore, from: &NodeId) -> Vec<serde_json::Value> {
    let mut edges = store
        .traverse(&kaban_core::TraverseQuery {
            start_ids: vec![from.clone()],
            rels: vec![],
            direction: kaban_core::TraverseDirection::Out,
            max_depth: 1,
            node_kinds: vec![],
            governance: vec![],
            limit: 0,
            workspace_id: None,
            ..Default::default()
        })
        .unwrap()
        .edges;
    edges.sort_by(|a, b| a.id.cmp(&b.id));
    edges
        .iter()
        .map(|e| serde_json::to_value(e).unwrap())
        .collect()
}

/// RW2-3 on a store at the current schema: a human `link` through the public path carries the
/// human layer at rest (`layer = human`, the reconcile path's projection) and survives a
/// default-rule re-upsert of its source byte-identical.
#[test]
fn a_human_link_on_the_current_schema_carries_the_human_layer_and_survives_complete() {
    let s = common::Scratch::new("e25e-link-layer", "current");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let (doc, chunk, human_target) = (NodeId::new(), NodeId::new(), NodeId::new());
    store
        .apply_batch(
            batch(vec![
                upsert(&doc, "Document", "a document"),
                upsert(&chunk, "Chunk", "a chunk"),
                upsert(&human_target, "Fact", "what the human linked"),
                edge(&chunk, &doc, "PART_OF"),
            ]),
            &ctx(),
        )
        .unwrap();
    store
        .link(&chunk, &human_target, "RELATES_TO", None, None, &ctx())
        .unwrap();
    let layer: Option<String> = rusqlite::Connection::open(s.db())
        .unwrap()
        .query_row(
            "SELECT e.layer FROM edge e JOIN node d ON d.rid = e.dst WHERE d.id = ?1",
            [human_target.as_str()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(layer.as_deref(), Some("human"), "the human layer at rest");
    let before = out_edge_rows(&store, &chunk);
    store
        .apply_batch(
            batch(vec![upsert(&chunk, "Chunk", "a chunk, re-ingested")]),
            &ctx(),
        )
        .unwrap();
    let after = out_edge_rows(&store, &chunk);
    assert_eq!(
        after
            .iter()
            .find(|e| e["dst"] == serde_json::json!(human_target.to_string())),
        before
            .iter()
            .find(|e| e["dst"] == serde_json::json!(human_target.to_string())),
        "the human link survives byte-identical: {after:?}"
    );
}

/// EN-E4 `Verify:` (E2.5a postcheck): an `Owned{"kaban-embed"}` batch re-upserting a chunk
/// leaves the `kaban-ingest` `MENTIONS` (and `PART_OF`) rows and the human `link` intact
/// (negative), and deletes the embed layer's own unnamed neighbour edge (positive).
#[test]
fn owned_embed_leaves_ingest_mentions_and_human_links() {
    let store = SqliteStore::open_memory().unwrap();
    let (doc, chunk, entity, human_target, neighbour) = (
        NodeId::new(),
        NodeId::new(),
        NodeId::new(),
        NodeId::new(),
        NodeId::new(),
    );
    let ingest = "kaban-ingest";
    store
        .apply_batch(
            owned_batch(
                vec![
                    upsert(&doc, "Document", "a document"),
                    upsert(&chunk, "Chunk", "a chunk"),
                    upsert(&entity, "Fact", "an entity the chunk mentions"),
                    upsert(&human_target, "Fact", "what the human linked"),
                    upsert(&neighbour, "Fact", "a vector neighbour"),
                    owned_edge(&chunk, &doc, "PART_OF", ingest),
                    owned_edge(&chunk, &entity, "MENTIONS", ingest),
                ],
                ingest,
            ),
            &ctx(),
        )
        .unwrap();
    store
        .link(&chunk, &human_target, "RELATES_TO", None, None, &ctx())
        .unwrap();
    let embed = |ops| {
        let mut b = batch(ops);
        b.edge_diff = EdgeDiff::Owned {
            owner: kaban_core::Owner::KabanEmbed,
        };
        b
    };
    store
        .apply_batch(
            embed(vec![
                upsert(&chunk, "Chunk", "a chunk"),
                owned_edge(&chunk, &neighbour, "RELATES_TO", "kaban-embed"),
            ]),
            &ctx(),
        )
        .unwrap();
    assert_eq!(out_neighbours(&store, &chunk).len(), 4);
    // the embed layer re-upserts the chunk naming no edge: its own neighbour edge goes
    store
        .apply_batch(
            embed(vec![upsert(&chunk, "Chunk", "a chunk (re-embedded)")]),
            &ctx(),
        )
        .unwrap();
    let mut expect = vec![
        doc.to_string(),
        entity.to_string(),
        human_target.to_string(),
    ];
    expect.sort();
    assert_eq!(
        out_neighbours(&store, &chunk),
        expect,
        "ingest MENTIONS and PART_OF and the human link stay; the embed edge is gone"
    );
}

/// E2.2 slice (EN-E3): `path_scope`, `dup_key`, `repo` and `store_scope` persist on insert and
/// update and read back through `get`; a scope-only change reports `updated: 1`; a node
/// without a scope keeps the content hash it had before E2.2.
#[test]
fn scope_dup_key_repo_and_store_scope_persist_and_a_scope_only_change_is_an_update() {
    use kaban_core::StoreScope;
    let store = SqliteStore::open_memory().unwrap();
    let id = NodeId::new();
    let scoped = |scope: &[&str]| Op::UpsertNode {
        id: id.clone(),
        node: NewNode {
            kind: "Decision".into(),
            text: "keep the budget".into(),
            props: Some(serde_json::json!({"decision": "keep", "rationale": "fixture"})),
            path_scope: scope.iter().map(|s| (*s).to_owned()).collect(),
            dup_key: Some("budget".into()),
            repo: Some("repo".into()),
            store_scope: StoreScope::Personal,
            ..Default::default()
        },
    };
    let r = store
        .apply_batch(batch(vec![scoped(&["src/**"])]), &ctx())
        .unwrap();
    assert_eq!(r.inserted, 1);
    let n = store.get(&id, false).unwrap().unwrap();
    assert_eq!(n.path_scope, vec!["src/**".to_owned()]);
    assert_eq!(n.dup_key.as_deref(), Some("budget"));
    assert_eq!(n.repo.as_deref(), Some("repo"));
    assert_eq!(n.store_scope, StoreScope::Personal);
    let r = store
        .apply_batch(batch(vec![scoped(&["docs/**"])]), &ctx())
        .unwrap();
    assert_eq!(
        (r.updated, r.unchanged),
        (1, 0),
        "a scope-only change is an update"
    );
    let n = store.get(&id, false).unwrap().unwrap();
    assert_eq!(n.path_scope, vec!["docs/**".to_owned()]);
    let r = store
        .apply_batch(batch(vec![scoped(&["docs/**"])]), &ctx())
        .unwrap();
    assert_eq!((r.updated, r.unchanged), (0, 1));
}

/// E2.2 (EN-E3): a batch node without a scope keeps the `kaban-batch/v1` content hash it had
/// before the scope globs joined the formula (pinned from the base bytes).
#[test]
fn a_node_without_a_scope_keeps_its_base_hash() {
    let store = SqliteStore::open_memory().unwrap();
    let plain = NodeId::parse("01a17a03-5995-7000-8000-00000000e220").unwrap();
    store
        .apply_batch(
            batch(vec![upsert(&plain, "Fact", "the plain hash pin")]),
            &ctx(),
        )
        .unwrap();
    let n = store.get(&plain, false).unwrap().unwrap();
    assert_eq!(
        n.content_hash, "5b36d4a9eb46540f2f1b0870b26d4584084e1f396939675a310f82f3939cbe0c",
        "nodes without a scope keep their hash"
    );
}

// ---- E2.6b (engine-spec EN-F20 verify pair): universal links at the batch seam ----

/// An `edge` claim about `src -rel-> dst` from `layer`/`method` with raw score `raw`
/// (1/10000) under lineage `lineage` (the shape of `tests/assertions.rs`).
fn edge_claim(
    src: &NodeId,
    rel: &str,
    dst: &NodeId,
    layer: Layer,
    method: Method,
    raw: u32,
    lineage: &str,
) -> BatchAssertion {
    BatchAssertion {
        subject: BatchSubject::Edge {
            src: src.clone(),
            rel: rel.into(),
            dst: dst.clone(),
        },
        claim_class: ClaimClass::Edge,
        predicate: None,
        object: None,
        value: serde_json::json!({}),
        layer,
        method,
        extractor_id: Some("fixture".into()),
        extractor_version: Some("1".into()),
        source_id: Some("doc-1".into()),
        lineage_id: lineage.into(),
        raw_score: Fixed::new(raw).unwrap(),
        rank: Rank::Normal,
        rank_reason: None,
        calibration_version: 1,
        supersedes: None,
    }
}

/// The outgoing edges of `from` as `(dst, props)`.
fn out_edges(store: &SqliteStore, from: &NodeId) -> Vec<(String, serde_json::Value)> {
    store
        .traverse(&kaban_core::TraverseQuery {
            start_ids: vec![from.clone()],
            rels: vec![],
            direction: kaban_core::TraverseDirection::Out,
            max_depth: 1,
            node_kinds: vec![],
            governance: vec![],
            limit: 0,
            workspace_id: None,
            ..Default::default()
        })
        .unwrap()
        .edges
        .into_iter()
        .map(|e| (e.dst, e.props))
        .collect()
}

/// EN-F20 (OD-AI-5 default): `ABOUT` is a judgement and no deterministic layer writes it. A
/// producer batch (`Owned` under a non-human owner) naming a bare `ABOUT` op is refused before
/// the transaction opens with the EN-F9 class (`edge_requires_assertion`: the claim path is a
/// producer's way to say it): no node, no edge, no audit row, no changes row. The human
/// `ABOUT` of a default-rule (human) batch lands, and a producer's `ABOUT` claim is projected as the
/// reconciler's edge, the op that names it for the diff admitted beside the claim.
#[test]
fn about_from_a_deterministic_owner_is_refused_before_any_row() {
    let store = SqliteStore::open_memory().unwrap();
    let (note, target, marker) = (NodeId::new(), NodeId::new(), NodeId::new());
    store
        .apply_batch(
            batch(vec![
                upsert(&note, "Fact", "a note about a pointer"),
                upsert(&target, "CodeRef", "src/lib.rs lib"),
            ]),
            &ctx(),
        )
        .unwrap();
    let before = footprint(&store, &note);
    for owner in [
        "kaban-ingest",
        "kaban-parse",
        "kaban-embed",
        "kaban-reconcile",
    ] {
        let err = store
            .apply_batch(
                owned_batch(
                    vec![
                        upsert(&marker, "Fact", "a node before the refused edge"),
                        owned_edge(&note, &target, "ABOUT", owner),
                    ],
                    owner,
                ),
                &ctx_with_audit("default"),
            )
            .expect_err("a deterministic owner's ABOUT must be refused");
        assert!(
            matches!(err, StoreError::EdgeRequiresAssertion),
            "{owner}: {err:?}"
        );
        assert!(
            store.get(&marker, true).unwrap().is_none(),
            "{owner}: nothing of the batch was stored"
        );
        assert!(
            out_edges(&store, &note).is_empty(),
            "{owner}: no ABOUT edge landed"
        );
        assert_rolled_back(&store, &note, &before, owner);
    }
    // the human path (the default rule is a human batch): the judgement lands
    store
        .apply_batch(batch(vec![edge(&note, &target, "ABOUT")]), &ctx())
        .unwrap();
    let human = out_edges(&store, &note);
    assert_eq!(human.len(), 1, "{human:?}");
    assert_eq!(human[0].0, target.to_string());
    assert_eq!(human[0].1["owner"], serde_json::json!("human"));
    // the claim path (EN-F12, E2.2): the producer's ABOUT is a claim, projected by the pass
    // as the reconciler's own edge; the op naming the same triple is the diff's name-keeper
    let second = NodeId::new();
    let mut claimed = owned_batch(
        vec![
            upsert(&second, "Fact", "a second note, its link a claim"),
            owned_edge(&second, &target, "ABOUT", "kaban-parse"),
        ],
        "kaban-parse",
    );
    claimed.assertions = vec![edge_claim(
        &second,
        "ABOUT",
        &target,
        Layer::Gliner,
        Method::Gliner,
        9000,
        "l1",
    )];
    let report = store.apply_batch(claimed, &ctx()).unwrap();
    assert_eq!(report.assertions_inserted, 1, "{report:?}");
    let projected = out_edges(&store, &second);
    assert_eq!(projected.len(), 1, "{projected:?}");
    assert_eq!(projected[0].0, target.to_string());
    assert_eq!(
        projected[0].1["owner"],
        serde_json::json!("kaban-reconcile"),
        "a layer claim's edge is the reconciler's write, never the producer's"
    );
    // the claim alone, without the op, lands too
    let third = NodeId::new();
    let mut claim_only = owned_batch(
        vec![upsert(&third, "Fact", "a third note, claim only")],
        "kaban-ingest",
    );
    claim_only.assertions = vec![edge_claim(
        &third,
        "ABOUT",
        &target,
        Layer::Gliner,
        Method::Gliner,
        9000,
        "l2",
    )];
    store.apply_batch(claim_only, &ctx()).unwrap();
    assert_eq!(out_edges(&store, &third).len(), 1);
}

/// EN-F20 `Verify:` `link` with a disallowed endpoint fails as today (`rel_def` check,
/// `EndpointNotAllowed`): the declared `to:` list of `TAGGED` is `[Tag]`, so a `Fact` target
/// is refused by `link`, by a batch's structural edge and by a batch's `edge` claim alike (the
/// one `edge_rule` judgement); no edge is written on any path.
#[test]
fn a_disallowed_endpoint_is_endpoint_not_allowed_on_every_edge_path() {
    let store = SqliteStore::open_memory().unwrap();
    let (a, b) = (NodeId::new(), NodeId::new());
    store
        .apply_batch(
            batch(vec![
                upsert(&a, "Fact", "a fact"),
                upsert(&b, "Fact", "not a tag"),
            ]),
            &ctx(),
        )
        .unwrap();
    let is_tagged_refusal = |err: &StoreError| {
        matches!(err, StoreError::EndpointNotAllowed { rel, from, to }
            if rel == "TAGGED" && from == "Fact" && to == "Fact")
    };
    let err = store
        .link(&a, &b, "TAGGED", None, None, &ctx())
        .expect_err("link: a Fact is not a Tag");
    assert!(is_tagged_refusal(&err), "{err:?}");
    let err = store
        .apply_batch(batch(vec![edge(&a, &b, "TAGGED")]), &ctx())
        .expect_err("batch edge: a Fact is not a Tag");
    assert!(is_tagged_refusal(&err), "{err:?}");
    let mut claimed = batch(vec![]);
    claimed.assertions = vec![edge_claim(
        &a,
        "TAGGED",
        &b,
        Layer::Gliner,
        Method::Gliner,
        9000,
        "l1",
    )];
    let err = store
        .apply_batch(claimed, &ctx())
        .expect_err("edge claim: a Fact is not a Tag");
    assert!(is_tagged_refusal(&err), "{err:?}");
    assert!(out_edges(&store, &a).is_empty(), "no edge on any path");
    // the same rel to a Tag is allowed, so the refusal is the endpoint's, not the rel's
    let tag = NodeId::new();
    store
        .apply_batch(
            batch(vec![upsert(&tag, "Tag", "a tag"), edge(&a, &tag, "TAGGED")]),
            &ctx(),
        )
        .unwrap();
    assert_eq!(out_edges(&store, &a).len(), 1);
}

/// OWN-83 (4), EN-E4 (engine-plan E2.8): `Complete { producer }` deletes exactly the unnamed
/// outgoing rows tagged with that producer. A chunk carries an ingest-tagged row (`props.owner
/// = kaban-ingest`), a human `link` (`owner = human`, `layer = human`) and an untagged legacy
/// row (no `owner`, no `layer`); the ingest producer re-upserts the chunk under
/// `{"mode":"complete","producer":"kaban-ingest"}` naming no edge: its own row goes, the
/// human link and the legacy row survive byte-identical. `producer: human` is refused by the
/// predicate, not the type: a human `Complete` re-upsert deletes nothing.
#[test]
fn a_complete_producer_batch_deletes_only_its_own_unnamed_rows() {
    let s = common::Scratch::new("e28-complete-producer", "current");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let (chunk, ingest_target, human_target, legacy_target) =
        (NodeId::new(), NodeId::new(), NodeId::new(), NodeId::new());
    let ingest = "kaban-ingest";
    store
        .apply_batch(
            owned_batch(
                vec![
                    upsert(&chunk, "Fact", "a chunk the ingest layer wrote"),
                    upsert(&ingest_target, "Fact", "what the ingest layer linked"),
                    upsert(&human_target, "Fact", "what the human linked"),
                    upsert(&legacy_target, "Fact", "what a pre-EN-E4 row pointed at"),
                    owned_edge(&chunk, &ingest_target, "RELATES_TO", ingest),
                ],
                ingest,
            ),
            &ctx(),
        )
        .unwrap();
    store
        .link(
            &chunk,
            &human_target,
            "RELATES_TO",
            Some(0.75),
            Some(serde_json::json!({"note": "kept by hand"})),
            &ctx(),
        )
        .unwrap();
    // a raw row of a producer that pre-dates `props.owner`: neither tag
    let raw = rusqlite::Connection::open(s.db()).unwrap();
    SqliteStore::bind_writer(&raw, &ActorId::default()).unwrap();
    raw.execute(
            "INSERT INTO edge(id, src, dst, rel, created_at, props, workspace_id)
             SELECT 'legacy-e28', s.rid, d.rid, 'RELATES_TO', '2026-10-26T09:32:26Z', '{}', s.workspace_id
             FROM node s, node d WHERE s.id = ?1 AND d.id = ?2",
            [chunk.as_str(), legacy_target.as_str()],
        )
        .unwrap();
    let before = out_edge_rows(&store, &chunk);
    assert_eq!(before.len(), 3, "the fixture: {before:?}");
    let row_to = |rows: &[serde_json::Value], dst: &NodeId| {
        rows.iter()
            .find(|e| e["dst"] == serde_json::json!(dst.to_string()))
            .cloned()
    };
    assert_eq!(
        row_to(&before, &ingest_target).unwrap()["props"]["owner"],
        serde_json::json!(ingest)
    );
    let complete = |producer: &str| -> WriteBatch {
        let mut b = batch(vec![upsert(&chunk, "Fact", "a chunk, re-ingested")]);
        b.edge_diff = serde_json::from_value(serde_json::json!({
            "mode": "complete",
            "producer": producer,
        }))
        .unwrap();
        b
    };

    // a human `Complete` identifies no producer's rows: nothing is deleted
    store.apply_batch(complete("human"), &ctx()).unwrap();
    assert_eq!(
        out_edge_rows(&store, &chunk),
        before,
        "a human producer deletes nothing"
    );

    // the ingest producer's `Complete`: its own unnamed row goes, nothing else moves
    store.apply_batch(complete(ingest), &ctx()).unwrap();
    let after = out_edge_rows(&store, &chunk);
    assert_eq!(
        row_to(&after, &ingest_target),
        None,
        "the producer's own unnamed row is deleted: {after:?}"
    );
    assert_eq!(
        row_to(&after, &human_target),
        row_to(&before, &human_target),
        "the human link survives byte-identical"
    );
    assert_eq!(
        row_to(&after, &legacy_target),
        row_to(&before, &legacy_target),
        "the untagged legacy row survives byte-identical"
    );
    assert_eq!(after.len(), 2, "{after:?}");
}
