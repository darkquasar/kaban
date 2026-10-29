//! Deterministic UTF-8 delivery boundaries measured against serialized JSON, never raw text.

use super::types::{JobEnvelope, JobError};
use kaban_api::KabanApi;
use kaban_api::ingest_jobs::{ChunkContinuation, ChunkPage, ChunkRangePage, ChunkView};
use kaban_core::JobId;
use kaban_core::{JobStatus, Lifecycle};
use std::path::Path;

/// Default folded AI-F7 result ceiling, including escaping and metadata.
pub const DEFAULT_RESULT_BYTES: usize = 64 * 1024;
/// Existing DI1 default slice target. A smaller result budget may shrink it further.
pub const DEFAULT_SLICE_BYTES: usize = 32 * 1024;

/// Bounded delivery settings; no ingestion cache or engine configuration is added.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageBudget {
    /// Maximum complete serialized JSON response size.
    pub max_result_bytes: usize,
    /// Reserved metadata allowance from the ingestion configuration.
    pub metadata_allowance: usize,
}

impl Default for PageBudget {
    fn default() -> Self {
        Self {
            max_result_bytes: DEFAULT_RESULT_BYTES,
            metadata_allowance: 4 * 1024,
        }
    }
}

/// Fixed delivery refusals; never echo source text or parser diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PageError {
    /// Even the metadata or one UTF-8 character cannot fit the result budget.
    #[error("result_budget_exceeded")]
    Budget,
    /// The supplied logical chunk offsets are reversed.
    #[error("stale_job_contract")]
    Contract,
}

/// Split a freshly prepared, current-policy-gated chunk into complete AI-F7 responses.
/// This helper grants no job lookup or file authority: the runtime must revalidate first.
///
/// # Errors
/// Refuses invalid offsets, impossible budgets, counter overflow or serialization failure.
pub fn slice_chunk(
    job: &JobId,
    chunk: &ChunkView,
    next_chunk: Option<u32>,
    budget: PageBudget,
) -> Result<Vec<ChunkPage>, PageError> {
    // DI1 source ranges can include markup and paragraph separators that are absent from
    // rendered text, or overlap after semantic splitting. They are not text-slice offsets.
    if chunk.byte_end < chunk.byte_start {
        return Err(PageError::Contract);
    }
    let target = budget
        .max_result_bytes
        .checked_sub(budget.metadata_allowance)
        .filter(|target| *target > 0)
        .ok_or(PageError::Budget)?
        .min(DEFAULT_SLICE_BYTES);
    let mut pages = Vec::new();
    let mut start: usize = 0;
    loop {
        let mut end = start.saturating_add(target).min(chunk.text.len());
        while !chunk.text.is_char_boundary(end) {
            end -= 1;
        }
        let mut page = response(job, chunk, start, end);
        // Reserve the largest counter/continuation representation while choosing boundaries.
        // Final counters and null continuations can only make the response smaller.
        if serialized_bytes(&page)? > budget.max_result_bytes {
            let mut lower = start;
            let mut upper = end;
            while lower < upper {
                let midpoint = lower + (upper - lower).div_ceil(2);
                let mut boundary = midpoint;
                while !chunk.text.is_char_boundary(boundary) {
                    boundary -= 1;
                }
                let candidate = response(job, chunk, start, boundary);
                if serialized_bytes(&candidate)? <= budget.max_result_bytes {
                    lower = midpoint;
                } else {
                    upper = midpoint - 1;
                }
            }
            end = lower;
            while !chunk.text.is_char_boundary(end) {
                end -= 1;
            }
            page = response(job, chunk, start, end);
            if serialized_bytes(&page)? > budget.max_result_bytes {
                return Err(PageError::Budget);
            }
        }
        if end == start && end < chunk.text.len() {
            return Err(PageError::Budget);
        }
        pages.push(page);
        start = end;
        if start == chunk.text.len() {
            break;
        }
    }
    let slices = u32::try_from(pages.len()).map_err(|_| PageError::Budget)?;
    for (index, page) in pages.iter_mut().enumerate() {
        let slice = u32::try_from(index).map_err(|_| PageError::Budget)?;
        page.chunk.slice = slice;
        page.chunk.slices = slices;
        page.next = if slice + 1 < slices {
            Some(ChunkContinuation {
                n: chunk.n,
                slice: slice + 1,
            })
        } else {
            next_chunk.map(|n| ChunkContinuation { n, slice: 0 })
        };
    }
    Ok(pages)
}

