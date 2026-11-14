//! The structured-ingest finish transaction over direct edges (DEL-08 survivors of the claim-era
//! file): a holder that lost its lease deletes nothing, a failing report callback rolls the edge
//! deletion back, a cap or overflow skips every delete, and chunk evidence of an earlier,
//! already pruned job is still in scope.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use kaban_core::{
    Actor, ChangeLogStore, EdgeDiff, IngestJob, IngestStore, JobFinishRequest,
    JobFinishStateUpdate, JobId, JobStatus, JobSubmitContract, MemoryStore, NewNode, NodeId, Op,
    SourceRev, StoreError, TraverseQuery, WorkspaceId, WriteBatch, WriteCtx,
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

fn edge(src: &NodeId, dst: &NodeId, rel: &str, props: Value) -> Op {
    Op::UpsertEdge {
        src: src.clone(),
        dst: dst.clone(),
        rel: rel.into(),
        props: Some(props),
    }
}

/// A submission edge as structured submit writes it: under the extractor producer of
/// `dialect`, scored, with the source lineage in the `source_id` column.
fn submitted_edge(dialect: &str, src: &NodeId, dst: &NodeId, rel: &str) -> Op {
    edge(
        src,
        dst,
        rel,
        json!({"producer":format!("extractor:{dialect}"),"method":"ingest_submit","score":10000,"source_id":"di-not-node"}),
    )
}

/// A document with a current and an earlier chunk, a record the job names, an orphan record
/// it does not, and four submission edges: the named `record -> document` provenance, the
/// orphan's provenance, an unnamed `record -> target` edge and the earlier chunk's evidence for
/// the orphan.
fn fixture() -> Fixture {
    fixture_for("llm")
}

/// [`fixture`] with the job and its four submission edges under the extractor `dialect`.
fn fixture_for(dialect: &str) -> Fixture {
    let parse_edge = |src: &NodeId, dst: &NodeId, rel: &str| submitted_edge(dialect, src, dst, rel);
    let store = SqliteStore::open_memory().unwrap();
    let ctx = WriteCtx::new(WorkspaceId::default(), Actor::Agent);
    let (document, chunk, old_chunk, section, record, orphan, target) = (
        NodeId::new(),
        NodeId::new(),
        NodeId::new(),
        NodeId::new(),
        NodeId::new(),
        NodeId::new(),
        NodeId::new(),
    );
    let ingest = json!({"owner":"kaban-ingest"});
    let structure = json!({"producer":"document_ingest"});
    let mut batch = WriteBatch::new(
        ctx.workspace_id.clone(),
        vec![
            node(
                &document,
                "Document",
                "source",
                json!({"owner":"kaban-ingest","source_id":"di-not-node","content_hash":"content","chunk_ids":[chunk]}),
            ),
            node(&chunk, "Chunk", "current", ingest.clone()),
            node(&old_chunk, "Chunk", "earlier", ingest.clone()),
            node(&section, "Section", "section", ingest.clone()),
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
            // the document structure is the document ingest's (PC-16 producer column)
            edge(&old_chunk, &section, "PART_OF", structure.clone()),
            edge(&section, &document, "PART_OF", structure),
            parse_edge(&record, &document, "DERIVED_FROM"),
            parse_edge(&orphan, &document, "DERIVED_FROM"),
            parse_edge(&record, &target, "RELATES_TO"),
            parse_edge(&old_chunk, &orphan, "EVIDENCE_FOR"),
        ],
    );
    batch.edge_diff = EdgeDiff::None;
    store.apply_batch(batch, &ctx).unwrap();
    let rev = SourceRev::from(&store.get(&document, true).unwrap().unwrap());
    let job = IngestJob {
        job: JobId::from_digest(&[10; 32]),
        workspace: ctx.workspace_id.clone(),
        source_id: "di-not-node".into(),
        relative_path: "source.md".into(),
        content_hash: "content".into(),
        pipeline_version: "1".into(),
        chunk_config_hash: "chunks".into(),
        mode: "structured".into(),
        dialect: dialect.into(),
        doc_type: "generic".into(),
        kinds: vec!["Fact".into()],
        plan_json: json!({"v":1,"document_id":document,"chunks":[{"id":chunk}],"submit_state":{"named_records":[record]},"finished_report_json":null}).to_string(),
        named_edges: vec![(record.to_string(), document.to_string(), "DERIVED_FROM".into())],
        submits: 2,
        created: 2,
        updated: 0,
        unchanged: 0,
        redactions: 0,
        status: JobStatus::Open,
        created_at: 1,
        expires_at: i64::MAX,
        finished_at: None,
    };
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

fn finish(f: &Fixture, cap: u32) -> Value {
    let text = f
        .store
        .job_finish(request(f, cap), &f.ctx, &mut |row, outcome| {
            let plan: Value = serde_json::from_str(&row.plan_json)?;
            let report_json = if row.status == JobStatus::Finished {
                plan["finished_report_json"].as_str().unwrap().into()
            } else {
                json!({"deleted":outcome.deleted_edges,"skipped_cap":outcome.skipped_cap})
                    .to_string()
            };
            Ok(JobFinishStateUpdate { report_json })
        })
        .unwrap();
    serde_json::from_str(&text).unwrap()
}

/// Every edge out of the record, the orphan and the earlier chunk, as comparable JSON.
fn edges(f: &Fixture) -> Value {
    let mut edges = f
        .store
        .traverse(&TraverseQuery {
            start_ids: vec![f.record.clone(), f.orphan.clone(), f.old_chunk.clone()],
            max_depth: 1,
            ..Default::default()
        })
        .unwrap()
        .edges;
    edges.sort_by(|a, b| a.id.cmp(&b.id));
    serde_json::to_value(edges).unwrap()
}

fn rels(f: &Fixture) -> Vec<String> {
    let mut rels: Vec<String> = edges(f)
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["rel"].as_str().unwrap().to_owned())
        .collect();
    rels.sort();
    rels
}

