//! Atomic retirement, replay safety, union scopes, retention and already-read races.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use kaban_core::{
    Actor, BatchAssertion, BatchSubject, ChangeLogStore, ClaimClass, EdgeDiff, Fixed, Governance,
    IngestJob, IngestStore, JobFinishRequest, JobFinishStateUpdate, JobId, JobStatus,
    JobSubmitContract, Layer, Lifecycle, MemoryStore, Method, NewNode, NodeId, Op, Rank, SourceRev,
    StoreError, TraverseQuery, WorkspaceId, WriteBatch, WriteCtx,
};
use kaban_store_sqlite::SqliteStore;
use serde_json::{Value, json};

struct Fixture {
    store: SqliteStore,
    job: IngestJob,
    document: NodeId,
    chunk: NodeId,
    old_chunk: NodeId,
    record: NodeId,
    orphan: NodeId,
    target: NodeId,
    ctx: WriteCtx,
    rev: SourceRev,
}

fn node(id: &NodeId, kind: &str, text: &str, props: Value) -> Op {
    Op::UpsertNode {
        id: id.clone(),
        node: NewNode {
            kind: kind.into(),
            text: text.into(),
            props: Some(props),
            ..Default::default()
        },
    }
}
fn edge(src: &NodeId, dst: &NodeId, rel: &str, owner: &str) -> Op {
    Op::UpsertEdge {
        src: src.clone(),
        dst: dst.clone(),
        rel: rel.into(),
        props: Some(json!({"owner":owner})),
    }
}
fn claim(src: &NodeId, dst: &NodeId, rel: &str, source: &str, layer: Layer) -> BatchAssertion {
    BatchAssertion {
        subject: BatchSubject::Edge {
            src: src.clone(),
            dst: dst.clone(),
            rel: rel.into(),
        },
        claim_class: ClaimClass::Edge,
        predicate: None,
        object: None,
        value: json!({"owner":"kaban-parse"}),
        layer,
        method: Method::IngestSubmit,
        extractor_id: None,
        extractor_version: None,
        source_id: Some(source.into()),
        lineage_id: format!("fixture-{source}"),
        raw_score: Fixed::ONE,
        rank: Rank::Normal,
        rank_reason: None,
        calibration_version: 0,
        supersedes: None,
    }
}
fn apply(store: &SqliteStore, ops: Vec<Op>, assertions: Vec<BatchAssertion>, ctx: &WriteCtx) {
    let mut batch = WriteBatch::new(ctx.workspace_id.clone(), ops);
    batch.edge_diff = EdgeDiff::None;
    batch.assertions = assertions;
    store.apply_batch(batch, ctx).unwrap();
}
fn fixture() -> Fixture {
    let store = SqliteStore::open_memory().unwrap();
    let ctx = WriteCtx::new("policy", WorkspaceId::default(), Actor::Agent);
    let (document, chunk, old_chunk, section, record, orphan, target) = (
        NodeId::new(),
        NodeId::new(),
        NodeId::new(),
        NodeId::new(),
        NodeId::new(),
        NodeId::new(),
        NodeId::new(),
    );
    apply(
        &store,
        vec![
            node(
                &document,
                "Document",
                "source",
                json!({"owner":"kaban-ingest","source_id":"di-not-node","content_hash":"content","chunk_ids":[chunk]}),
            ),
            node(&chunk, "Chunk", "current", json!({"owner":"kaban-ingest"})),
            node(
                &old_chunk,
                "Chunk",
                "earlier",
                json!({"owner":"kaban-ingest"}),
            ),
            node(
                &section,
                "Section",
                "section",
                json!({"owner":"kaban-ingest"}),
            ),
            node(
                &record,
                "Decision",
                "record",
                json!({"rationale":"record reason"}),
            ),
            node(
                &orphan,
                "Decision",
                "orphan",
                json!({"rationale":"orphan reason"}),
            ),
            node(&target, "Fact", "target", json!({})),
            edge(&old_chunk, &section, "PART_OF", "kaban-ingest"),
            edge(&section, &document, "PART_OF", "kaban-ingest"),
        ],
        vec![
            claim(
                &record,
                &document,
                "DERIVED_FROM",
                "di-not-node",
                Layer::Llm,
            ),
            claim(
                &orphan,
                &document,
                "DERIVED_FROM",
                "di-not-node",
                Layer::Llm,
            ),
            claim(&record, &target, "RELATES_TO", "di-not-node", Layer::Llm),
            claim(
                &old_chunk,
                &orphan,
                "EVIDENCE_FOR",
                "di-not-node",
                Layer::Llm,
            ),
        ],
        &ctx,
    );
    let rev = SourceRev::from(&store.get(&document, true).unwrap().unwrap());
    let job=IngestJob{job:JobId::from_digest(&[10;32]),workspace:ctx.workspace_id.clone(),source_id:"di-not-node".into(),relative_path:"source.md".into(),content_hash:"content".into(),pipeline_version:"1".into(),chunk_config_hash:"chunks".into(),mode:"structured".into(),dialect:"llm".into(),doc_type:"generic".into(),kinds:vec!["Fact".into()],plan_json:json!({"v":1,"document_id":document,"chunks":[{"id":chunk}],"submit_state":{"named_records":[record]},"finished_report_json":null}).to_string(),named_edges:vec![(record.to_string(),document.to_string(),"DERIVED_FROM".into())],submits:2,created:2,updated:0,unchanged:0,redactions:0,status:JobStatus::Open,created_at:1,expires_at:i64::MAX,finished_at:None};
    store.job_open(&job).unwrap();
    Fixture {
        store,
        job,
        document,
        chunk,
        old_chunk,
        record,
        orphan,
        target,
        ctx,
        rev,
    }
}
fn request(f: &Fixture, cap: u32) -> JobFinishRequest {
    JobFinishRequest {
        job: f.job.job.clone(),
        contract: JobSubmitContract::from_finish_job(&f.job).unwrap(),
        document: f.document.clone(),
        source_rev: f.rev.clone(),
        chunks: vec![f.chunk.clone()],
        finish_cap: cap,
        retention_days: 7,
    }
}
fn finish(f: &Fixture, cap: u32) -> String {
    f.store.job_finish(request(f,cap),&f.ctx,&mut |row,outcome| {
        let plan:Value=serde_json::from_str(&row.plan_json)?;
        let report_json=if row.status==JobStatus::Finished {plan["finished_report_json"].as_str().unwrap().into()} else {json!({"retired":outcome.retired_assertions,"deleted":outcome.deleted_edges,"orphans":outcome.orphans,"skipped_cap":outcome.skipped_cap}).to_string()};
        Ok(JobFinishStateUpdate{report_json,audit:vec![]})
    }).unwrap()
}
fn edges(f: &Fixture) -> Vec<kaban_core::Edge> {
    f.store
        .traverse(&TraverseQuery {
            start_ids: vec![f.record.clone(), f.orphan.clone(), f.old_chunk.clone()],
            max_depth: 1,
            ..Default::default()
        })
        .unwrap()
        .edges
}

