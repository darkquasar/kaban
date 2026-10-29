//! `recall`.

use super::card::Card;
use crate::enrich::{Enrichment, FollowUp};
use kaban_core::CodeRefFilter;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// `recall` input.
#[allow(clippy::struct_excessive_bools)] // four independent wire switches; EN-F49 adds `include_candidates`
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct RecallRequest {
    /// Retrieval sources (fts, fuzzy, reserved sim); absent uses enabled defaults.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layers: Option<Vec<kaban_core::RecallLayer>>,
    /// Also retrieve fuzzy matches when lexical retrieval succeeds.
    #[serde(default)]
    pub fuzzy: bool,
    /// Free-text query (keywords, identifiers, error strings).
    pub query: String,
    /// Restrict to these kinds.
    #[serde(default)]
    pub kinds: Vec<String>,
    /// Require at least one of these tags.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Workspace to search (default: the server's bound workspace; `"*"` for all). `workspace`
    /// is accepted as an alias.
    #[serde(alias = "workspace")]
    pub workspace_id: Option<String>,
    /// Only notes about this `CodeRef` (linked `ABOUT` or `EVIDENCE_FOR`; exact match on
    /// `relative_path`, and on `name_path` / `repo` when given) seed the recall.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub r#ref: Option<CodeRefFilter>,
    /// Max results (default 10, max 100).
    pub limit: Option<usize>,
    /// Graph expansion hops 0..2 (default 1).
    pub hops: Option<u8>,
    /// Also return superseded (archived) records reached through graph expansion. Invalidated
    /// and tombstoned records never return.
    #[serde(default)]
    pub include_superseded: bool,
    /// Also return archived records of any governance reached through graph expansion.
    #[serde(default)]
    pub include_archived: bool,
    /// Also return candidate records (an `x:` record still under review, a record a
    /// reconciliation left for review); default false: a candidate neither seeds the result
    /// nor joins it through graph expansion. `kaban review` and `explain` list them either way.
    #[serde(default)]
    pub include_candidates: bool,
    /// Return full text instead of summaries.
    #[serde(default)]
    pub full_text: bool,
    /// Enrichers to run on the `CodeRef` pointers of the result (core-spec F19). An id this
    /// process has not configured is an error listing the configured ones; none ships in v3.0.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub enrich: Vec<String>,
}

/// `recall` output.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RecallResponse {
    /// Number of cards.
    pub count: usize,
    /// Cards, best first.
    pub results: Vec<Card>,
    /// Suggestions the agent may run with other tools, derived from the `CodeRef` pointers
    /// linked to the cards of this result (core-spec F19); kaban never executes them.
    #[serde(default)]
    pub follow_ups: Vec<FollowUp>,
    /// Output of the enrichers named in `enrich[]` (omitted when none ran or produced nothing).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub enrichments: Vec<Enrichment>,
}
