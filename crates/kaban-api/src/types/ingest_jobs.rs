//! Canonical structured extractor wire types. API never depends on the ingestion runtime.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// Nonblocking manual extraction claim. No model is run by the server.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IngestClaimRequest {
    /// External extractor dialect.
    pub r#with: IngestDialect,
    /// Optional source type override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub r#type: Option<String>,
    /// Optional workspace-relative prefix; CONTENT, never a query language.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path_prefix: Option<String>,
    /// Worker identity; defaults to host:pid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker: Option<String>,
}

/// Live holder proof returned by an atomic claim.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IngestLease {
    /// Monotonic fence carried on next, submit and finish.
    pub fence: u64,
    /// Lease expiry in database epoch seconds.
    pub expires_at: i64,
}

/// An available job and gated brief, or precisely an idle null handle.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IngestClaimResponse {
    /// Null when the bounded scan found no available source.
    pub job: Option<String>,
    /// Present only when a lease was acquired.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub brief: Option<IngestBrief>,
    /// Present only when a lease was acquired.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease: Option<IngestLease>,
}

/// Fixed placement outcomes; governed revisions are candidates, never acceptance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum IngestRecordOutcome {
    /// A new record was committed.
    Created,
    /// Proposed record content changed.
    Updated,
    /// Identical content wrote no graph change.
    Unchanged,
    /// New proposed content for an immutable predecessor.
    RevisionCandidate,
}

/// Closed report skip vocabulary. References never convey foreign target information.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum IngestSkipReason {
    /// Invalid non-internal properties or a required field.
    InvalidProps,
    /// Kind outside the pinned source schema.
    KindNotAllowed,
    /// Forbidden or incompatible relationship.
    EdgeNotAllowed,
    /// Both occurrences of a duplicate local key are skipped.
    DuplicateLocalId,
    /// An existing hidden identity cannot be revived.
    HiddenExists,
    /// An existing identity has another kind.
    KindMismatch,
    /// A local reference is absent or ambiguous.
    UnknownRef,
    /// Foreign, missing and hidden external targets share this reason.
    UnknownTarget,
    /// Payload exceeds its ceiling.
    TooLarge,
    /// Pair was not requested by the authoritative brief.
    NotRequested,
    /// Duplicate requested pair judgement.
    Duplicate,
}

/// Closed evidence drop vocabulary; invalid evidence does not discard the record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum IngestEvidenceReason {
    /// Chunk not in the complete scoped job plan.
    ChunkNotInJob,
    /// Quote absent from the current sanitized chunk.
    QuoteNotFound,
}

/// Closed persistent job status on report responses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum IngestReportStatus {
    /// Further submits remain possible.
    Open,
    /// Final report is frozen.
    Finished,
    /// Original source or ontology contract changed.
    Stale,
    /// Job reached its bounded expiry.
    Expired,
}

/// One successfully committed record.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IngestPlacedRecord {
    /// Gated extractor-local key, not stored in accumulation.
    pub local_id: String,
    /// Committed record identity.
    pub id: String,
    /// Actual placement result.
    pub outcome: IngestRecordOutcome,
}

/// Acceptance-safe candidate and its immutable predecessor.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IngestRevisionCandidate {
    /// Gated extractor-local key.
    pub local_id: String,
    /// New proposed record identity.
    pub id: String,
    /// Accepted, superseded or invalidated predecessor identity.
    pub of: String,
}

/// One fixed conversion refusal, with an optional value-free property pointer.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IngestSkipped {
    /// Gated local reference, empty for a non-record item.
    pub local_id: String,
    /// Fixed reason.
    pub reason: IngestSkipReason,
    /// Fixed field or ontology-validator pointer; never a caller value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
}

/// One discarded evidence reference.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IngestEvidenceDropped {
    /// Gated local record key.
    pub local_id: String,
    /// Fixed evidence refusal.
    pub reason: IngestEvidenceReason,
}

/// Actual graph `ChangeLog` sequence range of publication.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IngestChanges {
    /// First sequence, absent for a graph no-op.
    pub first_seq: Option<u64>,
    /// Last sequence, absent for a graph no-op.
    pub last_seq: Option<u64>,
}

/// Workspace-scoped finalization request. A handle alone confers no authority.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IngestFinishRequest {
    /// Persistent ingestion handle.
    pub job: String,
    /// Monotonic holder proof, required only for claimed jobs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fence: Option<u64>,
}

/// Complete reconciliation or a fail-safe refusal to delete beyond the cap.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum IngestReconcileOutcome {
    /// Complete scoped population reconciled.
    Complete,
    /// Complete population exceeded its bound; no retirement or deletion occurred.
    SkippedCap,
}

