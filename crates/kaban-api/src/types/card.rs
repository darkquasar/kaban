//! The read projection shared by `remember`, `accept`, `recall`, `traverse`, `why`, the
//! project export and the sync job.

use kaban_core::{Governance, Lifecycle, StoreScope};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Where a card came from (core-spec F3 "cards with provenance"; core-plan C3.5). Every string
/// is a persisted node string, masked by the read projection like the rest of the card.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Provenance {
    /// Agent that produced the record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    /// Session that produced the record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Repository identity (the root directory name, M3-23).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    /// Commit the record was taken at, when stamped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit_sha: Option<String>,
    /// Which store the record lives in (`project`; `personal` arrives in M6).
    pub store: StoreScope,
}

/// Outgoing projection of a node (re-scanned on read when the policy says so).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Card {
    /// Node id.
    pub id: String,
    /// Node kind.
    pub kind: String,
    /// Storage visibility (`active` unless the read opted into archived records).
    pub lifecycle: Lifecycle,
    /// Governance state (`none` for kinds other than Decision, Convention and Failure).
    pub governance: Governance,
    /// Creation time (RFC 3339).
    pub created_at: String,
    /// Text (present when full text was requested or the node has no summary).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Summary, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Tags (omitted when empty).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// 0..1 confidence.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f64>,
    /// Workspace id (every row carries one, F17).
    pub workspace_id: String,
    /// Props (full text only, omitted when empty).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub props: Option<Value>,
    /// `field:rule` entries for redactions applied on read (omitted when none).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub redactions: Vec<String>,
    /// Who produced the record and where it lives.
    #[serde(default)]
    pub provenance: Provenance,
    /// Fused score (recall only), rounded to 3 decimals.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
    /// Why the card was returned (recall only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub why: Option<Vec<String>>,
}

/// Outgoing projection of an edge.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct EdgeCard {
    /// Edge id.
    pub id: String,
    /// Source node id.
    pub from: String,
    /// Relationship type.
    pub rel: String,
    /// Destination node id.
    pub to: String,
    /// 0..1 confidence.
    pub confidence: Option<f64>,
}
