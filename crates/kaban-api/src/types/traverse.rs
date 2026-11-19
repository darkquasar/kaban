//! `traverse`.

use super::card::{Card, EdgeCard};
use kaban_core::TraverseDirection;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// `traverse` input. Closed: a removed key (`follow_anchors`, `kind_set`, `governance`,
/// `include_superseded`, `include_candidates`) is refused naming it, with no alias.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
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
    /// Follow only the edges that pass every clause: a producer in `producers`, a method in
    /// `methods`, a score of at least `min_score`. An edge without a value in a filtered column
    /// (a structural edge carries no method or score) does not pass that clause. Default:
    /// every edge.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edge_filter: Option<EdgeFilter>,
    /// Max nodes (default 100, max 500).
    pub limit: Option<usize>,
    /// Workspace to traverse (default: the server's bound workspace; `"*"` for all).
    pub workspace_id: Option<String>,
}

/// The edge filter of `traverse` (EN-F46). Values are the stored vocabulary spellings:
/// producers `authored`, `hook_events`, `document_ingest`, `gazetteer`, `extractor:llm`,
/// `extractor:gliner`, `extractor:script`, `scip_import`, `git_history` (PC-16); methods such
/// as `link`, `remember`, `exact`, `alias`, `symbol`, `gliner`, `llm`. An unknown spelling is
/// refused (`unknown_producer`, `unknown_method`), so is an unknown key (the removed `layers`
/// among them).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EdgeFilter {
    /// Keep only edges whose score is at least this (0..1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_score: Option<f64>,
    /// Keep only edges from these producers (empty: any).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub producers: Vec<String>,
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
}
