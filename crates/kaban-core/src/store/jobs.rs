//! Backend-neutral persistent ingestion registry data. Handles carry no authority.

use crate::WorkspaceId;
use serde::{Deserialize, Serialize};

/// A validated lowercase 64-hex deterministic ingestion handle, never a source or node id.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct JobId(String);

/// Fixed, content-free malformed-handle refusal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("invalid_job_handle")]
pub struct InvalidJobId;

impl JobId {
    /// Canonical handle from a fixed-width digest, without a fallible text round-trip.
    #[must_use]
    pub fn from_digest(digest: &[u8; 32]) -> Self {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut value = String::with_capacity(64);
        for byte in digest {
            value.push(char::from(HEX[usize::from(byte >> 4)]));
            value.push(char::from(HEX[usize::from(byte & 15)]));
        }
        Self(value)
    }
    /// Validate the canonical spelling without echoing untrusted input.
    ///
    /// # Errors
    /// Returns `invalid_job_handle` for any noncanonical input.
    pub fn parse(value: &str) -> Result<Self, InvalidJobId> {
        if value.len() == 64
            && value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            Ok(Self(value.to_owned()))
        } else {
            Err(InvalidJobId)
        }
    }

    /// Canonical handle bytes; callers must still resolve workspace-scoped authority.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for JobId {
    type Error = InvalidJobId;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(&value)
    }
}
impl From<JobId> for String {
    fn from(value: JobId) -> Self {
        value.0
    }
}
impl std::fmt::Display for JobId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Existing immutable 0004 registry statuses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JobStatus {
    /// Initial contract; successful submit accumulation may grow but never reset it.
    Open,
    /// Canonical gated report bytes retained for bounded retries.
    Finished,
    /// Content/ontology/preparation contract no longer matches.
    Stale,
    /// Present but expired, distinct from unresolved/revoked handles.
    Expired,
}

impl JobStatus {
    /// Exact existing SQL spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Finished => "finished",
            Self::Stale => "stale",
            Self::Expired => "expired",
        }
    }
}

/// Metadata-only existing-table row. `plan` is a versioned private envelope: no source text,
/// quotes, headings or section paths. The runtime owns its codec and all content gates.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IngestJob {
    /// Deterministic job handle.
    pub job: JobId,
    /// Bound tenant; every lookup/update must also name it.
    pub workspace: WorkspaceId,
    /// DI `SourceId` spelling, explicitly NOT the Document `NodeId`.
    pub source_id: String,
    /// Current-policy gated relative source path; no filesystem capability.
    pub relative_path: String,
    /// Content identity recorded by the existing document preparation path.
    pub content_hash: String,
    /// Frozen DI pipeline version spelling.
    pub pipeline_version: String,
    /// Deterministic plain chunker configuration hash.
    pub chunk_config_hash: String,
    /// Existing mode spelling.
    pub mode: String,
    /// Closed extractor dialect spelling.
    pub dialect: String,
    /// Chosen source-type spelling.
    pub doc_type: String,
    /// Canonical allowed kind names, existing JSON-array representation.
    pub kinds: Vec<String>,
    /// Versioned envelope bytes, including validation snapshot and canonical final report.
    pub plan_json: String,
    /// Union of successful `(src, dst, rel)` triples; no per-submit replacement or silent truncation.
    pub named_edges: Vec<(String, String, String)>,
    /// Successful submits.
    pub submits: u64,
    /// Accumulated created record count.
    pub created: u64,
    /// Accumulated updated record count.
    pub updated: u64,
    /// Accumulated unchanged record count.
    pub unchanged: u64,
    /// Accumulated redaction count.
    pub redactions: u64,
    /// Lifecycle state.
    pub status: JobStatus,
    /// Creation time, epoch seconds.
    pub created_at: i64,
    /// Expiry time, epoch seconds.
    pub expires_at: i64,
    /// Finish time, absent before completion.
    pub finished_at: Option<i64>,
}

impl std::fmt::Debug for IngestJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IngestJob")
            .field("job", &self.job)
            .field("workspace", &self.workspace)
            .field("status", &self.status)
            .finish_non_exhaustive()
    }
}

