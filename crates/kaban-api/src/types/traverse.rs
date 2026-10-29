//! `traverse`.

use super::card::{Card, EdgeCard};
use kaban_core::{Governance, TraverseDirection};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub use kaban_core::AnchoredKind;

/// `traverse` input.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct TraverseRequest {
    /// Starting node ids (1..64).
    pub start_ids: Vec<String>,
    /// Follow only these relationship types.
    #[serde(default)]
    pub rels: Vec<String>,
    /// Which edges to follow: `out`, `in` or `both` (default both). A closed set: any other
    /// value is refused.
    pub direction: Option<TraverseDirection>,
    /// 1..4 (default 1)
    pub max_depth: Option<u8>,
    /// Keep only these node kinds. A kind the loaded ontology does not declare is refused
    /// (`unknown_kind`), as is an undeclared relationship type in `rels` (`unknown_rel`).
    #[serde(default)]
    pub node_kinds: Vec<String>,
    /// Keep only the kinds of this ontology-defined set, in addition to `node_kinds`:
    /// `governing` (the kinds whose records govern paths) is the one set; any other value is
    /// refused (`unknown_kind_set`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind_set: Option<String>,
    /// Follow only the edges that pass every clause: a source layer in `layers`, a method in
    /// `methods`, a score of at least `min_score`. An edge without a value in a filtered column
    /// (a structural edge written without a claim) does not pass that clause. Default: every
    /// edge.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edge_filter: Option<EdgeFilter>,
    /// Keep only nodes in these governance states (`none`, `proposed`, `accepted`,
    /// `superseded`, `invalidated`); default: any.
    #[serde(default)]
    pub governance: Vec<Governance>,
    /// Max nodes (default 100, max 500).
    pub limit: Option<usize>,
    /// Workspace to traverse (default: the server's bound workspace; `"*"` for all).
    pub workspace_id: Option<String>,
    /// Expand each reached node that anchors a code symbol or path one step into the current
    /// code-graph snapshot (EN-F19): the snapshot rows come back in `anchored` as `symbol` and
    /// `file` cards, 200 per anchored node and 500 in total; depth and the node limit are
    /// unchanged. Default false; nothing is added when no snapshot resolves.
    #[serde(default)]
    pub follow_anchors: bool,
    /// Also return candidate records (an `x:` record still under review, a record a
    /// reconciliation left for review); default false: candidates are absent from the nodes,
    /// the edges and the start ids alike. `kaban review` and `explain` list them either way.
    #[serde(default)]
    pub include_candidates: bool,
}

/// The edge filter of `traverse` (EN-F46). Values are the stored vocabulary spellings:
/// layers `human`, `v4`, `reconcile`, `script`, `llm`, `gliner`, `gazetteer`, `embed`;
/// methods such as `link`, `remember`, `exact`, `alias`, `symbol`, `gliner`, `llm`. An
/// unknown spelling is refused (`unknown_layer`, `unknown_method`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct EdgeFilter {
    /// Keep only edges whose score is at least this (0..1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_score: Option<f64>,
    /// Keep only edges from these source layers (empty: any).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub layers: Vec<String>,
    /// Keep only edges asserted by these methods (empty: any).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub methods: Vec<String>,
}

/// `traverse` output.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TraverseResponse {
    /// Nodes reached.
    pub nodes: Vec<Card>,
    /// Edges among the returned nodes.
    pub edges: Vec<EdgeCard>,
    /// Code-graph cards reached through the anchors of the returned nodes
    /// (`follow_anchors`); empty otherwise.
    #[serde(default)]
    pub anchored: Vec<AnchoredCard>,
}

/// One code-graph card of `anchored[]` (EN-F19; CG-JOIN v1 §9 order `(path, ref)`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AnchoredCard {
    /// The returned node whose anchor reached this card.
    pub from: String,
    /// `symbol` or `file`.
    pub kind: AnchoredKind,
    /// The file path: the card itself for a `file`, the defining file for a `symbol`.
    pub path: String,
    /// The symbol's stable `ref` (absent for a file).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub r#ref: Option<String>,
    /// The symbol's cross-commit key (absent for a file or a key-less symbol).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// The symbol's name (absent for a file).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}
