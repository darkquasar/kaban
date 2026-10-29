//! Nonblocking bounded manual claims; workers and extractors remain external.
use super::types::{IngestOpts, JobError};
use kaban_api::KabanApi;
use kaban_api::ingest_jobs::{IngestClaimRequest, IngestClaimResponse, IngestLease};
use std::path::Path;

/// Claim the first available path-ordered pending Document in this workspace.
/// # Errors
/// CONTENT refusal, source/preparation failure or backend transaction failure.
pub async fn claim(
    api: &KabanApi,
    root: &Path,
    request: &IngestClaimRequest,
) -> Result<IngestClaimResponse, JobError> {
    let raw = serde_json::to_value(request).map_err(|_| JobError::Source)?;
    api.prepare_args("ingest_claim", &raw)?;
    // Identity-bearing content must survive the gate unchanged before any write.
    let content = serde_json::json!({"type":request.r#type,"path_prefix":request.path_prefix,"worker":request.worker});
    if api.gate_response("ingest_claim", &content)? != content {
        return Err(JobError::Source);
    }
    let worker = request.worker.clone().unwrap_or_else(|| {
        format!(
            "{}:{}",
            std::env::var("HOSTNAME").unwrap_or_else(|_| "localhost".into()),
            std::process::id()
        )
    });
    let worker_value = serde_json::json!({"worker":worker});
    if worker.is_empty()
        || worker.len() > 256
        || api.gate_response("ingest_claim", &worker_value)? != worker_value
    {
        return Err(JobError::Source);
    }
    let pending = api.store().jobs_pending(
        api.workspace(),
        request.r#with.as_str(),
        request.path_prefix.as_deref(),
        api.engine().ingest.claim_scan_max,
    )?;
    let opts = IngestOpts {
        r#with: request.r#with,
        r#type: request.r#type.clone(),
        dry_run: true,
        ..IngestOpts::default()
    };
    for candidate in pending {
        let (brief, lease) = super::brief::prepare_brief(
            api,
            root,
            &candidate.path,
            &opts,
            Some((&candidate, &worker)),
        )
        .await?;
        if let Some(lease) = lease {
            let response = IngestClaimResponse {
                job: Some(brief.job.clone()),
                brief: Some(brief),
                lease: Some(IngestLease {
                    fence: lease.fence,
                    expires_at: lease.expires_at,
                }),
            };
            return serde_json::from_value(api.gate_response(
                "ingest_claim",
                &serde_json::to_value(response).map_err(|_| JobError::Source)?,
            )?)
            .map_err(|_| JobError::Source);
        }
    }
    Ok(IngestClaimResponse {
        job: None,
        brief: None,
        lease: None,
    })
}

pub(crate) fn holder(
    api: &KabanApi,
    job: &kaban_core::JobId,
    fence: Option<u64>,
    renew: bool,
) -> Result<(), JobError> {
    api.store()
        .job_holder(job, api.workspace(), fence, renew)
        .map_err(|error| match error {
            kaban_core::StoreError::LeaseLost { .. } => JobError::LeaseLost,
            kaban_core::StoreError::NotFound(_) => JobError::Revoked,
            other => JobError::Store(other),
        })
}