impl IngestJob {
    /// Digest of pinned effective preparation configuration for the completion artifact.
    #[must_use]
    pub fn marker_config_hash(&self) -> String {
        if let Ok(plan) = serde_json::from_str::<serde_json::Value>(&self.plan_json)
            && let Some(hash) = plan.get("config_hash").and_then(serde_json::Value::as_str)
            && !hash.is_empty()
        {
            return hash.to_owned();
        }
        blake3::hash(
            format!(
                "{}:{}:{}",
                self.pipeline_version, self.chunk_config_hash, self.doc_type
            )
            .as_bytes(),
        )
        .to_hex()
        .to_string()
    }
}

/// Immutable prepared registry contract, excluding only successful accumulation/lifecycle.
/// The private plan remains opaque to core except for its two documented mutable slots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobSubmitContract(serde_json::Value, Option<u64>);

impl JobSubmitContract {
    /// Attach the holder's proof; it never supplies producer or source authority.
    #[must_use]
    pub fn with_fence(mut self, fence: Option<u64>) -> Self {
        self.1 = fence;
        self
    }
    /// Holder proof to validate inside the publication transaction.
    #[must_use]
    pub const fn fence(&self) -> Option<u64> {
        self.1
    }
    /// Capture immutable finalization inputs, excluding the retention expiry that the
    /// first concurrent finish legitimately changes. Submit contracts remain unchanged.
    /// # Errors
    /// Malformed private envelope or backend-neutral encoding failure.
    pub fn from_finish_job(job: &IngestJob) -> crate::Result<Self> {
        let mut normalized = job.clone();
        normalized.expires_at = 0;
        Self::from_job(&normalized)
    }
    /// Capture the contract from a workspace-scoped row before source reopening and gating.
    ///
    /// # Errors
    /// Malformed private envelope or backend-neutral encoding failure.
    pub fn from_job(job: &IngestJob) -> crate::Result<Self> {
        let mut plan: serde_json::Value = serde_json::from_str(&job.plan_json)?;
        if plan.get("v").and_then(serde_json::Value::as_u64) != Some(1) {
            return Err(crate::StoreError::InvalidName("job".into(), "stale"));
        }
        let object = plan
            .as_object_mut()
            .ok_or(crate::StoreError::InvalidName("job".into(), "stale"))?;
        object.remove("submit_state");
        object.remove("finished_report_json");
        let mut value = serde_json::to_value(job)?;
        let fields = value
            .as_object_mut()
            .ok_or(crate::StoreError::InvalidName("job".into(), "stale"))?;
        for field in [
            "plan_json",
            "named_edges",
            "submits",
            "created",
            "updated",
            "unchanged",
            "redactions",
            "status",
            "finished_at",
        ] {
            fields.remove(field);
        }
        fields.insert("plan".into(), plan);
        Ok(Self(value, None))
    }
}

/// Trusted job-bound lease metadata, persisted in the existing private plan envelope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobLease {
    /// Tenant, dialect, Document and content-bound lease key.
    pub name: String,
    /// Gated worker identity.
    pub owner: String,
    /// Durable ownership generation, independent of the revision attempt count.
    pub fence: u64,
    /// Effective renewal duration, not supplied by the extractor.
    pub ttl_s: u32,
}

/// One bounded pending Document coordinate; paths still require the service CONTENT gate.
#[derive(Debug, Clone)]
pub struct PendingJob {
    /// Actual Document identity.
    pub document: crate::NodeId,
    /// Gated stored path, never a filesystem capability.
    pub path: String,
    /// Transaction-validated full source identity.
    pub source_rev: crate::SourceRev,
}

/// Trusted ordinary-open preparation, validated atomically with metadata insertion.
/// This is not a serialized extractor request or a lease acquisition.
#[derive(Debug, Clone)]
pub struct JobOpenRequest {
    /// Gated metadata contract, including the tenant and source identity.
    pub job: IngestJob,
    /// Actual owned Document identity, not the DI source ID.
    pub document: crate::NodeId,
    /// Exact published revision captured during preparation.
    pub source_rev: crate::SourceRev,
    /// Complete ordered live Chunk identities in the prepared contract.
    pub chunks: Vec<crate::NodeId>,
}