#[test]
fn protected_l3_relationship_survives_finish_omission() {
    use kaban_core::{GovernanceStore, PruningTarget};
    let mut f = fixture();
    let target = PruningTarget::Relationship {
        src: f.record.clone(),
        dst: f.target.clone(),
        rel: "RELATES_TO".into(),
    };
    f.store.pruning_protection_set(&target, &f.ctx).unwrap();
    finish(&f, 100);
    assert!(
        edges(&f).iter().any(|e| e.rel == "RELATES_TO"),
        "protected L3 relationship and supporting claim must survive finish omission"
    );
    assert!(
        !edges(&f).iter().any(|e| e.rel == "EVIDENCE_FOR"),
        "unprotected sibling is retired"
    );
    f.store.pruning_protection_clear(&target, &f.ctx).unwrap();
    f.job.job = JobId::from_digest(&[77; 32]);
    f.store.job_open(&f.job).unwrap();
    finish(&f, 100);
    assert!(
        !edges(&f).iter().any(|e| e.rel == "RELATES_TO"),
        "clear restores L3 omission eligibility"
    );
}

#[test]
fn protected_document_omission_does_not_revoke_its_job() {
    use kaban_core::{GovernanceStore, PruningTarget};
    let f = fixture();
    let target = PruningTarget::Node(f.document.clone());
    f.store.pruning_protection_set(&target, &f.ctx).unwrap();
    let batch = WriteBatch::new(
        f.ctx.workspace_id.clone(),
        vec![Op::Tombstone {
            id: f.document.clone(),
            reason: "producer omission".into(),
        }],
    );
    f.store.apply_batch(batch.clone(), &f.ctx).unwrap();
    assert!(
        f.store
            .job_get(&f.job.job, &f.ctx.workspace_id)
            .unwrap()
            .is_some()
    );
    assert!(f.store.get(&f.document, false).unwrap().is_some());
    f.store.pruning_protection_clear(&target, &f.ctx).unwrap();
    f.store.apply_batch(batch, &f.ctx).unwrap();
    assert!(
        f.store
            .job_get(&f.job.job, &f.ctx.workspace_id)
            .unwrap()
            .is_none()
    );
}

