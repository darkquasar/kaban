//! The memory tools' control rows and schemas (`remember`, `link`, `recall`, `traverse`,
//! `schema_*`, `forget`, `stats`) and the CLI-only layer maintenance verbs.

use super::{ControlEntry, Direction, Empty, SNAPSHOT_COMMIT, Shape, ToolSchemas, entry};
use crate::types::{
    ForgetRequest, ForgetResponse, LinkRequest, LinkResponse, RecallRequest, RecallResponse,
    RememberRequest, RememberResponse, SchemaExtendRequest, SchemaResponse, StatsRequest,
    StatsResponse, TraverseRequest, TraverseResponse,
};
use kaban_core::TypeDef;
use schemars::JsonSchema;

/// `forget.mode` wire values (`kaban_core::ForgetMode`, `rename_all = "lowercase"`).
const FORGET_MODES: &[&str] = &["tombstone", "invalidate", "archive", "purge"];

/// `traverse.direction` wire values (`kaban_core::TraverseDirection`, `rename_all = "lowercase"`;
/// MT-5, `c30-review-dispositions.md` note for C3.6).
const TRAVERSE_DIRECTIONS: &[&str] = &["out", "in", "both"];

/// `traverse.governance[]` wire values (`kaban_core::Governance`, `rename_all = "lowercase"`;
/// RE-15, E2.11): the governance states a traversal keeps.
const GOVERNANCE_VALUES: &[&str] = &["none", "proposed", "accepted", "superseded", "invalidated"];

/// `remember.scope` wire values (`kaban_core::StoreScope`, `rename_all = "lowercase"`).
const STORE_SCOPES: &[&str] = &["project", "personal"];
/// The write outcomes of a receipt (spec 05 SS-10 `WriteOutcome`, `rename_all = "snake_case"`).
pub(super) const WRITE_OUTCOMES: &[&str] =
    &["rejected", "committed", "committed_not_yet_searchable"];

/// `traverse.edge_filter.layers[]` wire values: the stored spellings of `kaban_core::Layer`,
/// `Layer::ALL` in order (EN-E6, EN-E7). Spelled out on purpose: `tests/control.rs` asserts
/// this list equals `Layer::ALL` and the schemars `enum` of the vocabulary, so a new layer
/// fails the registry test until this row moves (engine-spec EN-E7 `Verify:`).
pub const LAYER_VALUES: &[&str] = &[
    "human",
    "v4",
    "reconcile",
    "script",
    "llm",
    "gliner",
    "gazetteer",
    "embed",
    "fts",
    "fuzzy",
];

/// `traverse.edge_filter.methods[]` wire values: the stored spellings of `kaban_core::Method`,
/// `Method::ALL` in order (EN-E6, EN-E7); pinned like [`LAYER_VALUES`].
pub const METHOD_VALUES: &[&str] = &[
    "id",
    "path",
    "exact",
    "alias",
    "symbol",
    "tag",
    "embed",
    "gliner",
    "llm",
    "human",
    "measured",
    "rule",
    "heuristic",
    "link",
    "accept",
    "remember",
    "er",
    "er_judge",
    "reconciled",
    "ingest_submit",
];

const RECALL_LAYERS: &[&str] = &["fts", "fuzzy", "sim"];

/// CLI-only machine reset; document paths remain CONTENT, never control identifiers.
#[derive(Debug, serde::Deserialize, JsonSchema)]
#[allow(dead_code)]
struct LayerMaintenanceRequest {
    layer: kaban_core::Layer,
    #[serde(default)]
    document: Option<String>,
}

/// Schema mirror of the fixed maintenance refusal vocabulary.
#[derive(Debug, serde::Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[allow(dead_code)]
enum MaintenanceRefusal {
    #[serde(rename = "current_producer")]
    Current,
    #[serde(rename = "untracked_producer")]
    Untracked,
    #[serde(rename = "unavailable_producer")]
    Unavailable,
}

/// Schema mirror of core bounded retirement accounting; no MCP wire addition.
#[derive(Debug, serde::Deserialize, JsonSchema)]
#[allow(dead_code)]
struct MaintenanceResponse {
    transactions: u32,
    rows: u64,
    #[serde(default)]
    refused: Option<MaintenanceRefusal>,
    #[serde(default)]
    untracked: u64,
}

/// The memory tools' rows of [`super::REGISTRY`].
pub(super) const ROWS: &[ControlEntry] = &[
    entry(
        "forget_layer",
        Direction::Request,
        "layer",
        Shape::Enum(LAYER_VALUES),
    ),
    entry(
        "reindex_layer",
        Direction::Request,
        "layer",
        Shape::Enum(LAYER_VALUES),
    ),
    entry(
        "forget_layer",
        Direction::Response,
        "refused",
        Shape::Enum(&[
            "current_producer",
            "untracked_producer",
            "unavailable_producer",
        ]),
    ),
    entry(
        "reindex_layer",
        Direction::Response,
        "refused",
        Shape::Enum(&[
            "current_producer",
            "untracked_producer",
            "unavailable_producer",
        ]),
    ),
    // request side: node ids named by the caller, the forget mode enum
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
    // typed parse (`governance[]`, `scope`) or the handler (`unknown_layer`, `unknown_method`).
    // `node_kinds[]`, `rels[]` and `kind_set` stay content (EN-E6: kind names are content).
    entry(
        "traverse",
        Direction::Request,
        "governance[]",
        Shape::Enum(GOVERNANCE_VALUES),
    ),
    entry(
        "traverse",
        Direction::Request,
        "edge_filter.layers[]",
        Shape::Enum(LAYER_VALUES),
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
        "layers[]",
        Shape::Enum(RECALL_LAYERS),
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
    // EN-F19 (E2.7 deviation 9, E2.7b): the anchored node of a `follow_anchors` card
    entry(
        "traverse",
        Direction::Response,
        "anchored[].from",
        Shape::NodeId,
    ),
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
        super::schemas::<LayerMaintenanceRequest, MaintenanceResponse>("forget_layer"),
        super::schemas::<LayerMaintenanceRequest, MaintenanceResponse>("reindex_layer"),
    ]
}
