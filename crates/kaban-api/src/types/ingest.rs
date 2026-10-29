//! `index_docs`, `index_status` and the `kaban://documents` resource (docs ingest).

use super::receipt::WriteReceipt;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Content-free counters for the run-local gazetteer.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GazetteerTelemetry {
    /// Retained automaton bytes.
    pub memory_usage: u64,
    /// Sampled process RSS increase while constructing the automaton.
    pub build_peak_delta: u64,
    /// Process high-water increase during the build, as a sampling cross-check.
    pub build_hwm_delta: u64,
    /// Source names omitted after a budget rebuild.
    pub dropped: Vec<String>,
    /// Closed outcome: `ok` or `skipped_budget`.
    pub outcome: String,
    /// The explicit flag that bypassed the cap, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_lifted_by: Option<String>,
}

/// `index_docs` input (docs-ingest-spec D10; docs-ingest-plan.md DI1.6): the paths to ingest,
/// relative to the workspace root (or absolute inside it). Plain mode is the only mode here
/// (OWN-45; structured mode is the engine's).
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[allow(clippy::struct_excessive_bools)] // independent wire flags, not mutually exclusive states
pub struct IndexDocsRequest {
    /// Keep symbol sources and bypass the gazetteer memory cap for this run.
    #[serde(default)]
    pub symbols: bool,
    /// Link chunks using `gazetteer`, or disable linking with `none`; absent uses config.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
    /// Files or directories to ingest; every path must lie inside the workspace root.
    pub paths: Vec<String>,
    /// Workspace to write into (default: the server's bound workspace).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    /// Count and gate without writing.
    #[serde(default)]
    pub dry_run: bool,
    /// Also ingest source-code files as text documents (spec §7; off by default).
    #[serde(default)]
    pub include_code: bool,
    /// Skip files larger than this many bytes (default 2 MiB).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_file_bytes: Option<u64>,
    /// Extraction workers (default: the available parallelism).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workers: Option<usize>,
    /// Also re-ingest documents `forget --document` marked forgotten (DI1.8; a path naming
    /// the file itself always does).
    #[serde(default)]
    pub include_forgotten: bool,
}

/// `index_docs` output: what one ingestion run did (docs-ingest-spec D7, D8). Counts only; no
/// path, title or text.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct IndexDocsResponse {
    /// Content-free gazetteer run counters, absent when linking is disabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gazetteer: Option<GazetteerTelemetry>,
    /// The workspace the run wrote into (the bound workspace).
    pub workspace: String,
    /// Documents committed (`ok` or `partial`).
    pub documents: u32,
    /// Candidates skipped (walker rules, unchanged content key).
    pub skipped: u32,
    /// Documents recorded `failed`.
    pub failed: u32,
    /// Documents recorded `blocked` by the denylist.
    pub blocked: u32,
    /// Documents recorded `partial`.
    pub partial: u32,
    /// Chunks inserted or updated.
    pub chunks_upserted: u32,
    /// Chunks whose content matched (nothing written).
    pub chunks_unchanged: u32,
    /// Chunks tombstoned because they disappeared.
    pub chunks_tombstoned: u32,
    /// Sections tombstoned by the diff (DI1.8).
    #[serde(default)]
    pub sections_tombstoned: u32,
    /// Forgotten documents a directory or workspace walk left alone (DI1.8; `include_forgotten`
    /// or naming the file brings them back).
    #[serde(default)]
    pub skipped_forgotten: u32,
    /// Chunks replaced by a keyed stub.
    pub blocked_chunks: u32,
    /// Denylist rule id -> hits.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub rule_ids: std::collections::BTreeMap<String, u32>,
    /// Outbox rows written.
    pub outbox_rows: u64,
    /// Wall-clock duration.
    pub elapsed_ms: u64,
    /// The run stopped early on cancellation.
    pub cancelled: bool,
    /// Documents whose extraction exceeded the per-document deadline.
    pub timed_out: u32,
    /// Nothing was written (`dry_run`).
    pub dry_run: bool,
    /// The run's receipt (spec 05 SS-10, one barrier per ingest run): `committed` once the
    /// serving search reader covers the run's rows, `committed_not_yet_searchable` when the
    /// barrier expired or failed (the run stands, do not replay it). Absent only from a
    /// report the run itself produced before the service settled it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receipt: Option<WriteReceipt>,
}

/// One failed document of [`IndexStatus`] (docs-ingest-spec D8): the gated path and error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FailedDocument {
    /// The gated relative path.
    pub path: String,
    /// The recorded error (one of the pipeline's fixed texts, or a gated extractor message).
    pub error: String,
}

/// `index_status` output (docs-ingest-spec D6, D7): counts, failures and lag of the bound
/// workspace's document index.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct IndexStatus {
    /// Last completed gazetteer run counters, absent when never enabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gazetteer: Option<GazetteerTelemetry>,
    /// Active Document nodes by status (`ok`, `partial`, `failed`, `blocked`, `missing`).
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub documents_by_status: std::collections::BTreeMap<String, u32>,
    /// Active chunks over every document.
    pub chunks: u64,
    /// Chunks stored as keyed stubs (reject-class hits).
    pub blocked_chunks: u64,
    /// Text chunks flagged `table_unknown` over every document (PDF rows that may be a table).
    /// Omitted when zero.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub table_unknown_chunks: u64,
    /// Documents recorded `failed`, with their gated error.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failed: Vec<FailedDocument>,
    /// Denylist rule id -> hits over every document (counts only).
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub rule_ids: std::collections::BTreeMap<String, u32>,
    /// When the last ingestion run of this workspace finished (RFC 3339), if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run: Option<String>,
    /// Outbox rows not yet acknowledged by every consumer.
    pub outbox_lag: u64,
    /// The pipeline version documents are indexed with.
    pub pipeline_version: u32,
    /// The extractors compiled into this binary (`crate@version`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extractors: Vec<String>,
}

/// `skip_serializing_if` predicate of [`IndexStatus`]'s zero-default counters.
#[allow(clippy::trivially_copy_pass_by_ref)] // serde's predicate signature is `fn(&T) -> bool`
fn is_zero(n: &u64) -> bool {
    *n == 0
}

/// One row of the `kaban://documents` listing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DocumentSummary {
    /// The gated relative path.
    pub path: String,
    /// `ok`, `partial`, `failed`, `blocked` or `missing`.
    pub status: String,
    /// Active chunks of the document.
    pub chunks: u32,
    /// Detected language, when any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lang: Option<String>,
    /// Sanitized title, when any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

/// Body of the `kaban://documents` resource (docs-ingest-spec D7): the status plus one page of
/// the document list in the store's id order (stable across pages).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct DocumentsResource {
    /// The index status.
    pub status: IndexStatus,
    /// This page of documents.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub documents: Vec<DocumentSummary>,
    /// Offset of the first row of this page.
    pub offset: usize,
    /// Page size.
    pub limit: usize,
    /// Whether more rows follow this page.
    pub more: bool,
}
