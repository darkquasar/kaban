//! `why` and the governance event view.

use super::card::Card;
use super::symbol::{CodeField, MatchedBy};
use crate::enrich::FollowUp;
use kaban_core::{Governance, SignerKind};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// One `why` target (core-spec F2): a repository-relative path, or a free-text query.
// Since E2.6 also a code-graph symbol key (engine-spec EN-F17; CG-JOIN v1 §3, §6); the enum
// description above is a golden line and stays as it was.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WhyTarget {
    /// A path whose governing records are wanted (`path_scope` entries that cover it).
    Path(String),
    /// A query matched against the records' text.
    Query(String),
    /// A normalised SCIP symbol key (stage1-spec §4.2): the records anchored on a `CodeRef`
    /// with that `symbol`, plus those governing the symbol's defining file in the resolved
    /// snapshot. Scanned as content first, then grammar-checked (`symbol_invalid`).
    Symbol(String),
}

/// `why` input (core-spec F2; core-plan C3.4).
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct WhyRequest {
    /// Paths and queries, 1..=16 per call (`{"path": "src/store/sqlite.rs"}` or
    /// `{"query": "sqlite wal"}`).
    pub targets: Vec<WhyTarget>,
    /// Max records per target (default 10, max 100).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
    /// Workspace to read (default: the bound workspace; `"*"` for all; core-spec S6).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    /// Also return the superseded records behind each current one (stage1-spec G1-F13,
    /// read-gated like the rest of the call): each is listed after its successor with
    /// `superseded_by` filled. Default `false`: superseded records are left out, as always.
    #[serde(default)]
    pub include_superseded: bool,
}

/// One governing record returned by `why`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WhyCard {
    /// The record (compact card; `props` are not included).
    pub node: Card,
    /// Governance state (`accepted` records come first).
    pub governance: Governance,
    /// Currency of the record's `path_scope` since acceptance (core-spec F14).
    pub currency: crate::currency::Currency,
    /// The target this card answers (the path or the query as sent).
    pub matched_target: String,
    /// How it matched.
    pub matched_by: MatchedBy,
    /// `true` when the record's `.kaban/*.md` file and the database disagree at the same rev
    /// (core-spec F13; a human resolves it with `kaban resolve <id> --take file|db`). Omitted
    /// when false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub conflict: bool,
    /// The code-graph side of a `symbol` target (CG-JOIN v1 §6); absent on `path` and
    /// `query` targets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<CodeField>,
    /// The commit the anchor row that reached this card was scoped to (40 hex), present only
    /// when no snapshot resolved (`code: snapshot_absent`, OWN-83 (1); CG-JOIN v1 §3): the
    /// answer is the latest decision, not verified against a current index. Absent on a
    /// resolved lookup and on `path` and `query` targets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor_commit: Option<String>,
    /// The ids of the records that supersede this one, nearest first (stage1-spec §6; a
    /// control-registry `NodeId` path). Always present: `[]` for a current record.
    #[serde(default)]
    pub superseded_by: Vec<String>,
}

/// `why` output.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WhyResponse {
    /// Governing records, per target in request order; within a target accepted before
    /// proposed, then importance, then newest first.
    pub results: Vec<WhyCard>,
    /// `true` when a target had more governing records than `limit`.
    pub partial: bool,
    /// Suggestions the agent may run with other tools, derived from the `CodeRef` pointers
    /// linked to the cards of this result (core-spec F19); kaban never executes them.
    #[serde(default)]
    pub follow_ups: Vec<FollowUp>,
}

/// A governance event as it leaves the service (core-plan M3-6, WG-2): every string re-gated
/// on the read path; a rejected event is not returned at all.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct EventView {
    /// Position in the event log.
    pub seq: i64,
    /// `accepted`, `superseded`, `invalidated`, ...
    pub kind: String,
    /// Why (gated content).
    pub reason: String,
    /// Path scope recorded with the event (gated content).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub path_scope: Vec<String>,
    /// Declared signer identity, if recorded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signer: Option<String>,
    /// Who signed.
    pub signer_kind: SignerKind,
    /// Host, if recorded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// Session, if recorded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Unix time (seconds).
    pub ts: i64,
    /// Commit at the time of the event (gated content, like every other event string).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit_sha: Option<String>,
}
