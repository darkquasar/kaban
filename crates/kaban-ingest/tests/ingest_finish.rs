//! Public finish union, canonical bytes, current-policy refusal and lifecycle outcomes.
#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;
use kaban_api::KabanApi;
use kaban_core::{ChangeLogStore, IngestStore, JobId, JobStatus, MemoryStore, NodeId};
use kaban_ingest::jobs::{
    brief::brief,
    finish::finish,
    submit::submit,
    types::{IngestOpts, JobError},
};
use serde_json::{Value, json};

fn api(rig: &common::real::Rig, cap: u32) -> KabanApi {
    let mut engine = kaban_core::config::EngineConfig::default();
    engine.ingest.finish_cap = cap;
    KabanApi::with_config(
        rig.store.clone(),
        kaban_policy::Policy::empty(),
        rig.store.signer(),
        None,
        kaban_api::ApiConfig {
            engine: std::sync::Arc::new(engine),
            ..Default::default()
        },
    )
}
fn rig(name: &str) -> common::real::Rig {
    let mut rig = common::real::rig("kaban-e310-finish", name);
    let mut ontology = kaban_core::shipped::resolved().unwrap();
    ontology
        .priors
        .insert("script.ingest_submit.edge".into(), 1.0);
    ontology
        .priors
        .insert("script.ingest_submit.kind_of_record".into(), 1.0);
    rig.store = std::sync::Arc::new(
        kaban_store_sqlite::SqliteStore::open_memory()
            .unwrap()
            .with_ontology(std::sync::Arc::new(ontology))
            .unwrap(),
    );
    std::fs::write(
        rig.root.join("a.md"),
        "# Source\n\n## First\n\nFirst harmless source.\n\n## Second\n\nSecond harmless source.\n",
    )
    .unwrap();
    rig
}

#[tokio::test]
async fn finish_preserves_union_across_chunks_and_returns_exact_retained_bytes() {
    let rig = rig("union");
    let api = api(&rig, 10000);
    let prepared = brief(&api, &rig.root, "a.md", &IngestOpts::default())
        .await
        .unwrap();
    assert!(prepared.chunk_plan.chunks.len() >= 2);
    let mut ids = Vec::new();
    for (index, local, quote) in [
        (0, "first", "First harmless"),
        (1, "second", "Second harmless"),
    ] {
        let result=submit(&api,&rig.root,json!({"job":prepared.job,"chunk":index,"records":[{"local_id":local,"kind":"Fact","text":format!("{quote} assertion"),"confidence":1.0,"evidence":[{"chunk_id":prepared.chunk_plan.chunks[index].id,"quote":quote}]}]})).await.unwrap();
        ids.push(NodeId::parse(result["records"][0]["id"].as_str().unwrap()).unwrap());
    }
    let job = JobId::parse(&prepared.job).unwrap();
    let bytes = finish(&api, &rig.root, &job).await.unwrap();
    let report: Value = serde_json::from_str(&bytes).unwrap();
    assert_eq!(report["submits"], 2);
    assert_eq!(report["created"], 2);
    assert_eq!(report["reconcile"], "complete");
    assert_eq!(report["deleted_edges"], 0);
    let edges = rig
        .store
        .traverse(&kaban_core::TraverseQuery {
            start_ids: vec![ids[0].clone()],
            rels: vec!["EVIDENCE_FOR".into(), "DERIVED_FROM".into()],
            filters: kaban_core::ReadFilters {
                include_candidates: true,
            },
            ..Default::default()
        })
        .unwrap()
        .edges;
    assert!(edges.iter().any(|e| e.rel == "EVIDENCE_FOR"));
    assert!(edges.iter().any(|e| e.rel == "DERIVED_FROM"));
    let changes = rig.store.changes_read(0, 10000).unwrap();
    assert_eq!(finish(&api, &rig.root, &job).await.unwrap(), bytes);
    assert_eq!(rig.store.changes_read(0, 10000).unwrap(), changes);
    let row = rig.store.job_get(&job, api.workspace()).unwrap().unwrap();
    assert_eq!(row.status, JobStatus::Finished);
    assert!(!row.plan_json.contains("harmless"));
    assert!(!row.plan_json.contains("assertion\""));
    // the receipt (spec 05 SS-10) is per database and per run: its identities and position
    // are masked like the job and the change interval; its outcome is the snapshot's
    assert_eq!(report["receipt"]["seq"], report["changes"]["last_seq"]);
    let mut snapshot = report;
    snapshot["job"] = json!("<JOB>");
    snapshot["changes"]["first_seq"] = json!("<FIRST_SEQ>");
    snapshot["changes"]["last_seq"] = json!("<LAST_SEQ>");
    snapshot["receipt"]["instance_id"] = json!("<INSTANCE_ID>");
    snapshot["receipt"]["pack_lineage_id"] = json!("<PACK_LINEAGE_ID>");
    snapshot["receipt"]["seq"] = json!("<LAST_SEQ>");
    insta::assert_json_snapshot!("report", snapshot);
}

