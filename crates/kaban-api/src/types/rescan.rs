//! The policy rescan job's outcome.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// How a [`crate::KabanApi::rescan_if_policy_changed`] run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RescanStatus {
    /// The loaded policy had already judged every stored row: nothing was scanned.
    UpToDate,
    /// Every row the loaded policy had not judged is judged; `policy_hash` records the policy
    /// and `policy_scan_seq` the change-log position judged up to.
    Completed,
    /// The token was cancelled between batches: the batches that ran are committed, the rest
    /// is pending (an unfinished walk leaves `policy_hash` unchanged), so the next start
    /// resumes.
    Cancelled,
}

/// Result of a rescan run (core-spec S4; core-plan C2.7).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RescanOutcome {
    /// How the run ended.
    pub status: RescanStatus,
    /// `effective_hash()` of the loaded policy.
    pub policy_version: String,
    /// Batches (transactions) that ran.
    pub batches: u64,
    /// Rows (nodes and edges) judged.
    pub scanned: u64,
    /// Rows that passed unchanged.
    pub kept: u64,
    /// Rows masked in place.
    pub redacted: u64,
    /// Nodes tombstoned and edges deleted.
    pub rejected: u64,
}