/// Canonical retained finish report: fixed counts and identifiers, never source text.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IngestFinishReport {
    /// Persistent scoped handle.
    pub job: String,
    /// Always finished on success.
    pub status: IngestReportStatus,
    /// Successful submit count.
    pub submits: u64,
    /// Accumulated created records.
    pub created: u64,
    /// Accumulated updated records.
    pub updated: u64,
    /// Accumulated unchanged records.
    pub unchanged: u64,
    /// Accumulated successful redactions.
    pub redactions: u64,
    /// Accumulated named edges.
    pub edges: u64,
    /// Accumulated newly inserted assertions.
    pub assertions: u64,
    /// Proposed revisions, never acceptance.
    pub revision_candidates: u64,
    /// Revisions of terminal predecessors.
    pub predecessor_terminal: u64,
    /// Fixed skip reasons and counts.
    pub skipped: BTreeMap<String, u64>,
    /// Fixed discarded evidence reasons and counts.
    pub evidence_dropped: BTreeMap<String, u64>,
    /// Final reconciliation outcome.
    pub reconcile: IngestReconcileOutcome,
    /// Machine claims permanently retired.
    pub retired_assertions: u64,
    /// Omitted owned graph edges deleted.
    pub deleted_edges: u64,
    /// Proposed omitted records; informational, never tombstoned.
    pub orphans: Vec<String>,
    /// Actual first/last graph sequence across successful submits and reconciliation.
    pub changes: IngestChanges,
    /// The finish's receipt (spec 05 SS-10), with `changes.last_seq` as its position; absent
    /// only on a report retained before receipts existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receipt: Option<super::WriteReceipt>,
}

/// Canonical EN-F39 structured submit report, containing only gated references and counts.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IngestSubmitReport {
    /// Workspace-scoped persistent job handle.
    pub job: String,
    /// Persistent job status.
    pub status: IngestReportStatus,
    /// Optional caller chunk selector.
    pub chunk: Option<u32>,
    /// Committed records and actual outcomes.
    pub records: Vec<IngestPlacedRecord>,
    /// Proposed revisions of immutable predecessors.
    pub revision_candidates: Vec<IngestRevisionCandidate>,
    /// Candidates whose predecessor was superseded or invalidated.
    pub predecessor_terminal: u64,
    /// Fixed conversion refusals.
    pub skipped: Vec<IngestSkipped>,
    /// Evidence references discarded independently of record placement.
    pub evidence_dropped: Vec<IngestEvidenceDropped>,
    /// Successful redactions audited atomically with this submit.
    pub redactions: u64,
    /// Actual new records.
    pub created: u32,
    /// Actual changed records.
    pub updated: u32,
    /// Actual unchanged records.
    pub unchanged: u32,
    /// Successfully named edge triples.
    pub edges: u64,
    /// Actual newly inserted assertion rows.
    pub assertions: u32,
    /// Actual graph sequence range.
    pub changes: IngestChanges,
}

/// Extractor dialect: instructions only, never model execution.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum IngestDialect {
    /// Host-agent / grammar-constrained JSON dialect.
    Llm,
    /// User-owned labeled-span adapter dialect.
    Gliner,
    /// Generic manual instructions.
    #[default]
    None,
}

impl IngestDialect {
    /// Frozen wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Llm => "llm",
            Self::Gliner => "gliner",
            Self::None => "none",
        }
    }
}

/// One quote from a chunk in this job; gates and membership checks precede publication.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EvidenceRef {
    /// Job-plan chunk node id.
    pub chunk_id: String,
    /// Verbatim text, never persisted in the job registry.
    pub quote: String,
}

/// Folded §4.1 record payload. No synthesized title or identity field.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RecordDraft {
    /// Stable extractor-local key, 1..128 trimmed characters.
    pub local_id: String,
    /// One allowed kind in the brief's ontology snapshot.
    pub kind: String,
    /// Required nonempty record text, independently gated before placement.
    pub text: String,
    /// Optional summary; absent means absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Optional source-supported tags.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Optional confidence, never acceptance evidence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f64>,
    /// Ontology-restricted properties, with all internal properties excluded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub props: Option<Value>,
    /// Optional source-supported path globs.
    #[serde(default)]
    pub path_scope: Vec<String>,
    /// Optional existing symbol references; never discovered by executing an extractor.
    #[serde(default)]
    pub symbols: Vec<String>,
    /// Optional direct quotes from job chunks.
    #[serde(default)]
    pub evidence: Vec<EvidenceRef>,
}

