//! Document lifecycle changes revoke metadata handles in the same public transaction.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use kaban_core::{
    Actor, EdgeDiff, ForgetMode, IngestJob, IngestStore, JobId, JobStatus, MemoryStore, NewNode,
    NodeId, Op, WorkspaceId, WriteBatch, WriteCtx,
};
use kaban_core::{ImportedRecord, Lifecycle, RescanAction, SyncStore};
use kaban_store_sqlite::SqliteStore;
use serde_json::json;

fn fixture() -> (SqliteStore, NodeId, IngestJob, WriteCtx) {
    let store = SqliteStore::open_memory().unwrap();
    let ctx = WriteCtx::new("policy", WorkspaceId::default(), Actor::Agent);
    let document = NodeId::new();
    let mut batch = WriteBatch::new(
        ctx.workspace_id.clone(),
        vec![Op::UpsertNode {
            id: document.clone(),
            node: NewNode {
                kind: "Document".into(),
                text: "source".into(),
                props: Some(json!({"owner":"kaban-ingest","source_id":"di-not-node"})),
                ..Default::default()
            },
        }],
    );
    batch.edge_diff = EdgeDiff::None;
    store.apply_batch(batch, &ctx).unwrap();
    let job = IngestJob {
        job: JobId::from_digest(&[10; 32]),
        workspace: ctx.workspace_id.clone(),
        source_id: "di-not-node".into(),
        relative_path: "source.md".into(),
        content_hash: "hash".into(),
        pipeline_version: "1".into(),
        chunk_config_hash: "chunks".into(),
        mode: "structured".into(),
        dialect: "none".into(),
        doc_type: "generic".into(),
        kinds: vec!["Fact".into()],
        plan_json: json!({"v":1,"document_id":document}).to_string(),
        named_edges: vec![],
        submits: 0,
        created: 0,
        updated: 0,
        unchanged: 0,
        redactions: 0,
        status: JobStatus::Open,
        created_at: 1,
        expires_at: i64::MAX,
        finished_at: None,
    };
    store.job_open(&job).unwrap();
    (store, document, job, ctx)
}

#[test]
fn document_forget_atomically_removes_the_bound_job() {
    for mode in [
        ForgetMode::Tombstone,
        ForgetMode::Purge,
        ForgetMode::Invalidate,
    ] {
        let (store, document, job, ctx) = fixture();
        store.forget(&document, mode, &ctx).unwrap();
        assert!(
            store.job_get(&job.job, &job.workspace).unwrap().is_none(),
            "Document terminal transaction must revoke its job row"
        );
    }
}

#[test]
fn batch_tombstone_and_rescan_reject_revoke_and_failed_batch_rolls_back() {
    for route in [0, 1, 2] {
        let (store, document, job, ctx) = fixture();
        if route == 1 {
            store
                .rescan("changed", 100, &mut |n| {
                    if n.kind == "Document" {
                        RescanAction::Reject { audit: vec![] }
                    } else {
                        RescanAction::Keep
                    }
                })
                .unwrap();
        } else {
            let mut ops = vec![Op::Tombstone {
                id: document.clone(),
                reason: "test".into(),
            }];
            if route == 2 {
                ops.push(Op::Tombstone {
                    id: NodeId::new(),
                    reason: "missing".into(),
                });
            }
            let result = store.apply_batch(WriteBatch::new(ctx.workspace_id.clone(), ops), &ctx);
            assert_eq!(result.is_err(), route == 2);
        }
        assert_eq!(
            store.job_get(&job.job, &job.workspace).unwrap().is_none(),
            route != 2
        );
        assert_eq!(
            store.get(&document, true).unwrap().unwrap().lifecycle,
            if route == 2 {
                Lifecycle::Active
            } else {
                Lifecycle::Tombstoned
            }
        );
    }
}

#[test]
fn imported_terminal_document_and_purged_import_revoke_the_job() {
    for purge in [false, true] {
        let (store, document, job, ctx) = fixture();
        if purge {
            store.import_purged(&document, "Document", 2, &ctx).unwrap();
        } else {
            let n = store.get(&document, true).unwrap().unwrap();
            let record = ImportedRecord {
                id: document,
                kind: n.kind,
                text: n.text,
                summary: n.summary,
                tags: n.tags,
                confidence: n.confidence,
                importance: n.importance,
                lifecycle: Lifecycle::Tombstoned,
                governance: n.governance,
                path_scope: n.path_scope,
                props: n.props,
                rev: n.rev + 1,
                created_at: n.created_at,
                updated_at: n.updated_at,
                valid_from: n.valid_from,
                valid_to: n.valid_to,
                accepted_commit: n.accepted_commit,
                agent_id: n.agent_id,
                session_id: n.session_id,
                repo: n.repo,
                commit_sha: n.commit_sha,
                dup_key: n.dup_key,
                event: None,
            };
            store.import_record(&record, &ctx).unwrap();
        }
        assert!(store.job_get(&job.job, &job.workspace).unwrap().is_none());
    }
}

#[test]
fn revocation_binds_workspace_document_and_source_not_one_identifier() {
    let (store, document, job, ctx) = fixture();
    let mut other = job.clone();
    other.job = JobId::from_digest(&[11; 32]);
    other.source_id = "other-source".into();
    store.job_open(&other).unwrap();
    let mut foreign = job.clone();
    foreign.job = JobId::from_digest(&[12; 32]);
    foreign.workspace = WorkspaceId::new("foreign");
    store.job_open(&foreign).unwrap();
    let mut another = job.clone();
    another.job = JobId::from_digest(&[13; 32]);
    another.plan_json = json!({"v":1,"document_id":NodeId::new()}).to_string();
    store.job_open(&another).unwrap();
    store
        .forget(&document, ForgetMode::Tombstone, &ctx)
        .unwrap();
    assert!(store.job_get(&job.job, &job.workspace).unwrap().is_none());
    for retained in [other, foreign, another] {
        assert_eq!(
            store
                .job_get(&retained.job, &retained.workspace)
                .unwrap()
                .unwrap(),
            retained
        );
    }
}