#[test]
fn late_holder_finish_cannot_retire_after_ownership_is_lost() {
    use kaban_core::LeaseStore;
    let mut f = fixture();
    f.job.job = JobId::from_digest(&[92; 32]);
    let mut plan: Value = serde_json::from_str(&f.job.plan_json).unwrap();
    plan["lease"] =
        json!({"name":"ingest:invented:source", "owner":"old-worker", "fence":1, "ttl_s":300});
    f.job.plan_json = plan.to_string();
    f.store.job_open(&f.job).unwrap();
    let old = f
        .store
        .lease_acquire("ingest:invented:source", "old-worker", 300)
        .unwrap()
        .unwrap();
    f.store.lease_release(&old).unwrap();
    f.store
        .lease_acquire("ingest:invented:source", "new-worker", 300)
        .unwrap()
        .unwrap();
    let before = serde_json::to_value(edges(&f)).unwrap();
    let result = f.store.job_finish(request(&f, 100), &f.ctx, &mut |_, _| {
        Ok(JobFinishStateUpdate {
            report_json: "{}".into(),
            audit: vec![],
        })
    });
    assert!(
        matches!(result, Err(StoreError::LeaseLost { .. })),
        "late holder finalization must fail with lease_lost"
    );
    assert_eq!(serde_json::to_value(edges(&f)).unwrap(), before);
    assert_eq!(
        f.store
            .job_get(&f.job.job, &f.job.workspace)
            .unwrap()
            .unwrap(),
        f.job
    );
}

#[test]
fn extractor_reindex_retires_only_its_claims_preserving_human_and_other_layer_support() {
    use kaban_core::{GovernanceStore, PruningTarget};
    let f = fixture();
    f.store
        .pruning_protection_set(&PruningTarget::Node(f.record.clone()), &f.ctx)
        .unwrap();
    apply(
        &f.store,
        vec![],
        vec![
            claim(
                &f.record,
                &f.target,
                "RELATES_TO",
                "di-not-node",
                Layer::Human,
            ),
            claim(
                &f.old_chunk,
                &f.orphan,
                "EVIDENCE_FOR",
                "di-not-node",
                Layer::Gliner,
            ),
        ],
        &f.ctx,
    );
    let report = f.store.jobs_reindex(Layer::Llm, 1, &f.ctx).unwrap();
    assert!(
        report.transactions >= 4,
        "bounded one-row retirement batches"
    );
    let remaining = edges(&f);
    assert!(
        remaining
            .iter()
            .any(|edge| edge.rel == "RELATES_TO" && edge.props["owner"] == "human")
    );
    assert!(remaining.iter().any(|edge| edge.rel == "EVIDENCE_FOR"));
    assert!(!remaining.iter().any(|edge| edge.rel == "DERIVED_FROM"));
}