/// Record edge: exactly one destination selector is required by runtime validation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EdgeDraft {
    /// Local record source key in this submit.
    pub from_local_id: String,
    /// Ontology-permitted relationship; never automatic acceptance.
    pub rel: String,
    /// Destination local record key, mutually exclusive with `to_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_local_id: Option<String>,
    /// Workspace-resolved existing node, mutually exclusive with `to_local_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_id: Option<String>,
}

/// Existing-node subset of `EdgeDraft`, for AM-3 candidate reuse.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LinkDraft {
    /// Local record source key.
    pub from_local_id: String,
    /// Ontology-permitted relationship.
    pub rel: String,
    /// Existing candidate node id; resolves only in the job workspace.
    pub to_id: String,
}

/// Exact counterpart of the core ER pair reference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PairRef {
    /// First requested node.
    pub a: String,
    /// Second requested node.
    pub b: String,
}

/// Exact counterpart of core ER judgement; no invented confidence or authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PairJudgement {
    /// Pair requested by this brief.
    pub pair: PairRef,
    /// Whether the external judge considers it one entity.
    pub same: bool,
}

impl From<PairJudgement> for kaban_core::PairJudgement {
    fn from(value: PairJudgement) -> Self {
        Self {
            pair: kaban_core::PairRef::new(&value.pair.a, &value.pair.b),
            same: value.same,
        }
    }
}

/// Canonical `ingest_submit` body, also the sole source of extractor JSON Schema.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StructuredReturn {
    /// Workspace-scoped deterministic handle; never a capability.
    pub job: String,
    /// Monotonic holder proof, required only for claimed jobs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fence: Option<u64>,
    /// Optional source chunk number.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chunk: Option<u32>,
    /// Records supported by the source text.
    #[serde(default)]
    pub records: Vec<RecordDraft>,
    /// Record-to-record or existing-node relationships.
    #[serde(default)]
    pub edges: Vec<EdgeDraft>,
    /// AM-3 links to existing candidates.
    #[serde(default)]
    pub links: Vec<LinkDraft>,
    /// Existing candidate node ids, not new records.
    #[serde(default)]
    pub about: Vec<String>,
    /// External pair judgements, normal-rank only; requested pairs only.
    #[serde(default)]
    pub pair_judgements: Vec<PairJudgement>,
}

/// Canonical request name; no separate schema or duplicate payload definition.
pub type IngestSubmitRequest = StructuredReturn;

/// Canonical extractor payload schema, before ontology-specific kind/property restriction.
#[must_use]
pub fn structured_return_schema() -> Value {
    schemars::schema_for!(StructuredReturn).to_value()
}

/// One delivered slice of a logical source chunk. Offsets retain the logical source range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum IngestChunkKind {
    /// Prose.
    Text,
    /// Fenced code, preserving one logical chunk.
    Code,
    /// Table.
    Table,
    /// Keyed policy stub with no source text.
    Stub,
}

/// One delivered slice of a logical source chunk. Offsets retain the logical source range.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ChunkView {
    /// Source-order logical chunk number.
    pub n: u32,
    /// Existing document-ingestion chunk identity.
    pub id: String,
    /// Closed document-ingestion chunk kind.
    pub kind: IngestChunkKind,
    /// Sanitized heading path.
    pub section_path: Vec<String>,
    /// Sanitized explanatory prefix; not included in text offsets.
    pub context_prefix: String,
    /// Exact UTF-8 slice of the sanitized chunk text.
    pub text: String,
    /// Inclusive logical chunk start in the extracted source, unchanged across slices.
    pub byte_start: u64,
    /// Exclusive logical chunk end in the extracted source, unchanged across slices.
    pub byte_end: u64,
    /// Zero-based delivery slice number.
    pub slice: u32,
    /// Number of delivery slices in this logical chunk.
    pub slices: u32,
}

/// Resume position, or null when the requested document portion is exhausted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ChunkContinuation {
    /// Logical chunk number.
    pub n: u32,
    /// Zero-based delivery slice number.
    pub slice: u32,
}

/// Folded AI-F7 single-chunk next response, including all serialized overhead.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ChunkPage {
    /// Workspace-scoped job handle.
    pub job: String,
    /// One logical chunk slice.
    pub chunk: ChunkView,
    /// Resume position; null at document end.
    pub next: Option<ChunkContinuation>,
}

/// Consecutive whole chunks from an exclusive-ended range, bounded by serialized bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ChunkRangePage {
    /// Workspace-scoped job handle.
    pub job: String,
    /// Consecutive whole logical chunks, never partial chunks in range mode.
    pub chunks: Vec<ChunkView>,
    /// First logical chunk that did not fit, or null when the requested range is complete.
    pub next: Option<ChunkContinuation>,
}

