//! Prepared ordinary opens cannot restore metadata across a source lifecycle boundary.
#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;
use kaban_core::{
    Actor, ForgetMode, IngestJob, IngestStore, JobId, JobOpenRequest, JobStatus, MemoryStore,
    NewNode, NodeId, Op, SourceRev, WorkspaceId, WriteBatch, WriteCtx,
};
use kaban_store_sqlite::SqliteStore;
use serde_json::json;

fn fixture(store: &SqliteStore) -> (IngestJob, NodeId, NodeId, SourceRev, WriteCtx) {
    let ctx = WriteCtx::new("policy", WorkspaceId::default(), Actor::Agent);
    let document = NodeId::new();
    let chunk = NodeId::new();
    store.apply_batch(WriteBatch::new(ctx.workspace_id.clone(), vec![
        Op::UpsertNode { id: chunk.clone(), node: NewNode {
            kind: "Chunk".into(), text: "invented chunk".into(),
            props: Some(json!({"owner":"kaban-ingest"})), ..Default::default()
        }},
        Op::UpsertNode { id: document.clone(), node: NewNode {
            kind: "Document".into(), text: "invented source".into(),
            props: Some(json!({"owner":"kaban-ingest","source_id":"source-a","content_hash":"source-hash","chunk_ids":[chunk]})),
            ..Default::default()
        }},
    ]), &ctx).unwrap();
    let source_rev = SourceRev::from(&store.get(&document, false).unwrap().unwrap());
    let job = IngestJob {
        job: JobId::from_digest(&[41; 32]),
        workspace: ctx.workspace_id.clone(),
        source_id: "source-a".into(),
        relative_path: "a.md".into(),
        content_hash: "source-hash".into(),
        pipeline_version: "1".into(),
        chunk_config_hash: "chunks".into(),
        mode: "structured".into(),
        dialect: "none".into(),
        doc_type: "generic".into(),
        kinds: vec!["Fact".into()],
        plan_json: json!({"v":1,"document_id":document,"chunks":[{"id":chunk}],"submit_state":{}})
            .to_string(),
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
    (job, document, chunk, source_rev, ctx)
}

#[test]
fn prepared_ordinary_open_after_purge_or_tombstone_cannot_recreate_metadata() {
    for mode in [ForgetMode::Purge, ForgetMode::Tombstone] {
        let scratch = common::Scratch::new("kaban-wave4-open", &format!("{mode:?}"));
        let store = SqliteStore::open(&scratch.db(), &scratch.keys()).unwrap();
        let (job, document, chunk, source_rev, ctx) = fixture(&store);
        let request = JobOpenRequest {
            job: job.clone(),
            document: document.clone(),
            source_rev,
            chunks: vec![chunk],
        };
        // Capture the ordinary preparation first, then commit revocation on another connection.
        let other = SqliteStore::open(&scratch.db(), &scratch.keys()).unwrap();
        other.forget(&document, mode, &ctx).unwrap();
        let opened = store.job_open_checked(request);
        assert!(
            opened.is_err(),
            "prepared ordinary open must refuse a revoked Document"
        );
        assert!(store.job_get(&job.job, &job.workspace).unwrap().is_none());
        let raw = rusqlite::Connection::open(scratch.db()).unwrap();
        assert_eq!(
            raw.query_row("SELECT count(*) FROM ingest_jobs", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0,
            "purge/tombstone must leave no late job metadata at rest"
        );
    }
}

#[test]
fn ordinary_open_refuses_stale_revision_hash_chunks_and_tenant_without_partial_write() {
    for failure in [
        "revision",
        "hash",
        "tenant",
        "missing_chunk",
        "dead_chunk",
        "unowned_document",
    ] {
        let scratch = common::Scratch::new("kaban-wave4-open", failure);
        let store = SqliteStore::open(&scratch.db(), &scratch.keys()).unwrap();
        let (job, document, chunk, source_rev, ctx) = fixture(&store);
        let mut request = JobOpenRequest {
            job: job.clone(),
            document: document.clone(),
            source_rev,
            chunks: vec![chunk.clone()],
        };
        match failure {
            "revision" => request.source_rev.rev += 1,
            "hash" => request.source_rev.content_hash = "obsolete".into(),
            "tenant" => request.job.workspace = WorkspaceId::new("foreign"),
            "missing_chunk" => {
                request.chunks.clear();
                let mut plan: serde_json::Value =
                    serde_json::from_str(&request.job.plan_json).unwrap();
                plan["chunks"] = json!([]);
                request.job.plan_json = plan.to_string();
            }
            "dead_chunk" => {
                store.forget(&chunk, ForgetMode::Tombstone, &ctx).unwrap();
            }
            "unowned_document" => {
                store.apply_batch(WriteBatch::new(ctx.workspace_id.clone(), vec![Op::UpsertNode {
                    id: document.clone(), node: NewNode {
                        kind: "Document".into(), text: "invented source".into(),
                        props: Some(json!({"owner":"another-producer","source_id":"source-a","content_hash":"source-hash","chunk_ids":[chunk]})),
                        ..Default::default()
                    }
                }]), &ctx).unwrap();
                request.source_rev =
                    SourceRev::from(&store.get(&document, false).unwrap().unwrap());
            }
            _ => unreachable!(),
        }
        assert!(
            store.job_open_checked(request.clone()).is_err(),
            "{failure} must refuse ordinary opening"
        );
        assert!(store.job_get(&job.job, &job.workspace).unwrap().is_none());
        assert!(
            store
                .job_get(&job.job, &request.job.workspace)
                .unwrap()
                .is_none()
        );
        let raw = rusqlite::Connection::open(scratch.db()).unwrap();
        assert_eq!(
            raw.query_row("SELECT count(*) FROM ingest_jobs", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0,
            "{failure} must not insert metadata"
        );
    }
}

#[test]
fn checked_repeat_preserves_original_contract_and_counters() {
    let store = SqliteStore::open_memory().unwrap();
    let (mut job, document, chunk, source_rev, _) = fixture(&store);
    job.submits = 3;
    job.created = 4;
    job.updated = 5;
    job.unchanged = 6;
    job.redactions = 7;
    let mut request = JobOpenRequest {
        job: job.clone(),
        document,
        source_rev,
        chunks: vec![chunk],
    };
    assert_eq!(store.job_open_checked(request.clone()).unwrap(), job);
    request.job.created = 0;
    request.job.submits = 0;
    request.job.relative_path = "changed.md".into();
    request.job.plan_json = {
        let mut plan: serde_json::Value = serde_json::from_str(&job.plan_json).unwrap();
        plan["submit_state"] = json!({"changed":true});
        plan.to_string()
    };
    assert_eq!(store.job_open_checked(request).unwrap(), job);
}
