//! Public metadata registry contract: durable, idempotent and tenant-scoped.
#![allow(clippy::unwrap_used, clippy::expect_used)] // test assertions

use kaban_core::{IngestJob, IngestStore, JobId, JobStatus, WorkspaceId};
use kaban_store_sqlite::SqliteStore;
mod common;

fn row() -> IngestJob {
    IngestJob {
        job: JobId::from_digest(&[7; 32]),
        workspace: WorkspaceId::new("a"),
        source_id: "source-id-distinct-from-document".into(),
        relative_path: "docs/a.md".into(),
        content_hash: "hash".into(),
        pipeline_version: "1".into(),
        chunk_config_hash: "chunks".into(),
        mode: "structured".into(),
        dialect: "none".into(),
        doc_type: "generic".into(),
        kinds: vec!["Fact".into()],
        plan_json: "{\"v\":1}".into(),
        named_edges: Vec::new(),
        submits: 0,
        created: 0,
        updated: 0,
        unchanged: 0,
        redactions: 0,
        status: JobStatus::Open,
        created_at: 1,
        expires_at: 3600,
        finished_at: None,
    }
}

#[test]
fn repeated_open_keeps_original_contract_and_stale_state() {
    let store = SqliteStore::open_memory().unwrap();
    let original = row();
    assert_eq!(store.job_open(&original).unwrap(), original);
    assert!(
        store
            .job_mark_stale(&original.job, &original.workspace)
            .unwrap()
    );
    let mut changed = original.clone();
    changed.plan_json = "{\"v\":1,\"different_contract\":true}".into();
    changed.created_at = 2;
    let actual = store.job_open(&changed).unwrap();
    assert_eq!(actual.plan_json, original.plan_json);
    assert_eq!(actual.created_at, original.created_at);
    assert_eq!(actual.status, JobStatus::Stale);
    assert_eq!(actual.submits, 0);
}

#[test]
fn foreign_and_unknown_jobs_have_identical_lookup_behavior() {
    let store = SqliteStore::open_memory().unwrap();
    let original = row();
    store.job_open(&original).unwrap();
    let foreign = WorkspaceId::new("b");
    let unknown = JobId::from_digest(&[8; 32]);
    assert!(store.job_get(&original.job, &foreign).unwrap().is_none());
    assert!(store.job_get(&unknown, &foreign).unwrap().is_none());
    assert!(!store.job_mark_stale(&original.job, &foreign).unwrap());
    assert!(!store.job_mark_stale(&unknown, &foreign).unwrap());
    assert_eq!(
        store
            .job_get(&original.job, &original.workspace)
            .unwrap()
            .unwrap(),
        original
    );
    let mut collision = original.clone();
    collision.workspace = foreign;
    assert_eq!(
        store.job_open(&collision).unwrap_err().to_string(),
        "node not found: job_revoked"
    );
    assert_eq!(
        store
            .job_get(&original.job, &original.workspace)
            .unwrap()
            .unwrap(),
        original
    );
}

#[test]
fn handle_decoder_refuses_noncanonical_and_unknown_json_fields() {
    assert!(JobId::parse(&"A".repeat(64)).is_err());
    assert!(JobId::parse(&"a".repeat(63)).is_err());
    assert!(JobId::parse("credential-canary").is_err());
    assert_eq!(
        JobId::parse("credential-canary").unwrap_err().to_string(),
        "invalid_job_handle"
    );
    let valid = row();
    let bytes = serde_json::to_vec(&valid).unwrap();
    assert_eq!(serde_json::from_slice::<IngestJob>(&bytes).unwrap(), valid);
    let mut value = serde_json::to_value(&valid).unwrap();
    value["chunk_text"] = "CANARY_OUTSIDE_REGISTRY".into();
    assert!(serde_json::from_value::<IngestJob>(value).is_err());
}

#[test]
fn job_survives_close_and_reopen_without_tenant_rebinding() {
    let scratch = common::Scratch::new("kaban-e38-jobs", "reopen");
    let original = row();
    {
        let store = SqliteStore::open(&scratch.db(), &scratch.keys()).unwrap();
        store.job_open(&original).unwrap();
    }
    let reopened = SqliteStore::open(&scratch.db(), &scratch.keys()).unwrap();
    assert_eq!(
        reopened
            .job_get(&original.job, &original.workspace)
            .unwrap()
            .unwrap(),
        original
    );
    assert!(
        reopened
            .job_get(&original.job, &WorkspaceId::new("b"))
            .unwrap()
            .is_none()
    );
}