/// Canonical next request: exactly one of `n` or `chunks` is validated by the handler.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IngestNextRequest {
    /// Workspace-scoped canonical 64-hex handle.
    pub job: String,
    /// Monotonic holder proof, required only for claimed jobs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fence: Option<u64>,
    /// Logical source-order chunk number, mutually exclusive with `chunks`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub n: Option<u32>,
    /// Zero-based delivery slice; valid only together with `n`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slice: Option<u32>,
    /// Exclusive-ended logical chunk range in `a..b` spelling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chunks: Option<String>,
}

/// Object-root next transport response. Exactly one chunk representation is published.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IngestNextResponse {
    /// Workspace-scoped handle.
    pub job: String,
    /// Single-chunk delivery slice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chunk: Option<ChunkView>,
    /// Consecutive whole chunks in range mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chunks: Option<Vec<ChunkView>>,
    /// First not-yet-delivered position, or null on completion.
    pub next: Option<ChunkContinuation>,
}

impl From<ChunkPage> for IngestNextResponse {
    fn from(page: ChunkPage) -> Self {
        Self {
            job: page.job,
            chunk: Some(page.chunk),
            chunks: None,
            next: page.next,
        }
    }
}

impl From<ChunkRangePage> for IngestNextResponse {
    fn from(page: ChunkRangePage) -> Self {
        Self {
            job: page.job,
            chunk: None,
            chunks: Some(page.chunks),
            next: page.next,
        }
    }
}

/// One ingestion verb's preparation mode.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum IngestMode {
    /// Existing document ingestion, without a structured extraction job.
    Plain,
    /// Publish a brief for a user-owned extractor.
    #[default]
    Structured,
}

/// Folded brief document view; no source bytes are persisted with the job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IngestDocumentView {
    /// Existing document-ingestion identity.
    pub id: String,
    /// Current-policy admitted workspace-relative path.
    pub path: String,
    /// Closed document format spelling.
    pub format: String,
    /// Walked source byte length.
    pub size: u64,
    /// Sanitized document content identity.
    pub content_hash: String,
    /// Sanitized title, or null when absent.
    pub title: Option<String>,
    /// Sanitized language, or null when absent.
    pub lang: Option<String>,
    /// True only after actual document/chunk publication has been verified.
    pub indexed: bool,
}

/// Advisory source type; never supplies record fields or acceptance evidence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TypeCandidate {
    /// Source-type spelling from the deterministic signal selector.
    pub r#type: String,
    /// Bounded heuristic score, not model confidence.
    pub confidence: f64,
    /// Advisory signal descriptions, possibly empty.
    pub signals: Vec<String>,
}

/// Source-type selection and advisory candidates.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TypeSuggestion {
    /// Source-order independent deterministic candidates.
    pub candidates: Vec<TypeCandidate>,
    /// Explicit or suggested source type used for kind selection.
    pub chosen: String,
    /// Advisory warning/explanation; never an acceptance promise.
    pub note: String,
}

/// Ontology-derived extractor contract, frozen in the job envelope.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TargetSchema {
    /// Enabled allowed kind names from loaded ontology mappings.
    pub kinds: Vec<String>,
    /// Canonical extractor schema projected for these kinds.
    pub json_schema: Value,
    /// Required extractor fields per kind, including dotted props names.
    pub required: BTreeMap<String, Vec<String>>,
    /// Absence and quotation semantics.
    pub semantics: String,
    /// Explicit ontology-permitted source/relation/target triples.
    pub allowed_edges: Vec<[String; 3]>,
    /// Provenance edges added by kaban, never acceptance.
    pub kaban_adds: Vec<[String; 3]>,
    /// Governed records remain proposed.
    pub governance: String,
}

/// Published ingestion bounds, including the full serialized result ceiling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IngestLimits {
    /// Maximum submitted JSON body size.
    pub submit_bytes: usize,
    /// Maximum record props JSON size.
    pub props_bytes: usize,
    /// Maximum records in one submit.
    pub records_per_submit: usize,
    /// Maximum complete serialized brief/next response size.
    pub result_bytes: usize,
}

/// Static workflow advice and bounds; document content is always data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HandlingPlan {
    /// Total logical chunk count.
    pub chunks: usize,
    /// Incremental delivery instructions.
    pub deliver: String,
    /// Stable-local-id submit instructions.
    pub submit: String,
    /// Published byte/record ceilings.
    pub limits: IngestLimits,
    /// Current-policy-gated dialect and governance instructions.
    pub instructions: Vec<String>,
}

