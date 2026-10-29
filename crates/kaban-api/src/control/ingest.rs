//! The docs-ingest tools' schemas (`index_docs`, `index_status`; they declare no control rows)
//! and the reserved `embed_submit` shape (engine-spec EN-F50, EN-E6; engine-plan E2.11): its
//! rows and schemas are declared here so the completeness checks cover them before E3.7 fills
//! the handler; the adapter does not list it until then (`super::RESERVED`).

use super::{ControlEntry, Direction, Empty, Shape, ToolSchemas, entry};
use crate::ingest_jobs::{
    IngestBrief, IngestBriefRequest, IngestClaimRequest, IngestClaimResponse, IngestFinishReport,
    IngestFinishRequest, IngestNextRequest, IngestNextResponse, IngestSubmitReport,
    IngestSubmitRequest,
};
use crate::types::{IndexDocsRequest, IndexDocsResponse, IndexStatus};
use schemars::JsonSchema;

/// `embed_submit.model.dtype` wire values (EN-E7 `dtype = int8|f32`).
pub const DTYPE_VALUES: &[&str] = &["int8", "f32"];

/// `embed_submit.model.dims` bounds (EN-E6 `Int{1..=4096}`).
pub const DIMS: Shape = Shape::Int { min: 1, max: 4096 };

/// The element type of a submitted vector (EN-E7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
#[allow(dead_code)] // schema only until E3.7
enum EmbedDtype {
    /// Signed 8-bit quantised components.
    Int8,
    /// 32-bit float components.
    F32,
}

/// The model that produced the vectors (EN-F? E3.7 fills the handler).
#[derive(Debug, serde::Deserialize, JsonSchema)]
#[allow(dead_code)] // schema only until E3.7
struct EmbedModel {
    /// Model name as the producer spells it (content, RE-12: never a shape).
    name: String,
    /// Vector length, 1 to 4096.
    dims: u32,
    /// Component type.
    dtype: EmbedDtype,
    /// Whether the vectors are L2-normalised.
    normalize: bool,
    /// The producer's revision: exactly 40 lowercase hex digits.
    commit: String,
}

/// One submitted vector.
#[derive(Debug, serde::Deserialize, JsonSchema)]
#[allow(dead_code)] // schema only until E3.7
struct EmbedRow {
    /// The node the vector describes (a node id: control by shape).
    node_rid: String,
    /// The components, `model.dims` of them.
    vector: Vec<f64>,
}

/// `embed_submit` input (reserved).
#[derive(Debug, serde::Deserialize, JsonSchema)]
#[allow(dead_code)] // schema only until E3.7
struct EmbedSubmitRequest {
    /// The producing model.
    model: EmbedModel,
    /// The vectors.
    rows: Vec<EmbedRow>,
}

/// One row the handler refused.
#[derive(Debug, serde::Serialize, JsonSchema)]
#[allow(dead_code)] // schema only until E3.7
struct EmbedRejected {
    /// The node named by the refused row.
    node_rid: String,
    /// The fixed reason word (content).
    reason: String,
}

/// `embed_submit` output (reserved).
#[derive(Debug, serde::Serialize, JsonSchema)]
#[allow(dead_code)] // schema only until E3.7
struct EmbedSubmitResponse {
    /// Rows stored.
    accepted: u64,
    /// Rows refused, each with its reason.
    rejected: Vec<EmbedRejected>,
}

