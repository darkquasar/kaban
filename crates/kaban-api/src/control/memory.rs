//! The memory tools' control rows and schemas (`remember`, `link`, `recall`, `traverse`,
//! `schema_*`, `forget`, `stats`, `accept`, `why`, `explain`, `governance_history`) and the
//! CLI-only `unmerge` verb.

use super::{ControlEntry, Direction, Empty, SNAPSHOT_COMMIT, Shape, ToolSchemas, entry, secret};
use crate::api::{ResolveSymbolResponse, ResolveWinnerResponse};
use crate::explain::{ExplainStatusRequest, StatusExplanation};
use crate::governance_history::{GovernanceHistoryRequest, GovernanceHistoryResponse};
use crate::sync::{ImportReport, SyncReport};
use crate::types::{
    AcceptRequest, AcceptResponse, DeprecateResponse, ForgetRequest, ForgetResponse, LinkRequest,
    LinkResponse, RecallRequest, RecallResponse, RememberRequest, RememberResponse,
    SchemaExtendRequest, SchemaResponse, StatsRequest, StatsResponse, TraverseRequest,
    TraverseResponse, WhyRequest, WhyResponse,
};
use kaban_core::TypeDef;
use schemars::{JsonSchema, schema_for};

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
/// `sync.lease` wire values (`crate::sync::LeaseState`, `rename_all = "lowercase"`).
const LEASE_STATES: &[&str] = &["acquired", "held"];

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

/// A claim key: 64 lowercase hex digits (EN-F12a), as `unmerge.deprecated[]` reports them.
const CLAIM_KEY: Shape = Shape::Hex { min: 64, max: 64 };
const RECALL_LAYERS: &[&str] = &["fts", "fuzzy", "sim"];

/// `kaban unmerge <id> [--reason <text>]` as the CLI seam parses it (`crates/kaban/src/cli/
/// unmerge.rs`; E2.10, boundary review BR-8): the registry's view of the request. CLI-only
/// (`super::CLI_ONLY`): never an MCP tool.
#[derive(Debug, serde::Deserialize, JsonSchema)]
#[allow(dead_code)] // schema only: the CLI parses its own copy and the handler takes primitives
struct UnmergeRequest {
    /// The merged node or its canonical (a node id: control by shape).
    id: String,
    /// Why the merge is reversed; stored as the identity claim's `rank_reason` (content).
    #[serde(default)]
    reason: Option<String>,
}

/// `kaban resolve <id> --winner <value> [--group <key>]` or `--symbol <key>` as the CLI seam
/// parses it (`crates/kaban/src/cli/resolve.rs`; D3 review d3-4): the registry's view of the
/// request; every leaf is content (the id is parsed after the scan). CLI-only.
#[derive(Debug, serde::Deserialize, JsonSchema)]
#[allow(dead_code)] // schema only: the CLI parses its own copy
struct ResolveRequest {
    /// The node (or `CodeRef`) named.
    id: String,
    /// The winning value (`--winner`).
    #[serde(default)]
    winner: Option<String>,
    /// The group the winner decides (`--group`), when the node has several.
    #[serde(default)]
    group: Option<String>,
    /// The symbol key a stale `CodeRef` is re-targeted to (`--symbol`).
    #[serde(default)]
    symbol: Option<String>,
}

/// `kaban import` as the CLI seam parses it (`crates/kaban/src/cli/import.rs`). CLI-only.
#[derive(Debug, serde::Deserialize, JsonSchema)]
#[allow(dead_code)] // schema only: the CLI parses its own copy
struct ImportRequest {
    /// The repository root.
    repo: String,
}