fn response(job: &JobId, chunk: &ChunkView, start: usize, end: usize) -> ChunkPage {
    ChunkPage {
        job: job.as_str().to_owned(),
        chunk: ChunkView {
            text: chunk.text[start..end].to_owned(),
            slice: u32::MAX,
            slices: u32::MAX,
            n: chunk.n,
            id: chunk.id.clone(),
            kind: chunk.kind,
            section_path: chunk.section_path.clone(),
            context_prefix: chunk.context_prefix.clone(),
            byte_start: chunk.byte_start,
            byte_end: chunk.byte_end,
        },
        next: Some(ChunkContinuation {
            n: u32::MAX,
            slice: u32::MAX,
        }),
    }
}

fn serialized_bytes(page: &ChunkPage) -> Result<usize, PageError> {
    serde_json::to_vec(page)
        .map(|bytes| bytes.len())
        .map_err(|_| PageError::Budget)
}

/// Resolve one scoped job and reopen/revalidate its source on every call, without a cache.
/// Only registry staleness may be written; this never republishes the document graph.
/// The caller supplies its configured workspace root, never a job-provided capability.
///
/// # Errors
/// Foreign/unknown/revoked jobs share `job_revoked`; present expired/stale jobs fail exit 4.
/// Content/ontology drift marks only the scoped registry row stale. Invalid selectors refuse.
pub async fn next(
    api: &KabanApi,
    root: &Path,
    job: &JobId,
    n: u32,
    slice: u32,
    budget: PageBudget,
) -> Result<ChunkPage, JobError> {
    next_fenced(api, root, job, n, slice, budget, None).await
}

/// Deliver a chunk under the optional claimed-job holder proof.
/// # Errors
/// The same failures as [`next`], plus `lease_lost` for a lost claimed holder.
pub async fn next_fenced(
    api: &KabanApi,
    root: &Path,
    job: &JobId,
    n: u32,
    slice: u32,
    budget: PageBudget,
    fence: Option<u64>,
) -> Result<ChunkPage, JobError> {
    let (_, envelope, source) = revalidate_fenced(api, root, job, fence).await?;
    let index = usize::try_from(n).map_err(|_| JobError::Selector)?;
    let view = chunk_view(&envelope, &source, index)?;
    let following = envelope.chunks.get(index + 1).map(|chunk| chunk.n);
    let pages = slice_chunk(job, &view, following, budget)?;
    let page = pages
        .into_iter()
        .nth(usize::try_from(slice).map_err(|_| JobError::Selector)?)
        .ok_or(JobError::Selector)?;
    let value = serde_json::to_value(&page).map_err(|_| PageError::Budget)?;
    let gated = api.gate_response("ingest_next", &value)?;
    if serde_json::to_vec(&gated)
        .map_err(|_| PageError::Budget)?
        .len()
        > budget.max_result_bytes
    {
        return Err(PageError::Budget.into());
    }
    let page = serde_json::from_value(gated).map_err(|_| JobError::Source)?;
    super::claim::holder(api, job, fence, true)?;
    Ok(page)
}

/// Deliver a requested exclusive-ended range of consecutive whole chunks, reopening once.
/// Oversized chunks stop the range and are subsequently addressed with single-chunk slices.
///
/// # Errors
/// The same authority/staleness failures as [`next`], invalid ranges or impossible metadata.
pub async fn next_range(
    api: &KabanApi,
    root: &Path,
    job: &JobId,
    a: u32,
    b: u32,
    budget: PageBudget,
) -> Result<ChunkRangePage, JobError> {
    next_range_fenced(api, root, job, a, b, budget, None).await
}

/// Deliver a whole-chunk range under the optional claimed-job holder proof.
/// # Errors
/// The same failures as [`next_range`], plus `lease_lost` for a lost holder.
pub async fn next_range_fenced(
    api: &KabanApi,
    root: &Path,
    job: &JobId,
    a: u32,
    b: u32,
    budget: PageBudget,
    fence: Option<u64>,
) -> Result<ChunkRangePage, JobError> {
    let (_, envelope, source) = revalidate_fenced(api, root, job, fence).await?;
    let start = usize::try_from(a).map_err(|_| JobError::Selector)?;
    let end = usize::try_from(b).map_err(|_| JobError::Selector)?;
    if start >= end || end > envelope.chunks.len() {
        return Err(JobError::Selector);
    }
    let mut page = ChunkRangePage {
        job: job.as_str().to_owned(),
        chunks: Vec::new(),
        next: Some(ChunkContinuation { n: a, slice: 0 }),
    };
    for index in start..end {
        let view = chunk_view(&envelope, &source, index)?;
        let previous = page.clone();
        page.chunks.push(view);
        page.next = if index + 1 < end {
            Some(ChunkContinuation {
                n: u32::try_from(index + 1).map_err(|_| JobError::Selector)?,
                slice: 0,
            })
        } else {
            None
        };
        let gated = api.gate_response(
            "ingest_next",
            &serde_json::to_value(&page).map_err(|_| PageError::Budget)?,
        )?;
        if serde_json::to_vec(&gated)
            .map_err(|_| PageError::Budget)?
            .len()
            > budget.max_result_bytes
        {
            page = previous;
            break;
        }
    }
    let gated = api.gate_response(
        "ingest_next",
        &serde_json::to_value(&page).map_err(|_| PageError::Budget)?,
    )?;
    if serde_json::to_vec(&gated)
        .map_err(|_| PageError::Budget)?
        .len()
        > budget.max_result_bytes
    {
        return Err(PageError::Budget.into());
    }
    let page = serde_json::from_value(gated).map_err(|_| JobError::Source)?;
    super::claim::holder(api, job, fence, true)?;
    Ok(page)
}

