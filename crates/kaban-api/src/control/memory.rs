//! The memory tools' control rows and schemas (`remember`, `link`, `recall`, `traverse`,
//! `schema_*`, `forget`, `stats`).

use super::{ControlEntry, Direction, Empty, SNAPSHOT_COMMIT, Shape, ToolSchemas, entry};
use crate::types::{
    ForgetRequest, ForgetResponse, LinkRequest, LinkResponse, RecallRequest, RecallResponse,
    RememberRequest, RememberResponse, SchemaExtendRequest, SchemaResponse, StatsRequest,
    StatsResponse, TraverseRequest, TraverseResponse,
};
use kaban_core::TypeDef;

/// `forget.mode` wire values (`kaban_core::ForgetMode`, `rename_all = "lowercase"`).
const FORGET_MODES: &[&str] = &["tombstone", "invalidate", "archive", "purge"];

/// `traverse.direction` wire values (`kaban_core::TraverseDirection`, `rename_all = "lowercase"`;
/// MT-5, `c30-review-dispositions.md` note for C3.6).
const TRAVERSE_DIRECTIONS: &[&str] = &["out", "in", "both"];

/// `remember.scope` wire values (`kaban_core::StoreScope`, `rename_all = "lowercase"`).
const STORE_SCOPES: &[&str] = &["project", "personal"];
/// The write outcomes of a receipt (spec 05 SS-10 `WriteOutcome`, `rename_all = "snake_case"`).
pub(super) const WRITE_OUTCOMES: &[&str] =
    &["rejected", "committed", "committed_not_yet_searchable"];

/// `traverse.edge_filter.producers[]` wire values: the stored spellings of
/// `kaban_core::Producer`, `Producer::ALL` in order (PC-16). Spelled out on purpose:
/// `tests/control.rs` asserts this list equals `Producer::ALL` and the schemars `enum` of the
/// vocabulary, so a new producer fails the registry test until this row moves.
pub const PRODUCER_VALUES: &[&str] = &[
    "authored",
    "hook_events",
    "document_ingest",
    "gazetteer",
    "extractor:llm",
    "extractor:gliner",
    "extractor:script",
    "scip_import",
    "git_history",
];

/// `traverse.edge_filter.methods[]` wire values: the stored spellings of `kaban_core::Method`,
/// `Method::ALL` in order (EN-E6, EN-E7); pinned like [`PRODUCER_VALUES`].
pub const METHOD_VALUES: &[&str] = &[
    "id",
    "path",
    "exact",
    "alias",
    "symbol",
    "tag",
    "gliner",
    "llm",
    "measured",
    "rule",
    "heuristic",
    "link",
    "remember",
    "ingest_submit",
];

const RECALL_MODES: &[&str] = &["lexical", "fuzzy"];

/// `stats.indexes[].index` wire values (`IndexName`, `rename_all = "snake_case"`; PC-16).
const INDEX_NAMES: &[&str] = &["text", "graph", "code"];