/// D3 review d3-6 (SS-10 "no-op requests still wait for the relevant returned state"): a run
/// that submitted nothing emits no change row, and its receipt names the log head the finish
/// transaction saw (the state the run references, on a database that already holds rows),
/// never position 0; the change interval stays absent.
#[tokio::test]
async fn a_finish_that_emitted_no_changes_reports_the_head_it_saw_never_zero() {
    let rig = rig("no-op");
    let api = api(&rig, 10000);
    api.remember(kaban_api::RememberRequest {
        kind: "Fact".into(),
        text: "already stored before the run".into(),
        ..Default::default()
    })
    .unwrap();
    let prepared = brief(&api, &rig.root, "a.md", &IngestOpts::default())
        .await
        .unwrap();
    let head = rig.store.changes_head().unwrap();
    assert!(head > 0, "the database holds state at positive positions");
    let job = JobId::parse(&prepared.job).unwrap();
    let bytes = finish(&api, &rig.root, &job).await.unwrap();
    let report: Value = serde_json::from_str(&bytes).unwrap();
    assert_eq!(report["submits"], 0, "{report}");
    assert_eq!(report["changes"]["first_seq"], Value::Null, "{report}");
    assert_eq!(report["receipt"]["outcome"], "committed", "{report}");
    let seq = report["receipt"]["seq"].as_u64().expect("a position");
    assert!(seq >= head && seq > 0, "{report}");
    assert_eq!(
        seq,
        rig.store.changes_head().unwrap(),
        "the head inside the finish transaction: {report}"
    );
}

#[tokio::test]
async fn overflow_default_no_edge_and_present_expired_vs_pruned_refusals() {
    let rig = common::real::rig("kaban-e310-finish", "overflow");
    std::fs::write(rig.root.join("a.md"), "# Source\n\nHarmless body.\n").unwrap();
    let api = api(&rig, 1);
    let prepared = brief(&api, &rig.root, "a.md", &IngestOpts::default())
        .await
        .unwrap();
    let job = JobId::parse(&prepared.job).unwrap();
    submit(&api,&rig.root,json!({"job":prepared.job,"records":[{"local_id":"r","kind":"Fact","text":"Harmless assertion"},{"local_id":"s","kind":"Fact","text":"Second assertion"}]})).await.unwrap();
    let report: Value =
        serde_json::from_str(&finish(&api, &rig.root, &job).await.unwrap()).unwrap();
    assert_eq!(report["reconcile"], "skipped_cap");
    assert_eq!(report["deleted_edges"], 0);
    let row = rig.store.job_get(&job, api.workspace()).unwrap().unwrap();
    assert_eq!(
        rig.store
            .jobs_prune(api.workspace(), row.expires_at, 1)
            .unwrap(),
        1
    );
    assert!(matches!(
        finish(&api, &rig.root, &job).await,
        Err(JobError::Revoked)
    ));
    assert!(matches!(
        finish(&api, &rig.root, &JobId::from_digest(&[255; 32])).await,
        Err(JobError::Revoked)
    ));
    let mut expired = row;
    expired.job = JobId::from_digest(&[254; 32]);
    expired.status = JobStatus::Open;
    expired.expires_at = 0;
    rig.store.job_open(&expired).unwrap();
    assert!(matches!(
        finish(&api, &rig.root, &expired.job).await,
        Err(JobError::Expired)
    ));
    let foreign = KabanApi::with_config(
        rig.store.clone(),
        kaban_policy::Policy::empty(),
        rig.store.signer(),
        None,
        kaban_api::ApiConfig {
            workspace: kaban_core::WorkspaceId::new("foreign"),
            ..Default::default()
        },
    );
    assert!(matches!(
        finish(&foreign, &rig.root, &expired.job).await,
        Err(JobError::Revoked)
    ));
}

