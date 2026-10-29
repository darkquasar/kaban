//! Shared ingestion types used by every later task (docs-ingest-plan.md DI0.1; PR-7): the
//! closed vocabularies of formats, document states and section/chunk kinds, the stable source
//! identity, the run report and the error type.

use kaban_api::ApiError;
use kaban_core::StoreError;
use kaban_policy::PolicyError;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use thiserror::Error;

/// Document format, chosen by the walker from the extension (docs-ingest-spec D1; `Code` is
/// the excluded-by-default source-file class of spec §7, ingested as text with
/// `--include-code`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    /// `.md`, `.markdown` (pulldown-cmark).
    Markdown,
    /// `.txt` (chardetng detection).
    Text,
    /// `.pdf`.
    Pdf,
    /// `.html`, `.htm` (v3.1).
    Html,
    /// `.csv`, `.tsv` (v3.1).
    Csv,
    /// `.xlsx`, `.xls`, `.xlsb`, `.ods` (v3.1).
    Sheet,
    /// `.docx` (v3.1).
    Docx,
    /// A source-code file (spec §7: excluded unless `include_code`).
    Code,
}

/// Lifecycle state of an ingested `Document` node (docs-ingest-spec D2, D6, D8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DocStatus {
    /// Extracted and committed in full.
    Ok,
    /// Extracted with gaps (for example a PDF without a text layer, `needs_ocr`).
    Partial,
    /// The extractor failed or timed out; the node carries a gated `error` (D8).
    Failed,
    /// The denylist skipped the whole file (path rule or `on_document: skip_file`, D6).
    Blocked,
    /// A placeholder for an unresolved link target (D2).
    Missing,
}

/// What a `Section` node represents (docs-ingest-spec D2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SectionKind {
    /// A heading-delimited section.
    Heading,
    /// One PDF page.
    Page,
    /// One spreadsheet sheet.
    Sheet,
    /// A table.
    Table,
    /// A code block.
    Code,
    /// The frontmatter block.
    Frontmatter,
}

/// What a `Chunk` node holds (docs-ingest-spec D4, D6).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChunkKind {
    /// Prose.
    Text,
    /// A fenced code block, never split (D4).
    Code {
        /// The fence's language tag, when present.
        lang: Option<String>,
    },
    /// One table (rows never split, header repeated in `context_prefix`).
    Table,
    /// A reject-class hit replaced the text by an HMAC-keyed stub (D6; core-spec S1).
    Stub,
}

/// Stable identity of a source document: `blake3` over a domain-tagged, length-prefixed
/// encoding of `(workspace_id, relative_path)` (D-4), lowercase hex. It is constructible only
/// from a path that passed the denylist gate (DI0.2 `SourceId::new`), so a rejected name is
/// never hashed into an identity (docs-ingest-plan.md PR-2; core-spec S1).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
#[repr(transparent)]
pub struct SourceId(String);

impl SourceId {
    /// Wrap a computed digest (crate-internal: the public constructor is `SourceId::new` over a
    /// gated path).
    pub(crate) fn from_hex(hex: String) -> Self {
        Self(hex)
    }

    /// The identity as lowercase hex.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for SourceId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SourceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The only token estimator this binary ships (docs-ingest-plan.md §3): `chars / 4`, no BPE
/// data; recorded on every chunk as `token_estimator`.
pub const TOKEN_ESTIMATOR: &str = "chars4";

/// The estimator name's type: a static string. Named so that serde does not treat the field as
/// a borrowed `&str` (which would force `'de: 'static` on [`ChunkProps`]); the value is always
/// [`TOKEN_ESTIMATOR`] and `deserialize_estimator` maps it back.
pub type TokenEstimator = &'static str;

/// Deserialize `token_estimator` back onto the one static estimator name, so [`ChunkProps`]
/// round-trips through JSON while keeping the plan's `&'static str` field (any other value is
/// a different pipeline's chunk and is refused).
fn deserialize_estimator<'de, D: Deserializer<'de>>(d: D) -> Result<&'static str, D::Error> {
    let name = String::deserialize(d)?;
    if name == TOKEN_ESTIMATOR {
        Ok(TOKEN_ESTIMATOR)
    } else {
        Err(serde::de::Error::custom("unknown token estimator"))
    }
}

