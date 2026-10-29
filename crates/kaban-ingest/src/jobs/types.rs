//! Structured domain names reuse the API-owned canonical extractor payload.
//! Existing flat plain-ingest `LinkDraft` and `IngestReport` remain unchanged.

pub use kaban_api::ingest_jobs::{
    ChunkPage, ChunkView, EdgeDraft, EvidenceRef, IngestBrief, IngestDialect, IngestMode,
    LinkDraft, PairJudgement, PairRef, RecordDraft, StructuredReturn,
};

use kaban_core::{JobId, NodeId, WorkspaceId};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Runtime preparation options; paging does not change the frozen v1 handle identity.
#[derive(Debug, Clone)]
pub struct IngestOpts {
    /// Preparation mode; structured code remains refused.
    pub mode: IngestMode,
    /// External extractor instruction dialect, never executed by kaban.
    pub r#with: IngestDialect,
    /// Explicit closed linking spelling, defaults to none in structured mode.
    pub link: String,
    /// Explicit symbol dictionary override.
    pub symbols: bool,
    /// Optional explicit source type.
    pub r#type: Option<String>,
    /// Zero-based chunk-plan page.
    pub plan_page: u32,
    /// Prospective brief only; no placement, jobs, pruning or audit persistence.
    pub dry_run: bool,
    /// Complete serialized JSON response budget.
    pub max_result_bytes: usize,
}

impl Default for IngestOpts {
    fn default() -> Self {
        Self {
            mode: IngestMode::Structured,
            r#with: IngestDialect::None,
            link: "none".into(),
            symbols: false,
            r#type: None,
            plan_page: 0,
            dry_run: false,
            max_result_bytes: super::next::DEFAULT_RESULT_BYTES,
        }
    }
}

/// Fixed ingestion job failures; unresolved and foreign handles share one refusal.
#[derive(Debug, thiserror::Error)]
pub enum JobError {
    /// Expired, absent or superseded claimed-job holder.
    #[error("lease_lost")]
    LeaseLost,
    /// Unknown, revoked or foreign-workspace handle.
    #[error("job_revoked")]
    Revoked,
    /// A present job reached its bounded expiry.
    #[error("job_expired")]
    Expired,
    /// Content, ontology or original preparation contract changed.
    #[error("stale_job_contract")]
    Stale,
    /// A validated selector does not address this job's plan.
    #[error("invalid_chunk_selector")]
    Selector,
    /// Preparation refused the source without exposing it.
    #[error("source_unavailable")]
    Source,
    /// Existing policy/API refusal, containing no source text.
    #[error(transparent)]
    Api(#[from] kaban_api::ApiError),
    /// Existing storage refusal.
    #[error(transparent)]
    Store(#[from] kaban_core::StoreError),
    /// Complete serialized output cannot fit the byte budget.
    #[error(transparent)]
    Page(#[from] super::next::PageError),
}

impl JobError {
    /// Folded job-protocol CLI exit class.
    #[must_use]
    pub const fn exit_code(&self) -> i32 {
        match self {
            Self::Revoked => 3,
            Self::Api(kaban_api::ApiError::Rejected { .. }) => 2,
            Self::Store(_) | Self::Api(kaban_api::ApiError::Store(_)) => 5,
            _ => 4,
        }
    }
}

/// Closed DI1 chunk kind spelling; code language remains preparation metadata, not a kind.
pub(crate) const fn chunk_kind_name(
    kind: &crate::ChunkKind,
) -> kaban_api::ingest_jobs::IngestChunkKind {
    use kaban_api::ingest_jobs::IngestChunkKind;
    match kind {
        crate::ChunkKind::Text => IngestChunkKind::Text,
        crate::ChunkKind::Code { .. } => IngestChunkKind::Code,
        crate::ChunkKind::Table => IngestChunkKind::Table,
        crate::ChunkKind::Stub => IngestChunkKind::Stub,
    }
}

/// Persistent metadata for a sanitized chunk, never its text or heading path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobChunk {
    /// Source-order index.
    pub n: u32,
    /// Existing DI1 chunk identity.
    pub id: NodeId,
    /// Byte start in the source's sanitized stream.
    pub byte_start: usize,
    /// Exclusive byte end.
    pub byte_end: usize,
    /// Existing token estimate.
    pub token_estimate: usize,
    /// Closed DI chunk kind spelling.
    pub kind: crate::ChunkKind,
    /// The chunk exceeds the configured normal target.
    pub oversize: bool,
    /// Default byte-bound delivery slice count.
    pub slices: u32,
}

/// Immutable preparation options excluded from v1 handle identity, but checked on repeats.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreparationOptions {
    /// Linking mode, closed `gazetteer|none` spelling.
    pub link: String,
    /// Explicit symbol override.
    pub symbols: bool,
}

/// Private versioned encoding in 0004's existing `plan` column. E3.9 owns submit accumulation;
/// E3.10 owns canonical final report bytes and retention. No graph/job atomicity is implied.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobEnvelope {
    /// Only version 1 is admitted; unknown versions and legacy arrays fail closed.
    pub v: u32,
    /// Atomic claim binding; absent on ordinary unfenced brief jobs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease: Option<kaban_core::JobLease>,
    /// Effective server-side ingestion configuration digest for artifact identity.
    #[serde(default)]
    pub config_hash: String,
    /// Explicit Document `NodeId`, NEVER the `SourceId` from the scalar row.
    pub document_id: NodeId,
    /// Canonical loaded validation/schema/signal inputs, not current ontology guesses.
    pub ontology_hash: String,
    /// Policy hash for audit; current-policy revalidation is still mandatory per request.
    pub policy_fingerprint: String,
    /// Keyed source-byte identity; detects edits even when sanitized content stays equal.
    pub source_bytes_hmac: String,
    /// Exact gated validation/schema snapshot from the first brief.
    pub target_schema: Value,
    /// Exact allowed relationships from that snapshot.
    pub allowed_rels: Vec<String>,
    /// Metadata-only source-order plan.
    pub chunks: Vec<JobChunk>,
    /// First admitted options, immutable across repeat briefs.
    pub options: PreparationOptions,
    /// Future E3.9 accumulator; initially an empty object. Not extractor-controlled input.
    pub submit_state: Value,
    /// E3.10 stores exact serialized gated UTF-8 wire report, never a reconstructed object.
    pub finished_report_json: Option<String>,
}

