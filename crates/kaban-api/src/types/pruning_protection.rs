//! Closed pruning-protection requests; authority comes only from the adapter.
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The operation on one durable pruning mark.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PruningOperation {
    /// Read explicit and effective protection.
    Get,
    /// Set an explicit durable mark.
    Set,
    /// Clear only the explicit mark.
    Clear,
}
/// Which graph entry is addressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PruningTargetKind {
    /// A node id.
    Node,
    /// A stable assertion key.
    Claim,
    /// Source, destination and relationship name.
    Relationship,
}
/// Inspect/set/clear pruning protection, not replacement protection. Only fields of
/// the selected target kind may be supplied; actor and tenant are adapter-bound.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PruningProtectionRequest {
    /// Closed get/set/clear operation.
    pub operation: PruningOperation,
    /// Node, claim or relationship.
    pub target_kind: PruningTargetKind,
    /// Node id for a node target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Stable assertion key for a claim target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// Source node id for a relationship target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub src: Option<String>,
    /// Destination node id for a relationship target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dst: Option<String>,
    /// Relationship name (CONTENT, always policy-gated).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rel: Option<String>,
}
/// Explicit and effective protection are distinct: clearing a mark can leave a
/// dependency protected by another marked entry.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PruningProtectionResponse {
    /// Closed operation performed.
    pub operation: PruningOperation,
    /// Graph entry kind.
    pub target_kind: PruningTargetKind,
    /// Whether a durable mark exists on this exact target.
    pub explicit: bool,
    /// Whether the entry is retained by explicit or dependency protection.
    pub effective: bool,
    /// The receipt of a `set` or `clear` (spec 05 SS-10); absent on `get`, which writes nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receipt: Option<super::WriteReceipt>,
}
