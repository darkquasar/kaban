//! Atomic publication through the public job/store seam; no runtime codec dependencies.
#![allow(clippy::unwrap_used, clippy::expect_used)] // public behavior assertions

use kaban_core::{
    Actor, AuditEntry, AuditOp, AuditStore, BatchAssertion, BatchReport, BatchSubject,
    ChangeLogStore, ClaimClass, EdgeDiff, Fixed, IngestJob, IngestStore, JobId, JobStatus,
    JobSubmitContract, JobSubmitRequest, JobSubmitStateUpdate, Layer, MemoryStore, Method, NewNode,
    NodeId, Op, Rank, SourceRev, StoreError, WorkspaceId, WriteBatch, WriteCtx,
};
use kaban_store_sqlite::SqliteStore;
use serde_json::json;

struct Fixture {
    store: SqliteStore,
    job: IngestJob,
    document: NodeId,
    chunk: NodeId,
    revision: SourceRev,
    ctx: WriteCtx,
}

fn node(id: &NodeId, kind: &str, text: &str, props: serde_json::Value) -> Op {
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

fn batch(ops: Vec<Op>) -> WriteBatch {
    let mut batch = WriteBatch::new(WorkspaceId::default(), ops);
    batch.edge_diff = EdgeDiff::None;
    batch
}

#[test]
fn ordinary_owned_and_complete_batches_cannot_delete_claim_backed_projection() {
    for complete in [false, true] {
        for (method, layer, owner) in [
            (
                Method::Remember,
                Layer::Llm,
                kaban_core::Owner::KabanReconcile,
            ),
            (
                Method::IngestSubmit,
                Layer::Llm,
                kaban_core::Owner::KabanParse,
            ),
            (
                Method::IngestSubmit,
                Layer::Gliner,
                kaban_core::Owner::KabanParse,
            ),
            (Method::IngestSubmit, Layer::Human, kaban_core::Owner::Human),
        ] {
            let store = SqliteStore::open_memory().unwrap();
            let ctx = WriteCtx::new("policy", WorkspaceId::default(), Actor::Agent);
            let src = NodeId::new();
            let dst = NodeId::new();
            let mut initial = batch(vec![
                node(&src, "Fact", "source", json!({})),
                node(&dst, "Fact", "target", json!({})),
            ]);
            initial.assertions.push(BatchAssertion {
                subject: BatchSubject::Edge {
                    src: src.clone(),
                    rel: "RELATES_TO".into(),
                    dst: dst.clone(),
                },
                claim_class: ClaimClass::Edge,
                predicate: None,
                object: None,
                value: json!({}),
                layer,
                method,
                extractor_id: None,
                extractor_version: None,
                source_id: None,
                lineage_id: "fixture".into(),
                raw_score: Fixed::ONE,
                rank: Rank::Normal,
                rank_reason: None,
                calibration_version: 0,
                supersedes: None,
            });
            store.apply_batch(initial, &ctx).unwrap();
            let query = kaban_core::TraverseQuery {
                start_ids: vec![src.clone()],
                rels: vec!["RELATES_TO".into()],
                ..Default::default()
            };
            let before = store.traverse(&query).unwrap().edges;
            assert_eq!(before.len(), 1);
            assert_eq!(before[0].props["owner"], owner.as_str());
            let mut ordinary = batch(vec![node(&src, "Fact", "source", json!({}))]);
            ordinary.edge_diff = if complete {
                EdgeDiff::Complete { producer: owner }
            } else {
                EdgeDiff::Owned { owner }
            };
            store.apply_batch(ordinary, &ctx).unwrap();
            assert_eq!(
                serde_json::to_value(store.traverse(&query).unwrap().edges).unwrap(),
                serde_json::to_value(before).unwrap(),
                "ordinary edge deletion must preserve claim-backed projection"
            );
        }
    }
}

#[test]
fn ingest_method_never_bypasses_governed_semantics_candidate_decision() {
    let mut ontology = kaban_core::shipped::resolved().unwrap();
    ontology
        .rels
        .values_mut()
        .find(|rel| rel.id.name == "RELATES_TO")
        .unwrap()
        .governed_semantics = true;
    let store = SqliteStore::open_memory()
        .unwrap()
        .with_ontology(std::sync::Arc::new(ontology))
        .unwrap();
    let ctx = WriteCtx::new("policy", WorkspaceId::default(), Actor::Agent);
    let src = NodeId::new();
    let dst = NodeId::new();
    let mut request = batch(vec![
        node(&src, "Fact", "source", json!({})),
        node(&dst, "Fact", "target", json!({})),
    ]);
    request.assertions.push(BatchAssertion {
        subject: BatchSubject::Edge {
            src: src.clone(),
            rel: "RELATES_TO".into(),
            dst,
        },
        claim_class: ClaimClass::Edge,
        predicate: None,
        object: None,
        value: json!({"owner":"kaban-parse"}),
        layer: Layer::Llm,
        method: Method::IngestSubmit,
        extractor_id: None,
        extractor_version: None,
        source_id: None,
        lineage_id: "fixture".into(),
        raw_score: Fixed::ONE,
        rank: Rank::Normal,
        rank_reason: None,
        calibration_version: 0,
        supersedes: None,
    });
    let report = store.apply_batch(request, &ctx).unwrap();
    assert_eq!(report.assertions_inserted, 1);
    assert_eq!(
        report.verdicts.get(&kaban_core::AssertionStatus::Candidate),
        Some(&1)
    );
    assert!(
        store
            .traverse(&kaban_core::TraverseQuery {
                start_ids: vec![src],
                rels: vec!["RELATES_TO".into()],
                filters: kaban_core::ReadFilters {
                    include_candidates: true
                },
                ..Default::default()
            })
            .unwrap()
            .edges
            .is_empty()
    );
}

fn fixture() -> Fixture {
    let store = SqliteStore::open_memory().unwrap();
    let ctx = WriteCtx::new("policy", WorkspaceId::default(), Actor::Agent);
    let document = NodeId::new();
    let chunk = NodeId::new();
    store.apply_batch(batch(vec![
        node(&document, "Document", "source", json!({"owner":"kaban-ingest", "source_id":"di-source-not-document", "content_hash":"content", "chunk_ids":[chunk.as_str()]})),
        node(&chunk, "Chunk", "source evidence", json!({"owner":"kaban-ingest"})),
    ]), &ctx).unwrap();
    let source = store.get(&document, false).unwrap().unwrap();
    let revision = SourceRev {
        content_hash: source.content_hash,
        rev: source.rev,
    };
    let job = IngestJob {
        job: JobId::from_digest(&[39;32]), workspace: WorkspaceId::default(),
        source_id: "di-source-not-document".into(), relative_path: "source.md".into(),
        content_hash: "content".into(), pipeline_version: "1".into(),
        chunk_config_hash: "chunks".into(), mode: "structured".into(), dialect: "none".into(),
        doc_type: "generic".into(), kinds: vec!["Fact".into()],
        plan_json: json!({"v":1,"document_id":document,"chunks":[{"id":chunk}],"submit_state":{},"finished_report_json":null}).to_string(),
        named_edges: vec![], submits:0, created:0, updated:0, unchanged:0, redactions:0,
        status:JobStatus::Open, created_at:1, expires_at:i64::MAX, finished_at:None,
    };
    store.job_open(&job).unwrap();
    Fixture {
        store,
        job,
        document,
        chunk,
        revision,
        ctx,
    }
}

fn request(f: &Fixture, ops: Vec<Op>) -> JobSubmitRequest {
    JobSubmitRequest {
        job: f.job.job.clone(),
        contract: JobSubmitContract::from_job(&f.job).unwrap(),
        document: f.document.clone(),
        source_rev: f.revision.clone(),
        chunks: vec![f.chunk.clone()],
        batch: batch(ops),
    }
}

fn accumulate(job: &IngestJob, report: &BatchReport) -> kaban_core::Result<JobSubmitStateUpdate> {
    let mut plan: serde_json::Value = serde_json::from_str(&job.plan_json)?;
    plan["submit_state"]["actual_created"] = json!(job.created + u64::from(report.inserted));
    Ok(JobSubmitStateUpdate {
        plan_json: plan.to_string(),
        named_edges: job.named_edges.clone(),
    })
}

#[test]
fn late_holder_submit_cannot_publish_after_ownership_is_lost() {
    use kaban_core::LeaseStore;
    let mut f = fixture();
    f.job.job = JobId::from_digest(&[91; 32]);
    let mut plan: serde_json::Value = serde_json::from_str(&f.job.plan_json).unwrap();
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
    let id = NodeId::new();
    let before = f.store.changes_read(0, 10_000).unwrap();
    let result = f.store.job_submit(
        request(&f, vec![node(&id, "Fact", "late-holder", json!({}))]),
        &f.ctx,
        &mut accumulate,
    );
    assert!(
        matches!(result, Err(StoreError::LeaseLost { .. })),
        "late holder publication must fail with lease_lost"
    );
    assert!(f.store.get(&id, true).unwrap().is_none());
    assert_eq!(f.store.changes_read(0, 10_000).unwrap(), before);
    assert_eq!(
        f.store
            .job_get(&f.job.job, &f.job.workspace)
            .unwrap()
            .unwrap(),
        f.job
    );
}

fn audit(ctx: &mut WriteCtx) {
    ctx.audit.push(AuditEntry {
        op: AuditOp::Redact,
        tool: "ingest_submit".into(),
        field: "text".into(),
        workspace_id: ctx.workspace_id.clone(),
        policy_version: ctx.policy_version.clone(),
        input_hmac: "invented-hmac".into(),
        rules: vec!["invented-rule".into()],
        node_id: None,
    });
}

#[test]
fn graph_audit_and_actual_counters_commit_together_and_repeat_is_unchanged() {
    let mut f = fixture();
    audit(&mut f.ctx);
    let id = NodeId::new();
    let ops = || vec![node(&id, "Fact", "masked record", json!({}))];
    let receipt = f
        .store
        .job_submit(request(&f, ops()), &f.ctx, &mut accumulate)
        .unwrap();
    assert_eq!(
        (
            receipt.batch.inserted,
            receipt.job.submits,
            receipt.job.created,
            receipt.job.redactions
        ),
        (1, 1, 1, 1)
    );
    assert_eq!(f.store.audit_verify().unwrap().entries, 1);
    let changes = f.store.changes_read(0, 10_000).unwrap();
    let repeated = f
        .store
        .job_submit(request(&f, ops()), &f.ctx, &mut accumulate)
        .unwrap();
    assert_eq!(
        (
            repeated.batch.unchanged,
            repeated.job.submits,
            repeated.job.created,
            repeated.job.unchanged
        ),
        (1, 2, 1, 1)
    );
    assert_eq!(repeated.batch.outbox_first_seq, None);
    assert_eq!(f.store.changes_read(0, 10_000).unwrap(), changes);
    assert_eq!(
        f.store
            .job_get(&f.job.job, &f.job.workspace)
            .unwrap()
            .unwrap(),
        repeated.job
    );
}

#[test]
fn accumulation_failure_rolls_back_graph_audit_and_counters() {
    let mut f = fixture();
    audit(&mut f.ctx);
    let id = NodeId::new();
    let changes = f.store.changes_read(0, 10_000).unwrap();
    let mut fail = |_: &IngestJob, _: &BatchReport| {
        Err(StoreError::InvalidName("submit".into(), "injected_failure"))
    };
    assert!(
        f.store
            .job_submit(
                request(&f, vec![node(&id, "Fact", "sentinel", json!({}))]),
                &f.ctx,
                &mut fail
            )
            .is_err()
    );
    assert!(f.store.get(&id, true).unwrap().is_none());
    assert_eq!(
        f.store
            .job_get(&f.job.job, &f.job.workspace)
            .unwrap()
            .unwrap(),
        f.job
    );
    assert_eq!(f.store.audit_verify().unwrap().entries, 0);
    assert_eq!(f.store.changes_read(0, 10_000).unwrap(), changes);
}

#[test]
fn callback_cannot_replace_immutable_contract_or_final_report() {
    for field in ["document_id", "finished_report_json"] {
        let f = fixture();
        let id = NodeId::new();
        let mut forge = |job: &IngestJob, _: &BatchReport| {
            let mut plan: serde_json::Value = serde_json::from_str(&job.plan_json)?;
            plan[field] = json!("changed");
            Ok(JobSubmitStateUpdate {
                plan_json: plan.to_string(),
                named_edges: vec![],
            })
        };
        assert!(
            f.store
                .job_submit(
                    request(&f, vec![node(&id, "Fact", "sentinel", json!({}))]),
                    &f.ctx,
                    &mut forge
                )
                .is_err()
        );
        assert!(f.store.get(&id, true).unwrap().is_none());
        assert_eq!(
            f.store
                .job_get(&f.job.job, &f.job.workspace)
                .unwrap()
                .unwrap(),
            f.job
        );
    }
}

#[test]
fn current_row_source_revision_and_complete_chunks_are_rechecked_before_writes() {
    for case in [
        "stale_row",
        "revision",
        "chunks",
        "foreign",
        "source_id_as_document",
    ] {
        let f = fixture();
        let id = NodeId::new();
        let mut req = request(&f, vec![node(&id, "Fact", "sentinel", json!({}))]);
        let mut ctx = f.ctx.clone();
        match case {
            "stale_row" => {
                f.store
                    .job_mark_stale(&f.job.job, &f.job.workspace)
                    .unwrap();
            }
            "revision" => {
                req.source_rev.rev += 1;
            }
            "chunks" => {
                req.chunks.clear();
            }
            "foreign" => {
                ctx.workspace_id = WorkspaceId::new("other");
                req.batch.workspace_id = ctx.workspace_id.clone();
            }
            _ => {
                req.document = NodeId::new();
            }
        }
        assert!(
            f.store.job_submit(req, &ctx, &mut accumulate).is_err(),
            "{case}"
        );
        assert!(f.store.get(&id, true).unwrap().is_none());
        assert_eq!(
            f.store
                .job_get(&f.job.job, &f.job.workspace)
                .unwrap()
                .unwrap()
                .submits,
            0
        );
    }
}

#[test]
fn source_mutation_inside_submit_rolls_back_even_after_the_initial_guard_passes() {
    let f = fixture();
    let id = NodeId::new();
    let changes = f.store.changes_read(0, 10_000).unwrap();
    let ops = vec![
        node(&id, "Fact", "sentinel", json!({})),
        node(
            &f.document,
            "Document",
            "changed source",
            json!({"owner":"kaban-ingest", "source_id":f.job.source_id,"content_hash":f.job.content_hash,"chunk_ids":[f.chunk]}),
        ),
    ];
    assert!(
        f.store
            .job_submit(request(&f, ops), &f.ctx, &mut accumulate)
            .is_err()
    );
    assert!(f.store.get(&id, true).unwrap().is_none());
    assert_eq!(f.store.changes_read(0, 10_000).unwrap(), changes);
    assert_eq!(
        f.store
            .job_get(&f.job.job, &f.job.workspace)
            .unwrap()
            .unwrap(),
        f.job
    );
}

#[test]
fn present_expiry_and_counter_overflow_never_publish_or_call_successfully() {
    for expired in [true, false] {
        let mut f = fixture();
        f.job.job = JobId::from_digest(&[40; 32]);
        if expired {
            f.job.expires_at = 1;
        } else {
            f.job.created = i64::MAX as u64;
        }
        f.store.job_open(&f.job).unwrap();
        let id = NodeId::new();
        let changes = f.store.changes_read(0, 10_000).unwrap();
        assert!(
            f.store
                .job_submit(
                    request(&f, vec![node(&id, "Fact", "sentinel", json!({}))]),
                    &f.ctx,
                    &mut accumulate
                )
                .is_err()
        );
        assert!(f.store.get(&id, true).unwrap().is_none());
        assert_eq!(f.store.changes_read(0, 10_000).unwrap(), changes);
        assert_eq!(
            f.store
                .job_get(&f.job.job, &f.job.workspace)
                .unwrap()
                .unwrap(),
            f.job
        );
    }
}

#[test]
fn l3_claim_method_rank_and_lineage_commit_or_rollback_with_the_job() {
    for fail in [false, true] {
        let f = fixture();
        let id = NodeId::new();
        let mut req = request(
            &f,
            vec![node(&id, "Fact", "masked claim record", json!({}))],
        );
        req.batch.assertions.push(BatchAssertion {
            subject: BatchSubject::Record(id),
            claim_class: ClaimClass::KindOfRecord,
            predicate: None,
            object: None,
            value: json!({"kind":"Fact", "props":{}}),
            layer: Layer::Llm,
            method: Method::IngestSubmit,
            extractor_id: Some("llm".into()),
            extractor_version: Some("1".into()),
            source_id: Some(f.job.source_id.clone()),
            lineage_id: "invented-brief-and-extractor".into(),
            raw_score: Fixed::new(8000).unwrap(),
            rank: Rank::Normal,
            rank_reason: None,
            calibration_version: 0,
            supersedes: None,
        });
        let mut callback = |job: &IngestJob, report: &BatchReport| {
            assert_eq!(report.assertions_inserted, 1);
            if fail {
                Err(StoreError::InvalidName("submit".into(), "injected_failure"))
            } else {
                accumulate(job, report)
            }
        };
        let result = f.store.job_submit(req, &f.ctx, &mut callback);
        assert_eq!(result.is_err(), fail);
        let mut observed = Vec::new();
        f.store
            .rescan_assertions("policy", &mut |claim, _| {
                observed.push((
                    claim.method.as_str().to_owned(),
                    claim.rank,
                    claim.lineage_id.clone(),
                    claim.raw_score,
                ));
                Some(claim.value.as_value().clone())
            })
            .unwrap();
        if fail {
            assert!(observed.is_empty(), "no claim survives failed accumulation");
        } else {
            assert_eq!(
                observed,
                vec![(
                    "ingest_submit".into(),
                    Rank::Normal,
                    "invented-brief-and-extractor".into(),
                    Fixed::new(8000).unwrap()
                )]
            );
        }
    }
}