impl std::fmt::Debug for JobEnvelope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JobEnvelope")
            .field("v", &self.v)
            .field("document_id", &self.document_id)
            .field("chunks", &self.chunks.len())
            .finish_non_exhaustive()
    }
}

/// Fixed codec refusal. No unknown fields, source contents or JSON parser text are echoed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("stale_job_contract")]
pub struct InvalidEnvelope;

impl JobEnvelope {
    /// Decode existing plan bytes, never infer old snapshots from the current ontology.
    ///
    /// # Errors
    /// Legacy arrays, unknown versions, invalid metadata or nonobject submit state refuse.
    pub fn decode(bytes: &str) -> Result<Self, InvalidEnvelope> {
        let envelope: Self = serde_json::from_str(bytes).map_err(|_| InvalidEnvelope)?;
        if envelope.v != 1
            || !matches!(envelope.options.link.as_str(), "gazetteer" | "none")
            || !envelope.submit_state.is_object()
            || !envelope.target_schema.is_object()
            || envelope.chunks.iter().enumerate().any(|(index, chunk)| {
                usize::try_from(chunk.n).ok() != Some(index)
                    || chunk.byte_end < chunk.byte_start
                    || chunk.slices == 0
            })
        {
            return Err(InvalidEnvelope);
        }
        Ok(envelope)
    }

    /// Encode the same validated metadata representation.
    ///
    /// # Errors
    /// Invalid metadata/version or serializer failure yields a fixed refusal.
    pub fn encode(&self) -> Result<String, InvalidEnvelope> {
        let bytes = serde_json::to_string(self).map_err(|_| InvalidEnvelope)?;
        Self::decode(&bytes)?;
        Ok(bytes)
    }
}

/// Frozen folded §4.4 v1 identity. Ontology, policy, paging, clocks, options and dry-run are
/// deliberately excluded; incompatible original contracts go stale instead of being reset.
#[allow(clippy::too_many_arguments)] // exact normative preimage fields
#[must_use]
pub fn job_id(
    workspace: &WorkspaceId,
    source: &crate::SourceId,
    content_hash: &str,
    pipeline_version: &str,
    chunk_config_hash: &str,
    mode: &str,
    dialect: IngestDialect,
    doc_type: &str,
) -> JobId {
    let mut hash = blake3::Hasher::new();
    hash.update(b"kaban-parse/v1/job");
    for bytes in [
        workspace.as_str(),
        source.as_str(),
        content_hash,
        pipeline_version,
        chunk_config_hash,
        mode,
        dialect.as_str(),
        doc_type,
    ]
    .map(str::as_bytes)
    {
        hash.update(&(bytes.len() as u64).to_le_bytes());
        hash.update(bytes);
    }
    JobId::from_digest(hash.finalize().as_bytes())
}