#[test]
fn explicit_reindex_then_same_source_submit_restores_only_named_reset_claims() {
    let mut f = fixture();
    f.store.jobs_reindex(Layer::Llm, 1, &f.ctx).unwrap();
    assert!(!edges(&f).iter().any(|edge| edge.rel == "RELATES_TO"));
    f.job.job = JobId::from_digest(&[93; 32]);
    f.job.dialect = "llm".into();
    f.store.job_open(&f.job).unwrap();
    let mut batch = WriteBatch::new(f.ctx.workspace_id.clone(), vec![]);
    batch.edge_diff = EdgeDiff::None;
    batch.assertions = vec![claim(
        &f.record,
        &f.target,
        "RELATES_TO",
        "di-not-node",
        Layer::Llm,
    )];
    let request = kaban_core::JobSubmitRequest {
        job: f.job.job.clone(),
        contract: JobSubmitContract::from_job(&f.job).unwrap(),
        document: f.document.clone(),
        source_rev: f.rev.clone(),
        chunks: vec![f.chunk.clone()],
        batch,
    };
    f.store
        .job_submit(request, &f.ctx, &mut |job, _| {
            Ok(kaban_core::JobSubmitStateUpdate {
                plan_json: job.plan_json.clone(),
                named_edges: vec![(
                    f.record.to_string(),
                    f.target.to_string(),
                    "RELATES_TO".into(),
                )],
            })
        })
        .unwrap();
    assert!(
        edges(&f).iter().any(|edge| edge.rel == "RELATES_TO"),
        "explicit unchanged-source submit must restore exactly its reindexed claim"
    );
    assert!(
        !edges(&f).iter().any(|edge| edge.rel == "EVIDENCE_FOR"),
        "omitted reset claim stays retired"
    );
}

#[test]
fn omitted_claims_retire_atomically_and_replay_never_resurrects_them() {
    let f = fixture();
    assert!(edges(&f).iter().any(|e| e.rel == "RELATES_TO"));
    let report: Value = serde_json::from_str(&finish(&f, 100)).unwrap();
    assert_eq!(report["retired"], 3);
    assert_eq!(report["deleted"], 3);
    assert_eq!(report["orphans"], json!([f.orphan]));
    let remaining = edges(&f);
    assert!(
        remaining
            .iter()
            .any(|e| e.rel == "DERIVED_FROM" && e.src == f.record.as_str())
    );
    assert!(
        !remaining
            .iter()
            .any(|e| e.rel == "RELATES_TO" || e.rel == "EVIDENCE_FOR")
    );
    assert_eq!(
        f.store.get(&f.orphan, true).unwrap().unwrap().lifecycle,
        Lifecycle::Active
    );
    let ids = f
        .store
        .changes_read(0, 10000)
        .unwrap()
        .into_iter()
        .filter(|c| c.entity == kaban_core::ChangeEntity::Assertion)
        .filter_map(|c| c.id.parse().ok())
        .collect::<Vec<_>>();
    f.store.redecide_assertions(&ids).unwrap();
    assert_eq!(
        serde_json::to_value(edges(&f)).unwrap(),
        serde_json::to_value(remaining).unwrap()
    );
}

