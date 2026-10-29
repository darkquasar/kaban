//! The policy rescan job's outcome.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// How a [`crate::KabanApi::rescan_if_policy_changed`] run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RescanStatus {
    /// `index_meta.policy_hash` already matched the loaded policy: nothing was scanned.
    UpToDate,
    /// Every node is now stamped with the loaded policy and `policy_hash` records it.
    Completed,
    /// The token was cancelled between batches: the batches that ran are committed, the rest
    /// is pending and `policy_hash` is unchanged, so the next start resumes.
    Cancelled,
}

/// Result of a rescan run (core-spec S4; core-plan C2.7).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RescanOutcome {
    /// How the run ended.
    pub status: RescanStatus,
    /// `effective_hash()` of the loaded policy (the version stamped on every judged node).
    pub policy_version: String,
    /// Batches (transactions) that ran.
    pub batches: u64,
    /// Nodes judged.
    pub scanned: u64,
    /// Nodes that passed (stamped only).
    pub kept: u64,
    /// Nodes masked in place.
    pub redacted: u64,
    /// Nodes tombstoned with a `policy-reject` event.
    pub rejected: u64,
}
