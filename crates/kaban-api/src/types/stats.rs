//! `stats`.

use crate::layers::LayerRow;
use kaban_core::StoreScope;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// `stats` input.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct StatsRequest {
    /// Workspace to count (default: the server's bound workspace; `"*"` for all). Database
    /// file bytes are always global.
    pub workspace_id: Option<String>,
}

/// Policy summary in `stats`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PolicyStats {
    /// Policy version hash (sha256 of the policy file bytes).
    pub version: String,
    /// Effective hash of the loaded rules (builtin and user; the version stamped on judged
    /// nodes and compared by the rescan job, MD-19).
    pub effective_hash: String,
    /// Number of regex rules.
    pub rules: usize,
    /// Number of denylist words.
    pub words: usize,
    /// Number of denylist phrases.
    pub phrases: usize,
    /// Whether stored text is re-scanned on read.
    pub scan_on_read: bool,
}

/// `stats` output: the store's counters plus the policy summary (core-spec F6; M3-26 superset).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct StatsResponse {
    /// Store counters (`nodes`, `edges`, `by_kind`, `by_rel`, `db_bytes`, `changes_lag`: how far
    /// the slowest `ChangeLog` sink trails the newest change, F11, with `outbox_lag` as its
    /// deprecated alias until 3.2 (EN-E10b); `engine`, `schema_version`, `conflicts`).
    #[serde(flatten)]
    pub store: serde_json::Map<String, Value>,
    /// Stores this server reads (`project`; `personal` arrives in M6, F18).
    pub stores: Vec<StoreScope>,
    /// Policy summary.
    pub policy: PolicyStats,
    /// Five default-matrix categories and last valid producer runs (EN-F52).
    #[serde(default)]
    pub layers: Vec<LayerRow>,
}