pub(crate) async fn revalidate_fenced(
    api: &KabanApi,
    root: &Path,
    job: &JobId,
    fence: Option<u64>,
) -> Result<
    (
        kaban_core::IngestJob,
        JobEnvelope,
        super::brief::PreparedSource,
    ),
    JobError,
> {
    super::claim::holder(api, job, fence, false)?;
    let row = api
        .store()
        .job_get(job, api.workspace())?
        .ok_or(JobError::Revoked)?;
    let envelope = JobEnvelope::decode(&row.plan_json).map_err(|_| JobError::Stale)?;
    let document = api
        .store()
        .get(&envelope.document_id, true)?
        .ok_or(JobError::Revoked)?;
    if document.workspace_id != api.workspace().as_str()
        || document.lifecycle != Lifecycle::Active
        || document.kind != "Document"
    {
        return Err(JobError::Revoked);
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| JobError::Expired)?;
    if row.status == JobStatus::Expired
        || i64::try_from(now.as_secs()).unwrap_or(i64::MAX) >= row.expires_at
    {
        return Err(JobError::Expired);
    }
    if row.status == JobStatus::Stale {
        return Err(JobError::Stale);
    }
    if ontology_fingerprint(api)? != envelope.ontology_hash {
        if envelope.lease.is_none() {
            api.store().job_mark_stale(job, api.workspace())?;
        }
        return Err(JobError::Stale);
    }
    let (fingerprint, graph) = super::brief::reopen_source(api, root, &row.relative_path).await?;
    if fingerprint != envelope.source_bytes_hmac {
        if envelope.lease.is_none() {
            api.store().job_mark_stale(job, api.workspace())?;
        }
        return Err(JobError::Stale);
    }
    let source = super::brief::PreparedSource {
        graph: graph.ok_or(JobError::Source)?,
        source_bytes_hmac: fingerprint,
    };
    Ok((row, envelope, source))
}

pub(crate) fn chunk_view(
    envelope: &JobEnvelope,
    source: &super::brief::PreparedSource,
    index: usize,
) -> Result<ChunkView, JobError> {
    let plan = envelope.chunks.get(index).ok_or(JobError::Selector)?;
    let (section, chunk) = source
        .graph
        .sections
        .iter()
        .flat_map(|section| section.chunks.iter().map(move |chunk| (section, chunk)))
        .nth(index)
        .ok_or(JobError::Source)?;
    if source.graph.document.id != envelope.document_id
        || chunk.props.kind != plan.kind
        || usize::try_from(chunk.props.byte_start).ok() != Some(plan.byte_start)
        || usize::try_from(chunk.props.byte_end).ok() != Some(plan.byte_end)
    {
        return Err(JobError::Source);
    }
    Ok(ChunkView {
        n: plan.n,
        id: plan.id.as_str().to_owned(),
        kind: super::types::chunk_kind_name(&plan.kind),
        section_path: section.props.heading_path.clone(),
        context_prefix: chunk.props.context_prefix.clone(),
        text: chunk.props.text.clone(),
        byte_start: chunk.props.byte_start,
        byte_end: chunk.props.byte_end,
        slice: 0,
        slices: 1,
    })
}

/// Loaded-file hashes in registry order bind source mappings, relationships and props.
pub(crate) fn ontology_fingerprint(api: &KabanApi) -> Result<String, JobError> {
    let files = api.registry().list();
    let bytes = serde_json::to_vec(&files).map_err(|_| JobError::Source)?;
    Ok(blake3::hash(&bytes).to_hex().to_string())
}