#[test]
fn cap_and_overflow_skip_every_retirement_and_delete() {
    use kaban_core::{GovernanceStore, PruningTarget};
    for overflow in [false, true] {
        let mut f = fixture();
        f.store
            .pruning_protection_set(&PruningTarget::Node(f.record.clone()), &f.ctx)
            .unwrap();
        if overflow {
            let mut plan: Value = serde_json::from_str(&f.job.plan_json).unwrap();
            plan["submit_state"]["overflow"] = json!(true);
            let req = kaban_core::JobSubmitRequest {
                job: f.job.job.clone(),
                contract: JobSubmitContract::from_job(&f.job).unwrap(),
                document: f.document.clone(),
                source_rev: f.rev.clone(),
                chunks: vec![f.chunk.clone()],
                batch: {
                    let mut b = WriteBatch::new(f.ctx.workspace_id.clone(), vec![]);
                    b.edge_diff = EdgeDiff::None;
                    b
                },
            };
            f.job = f
                .store
                .job_submit(req, &f.ctx, &mut |row, _| {
                    Ok(kaban_core::JobSubmitStateUpdate {
                        plan_json: plan.to_string(),
                        named_edges: row.named_edges.clone(),
                    })
                })
                .unwrap()
                .job;
        }
        let before = serde_json::to_value(edges(&f)).unwrap();
        let report: Value =
            serde_json::from_str(&finish(&f, if overflow { 100 } else { 2 })).unwrap();
        assert_eq!(report["skipped_cap"], true);
        assert_eq!(report["retired"], 0);
        assert_eq!(report["deleted"], 0);
        assert_eq!(serde_json::to_value(edges(&f)).unwrap(), before);
    }
}

#[test]
fn callback_failure_rolls_back_retirement_report_and_lifecycle() {
    let f = fixture();
    let before = serde_json::to_value(edges(&f)).unwrap();
    let changes = f.store.changes_read(0, 10000).unwrap();
    let result = f
        .store
        .job_finish(request(&f, 100), &f.ctx, &mut |_, outcome| {
            assert_eq!(outcome.retired_assertions, 3);
            Err(StoreError::InvalidName("test".into(), "injected_failure"))
        });
    assert!(result.is_err());
    assert_eq!(
        f.store
            .job_get(&f.job.job, &f.job.workspace)
            .unwrap()
            .unwrap(),
        f.job
    );
    assert_eq!(serde_json::to_value(edges(&f)).unwrap(), before);
    assert_eq!(f.store.changes_read(0, 10000).unwrap(), changes);
}

#[test]
fn already_read_finish_retries_return_exact_retained_bytes_and_prune_is_bounded() {
    let f = fixture();
    let first = finish(&f, 100);
    let changes = f.store.changes_read(0, 10000).unwrap();
    assert_eq!(finish(&f, 100), first);
    assert_eq!(f.store.changes_read(0, 10000).unwrap(), changes);
    let row = f
        .store
        .job_get(&f.job.job, &f.job.workspace)
        .unwrap()
        .unwrap();
    assert_eq!(row.status, JobStatus::Finished);
    assert_eq!(row.expires_at - row.finished_at.unwrap(), 7 * 86400);
    assert_eq!(
        f.store
            .jobs_prune(&WorkspaceId::new("foreign"), row.expires_at, 64)
            .unwrap(),
        0
    );
    assert_eq!(
        f.store
            .jobs_prune(&row.workspace, row.expires_at - 1, 64)
            .unwrap(),
        0
    );
    assert_eq!(
        f.store
            .jobs_prune(&row.workspace, row.expires_at, 0)
            .unwrap(),
        0
    );
    assert_eq!(
        f.store
            .jobs_prune(&row.workspace, row.expires_at, 1)
            .unwrap(),
        1
    );
    assert!(f.store.job_get(&row.job, &row.workspace).unwrap().is_none());
    assert!(matches!(
        f.store
            .job_finish(request(&f, 100), &f.ctx, &mut |_, _| unreachable!()),
        Err(StoreError::NotFound(_))
    ));
}

