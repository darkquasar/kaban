//! Checked-source atomic finalization with exact retained UTF-8 report retries.
use super::submit::SubmitState;
use super::types::{JobEnvelope, JobError};
use kaban_api::ingest_jobs::{
    IngestChanges, IngestFinishReport, IngestReconcileOutcome, IngestReportStatus,
};
use kaban_api::{ApiError, KabanApi};
use kaban_core::{JobFinishRequest, JobId, JobStatus, JobSubmitContract, SourceRev, StoreError};
use std::path::Path;

/// Finish the union of all successful submits; return canonical gated JSON bytes.
/// Every call reopens the source and checks current authority, ontology and policy.
/// # Errors
/// Fixed revoked/expired/stale/source refusals, content gates and atomic backend failures.
pub async fn finish(api: &KabanApi, root: &Path, job: &JobId) -> Result<String, JobError> {
    finish_fenced(api, root, job, None).await
}

/// Finish a claimed or ordinary job with transaction-current holder validation.
/// # Errors
/// The same refusals as [`finish`], plus fixed `lease_lost` for a lost holder.
pub async fn finish_fenced(
    api: &KabanApi,
    root: &Path,
    job: &JobId,
    fence: Option<u64>,
) -> Result<String, JobError> {
    api.prepare_args("ingest_finish", &serde_json::json!({"job":job.as_str()}))?;
    let initial = api
        .store()
        .job_get(job, api.workspace())?
        .ok_or(JobError::Revoked)?;
    let envelope = JobEnvelope::decode(&initial.plan_json).map_err(|_| JobError::Stale)?;
    let document = api
        .store()
        .get(&envelope.document_id, true)?
        .ok_or(JobError::Revoked)?;
    let revision = SourceRev::from(&document);
    let contract = JobSubmitContract::from_finish_job(&initial)?;
    let (row, envelope, _) = super::next::revalidate_fenced(api, root, job, fence).await?;
    if JobSubmitContract::from_finish_job(&row)? != contract {
        return Err(JobError::Stale);
    }
    let request = JobFinishRequest {
        job: job.clone(),
        contract: contract.with_fence(fence),
        document: envelope.document_id,
        source_rev: revision,
        chunks: envelope.chunks.into_iter().map(|c| c.id).collect(),
        finish_cap: api.engine().ingest.finish_cap,
        retention_days: api.engine().ingest.retention_days,
    };
    let bytes = api
        .publish_ingest_finish(request, &mut |current, outcome| {
            let envelope = JobEnvelope::decode(&current.plan_json)
                .map_err(|_| StoreError::InvalidName("job".into(), "stale"))?;
            if current.status == JobStatus::Finished {
                return envelope
                    .finished_report_json
                    .ok_or(StoreError::InvalidName("job".into(), "stale"));
            }
            let state: SubmitState = serde_json::from_value(envelope.submit_state)?;
            serde_json::to_string(&IngestFinishReport {
                job: current.job.to_string(),
                status: IngestReportStatus::Finished,
                submits: current.submits,
                created: current.created,
                updated: current.updated,
                unchanged: current.unchanged,
                redactions: current.redactions,
                edges: state.edges,
                assertions: state.assertions,
                revision_candidates: state.revision_candidates,
                predecessor_terminal: state.predecessor_terminal,
                skipped: state.skipped,
                evidence_dropped: state.evidence_dropped,
                reconcile: if outcome.skipped_cap {
                    IngestReconcileOutcome::SkippedCap
                } else {
                    IngestReconcileOutcome::Complete
                },
                retired_assertions: outcome.retired_assertions,
                deleted_edges: outcome.deleted_edges,
                orphans: outcome.orphans.iter().map(ToString::to_string).collect(),
                changes: IngestChanges {
                    first_seq: state.first_seq.or(outcome.first_seq),
                    last_seq: outcome.last_seq.or(state.last_seq),
                },
                // built inside the store's callback: the receipt reads nothing, and its
                // position is the head the finish transaction saw (SS-10: a run that
                // emitted no change row still names the state it references, never 0)
                receipt: Some(api.committed_receipt(Some(outcome.terminal_seq))),
            })
            .map_err(StoreError::from)
        })
        .map_err(map_error)?;
    let bytes = settle_report(api, bytes);
    // Keep a present expired target distinguishable: pruning happens only after success.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
    api.store().jobs_prune(api.workspace(), now, 64)?;
    Ok(bytes)
}

/// The finish report's receipt after the write barrier (spec 05 SS-10, once per run): the
/// report is serialized inside the store's callback, so the barrier runs on the way out, after
/// the finish transaction closed. A report that does not parse is returned as it was.
fn settle_report(api: &KabanApi, bytes: String) -> String {
    let Ok(mut report) = serde_json::from_str::<IngestFinishReport>(&bytes) else {
        return bytes;
    };
    let Some(receipt) = report.receipt.take() else {
        return bytes;
    };
    report.receipt = Some(api.await_searchable(receipt));
    serde_json::to_string(&report).unwrap_or(bytes)
}

fn map_error(error: ApiError) -> JobError {
    match error {
        ApiError::Store(StoreError::LeaseLost { .. }) => JobError::LeaseLost,
        ApiError::Store(StoreError::NotFound(_)) => JobError::Revoked,
        ApiError::Store(StoreError::InvalidName(name, "expired")) if name == "job" => {
            JobError::Expired
        }
        ApiError::Store(StoreError::InvalidName(name, "stale")) if name == "job" => JobError::Stale,
        other => JobError::Api(other),
    }
}