/// The memory tools' rows of [`super::REGISTRY`].
pub(super) const ROWS: &[ControlEntry] = &[
    entry("remember", Direction::Request, "links[].to", Shape::NodeId),
    entry(
        "remember",
        Direction::Request,
        "links[].from",
        Shape::NodeId,
    ),
    entry(
        "remember",
        Direction::Request,
        "refs[].snapshot_commit",
        SNAPSHOT_COMMIT,
    ),
    entry("link", Direction::Request, "from", Shape::NodeId),
    entry("link", Direction::Request, "to", Shape::NodeId),
    entry("traverse", Direction::Request, "start_ids[]", Shape::NodeId),
    entry(
        "traverse",
        Direction::Request,
        "direction",
        Shape::Enum(TRAVERSE_DIRECTIONS),
    ),
    // RE-15 (E2.11): the closed request vocabularies of `traverse` and `remember`; a valid
    // spelling is validated, any other string is scanned as content and then refused by the
    // typed parse (`scope`) or the handler (`unknown_producer`, `unknown_method`).
    // `node_kinds[]` and `rels[]` stay content (EN-E6: kind names are content).
    entry(
        "traverse",
        Direction::Request,
        "edge_filter.producers[]",
        Shape::Enum(PRODUCER_VALUES),
    ),
    entry(
        "traverse",
        Direction::Request,
        "edge_filter.methods[]",
        Shape::Enum(METHOD_VALUES),
    ),
    entry(
        "remember",
        Direction::Request,
        "scope",
        Shape::Enum(STORE_SCOPES),
    ),
    entry("forget", Direction::Request, "id", Shape::NodeId),
    entry(
        "forget",
        Direction::Request,
        "mode",
        Shape::Enum(FORGET_MODES),
    ),
    // response side: ids kaban minted, the forget mode, the policy version hash
    entry("remember", Direction::Response, "id", Shape::NodeId),
    entry(
        "remember",
        Direction::Response,
        "candidate_duplicates[].id",
        Shape::NodeId,
    ),
    entry("link", Direction::Response, "id", Shape::NodeId),
    entry("link", Direction::Response, "from", Shape::NodeId),
    entry("link", Direction::Response, "to", Shape::NodeId),
    entry("recall", Direction::Response, "results[].id", Shape::NodeId),
    entry(
        "recall",
        Direction::Request,
        "modes[]",
        Shape::Enum(RECALL_MODES),
    ),
    entry("traverse", Direction::Response, "nodes[].id", Shape::NodeId),
    entry("traverse", Direction::Response, "edges[].id", Shape::NodeId),
    entry(
        "traverse",
        Direction::Response,
        "edges[].from",
        Shape::NodeId,
    ),
    entry("traverse", Direction::Response, "edges[].to", Shape::NodeId),
    entry("forget", Direction::Response, "id", Shape::NodeId),
    entry(
        "forget",
        Direction::Response,
        "mode",
        Shape::Enum(FORGET_MODES),
    ),
    entry(
        "stats",
        Direction::Response,
        "policy.version",
        Shape::Hex { min: 64, max: 64 },
    ),
    entry(
        "stats",
        Direction::Response,
        "policy.effective_hash",
        Shape::Hex { min: 64, max: 64 },
    ),
    entry(
        "stats",
        Direction::Response,
        "indexes[].index",
        Shape::Enum(INDEX_NAMES),
    ),
    entry(
        "remember",
        Direction::Response,
        "receipt.outcome",
        Shape::Enum(WRITE_OUTCOMES),
    ),
    entry(
        "remember",
        Direction::Response,
        "receipt.instance_id",
        Shape::NodeId,
    ),
    entry(
        "remember",
        Direction::Response,
        "receipt.pack_lineage_id",
        Shape::NodeId,
    ),
    entry(
        "link",
        Direction::Response,
        "receipt.outcome",
        Shape::Enum(WRITE_OUTCOMES),
    ),
    entry(
        "link",
        Direction::Response,
        "receipt.instance_id",
        Shape::NodeId,
    ),
    entry(
        "link",
        Direction::Response,
        "receipt.pack_lineage_id",
        Shape::NodeId,
    ),
    entry(
        "forget",
        Direction::Response,
        "receipt.outcome",
        Shape::Enum(WRITE_OUTCOMES),
    ),
    entry(
        "forget",
        Direction::Response,
        "receipt.instance_id",
        Shape::NodeId,
    ),
    entry(
        "forget",
        Direction::Response,
        "receipt.pack_lineage_id",
        Shape::NodeId,
    ),
];

/// The memory tools' entries of [`super::tool_schemas`].
pub(super) fn schemas() -> Vec<ToolSchemas> {
    vec![
        super::schemas::<RememberRequest, RememberResponse>("remember"),
        super::schemas::<LinkRequest, LinkResponse>("link"),
        super::schemas::<RecallRequest, RecallResponse>("recall"),
        super::schemas::<TraverseRequest, TraverseResponse>("traverse"),
        super::schemas::<Empty, SchemaResponse>("schema_describe"),
        super::schemas::<SchemaExtendRequest, TypeDef>("schema_extend"),
        super::schemas::<ForgetRequest, ForgetResponse>("forget"),
        super::schemas::<StatsRequest, StatsResponse>("stats"),
    ]
}
