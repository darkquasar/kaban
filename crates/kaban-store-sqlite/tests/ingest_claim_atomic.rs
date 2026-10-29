//! Public transaction seams: bounded selection, exact identities, rollback and retention.
#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;
use kaban_core::{
    Actor, EdgeDiff, IngestJob, IngestStore, JobClaimRequest, JobFinishRequest,
    JobFinishStateUpdate, JobId, JobStatus, JobSubmitContract, Layer, LeaseStore, MemoryStore,
    NewNode, NodeId, Op, SourceRev, StoreError, WorkspaceId, WriteBatch, WriteCtx,
};
use kaban_store_sqlite::SqliteStore;
use serde_json::{Value, json};

fn ctx() -> WriteCtx {
    WriteCtx::new("fixture-policy", WorkspaceId::default(), Actor::Agent)
}
fn document(store: &SqliteStore, path: &str, workspace: &WorkspaceId) -> IngestJob {
    let id = NodeId::new();
    let source = format!("source-{path}");
    let mut batch = WriteBatch::new(
        workspace.clone(),
        vec![Op::UpsertNode {
            id: id.clone(),
            node: NewNode {
                kind: "Document".into(),
                text: "invented document".into(),
                props: Some(
                    json!({"owner":"kaban-ingest","source_id":source,"content_hash":path,"path":path,"chunk_ids":[]}),
                ),
                ..Default::default()
            },
        }],
    );
    batch.edge_diff = EdgeDiff::None;
    let mut context = ctx();
    context.workspace_id = workspace.clone();
    store.apply_batch(batch, &context).unwrap();
    let mut digest = [0; 32];
    digest[..16].copy_from_slice(id.as_str().as_bytes().get(..16).unwrap());
    // UUID suffix differentiates fixtures even when the common leading bytes repeat.
    digest[16..].copy_from_slice(&id.as_str().as_bytes()[id.as_str().len() - 16..]);
    IngestJob{job:JobId::from_digest(&digest),workspace:workspace.clone(),source_id:source,relative_path:path.into(),content_hash:path.into(),pipeline_version:"1".into(),chunk_config_hash:"fixture".into(),mode:"structured".into(),dialect:"gliner".into(),doc_type:"generic".into(),kinds:vec![],plan_json:json!({"v":1,"document_id":id,"chunks":[],"submit_state":{},"finished_report_json":null}).to_string(),named_edges:vec![],submits:0,created:0,updated:0,unchanged:0,redactions:0,status:JobStatus::Open,created_at:1,expires_at:i64::MAX,finished_at:None}
}
fn id(job: &IngestJob) -> NodeId {
    serde_json::from_str::<Value>(&job.plan_json).unwrap()["document_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap()
}
fn claim(store: &SqliteStore, job: &IngestJob) -> Option<kaban_core::Lease> {
    store
        .job_claim(JobClaimRequest {
            job: job.clone(),
            source_rev: SourceRev::from(&store.get(&id(job), true).unwrap().unwrap()),
            worker: "fixture:1".into(),
            ttl_s: 300,
            max_attempts: 3,
        })
        .unwrap()
}
fn finish_request(store: &SqliteStore, job: &IngestJob, fence: u64) -> JobFinishRequest {
    let row = store.job_get(&job.job, &job.workspace).unwrap().unwrap();
    JobFinishRequest {
        job: job.job.clone(),
        contract: JobSubmitContract::from_finish_job(&row)
            .unwrap()
            .with_fence(Some(fence)),
        document: id(job),
        source_rev: SourceRev::from(&store.get(&id(job), true).unwrap().unwrap()),
        chunks: vec![],
        finish_cap: 10000,
        retention_days: 7,
    }
}
#[test]
fn finish_marker_release_and_report_rollback_together_and_retry_cannot_change_them() {
    let scratch = common::Scratch::new("kaban-e310b-store", "marker");
    let store = SqliteStore::open(&scratch.db(), &scratch.keys()).unwrap();
    let job = document(&store, "a.md", &WorkspaceId::default());
    let lease = claim(&store, &job).unwrap();
    let raw = rusqlite::Connection::open(scratch.db()).unwrap();
    assert!(
        store
            .job_finish(
                finish_request(&store, &job, lease.fence),
                &ctx(),
                &mut |_, _| Err(StoreError::InvalidName("fixture".into(), "refuse"))
            )
            .is_err()
    );
    assert_eq!(
        raw.query_row("SELECT count(*) FROM derived_artifact", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        store
            .job_get(&job.job, &job.workspace)
            .unwrap()
            .unwrap()
            .status,
        JobStatus::Open
    );
    store
        .job_holder(&job.job, &job.workspace, Some(1), false)
        .unwrap();
    let bytes = store
        .job_finish(finish_request(&store, &job, 1), &ctx(), &mut |_, _| {
            Ok(JobFinishStateUpdate {
                report_json: "{}".into(),
                audit: vec![],
            })
        })
        .unwrap();
    assert_eq!(bytes, "{}");
    let (source_rev, key): (String, String) = raw
        .query_row(
            "SELECT source_rev,artifact_key FROM derived_artifact",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    let revision: SourceRev = serde_json::from_str(&source_rev).unwrap();
    assert_eq!(
        revision,
        SourceRev::from(&store.get(&id(&job), true).unwrap().unwrap())
    );
    assert_eq!(key.len(), 64);
    assert!(
        store
            .job_finish(finish_request(&store, &job, 1), &ctx(), &mut |_, _| Ok(
                JobFinishStateUpdate {
                    report_json: "{\"changed\":true}".into(),
                    audit: vec![]
                }
            ))
            .is_err()
    );
    let successor = store
        .lease_acquire(&lease.name, "other", 300)
        .unwrap()
        .unwrap();
    assert_eq!(successor.fence, 2);
    assert!(matches!(
        store.job_finish(finish_request(&store, &job, 1), &ctx(), &mut |_, _| Ok(
            JobFinishStateUpdate {
                report_json: "{}".into(),
                audit: vec![]
            }
        )),
        Err(StoreError::LeaseLost { .. })
    ));
    store.jobs_prune(&job.workspace, i64::MAX, 1).unwrap();
    assert!(store.job_get(&job.job, &job.workspace).unwrap().is_none());
    assert!(
        store
            .jobs_pending(&job.workspace, "gliner", None, 1000)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn source_and_workspace_are_independent_of_valid_fence_and_claim_rollback_is_atomic() {
    let store = SqliteStore::open_memory().unwrap();
    let job = document(&store, "a.md", &WorkspaceId::default());
    let mut revision = SourceRev::from(&store.get(&id(&job), true).unwrap().unwrap());
    revision.rev += 1;
    assert!(
        store
            .job_claim(JobClaimRequest {
                job: job.clone(),
                source_rev: revision,
                worker: "fixture:1".into(),
                ttl_s: 300,
                max_attempts: 3
            })
            .is_err()
    );
    assert!(store.job_get(&job.job, &job.workspace).unwrap().is_none());
    let lease = claim(&store, &job).unwrap();
    assert_eq!(lease.fence, 1, "failed claim never consumed an attempt");
    let mut req = finish_request(&store, &job, 1);
    req.source_rev.rev += 1;
    assert!(
        store
            .job_finish(req, &ctx(), &mut |_, _| Ok(JobFinishStateUpdate {
                report_json: "{}".into(),
                audit: vec![]
            }))
            .is_err()
    );
    assert!(matches!(
        store.job_holder(&job.job, &WorkspaceId::new("foreign"), Some(1), true),
        Err(StoreError::NotFound(_))
    ));
    assert!(
        store
            .jobs_pending(&WorkspaceId::new("foreign"), "gliner", None, 1000)
            .unwrap()
            .is_empty()
    );
    store.lease_release(&lease).unwrap();
    let mut incompatible = job.clone();
    let mut plan: Value = serde_json::from_str(&incompatible.plan_json).unwrap();
    plan["different_contract"] = json!(true);
    incompatible.plan_json = plan.to_string();
    assert!(
        store
            .job_claim(JobClaimRequest {
                job: incompatible,
                source_rev: SourceRev::from(&store.get(&id(&job), true).unwrap().unwrap()),
                worker: "rejected:2".into(),
                ttl_s: 300,
                max_attempts: 3,
            })
            .is_err(),
        "binding refusal rolls back allocated token and attempt"
    );
    let second = claim(&store, &job).unwrap();
    assert_eq!(second.fence, 2, "failed binding consumed no generation");
    assert!(
        claim(&store, &job).is_none(),
        "contention consumes no counters"
    );
    store.lease_release(&second).unwrap();
    let third = claim(&store, &job).unwrap();
    assert_eq!(third.fence, 3);
    store.lease_release(&third).unwrap();
    assert!(
        claim(&store, &job).is_none(),
        "only successful claims counted toward cap"
    );
}

#[test]
fn default_pending_scan_is_path_ordered_bounded_at_one_thousand_and_keeps_exhausted_rows() {
    let store = SqliteStore::open_memory().unwrap();
    let workspace = WorkspaceId::default();
    let first = document(&store, "0000.md", &workspace);
    let mut jobs = vec![first.clone()];
    // This measures the required configured bound, not a timing benchmark.
    for n in 1..=1000 {
        jobs.push(document(&store, &format!("{n:04}.md"), &workspace));
    }
    let candidates = store
        .jobs_pending(&workspace, "gliner", None, 1000)
        .unwrap();
    assert_eq!(candidates.len(), 1000);
    assert_eq!(candidates.first().unwrap().path, "0000.md");
    assert_eq!(candidates.last().unwrap().path, "0999.md");
    assert_eq!(
        store
            .jobs_pending(&workspace, "gliner", Some("1000"), 1000)
            .unwrap()
            .len(),
        1
    );
    for n in 1..=3 {
        let lease = claim(&store, &first).unwrap();
        assert_eq!(lease.fence, n);
        store.lease_release(&lease).unwrap();
    }
    assert!(claim(&store, &first).is_none());
    assert_eq!(store.jobs_exhausted(&workspace, 3).unwrap(), 1);
    store.jobs_prune(&workspace, i64::MAX, 1000).unwrap();
    assert!(claim(&store, &first).is_none());
    assert_eq!(store.jobs_exhausted(&workspace, 3).unwrap(), 1);
    let report = store.jobs_reindex(Layer::Gliner, 1, &ctx()).unwrap();
    assert!(report.transactions > 0);
    assert!(
        claim(&store, &first).is_none(),
        "reset cannot erase an unmarked exhausted attempt cap"
    );
    let obsolete_name = format!("ingest:{workspace}:gliner:{}@old-revision", id(&first));
    let obsolete = store
        .lease_acquire(&obsolete_name, "old", 300)
        .unwrap()
        .unwrap();
    store.lease_release(&obsolete).unwrap();
    store.jobs_prune(&workspace, i64::MAX, 1000).unwrap();
    assert_eq!(
        store
            .lease_acquire(&obsolete_name, "new", 300)
            .unwrap()
            .unwrap()
            .fence,
        1,
        "obsolete revision lease was pruned"
    );
    assert!(
        claim(&store, &first).is_none(),
        "current exhausted lease survives pruning"
    );
    for job in &jobs[1..1000] {
        assert!(claim(&store, job).is_some());
    }
    let bounded = store
        .jobs_pending(&workspace, "gliner", None, 1000)
        .unwrap();
    assert_eq!(bounded.len(), 1000);
    for (candidate, job) in bounded.iter().zip(&jobs) {
        assert_eq!(candidate.document, id(job));
        assert!(
            claim(&store, job).is_none(),
            "bounded candidate is contended or exhausted"
        );
    }
    assert!(
        store
            .job_get(&jobs[1000].job, &workspace)
            .unwrap()
            .is_none(),
        "bounded scan never claimed the available 1001st document"
    );
}