/// Props of a `Document` node (docs-ingest-spec D2, D6, D8; docs-ingest-plan.md DI1.1). Every
/// string here is sanitized by the gate (DI1.4) before the struct is built: `path` passed the
/// path gate unchanged, `title`, the `frontmatter` string leaves and `error` are the masked
/// versions. Keys are the `snake_case` names DI0's skip decision reads (`content_hash`,
/// `blocked_hmac`, `pipeline_version`, `extractor_version`, `config_hash`); absent options and
/// empty maps are omitted (`serde-skip-empty`) and default on the way in (`serde-default-compat`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct DocumentProps {
    /// [`SourceId`] hex: stable across edits and versions.
    pub source_id: String,
    /// The gated relative path (DI0.2; a path with a hit is never stored).
    pub path: String,
    /// `blake3` of the canonical sanitized representation (title, masked frontmatter, section
    /// tree, chunk texts after masking or stubbing; `crate::content_hash`, RD1-1), never of the
    /// raw bytes; `None` for a blocked document.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_hash: Option<String>,
    /// Keyed identity of a blocked document's bytes (`keyed_id("bytes")`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_hmac: Option<String>,
    /// Keyed positional fingerprint of the sanitized layout (`HMAC(audit_key, section and
    /// chunk offsets, link and tag offsets)`, `docs/ids.md`): with `content_hash` it decides
    /// freshness, so a whitespace-only edit refreshes the stored offsets (RD2-4). `None` for
    /// blocked and failed documents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position_fingerprint: Option<String>,
    /// `0` in overwrite mode; the snapshot number with `--history` (D3).
    #[serde(default)]
    pub version: u32,
    /// Whether this node is the current version of its source.
    #[serde(default)]
    pub current: bool,
    /// Format chosen by the walker.
    pub format: Format,
    /// File size in bytes.
    #[serde(default)]
    pub size: u64,
    /// Modification time, Unix seconds UTC (not identity).
    #[serde(default)]
    pub mtime: i64,
    /// Sanitized title (frontmatter `title` or the first heading).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// ISO 639-3 code from `whatlang`, when detected reliably.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lang: Option<String>,
    /// Frontmatter as JSON (`BTreeMap`-ordered object, string leaves sanitized).
    #[serde(default)]
    pub frontmatter: Value,
    /// [`crate::PIPELINE_VERSION`] at write time.
    pub pipeline_version: u32,
    /// The extractor that produced the document (`crate@version`).
    #[serde(default)]
    pub extractor: String,
    /// Skip-key component (DI0.2): the extractor's `crate@version`.
    #[serde(default)]
    pub extractor_version: String,
    /// Skip-key component (DI0.2): the configuration digest.
    #[serde(default)]
    pub config_hash: String,
    /// Lifecycle state.
    pub status: DocStatus,
    /// Gated extractor error message (D8).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// A PDF without a text layer (D8).
    #[serde(default)]
    pub needs_ocr: bool,
    /// Page count for paged formats.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page_count: Option<u32>,
    /// Chunks replaced by a keyed stub (D6).
    #[serde(default)]
    pub blocked_chunks: u32,
    /// Text chunks flagged `table_unknown` (PDF rows that may be a table, OD-D6, RD3-4).
    /// Omitted when zero, so stored props of existing documents are unchanged.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub table_unknown_chunks: u32,
    /// Denylist rule id -> hits in this document (counts only, never matched text).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub rule_ids: BTreeMap<String, u32>,
}

/// `skip_serializing_if` predicate of the zero-default counters.
#[allow(clippy::trivially_copy_pass_by_ref)] // serde's predicate signature is `fn(&T) -> bool`
fn is_zero(n: &u32) -> bool {
    *n == 0
}

/// Props of a `Section` node (docs-ingest-spec D2). Positional (`order`, `byte_*`, `page_*`) and
/// never part of the section's id (`section_id` uses the heading path and ordinal).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct SectionProps {
    /// Heading texts from the top level down to this section (empty before the first heading).
    #[serde(default)]
    pub heading_path: Vec<String>,
    /// Heading level (`0` for the preamble, `1..=6` for Markdown headings, `1` for a page).
    pub level: u8,
    /// Position among the document's sections.
    pub order: u32,
    /// Byte range of the section in the extracted text.
    pub byte_start: u64,
    /// End of the byte range (exclusive).
    pub byte_end: u64,
    /// First page (paged formats).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page_start: Option<u32>,
    /// Last page (paged formats).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page_end: Option<u32>,
    /// What the section represents.
    pub kind: SectionKind,
}