#[test]
fn human_foreign_owner_source_and_human_retired_provenance_survive() {
    let f = fixture();
    apply(
        &f.store,
        vec![edge(&f.record, &f.target, "ABOUT", "kaban-ingest")],
        vec![
            claim(
                &f.record,
                &f.target,
                "RELATES_TO",
                "human-source",
                Layer::Human,
            ),
            claim(
                &f.chunk,
                &f.target,
                "EVIDENCE_FOR",
                "other-source",
                Layer::Llm,
            ),
        ],
        &f.ctx,
    );
    let mut observed = Vec::new();
    f.store
        .rescan_assertions("observe-1", &mut |a, _| {
            if a.layer == Layer::Llm && a.predicate.as_deref() == Some("DERIVED_FROM") {
                observed.push(a.key.0.clone());
            }
            None
        })
        .unwrap();
    let key = observed.first().unwrap();
    f.store
        .deprecate_assertion(key, "human choice", &f.ctx)
        .unwrap();
    let mut prior = None;
    f.store
        .rescan_assertions("observe-2", &mut |a, _| {
            if &a.key.0 == key {
                prior = Some((a.rank, a.rank_reason.clone()));
            }
            None
        })
        .unwrap();
    finish(&f, 100);
    let foreign = f
        .store
        .traverse(&TraverseQuery {
            start_ids: vec![f.chunk.clone()],
            max_depth: 1,
            ..Default::default()
        })
        .unwrap();
    assert!(
        foreign
            .edges
            .iter()
            .any(|e| e.rel == "EVIDENCE_FOR" && e.dst == f.target.as_str())
    );
    let remaining = edges(&f);
    assert!(
        remaining
            .iter()
            .any(|e| e.rel == "ABOUT" && e.props["owner"] == "kaban-ingest")
    );
    assert!(
        remaining
            .iter()
            .any(|e| e.rel == "RELATES_TO" && e.props["owner"] == "human")
    );
    let mut after = None;
    f.store
        .rescan_assertions("observe-3", &mut |a, _| {
            if &a.key.0 == key {
                after = Some((a.rank, a.rank_reason.clone()));
            }
            None
        })
        .unwrap();
    assert_eq!(after, prior);
    assert_eq!(
        f.store.get(&f.record, true).unwrap().unwrap().governance,
        Governance::Proposed
    );
}

#[test]
fn historical_chunk_evidence_retires_after_prior_finished_job_is_pruned() {
    let f = fixture();
    let mut prior = f.job.clone();
    prior.job = JobId::from_digest(&[11; 32]);
    prior.status = JobStatus::Finished;
    prior.finished_at = Some(1);
    prior.expires_at = 2;
    let mut plan: Value = serde_json::from_str(&prior.plan_json).unwrap();
    plan["chunks"] = json!([{"id":f.old_chunk}]);
    prior.plan_json = plan.to_string();
    f.store.job_open(&prior).unwrap();
    assert_eq!(f.store.jobs_prune(&f.ctx.workspace_id, 3, 64).unwrap(), 1);
    assert!(
        f.store
            .job_get(&prior.job, &prior.workspace)
            .unwrap()
            .is_none()
    );
    let report: Value = serde_json::from_str(&finish(&f, 100)).unwrap();
    assert_eq!(report["retired"], 3);
    assert!(!edges(&f).iter().any(|e| e.rel == "EVIDENCE_FOR"));
}

#[test]
fn deleted_after_preparation_finish_and_submit_cannot_publish() {
    let f = fixture();
    let req = request(&f, 100);
    f.store
        .forget(&f.document, kaban_core::ForgetMode::Purge, &f.ctx)
        .unwrap();
    assert!(matches!(
        f.store.job_finish(req, &f.ctx, &mut |_, _| unreachable!()),
        Err(StoreError::NotFound(_))
    ));
    let id = NodeId::new();
    let req = kaban_core::JobSubmitRequest {
        job: f.job.job.clone(),
        contract: JobSubmitContract::from_job(&f.job).unwrap(),
        document: f.document.clone(),
        source_rev: f.rev.clone(),
        chunks: vec![f.chunk.clone()],
        batch: {
            let mut b = WriteBatch::new(
                f.ctx.workspace_id.clone(),
                vec![node(&id, "Fact", "late", json!({}))],
            );
            b.edge_diff = EdgeDiff::None;
            b
        },
    };
    assert!(
        f.store
            .job_submit(req, &f.ctx, &mut |_, _| unreachable!())
            .is_err()
    );
    assert!(f.store.get(&id, true).unwrap().is_none());
}