/// Prepared claim acquisition, composed with job opening in one transaction.
#[derive(Debug, Clone)]
pub struct JobClaimRequest {
    /// Prepared metadata-only contract.
    pub job: IngestJob,
    /// Exact active Document revision selected before source reopening.
    pub source_rev: crate::SourceRev,
    /// Gated worker identity.
    pub worker: String,
    /// Configured TTL.
    pub ttl_s: u32,
    /// Configured maximum attempts for an unmarked revision.
    pub max_attempts: u32,
}

/// Gated graph publication bound to the immutable preparation and live DI identities.
#[derive(Debug)]
pub struct JobSubmitRequest {
    /// Workspace-scoped handle; the write context supplies the workspace.
    pub job: JobId,
    /// Contract captured before gating, compared with the transaction-current row.
    pub contract: JobSubmitContract,
    /// Actual Document identity, never the DI `SourceId`.
    pub document: crate::NodeId,
    /// Current database source identity, captured before source reopening.
    pub source_rev: crate::SourceRev,
    /// Admitted live Chunk identities of that Document.
    pub chunks: Vec<crate::NodeId>,
    /// Already-gated batch; submit never performs complete-edge deletion.
    pub batch: crate::WriteBatch,
}

/// Bounded runtime accumulation only; the backend owns the actual scalar counters.
#[derive(Debug)]
pub struct JobSubmitStateUpdate {
    /// Same private contract with updated bounded `submit_state` only.
    pub plan_json: String,
    /// Successful `(src, dst, rel)` union (possibly bounded with an explicit overflow flag).
    pub named_edges: Vec<(String, String, String)>,
}

/// Committed publication and transaction-current accumulated registry row.
#[derive(Debug)]
pub struct JobSubmitReceipt {
    /// Actual node/assertion outcomes and graph sequence range.
    pub batch: crate::BatchReport,
    /// Updated row from the same transaction.
    pub job: IngestJob,
}

/// Checked preparation for atomic finalization; no graph mutation is supplied by callers.
#[derive(Debug)]
pub struct JobFinishRequest {
    /// Scoped handle.
    pub job: JobId,
    /// Immutable preparation captured before reopening.
    pub contract: JobSubmitContract,
    /// Real Document identity.
    pub document: crate::NodeId,
    /// Source identity captured before asynchronous reopening.
    pub source_rev: crate::SourceRev,
    /// Complete current Chunk identities.
    pub chunks: Vec<crate::NodeId>,
    /// Maximum complete eligible population, never a truncation authority.
    pub finish_cap: u32,
    /// Retained finished report lifetime.
    pub retention_days: u32,
}

/// Transaction-local reconciliation outcomes; orphan identities are informational only.
#[derive(Debug, Default)]
pub struct JobFinishOutcome {
    /// Cap or accumulator overflow prevented all retirement.
    pub skipped_cap: bool,
    /// Retired machine claims.
    pub retired_assertions: u64,
    /// Deleted owned graph edges.
    pub deleted_edges: u64,
    /// Proposed omitted records, never tombstoned.
    pub orphans: Vec<crate::NodeId>,
    /// Actual reconciliation graph changes.
    pub first_seq: Option<u64>,
    /// Last reconciliation sequence.
    pub last_seq: Option<u64>,
    /// The log head read inside the finish transaction (spec 05 SS-10): the run's terminal
    /// sequence, `last_seq` when the finish wrote change rows, else the position of the state
    /// the run references (never 0 on a database that holds rows).
    pub terminal_seq: u64,
}

/// Gated canonical report and successful audit, committed with reconciliation.
#[derive(Debug)]
pub struct JobFinishStateUpdate {
    /// Exact gated UTF-8 JSON bytes; retries must return these unchanged.
    pub report_json: String,
    /// Successful gate audit derived by the API adapter.
    pub audit: Vec<crate::AuditEntry>,
}