/// Props of a `Chunk` node (docs-ingest-spec D2, D4, D6; §15 note 1 adds `bytes` and `slices`).
/// `text` is the sanitized (masked) text or empty for a stub; `text_hash` is the blake3 of that
/// sanitized text and the only text-derived input of `chunk_id`, so a masked chunk's id never
/// encodes the secret.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ChunkProps {
    /// Sanitized chunk text (empty for a stub).
    pub text: String,
    /// `blake3` hex of `text`.
    pub text_hash: String,
    /// `chars / 4` estimate of `text`.
    pub token_estimate: u32,
    /// Always [`TOKEN_ESTIMATOR`].
    #[serde(deserialize_with = "deserialize_estimator")]
    pub token_estimator: TokenEstimator,
    /// Position among the document's chunks.
    pub order: u32,
    /// Byte range in the extracted text (positional, not identity).
    pub byte_start: u64,
    /// End of the byte range (exclusive).
    pub byte_end: u64,
    /// Page for paged formats.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page: Option<u32>,
    /// `"<title> > <h1> > <h2>"` (plus the header row for a table), sanitized.
    #[serde(default)]
    pub context_prefix: String,
    /// Language of the chunk when detected (defaults to the document's).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lang: Option<String>,
    /// What the chunk holds.
    pub kind: ChunkKind,
    /// The chunk exceeds `hard_max_tokens` and was kept whole (code fence or table, D4).
    #[serde(default)]
    pub oversize: bool,
    /// PDF rows that may be a table (DI2.1, OD-D6).
    #[serde(default)]
    pub table_unknown: bool,
    /// `HMAC(audit_key, original_text)` of a stub (D6; core-spec S1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stub_hmac: Option<String>,
    /// UTF-8 length of `text` (spec D4 [A-2]).
    #[serde(default)]
    pub bytes: u32,
    /// Char-boundary slices a handle delivery needs at the default slice size (spec D4 [A-2]).
    #[serde(default)]
    pub slices: u16,
}

/// A `Document` node ready to write (docs-ingest-plan.md DI0.3 shape; built by DI1.4
/// `prepare_document` from sanitized values only).
#[derive(Debug, Clone, PartialEq)]
pub struct DocumentNode {
    /// `document_id(source)`.
    pub id: kaban_core::NodeId,
    /// The source identity.
    pub source: SourceId,
    /// Sanitized props.
    pub props: DocumentProps,
}

/// A `Chunk` node ready to write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkNode {
    /// `chunk_id(source, section_path, text_hash, ordinal_among_identical, pipeline_version)`.
    pub id: kaban_core::NodeId,
    /// The enclosing section's heading path.
    pub section_path: Vec<String>,
    /// Sanitized props (`text_hash` over the masked text).
    pub props: ChunkProps,
}

/// A `Section` node ready to write, with its chunks in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SectionNode {
    /// `section_id(source, heading_path, ordinal, pipeline_version)`.
    pub id: kaban_core::NodeId,
    /// Positional props.
    pub props: SectionProps,
    /// The section's chunks in document order.
    pub chunks: Vec<ChunkNode>,
}

/// Everything the batch builder (DI1.5) needs for one document that passed the gate: nodes
/// with ids assigned and the gated links and tags. `mentions` (engine E3.6) are not here.
#[derive(Debug, Clone, PartialEq)]
pub struct PreparedGraph {
    /// The Document node.
    pub document: DocumentNode,
    /// Sections with their chunks.
    pub sections: Vec<SectionNode>,
    /// Links with gated targets and aliases (rejecting ones dropped).
    pub links: Vec<crate::extract::LinkDraft>,
    /// Tags with gated names (rejecting ones dropped).
    pub tags: Vec<crate::extract::TagDraft>,
}