/// The reserved `embed_submit` rows of [`super::REGISTRY`] (EN-E6): `model.name` is content.
pub(super) const ROWS: &[ControlEntry] = &[
    entry(
        "ingest_claim",
        Direction::Request,
        "with",
        Shape::Enum(&["llm", "gliner", "none"]),
    ),
    entry(
        "ingest_claim",
        Direction::Response,
        "job",
        Shape::Hex { min: 64, max: 64 },
    ),
    entry(
        "ingest_claim",
        Direction::Response,
        "brief.job",
        Shape::Hex { min: 64, max: 64 },
    ),
    entry(
        "ingest_claim",
        Direction::Response,
        "brief.mode",
        Shape::Enum(&["plain", "structured"]),
    ),
    entry(
        "ingest_claim",
        Direction::Response,
        "brief.with",
        Shape::Enum(&["llm", "gliner", "none"]),
    ),
    entry(
        "ingest_claim",
        Direction::Response,
        "brief.document.id",
        Shape::NodeId,
    ),
    entry(
        "ingest_claim",
        Direction::Response,
        "brief.document.content_hash",
        Shape::Hex { min: 64, max: 64 },
    ),
    entry(
        "ingest_claim",
        Direction::Response,
        "brief.document.format",
        Shape::Enum(&[
            "markdown", "text", "pdf", "html", "csv", "sheet", "docx", "code",
        ]),
    ),
    entry(
        "ingest_claim",
        Direction::Response,
        "brief.chunk_plan.chunks[].id",
        Shape::NodeId,
    ),
    entry(
        "ingest_claim",
        Direction::Response,
        "brief.chunk_plan.chunks[].kind",
        Shape::Enum(&["text", "code", "table", "stub"]),
    ),
    entry(
        "ingest_claim",
        Direction::Response,
        "brief.candidates[].id",
        Shape::NodeId,
    ),
    entry(
        "ingest_claim",
        Direction::Response,
        "brief.candidates[].chunk",
        Shape::NodeId,
    ),
    entry(
        "ingest_claim",
        Direction::Response,
        "brief.candidates[].method",
        Shape::Enum(&["exact", "alias", "link", "tag", "path", "symbol", "id"]),
    ),
    entry(
        "ingest_claim",
        Direction::Response,
        "brief.about_candidates[].id",
        Shape::NodeId,
    ),
    entry(
        "ingest_claim",
        Direction::Response,
        "brief.uncertain_pairs[].a",
        Shape::NodeId,
    ),
    entry(
        "ingest_claim",
        Direction::Response,
        "brief.uncertain_pairs[].b",
        Shape::NodeId,
    ),
    entry(
        "ingest_claim",
        Direction::Response,
        "brief.next[].tool",
        Shape::Enum(&["ingest_next", "ingest_brief"]),
    ),
    entry(
        "ingest_claim",
        Direction::Response,
        "brief.next[].args.job",
        Shape::Hex { min: 64, max: 64 },
    ),
    entry(
        "ingest_next",
        Direction::Request,
        "fence",
        Shape::Int {
            min: 1,
            max: i64::MAX as u64,
        },
    ),
    entry(
        "ingest_submit",
        Direction::Request,
        "fence",
        Shape::Int {
            min: 1,
            max: i64::MAX as u64,
        },
    ),
    entry(
        "ingest_finish",
        Direction::Request,
        "fence",
        Shape::Int {
            min: 1,
            max: i64::MAX as u64,
        },
    ),
    entry(
        "ingest_finish",
        Direction::Request,
        "job",
        Shape::Hex { min: 64, max: 64 },
    ),
    entry(
        "ingest_finish",
        Direction::Response,
        "job",
        Shape::Hex { min: 64, max: 64 },
    ),
    entry(
        "ingest_finish",
        Direction::Response,
        "status",
        Shape::Enum(&["open", "finished", "stale", "expired"]),
    ),
    entry(
        "ingest_finish",
        Direction::Response,
        "reconcile",
        Shape::Enum(&["complete", "skipped_cap"]),
    ),
    entry(
        "ingest_finish",
        Direction::Response,
        "orphans[]",
        Shape::NodeId,
    ),
    // spec 05 SS-10: the finish's receipt (identities are UUIDs, SS-27)
    entry(
        "ingest_finish",
        Direction::Response,
        "receipt.outcome",
        Shape::Enum(super::memory::WRITE_OUTCOMES),
    ),
    entry(
        "ingest_finish",
        Direction::Response,
        "receipt.instance_id",
        Shape::NodeId,
    ),
    entry(
        "ingest_finish",
        Direction::Response,
        "receipt.pack_lineage_id",
        Shape::NodeId,
    ),
    // spec 05 SS-10: the run's receipt (one barrier per ingest run; identities are UUIDs)
    entry(
        "index_docs",
        Direction::Response,
        "receipt.outcome",
        Shape::Enum(super::memory::WRITE_OUTCOMES),
    ),
    entry(
        "index_docs",
        Direction::Response,
        "receipt.instance_id",
        Shape::NodeId,
    ),
    entry(
        "index_docs",
        Direction::Response,
        "receipt.pack_lineage_id",
        Shape::NodeId,
    ),
    entry(
        "ingest_submit",
        Direction::Request,
        "job",
        Shape::Hex { min: 64, max: 64 },
    ),
    entry(
        "ingest_submit",
        Direction::Request,
        "records[].evidence[].chunk_id",
        Shape::NodeId,
    ),
    entry(
        "ingest_submit",
        Direction::Request,
        "edges[].to_id",
        Shape::NodeId,
    ),
    entry(
        "ingest_submit",
        Direction::Request,
        "links[].to_id",
        Shape::NodeId,
    ),
    entry(
        "ingest_submit",
        Direction::Request,
        "about[]",
        Shape::NodeId,
    ),
    entry(
        "ingest_submit",
        Direction::Request,
        "pair_judgements[].pair.a",
        Shape::NodeId,
    ),
    entry(
        "ingest_submit",
        Direction::Request,
        "pair_judgements[].pair.b",
        Shape::NodeId,
    ),
    entry(
        "ingest_submit",
        Direction::Response,
        "job",
        Shape::Hex { min: 64, max: 64 },
    ),
    entry(
        "ingest_submit",
        Direction::Response,
        "status",
        Shape::Enum(&["open", "finished", "stale", "expired"]),
    ),
    entry(
        "ingest_submit",
        Direction::Response,
        "records[].id",
        Shape::NodeId,
    ),
    entry(
        "ingest_submit",
        Direction::Response,
        "records[].outcome",
        Shape::Enum(&["created", "updated", "unchanged", "revision_candidate"]),
    ),
    entry(
        "ingest_submit",
        Direction::Response,
        "revision_candidates[].id",
        Shape::NodeId,
    ),
    entry(
        "ingest_submit",
        Direction::Response,
        "revision_candidates[].of",
        Shape::NodeId,
    ),
    entry(
        "ingest_submit",
        Direction::Response,
        "skipped[].reason",
        Shape::Enum(&[
            "invalid_props",
            "kind_not_allowed",
            "edge_not_allowed",
            "duplicate_local_id",
            "hidden_exists",
            "kind_mismatch",
            "unknown_ref",
            "unknown_target",
            "too_large",
            "not_requested",
            "duplicate",
        ]),
    ),
    entry(
        "ingest_submit",
        Direction::Response,
        "evidence_dropped[].reason",
        Shape::Enum(&["chunk_not_in_job", "quote_not_found"]),
    ),
    entry(
        "ingest_brief",
        Direction::Request,
        "mode",
        Shape::Enum(&["plain", "structured"]),
    ),
    entry(
        "ingest_brief",
        Direction::Request,
        "with",
        Shape::Enum(&["llm", "gliner", "none"]),
    ),
    entry(
        "ingest_brief",
        Direction::Request,
        "link",
        Shape::Enum(&["none", "gazetteer"]),
    ),
    entry(
        "ingest_brief",
        Direction::Response,
        "job",
        Shape::Hex { min: 64, max: 64 },
    ),
    entry(
        "ingest_brief",
        Direction::Response,
        "mode",
        Shape::Enum(&["plain", "structured"]),
    ),
    entry(
        "ingest_brief",
        Direction::Response,
        "with",
        Shape::Enum(&["llm", "gliner", "none"]),
    ),
    entry(
        "ingest_brief",
        Direction::Response,
        "document.id",
        Shape::NodeId,
    ),
    entry(
        "ingest_brief",
        Direction::Response,
        "document.content_hash",
        Shape::Hex { min: 64, max: 64 },
    ),
    entry(
        "ingest_brief",
        Direction::Response,
        "document.format",
        Shape::Enum(&[
            "markdown", "text", "pdf", "html", "csv", "sheet", "docx", "code",
        ]),
    ),
    entry(
        "ingest_brief",
        Direction::Response,
        "chunk_plan.chunks[].id",
        Shape::NodeId,
    ),
    entry(
        "ingest_brief",
        Direction::Response,
        "chunk_plan.chunks[].kind",
        Shape::Enum(&["text", "code", "table", "stub"]),
    ),
    entry(
        "ingest_brief",
        Direction::Response,
        "candidates[].id",
        Shape::NodeId,
    ),
    entry(
        "ingest_brief",
        Direction::Response,
        "candidates[].chunk",
        Shape::NodeId,
    ),
    entry(
        "ingest_brief",
        Direction::Response,
        "candidates[].method",
        Shape::Enum(&["exact", "alias", "link", "tag", "path", "symbol", "id"]),
    ),
    entry(
        "ingest_brief",
        Direction::Response,
        "about_candidates[].id",
        Shape::NodeId,
    ),
    entry(
        "ingest_brief",
        Direction::Response,
        "uncertain_pairs[].a",
        Shape::NodeId,
    ),
    entry(
        "ingest_brief",
        Direction::Response,
        "uncertain_pairs[].b",
        Shape::NodeId,
    ),
    entry(
        "ingest_brief",
        Direction::Response,
        "next[].tool",
        Shape::Enum(&["ingest_next", "ingest_brief"]),
    ),
    entry(
        "ingest_brief",
        Direction::Response,
        "next[].args.job",
        Shape::Hex { min: 64, max: 64 },
    ),
    entry(
        "ingest_next",
        Direction::Request,
        "job",
        Shape::Hex { min: 64, max: 64 },
    ),
    entry(
        "ingest_next",
        Direction::Response,
        "job",
        Shape::Hex { min: 64, max: 64 },
    ),
    entry(
        "ingest_next",
        Direction::Response,
        "chunk.id",
        Shape::NodeId,
    ),
    entry(
        "ingest_next",
        Direction::Response,
        "chunks[].id",
        Shape::NodeId,
    ),
    entry(
        "ingest_next",
        Direction::Response,
        "chunk.kind",
        Shape::Enum(&["text", "code", "table", "stub"]),
    ),
    entry(
        "ingest_next",
        Direction::Response,
        "chunks[].kind",
        Shape::Enum(&["text", "code", "table", "stub"]),
    ),
    entry(
        "index_docs",
        Direction::Request,
        "link",
        Shape::Enum(&["none", "gazetteer"]),
    ),
    entry("embed_submit", Direction::Request, "model.dims", DIMS),
    entry(
        "embed_submit",
        Direction::Request,
        "model.dtype",
        Shape::Enum(DTYPE_VALUES),
    ),
    entry(
        "embed_submit",
        Direction::Request,
        "model.commit",
        Shape::Hex { min: 40, max: 40 },
    ),
    entry(
        "embed_submit",
        Direction::Request,
        "rows[].node_rid",
        Shape::NodeId,
    ),
    entry(
        "embed_submit",
        Direction::Response,
        "rejected[].node_rid",
        Shape::NodeId,
    ),
];

/// The docs-ingest tools' entries of [`super::tool_schemas`], plus the reserved `embed_submit`.
pub(super) fn schemas() -> Vec<ToolSchemas> {
    vec![
        super::schemas::<IngestClaimRequest, IngestClaimResponse>("ingest_claim"),
        super::schemas::<IngestBriefRequest, IngestBrief>("ingest_brief"),
        super::schemas::<IngestNextRequest, IngestNextResponse>("ingest_next"),
        super::schemas::<IngestSubmitRequest, IngestSubmitReport>("ingest_submit"),
        super::schemas::<IngestFinishRequest, IngestFinishReport>("ingest_finish"),
        // docs-ingest DI1.6 (behind the `kaban` crate's `ingest` feature; the adapter test
        // runs with default features)
        super::schemas::<IndexDocsRequest, IndexDocsResponse>("index_docs"),
        super::schemas::<Empty, IndexStatus>("index_status"),
        // reserved shape (EN-F50; `super::RESERVED`): no handler before E3.7
        super::schemas::<EmbedSubmitRequest, EmbedSubmitResponse>("embed_submit"),
    ]
}