#[tokio::test]
async fn changed_policy_refuses_retained_success_without_remasking_or_rewriting_it() {
    let rig = rig("policy");
    let api = api(&rig, 10000);
    let prepared = brief(&api, &rig.root, "a.md", &IngestOpts::default())
        .await
        .unwrap();
    let job = JobId::parse(&prepared.job).unwrap();
    let bytes = finish(&api, &rig.root, &job).await.unwrap();
    let before = rig.store.job_get(&job, api.workspace()).unwrap().unwrap();
    // The source stays harmless; this current rule affects the retained report's fixed
    // count key/value path, proving report re-gating rather than source refusal.
    let file = kaban_policy::Policy::parse(
        b"words:\n  - { term: submits, action: redact, id: report-change }\n",
    )
    .unwrap();
    let policy = kaban_policy::Policy::from_file(file, "new".into()).unwrap();
    let changed = KabanApi::new(rig.store.clone(), policy, rig.store.signer(), None);
    // Closed control values are validated, not scanned. A CONTENT map key is the
    // report-policy seam; use the public gate adapter to retain invented free content.
    let source = rig
        .store
        .get(&NodeId::parse(&prepared.document.id).unwrap(), true)
        .unwrap()
        .unwrap();
    let req = kaban_core::JobFinishRequest {
        job: job.clone(),
        contract: kaban_core::JobSubmitContract::from_finish_job(&before).unwrap(),
        document: NodeId::parse(&source.id).unwrap(),
        source_rev: kaban_core::SourceRev::from(&source),
        chunks: prepared
            .chunk_plan
            .chunks
            .iter()
            .map(|c| NodeId::parse(&c.id).unwrap())
            .collect(),
        finish_cap: 10000,
        retention_days: 7,
    };
    let result = changed.publish_ingest_finish(req, &mut |_, _| Ok(bytes.clone()));
    // Current policy must not silently produce different retained success bytes.
    assert!(
        result.is_err(),
        "current report key redaction must refuse exact retained success"
    );
    assert!(finish(&changed, &rig.root, &job).await.is_err());
    assert_eq!(
        rig.store.job_get(&job, api.workspace()).unwrap().unwrap(),
        before
    );
    let reject = kaban_policy::Policy::from_file(
        kaban_policy::Policy::parse(
            b"rules:\n  - { regex: 'a\\.md', action: reject, id: changed-source }\n",
        )
        .unwrap(),
        "reject".into(),
    )
    .unwrap();
    let changed = KabanApi::new(rig.store.clone(), reject, rig.store.signer(), None);
    assert!(finish(&changed, &rig.root, &job).await.is_err());
    assert_eq!(
        rig.store.job_get(&job, api.workspace()).unwrap().unwrap(),
        before
    );
}

#[tokio::test]
async fn forget_revokes_next_and_finish_and_preview_never_prunes() {
    let rig = rig("revoke");
    let api = api(&rig, 10000);
    let prepared = brief(&api, &rig.root, "a.md", &IngestOpts::default())
        .await
        .unwrap();
    let job = JobId::parse(&prepared.job).unwrap();
    let mut old = rig.store.job_get(&job, api.workspace()).unwrap().unwrap();
    old.job = JobId::from_digest(&[253; 32]);
    old.status = JobStatus::Finished;
    old.expires_at = 0;
    rig.store.job_open(&old).unwrap();
    brief(
        &api,
        &rig.root,
        "a.md",
        &IngestOpts {
            dry_run: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(
        rig.store
            .job_get(&old.job, api.workspace())
            .unwrap()
            .is_some()
    );
    let ctx = kaban_core::WriteCtx::new(
        api.policy_version(),
        api.workspace().clone(),
        kaban_core::Actor::HumanCli,
    );
    rig.store
        .forget(
            &NodeId::parse(&prepared.document.id).unwrap(),
            kaban_core::ForgetMode::Tombstone,
            &ctx,
        )
        .unwrap();
    assert!(matches!(
        finish(&api, &rig.root, &job).await,
        Err(JobError::Revoked)
    ));
    assert!(matches!(
        kaban_ingest::jobs::next::next(
            &api,
            &rig.root,
            &job,
            0,
            0,
            kaban_ingest::jobs::next::PageBudget::default()
        )
        .await,
        Err(JobError::Revoked)
    ));
}