#[test]
fn an_omitting_finish_deletes_the_unnamed_submission_edges_only() {
    let f = fixture();
    let report = finish(&f, 100);
    assert_eq!(report["skipped_cap"], false, "{report}");
    assert_eq!(report["deleted"], 3, "{report}");
    // the named provenance and the ingest structure stay
    assert!(
        rels(&f)
            .iter()
            .all(|rel| rel == "DERIVED_FROM" || rel == "PART_OF")
    );
}

#[test]
fn late_holder_finish_cannot_delete_after_ownership_is_lost() {
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
    let before = edges(&f);
    let result = f.store.job_finish(request(&f, 100), &f.ctx, &mut |_, _| {
        Ok(JobFinishStateUpdate {
            report_json: "{}".into(),
        })
    });
    assert!(
        matches!(result, Err(StoreError::LeaseLost { .. })),
        "late holder finalization must fail with lease_lost"
    );
    assert_eq!(edges(&f), before);
    assert_eq!(
        f.store
            .job_get(&f.job.job, &f.job.workspace)
            .unwrap()
            .unwrap(),
        f.job
    );
}

#[test]
fn cap_and_overflow_skip_every_delete() {
    for overflow in [false, true] {
        let mut f = fixture();
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
                redactions: 0,
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
        let before = edges(&f);
        let report = finish(&f, if overflow { 100 } else { 2 });
        assert_eq!(report["skipped_cap"], true, "{report}");
        assert_eq!(report["deleted"], 0, "{report}");
        assert_eq!(edges(&f), before);
    }
}