/// What one ingestion run did (docs-ingest-spec D7 `index_status`, D8). Counts only: no
/// path, title or text ever appears here.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IngestReport {
    /// Content-free gazetteer run counters, absent when linking is disabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gazetteer: Option<kaban_api::GazetteerTelemetry>,
    /// Documents committed (`ok` or `partial`).
    pub documents: u32,
    /// Candidates skipped before extraction (walker rules, unchanged skip key).
    pub skipped: u32,
    /// Documents recorded `failed` (D8), including timeouts.
    pub failed: u32,
    /// Documents recorded `blocked` by the denylist (D6).
    pub blocked: u32,
    /// Documents recorded `partial`.
    pub partial: u32,
    /// Chunks inserted or updated.
    pub chunks_upserted: u32,
    /// Chunks whose content hash matched (nothing written).
    pub chunks_unchanged: u32,
    /// Chunks tombstoned because they disappeared from their document.
    pub chunks_tombstoned: u32,
    /// Sections tombstoned because they disappeared from their document (DI1.8, RD2-8:
    /// counted apart from chunks).
    #[serde(default)]
    pub sections_tombstoned: u32,
    /// Documents `forget --document` marked forgotten that a directory or workspace walk left
    /// alone (DI1.8, RD2-5; an explicit file path or `--include-forgotten` reactivates them).
    #[serde(default)]
    pub skipped_forgotten: u32,
    /// Chunks replaced by a keyed stub (reject-class hit).
    pub blocked_chunks: u32,
    /// Denylist rule id -> number of hits (sorted map, N5).
    pub rule_ids: BTreeMap<String, u32>,
    /// Outbox rows written by the run.
    pub outbox_rows: u64,
    /// Wall-clock duration.
    pub elapsed_ms: u64,
    /// The run stopped early because its token was cancelled; everything counted above was
    /// committed whole (D-20: a token cancelled before the walk starts is `IngestError::Cancelled`).
    pub cancelled: bool,
    /// Documents whose extraction exceeded the per-document deadline (recorded `failed`).
    pub timed_out: u32,
    /// Timed-out documents whose non-cooperative extractor was still running when the run
    /// returned (its worker permit stays held until it returns, PR-8; DI2.2a, RD3-3). The
    /// adapter boundary ([`crate::run_blocking`]) does not wait for these threads; a non-zero
    /// count is logged once as a warning carrying the count only.
    #[serde(default)]
    pub timed_out_outstanding: u32,
}

/// Why an ingestion run failed as a whole. Per-document failures never surface here: they are
/// recorded on the `Document` node and counted in [`IngestReport`] (docs-ingest-spec D8).
/// Messages carry no path, title or content (core-spec S1, S3).
#[derive(Debug, Error)]
#[non_exhaustive] // later milestones add variants (extractors, watch); match with a fallback arm
pub enum IngestError {
    /// A store call failed.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// A service-layer call failed.
    #[error(transparent)]
    Api(#[from] ApiError),
    /// The policy could not be used.
    #[error(transparent)]
    Policy(#[from] PolicyError),
    /// An I/O failure on a path that already passed the denylist gate.
    #[error("i/o error on {path}: {source}")]
    Io {
        /// The gated (sanitized) relative path.
        path: String,
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },
    /// Every worker permit is held by a timed-out, non-cooperative extractor, so the run
    /// stopped dispatching (docs-ingest-plan.md DI0.3, PR-8). `partial` counts the documents
    /// committed before the stop (D-13).
    #[error("ingestion stopped: all {outstanding} worker(s) are held by timed-out extractors")]
    WorkersExhausted {
        /// Blocking tasks still holding a permit.
        outstanding: usize,
        /// What was committed before dispatch stopped.
        partial: Box<IngestReport>,
    },
    /// The cancellation token was already cancelled before the walk started (D-20); a token
    /// cancelled mid-run yields `Ok(IngestReport { cancelled: true, .. })` instead.
    #[error("ingestion cancelled before it started")]
    Cancelled,
    /// The ingestion configuration is unusable (for example a deny glob that does not compile).
    /// The message names configuration, never document content.
    #[error("invalid ingestion configuration: {0}")]
    Config(String),
    /// A pool task did not complete (a panic inside a worker or the committer); the run stops.
    #[error("ingestion internal error: {0}")]
    Internal(&'static str),
    /// A gate stage was cancelled or ran past the document deadline (DI1.4).
    #[error(transparent)]
    Extract(#[from] crate::cancel::ExtractError),
}