/// Metadata-only entry in a byte-bounded brief plan page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ChunkRef {
    /// Logical source-order number.
    pub n: u32,
    /// Existing chunk identity.
    pub id: String,
    /// Closed chunk kind spelling.
    pub kind: IngestChunkKind,
    /// Sanitized text byte length, not source range length.
    pub bytes: usize,
    /// Logical source range start.
    pub byte_start: u64,
    /// Logical source range end.
    pub byte_end: u64,
    /// Existing bounded token estimate.
    pub token_estimate: u32,
    /// Logical chunk exceeds normal target size.
    pub oversize: bool,
    /// Delivery slices after measuring actual serialized bytes.
    pub slices: u32,
}

/// Byte-bounded chunk-plan page; no entry-count-only cap.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ChunkPlan {
    /// Zero-based plan page.
    pub page: u32,
    /// Total plan pages.
    pub pages: u32,
    /// Consecutive source-order entries on this page.
    pub chunks: Vec<ChunkRef>,
}

/// Suggested explicit follow-up tool call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IngestNextCall {
    /// Closed ingestion tool name, not an executable.
    pub tool: String,
    /// Typed-control-validated follow-up arguments.
    pub args: IngestNextRequest,
}

/// Folded §4.1 brief shape; canonical API-owned wire type consumed by the runtime.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IngestBrief {
    /// Workspace-scoped deterministic handle, not a capability.
    pub job: String,
    /// Requested preparation mode.
    pub mode: IngestMode,
    /// Explicit extractor instruction dialect.
    pub r#with: IngestDialect,
    /// Prepared document placement result.
    pub document: IngestDocumentView,
    /// Advisory type selection.
    pub suggestion: TypeSuggestion,
    /// Frozen ontology-derived extractor schema.
    pub target_schema: TargetSchema,
    /// Static workflow advice and bounds.
    pub handling_plan: HandlingPlan,
    /// One byte-bounded logical chunk-plan page.
    pub chunk_plan: ChunkPlan,
    /// Dialect-specific labeled-span/grammar guidance, never an execution request.
    pub extractor_hints: Value,
    /// Candidate-only gazetteer matches; structured preparation writes no mention edges.
    #[serde(default)]
    pub candidates: Vec<IngestCandidate>,
    /// Existing candidate identities suitable for explicit about references.
    #[serde(default)]
    pub about_candidates: Vec<AboutCandidate>,
    /// Explicit uncertain pairs, never inferred acceptance.
    #[serde(default)]
    pub uncertain_pairs: Vec<PairRef>,
    /// Suggested explicit continuation tool calls.
    pub next: Vec<IngestNextCall>,
}

/// Aggregate or document-title match to an existing node, never an automatic ABOUT edge.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AboutCandidate {
    /// Existing same-workspace target identity.
    pub id: String,
    /// Loaded target kind; content, not a control.
    pub kind: String,
    /// Advisory matcher score, never acceptance evidence.
    pub score: f64,
}

/// One gated known-entity match, never permission to create an edge.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IngestCandidate {
    /// Evidence chunk identity.
    pub chunk: String,
    /// Existing same-workspace target.
    pub id: String,
    /// Loaded target kind.
    pub kind: String,
    /// Sanitized title used only to avoid duplicates.
    pub title: String,
    /// Closed core matcher method spelling.
    pub method: String,
    /// Match range start in sanitized text.
    pub byte_start: u64,
    /// Match range end in sanitized text.
    pub byte_end: u64,
    /// Advisory matcher score.
    pub score: f64,
    /// Ambiguous match stays candidate-only.
    pub ambiguous: bool,
}

/// Single-source brief request, shared by CLI and MCP.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IngestBriefRequest {
    /// Workspace-relative document path; content, never a capability.
    pub path: String,
    /// Preparation mode.
    #[serde(default)]
    pub mode: IngestMode,
    /// External instruction dialect.
    #[serde(default)]
    pub r#with: IngestDialect,
    /// Explicit linking mode, normally none.
    #[serde(default = "no_link")]
    pub link: String,
    /// Explicit symbol budget override.
    #[serde(default)]
    pub symbols: bool,
    /// Optional advisory source type; unknown falls back with a warning.
    #[serde(default)]
    pub r#type: Option<String>,
    /// Zero-based byte-bounded chunk-plan page.
    #[serde(default)]
    pub plan_page: u32,
    /// Preview only, no persistence or pruning.
    #[serde(default)]
    pub dry_run: bool,
}

fn no_link() -> String {
    "none".into()
}
