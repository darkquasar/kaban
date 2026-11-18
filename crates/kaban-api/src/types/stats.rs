//! `stats`.

use crate::producers::ProducerRow;
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
    /// The seven producers with their default state, last valid run and budgets (EN-F52,
    /// PC-16).
    #[serde(default)]
    pub producers: Vec<ProducerRow>,
    /// The derived indexes on the catalog (PC-16), text, graph and code in that order; empty
    /// when the service has no catalog wired.
    #[serde(default)]
    pub indexes: Vec<CatalogIndexStatus>,
}

/// A derived index on the catalog (PC-16).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum IndexName {
    /// The text index.
    Text,
    /// The memory graph.
    Graph,
    /// The code graph.
    Code,
}

/// The catalog state of one derived index (PC-16's `IndexStatus`; named apart from the
/// document index's `index_status` response), from the catalog's verification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CatalogIndexStatus {
    /// Which index.
    pub index: IndexName,
    /// Pending sediment rows not yet sealed into a layer.
    pub sediment_rows: u64,
    /// Live layers (extents over the base).
    pub layers: u32,
    /// The generation that published the live base, if the index has one.
    pub base_generation: Option<u64>,
    /// The newest generation that published a live extent of the index (the last compaction
    /// that changed it), if any.
    pub last_compaction: Option<i64>,
}

/// What the catalog's verification found, as the service reports it (`doctor`, `stats`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CatalogVerify {
    /// Text, graph and code, in that order.
    pub indexes: Vec<CatalogIndexStatus>,
    /// One line per inconsistency (0 lines: the catalog is consistent).
    pub inconsistencies: Vec<String>,
    /// Views in `needs_reimport` (retired by retention or refused): a state doctor fails on.
    pub needs_reimport: Vec<String>,
}

/// The catalog's cheap summary (`stats`, doctor's `catalog_budget` row): one SQL summary, no
/// container scan and no hashing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CatalogStatus {
    /// Text, graph and code, in that order.
    pub indexes: Vec<CatalogIndexStatus>,
    /// Units the last compaction refused at their smallest size (7.6).
    pub refused_units: u64,
}

/// Which derived indexes `reindex` rebuilds (PC-7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReindexTarget {
    /// The text index.
    Text,
    /// The memory graph.
    Graph,
    /// Every derived index.
    #[default]
    All,
}

/// What `reindex` rebuilt.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ReindexReport {
    /// Active rows the text index re-indexed (`None`: text was not asked for).
    pub rows: Option<u64>,
    /// Whether the memory graph was rebuilt: `false` while the graph has no index on the
    /// catalog to rebuild (until spec 11 T11).
    pub graph_rebuilt: bool,
}

/// One unit of compaction work (a seal, fold or purge of one index class).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CompactUnit {
    /// The index class (its catalog name).
    pub class: String,
    /// `seal`, `fold` or `purge`.
    pub kind: String,
    /// The extents it replaced (or would have), in chain order.
    pub extents: Vec<u64>,
    /// Sediment rows it sealed (or would have).
    pub sediment_rows: u64,
    /// The generation its switch published (`None`: skipped).
    pub generation: Option<u64>,
    /// Why it was skipped (`None`: published).
    pub skipped: Option<String>,
}

/// What one compaction did (`kaban compact`, PC-7): the units that published and those
/// skipped, the reclamation, the bounded vacuum and the checkpoint.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent report flags, each a named outcome"
)]
pub struct CompactReport {
    /// Why the whole pass was skipped before any unit ran (`inputs_exceed_maintenance_memory`,
    /// `maintenance_deadline_exceeded`); `None`: it ran.
    pub pass_skipped: Option<String>,
    /// Units that published, in the order they ran.
    pub published: Vec<CompactUnit>,
    /// Units skipped, in the order they ran.
    pub skipped: Vec<CompactUnit>,
    /// Retired extents destroyed.
    pub reclaimed: Vec<u64>,
    /// Retired extents left for a later round (pinned, behind the barrier, cold).
    pub pending: Vec<u64>,
    /// Allocated container bytes released.
    pub released_bytes: u64,
    /// Bytes only overwritten with zeros: logically gone, no space reclaimed.
    pub zero_filled_bytes: u64,
    /// The WAL was not yet backfilled past a retiring commit (`barrier_pending`).
    pub barrier_pending: bool,
    /// The platform refused a full sync (`degraded_durability`).
    pub degraded_durability: bool,
    /// The platform cannot deallocate container ranges (`space_reclaim_unsupported`).
    pub space_reclaim_unsupported: bool,
    /// SQLite free pages the bounded incremental vacuum released.
    pub vacuumed_pages: u64,
    /// The scheduled checkpoint ran (its interval had elapsed).
    pub checkpointed: bool,
    /// Views past the `[catalog] view_retention` marked `needs_reimport`, in name order.
    pub views_retired: Vec<String>,
}