#[test]
fn callback_failure_rolls_back_edge_deletion_report_and_lifecycle() {
    let f = fixture();
    let before = edges(&f);
    let changes = f.store.changes_read(0, 10000).unwrap();
    let result = f
        .store
        .job_finish(request(&f, 100), &f.ctx, &mut |_, outcome| {
            // the deletes ran inside the transaction before the callback failed
            assert_eq!(outcome.deleted_edges, 3);
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
    assert_eq!(edges(&f), before, "every deleted edge is back");
    assert_eq!(f.store.changes_read(0, 10000).unwrap(), changes);
}

#[test]
fn historical_chunk_evidence_is_deleted_after_the_prior_finished_job_is_pruned() {
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
    let report = finish(&f, 100);
    assert_eq!(report["deleted"], 3, "{report}");
    assert!(!rels(&f).iter().any(|rel| rel == "EVIDENCE_FOR"));
}

/// Every edge touching a fixture node, keyed by `(src, dst, rel)`, as comparable JSON.
fn all_edges(f: &Fixture) -> std::collections::BTreeMap<(String, String, String), Value> {
    f.store
        .traverse(&TraverseQuery {
            start_ids: vec![
                f.document.clone(),
                f.chunk.clone(),
                f.old_chunk.clone(),
                f.record.clone(),
                f.orphan.clone(),
                f.target.clone(),
            ],
            max_depth: 1,
            ..Default::default()
        })
        .unwrap()
        .edges
        .into_iter()
        .map(|e| {
            (
                (e.src.clone(), e.dst.clone(), e.rel.clone()),
                serde_json::to_value(e).unwrap(),
            )
        })
        .collect()
}

fn key(src: &NodeId, dst: &NodeId, rel: &str) -> (String, String, String) {
    (src.to_string(), dst.to_string(), rel.into())
}

/// PC-20 (2) producer isolation across extractor dialects on one source: a finish deletes only
/// its own producer's omitted rows. Each ordering writes the first producer's fixture, then a
/// second producer's rows over the same records (its own provenance for `target`, relations out
/// of the first producer's `record`, evidence from both chunks), finishes the first job, then
/// opens and finishes a job of the second dialect that names one of its rows.
#[test]
fn a_finish_never_deletes_another_extractor_producers_rows() {
    for (first, second) in [("llm", "gliner"), ("gliner", "llm")] {
        let mut f = fixture_for(first);
        let theirs = vec![
            submitted_edge(second, &f.target, &f.document, "DERIVED_FROM"),
            submitted_edge(second, &f.record, &f.orphan, "RELATES_TO"),
            submitted_edge(second, &f.target, &f.record, "RELATES_TO"),
            submitted_edge(second, &f.chunk, &f.record, "EVIDENCE_FOR"),
            submitted_edge(second, &f.old_chunk, &f.target, "EVIDENCE_FOR"),
        ];
        let their_keys = [
            key(&f.target, &f.document, "DERIVED_FROM"),
            key(&f.record, &f.orphan, "RELATES_TO"),
            key(&f.target, &f.record, "RELATES_TO"),
            key(&f.chunk, &f.record, "EVIDENCE_FOR"),
            key(&f.old_chunk, &f.target, "EVIDENCE_FOR"),
        ];
        let mut batch = WriteBatch::new(f.ctx.workspace_id.clone(), theirs);
        batch.edge_diff = EdgeDiff::None;
        f.store.apply_batch(batch, &f.ctx).unwrap();
        let before = all_edges(&f);
        let report = finish(&f, 100);
        assert_eq!(report["deleted"], 3, "{first} then {second}: {report}");
        let after = all_edges(&f);
        for k in &their_keys {
            assert_eq!(
                after.get(k),
                before.get(k),
                "{first} finish kept {second}'s row {k:?} byte for byte"
            );
        }
        let mine = key(&f.record, &f.document, "DERIVED_FROM");
        assert!(after.contains_key(&mine), "{first}'s named row stays");
        for k in [
            key(&f.orphan, &f.document, "DERIVED_FROM"),
            key(&f.record, &f.target, "RELATES_TO"),
            key(&f.old_chunk, &f.orphan, "EVIDENCE_FOR"),
        ] {
            assert!(
                !after.contains_key(&k),
                "{first}'s omitted row {k:?} is gone"
            );
        }

        let mut other = f.job.clone();
        other.job = JobId::from_digest(&[12; 32]);
        other.dialect = second.into();
        other.plan_json = json!({"v":1,"document_id":f.document,"chunks":[{"id":f.chunk}],"submit_state":{"named_records":[]},"finished_report_json":null}).to_string();
        other.named_edges = vec![(
            f.chunk.to_string(),
            f.record.to_string(),
            "EVIDENCE_FOR".into(),
        )];
        f.store.job_open(&other).unwrap();
        f.job = other;
        let report = finish(&f, 100);
        assert_eq!(report["deleted"], 4, "{second} after {first}: {report}");
        let last = all_edges(&f);
        assert_eq!(
            last.get(&mine),
            after.get(&mine),
            "{second} finish kept {first}'s row byte for byte"
        );
        assert!(last.contains_key(&key(&f.chunk, &f.record, "EVIDENCE_FOR")));
        for k in &their_keys[..3] {
            assert!(
                !last.contains_key(k),
                "{second}'s omitted row {k:?} is gone"
            );
        }
        assert!(!last.contains_key(&their_keys[4]));
    }
}