/// `kaban sync` as the CLI seam parses it (`crates/kaban/src/cli/sync.rs`). CLI-only.
#[derive(Debug, serde::Deserialize, JsonSchema)]
#[allow(dead_code)] // schema only: the CLI parses its own copy
struct SyncRequest {
    /// The repository root.
    repo: String,
    /// `--stage`: stage the written files.
    #[serde(default)]
    stage: bool,
    /// `--digest-only`: rewrite the digest, import nothing.
    #[serde(default)]
    digest_only: bool,
    /// `--ontology <name>`: the ontologies whose records are synced.
    #[serde(default)]
    ontologies: Option<Vec<String>>,
}

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
        "pruning_protection",
        Direction::Request,
        "operation",
        Shape::Enum(&["get", "set", "clear"]),
    ),
    entry(
        "pruning_protection",
        Direction::Response,
        "operation",
        Shape::Enum(&["get", "set", "clear"]),
    ),
    entry(
        "pruning_protection",
        Direction::Request,
        "target_kind",
        Shape::Enum(&["node", "claim", "relationship"]),
    ),
    entry(
        "pruning_protection",
        Direction::Response,
        "target_kind",
        Shape::Enum(&["node", "claim", "relationship"]),
    ),
    entry(
        "pruning_protection",
        Direction::Request,
        "id",
        Shape::NodeId,
    ),
    entry(
        "pruning_protection",
        Direction::Request,
        "src",
        Shape::NodeId,
    ),
    entry(
        "pruning_protection",
        Direction::Request,
        "dst",
        Shape::NodeId,
    ),
    entry("pruning_protection", Direction::Request, "key", CLAIM_KEY),
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
    // `kaban unmerge <id>` (E2.10; BR-8): the id the human names, the ids and claim keys the
    // outcome reports; `reason` and `redactions[]` are content
    entry("unmerge", Direction::Request, "id", Shape::NodeId),
    entry("unmerge", Direction::Response, "id", Shape::NodeId),
    entry(
        "unmerge",
        Direction::Response,
        "canonical_id",
        Shape::NodeId,
    ),
    entry("unmerge", Direction::Response, "restored[]", Shape::NodeId),
    entry("unmerge", Direction::Response, "deprecated[]", CLAIM_KEY),
    entry("forget", Direction::Request, "id", Shape::NodeId),
    entry(
        "forget",
        Direction::Request,
        "mode",
        Shape::Enum(FORGET_MODES),
    ),
    entry("accept", Direction::Request, "id", Shape::NodeId),
    // the handshake token on its way back in: validated, never scanned, never issued here
    secret(
        "accept",
        Direction::Request,
        "confirmation_token",
        Shape::Token64,
        false,
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
    entry("accept", Direction::Response, "node.id", Shape::NodeId),
    entry(
        "why",
        Direction::Response,
        "results[].node.id",
        Shape::NodeId,
    ),
    // `why(include_superseded)` (v4 CG6.1, stage1-spec §6): the successor ids of a card
    entry(
        "why",
        Direction::Response,
        "results[].superseded_by[]",
        Shape::NodeId,
    ),
    // `governance_history` (v4 CG6.2, stage1-spec §6.1, G1-F13): the `id` selector, each
    // record's id and its chain ids; `symbol`, `path` and `workspace_id` stay content
    entry(
        "governance_history",
        Direction::Request,
        "id",
        Shape::NodeId,
    ),
    entry(
        "governance_history",
        Direction::Response,
        "records[].card.id",
        Shape::NodeId,
    ),
    entry(
        "governance_history",
        Direction::Response,
        "records[].superseded_by[]",
        Shape::NodeId,
    ),
    entry(
        "governance_history",
        Direction::Response,
        "records[].supersedes[]",
        Shape::NodeId,
    ),
    // `explain` (E2.5, EN-F14a): a node-id address is control by shape; any other address
    // (a key, `x:Kind`, `path:offset`) is content and is gated before the grammar sees it
    entry("explain", Direction::Request, "address", Shape::NodeId),
    entry(
        "explain",
        Direction::Response,
        "subject.node",
        Shape::NodeId,
    ),
    // the one issuing response: the token is handed to the caller here and nowhere else
    secret(
        "accept",
        Direction::Response,
        "confirmation_token",
        Shape::Token64,
        true,
    ),
    // spec 05 SS-10: the receipt every write returns; its identities are kaban-minted UUIDs
    // (SS-27), its outcome a closed vocabulary
    entry(
        "accept",
        Direction::Response,
        "receipt.outcome",
        Shape::Enum(WRITE_OUTCOMES),
    ),
    entry(
        "accept",
        Direction::Response,
        "receipt.instance_id",
        Shape::NodeId,
    ),
    entry(
        "accept",
        Direction::Response,
        "receipt.pack_lineage_id",
        Shape::NodeId,
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
    entry(
        "unmerge",
        Direction::Response,
        "receipt.outcome",
        Shape::Enum(WRITE_OUTCOMES),
    ),
    entry(
        "unmerge",
        Direction::Response,
        "receipt.instance_id",
        Shape::NodeId,
    ),
    entry(
        "unmerge",
        Direction::Response,
        "receipt.pack_lineage_id",
        Shape::NodeId,
    ),
    entry(
        "pruning_protection",
        Direction::Response,
        "receipt.outcome",
        Shape::Enum(WRITE_OUTCOMES),
    ),
    entry(
        "pruning_protection",
        Direction::Response,
        "receipt.instance_id",
        Shape::NodeId,
    ),
    entry(
        "pruning_protection",
        Direction::Response,
        "receipt.pack_lineage_id",
        Shape::NodeId,
    ),
    // the CLI-only writes under their seam scopes (D3 review d3-4): `explain --deprecate`,
    // `resolve --winner` / `--symbol`, `import` and `sync` (whose import report nests it); a
    // policy term can never withhold or alter a committed mutation's own outcome
    entry(
        "explain",
        Direction::Response,
        "receipt.outcome",
        Shape::Enum(WRITE_OUTCOMES),
    ),
    entry(
        "explain",
        Direction::Response,
        "receipt.instance_id",
        Shape::NodeId,
    ),
    entry(
        "explain",
        Direction::Response,
        "receipt.pack_lineage_id",
        Shape::NodeId,
    ),
    entry(
        "resolve",
        Direction::Response,
        "receipt.outcome",
        Shape::Enum(WRITE_OUTCOMES),
    ),
    entry(
        "resolve",
        Direction::Response,
        "receipt.instance_id",
        Shape::NodeId,
    ),
    entry(
        "resolve",
        Direction::Response,
        "receipt.pack_lineage_id",
        Shape::NodeId,
    ),
    entry(
        "import",
        Direction::Response,
        "receipt.outcome",
        Shape::Enum(WRITE_OUTCOMES),
    ),
    entry(
        "import",
        Direction::Response,
        "receipt.instance_id",
        Shape::NodeId,
    ),
    entry(
        "import",
        Direction::Response,
        "receipt.pack_lineage_id",
        Shape::NodeId,
    ),
    entry(
        "sync",
        Direction::Response,
        "import.receipt.outcome",
        Shape::Enum(WRITE_OUTCOMES),
    ),
    entry(
        "sync",
        Direction::Response,
        "import.receipt.instance_id",
        Shape::NodeId,
    ),
    entry(
        "sync",
        Direction::Response,
        "import.receipt.pack_lineage_id",
        Shape::NodeId,
    ),
    entry(
        "sync",
        Direction::Response,
        "lease",
        Shape::Enum(LEASE_STATES),
    ),
];

/// The memory tools' entries of [`super::tool_schemas`].
pub(super) fn schemas() -> Vec<ToolSchemas> {
    vec![
        super::schemas::<crate::PruningProtectionRequest, crate::PruningProtectionResponse>(
            "pruning_protection",
        ),
        super::schemas::<RememberRequest, RememberResponse>("remember"),
        super::schemas::<LinkRequest, LinkResponse>("link"),
        super::schemas::<RecallRequest, RecallResponse>("recall"),
        super::schemas::<TraverseRequest, TraverseResponse>("traverse"),
        super::schemas::<Empty, SchemaResponse>("schema_describe"),
        super::schemas::<SchemaExtendRequest, TypeDef>("schema_extend"),
        super::schemas::<ForgetRequest, ForgetResponse>("forget"),
        super::schemas::<StatsRequest, StatsResponse>("stats"),
        super::schemas::<AcceptRequest, AcceptResponse>("accept"),
        super::schemas::<WhyRequest, WhyResponse>("why"),
        // the one-stop status explanation (engine-plan E2.5; EN-F14a), or with `--deprecate`
        // the deprecation's receipt-bearing report under the same seam scope
        super::schemas_any_of::<ExplainStatusRequest>(
            "explain",
            &[
                schema_for!(StatusExplanation),
                schema_for!(DeprecateResponse),
            ],
        ),
        // the governance history of the records one selector reaches (v4 CG6.2; G1-F13)
        super::schemas::<GovernanceHistoryRequest, GovernanceHistoryResponse>("governance_history"),
        // the CLI-only merge reversal (E2.10; BR-8): `super::CLI_ONLY`, never advertised
        super::schemas::<UnmergeRequest, crate::UnmergeResponse>("unmerge"),
        // the other CLI-only writes under their seam scopes (`super::CLI_ONLY`; D3 review
        // d3-4): the receipt-bearing reports of `resolve --winner` / `--symbol` (the conflict
        // `--take` report carries no control leaf and stays content), `import` and `sync`
        super::schemas_any_of::<ResolveRequest>(
            "resolve",
            &[
                schema_for!(ResolveWinnerResponse),
                schema_for!(ResolveSymbolResponse),
            ],
        ),
        super::schemas::<ImportRequest, ImportReport>("import"),
        super::schemas::<SyncRequest, SyncReport>("sync"),
        super::schemas::<LayerMaintenanceRequest, MaintenanceResponse>("forget_layer"),
        super::schemas::<LayerMaintenanceRequest, MaintenanceResponse>("reindex_layer"),
    ]
}
