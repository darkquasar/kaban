//! `ChangeLog` types (core-spec F11; engine-spec EN-E10b, OWN-53): the `changes` rows that sinks
//! consume, and the cursor vocabulary shared with leases (F15). The table was called the
//! "outbox" before E2.0 (EN-E10b); the table name `changes` is unchanged.

use serde::{Deserialize, Serialize};

/// Which table a change row describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum ChangeEntity {
    /// A `node` row.
    Node,
    /// An `edge` row.
    Edge,
}

impl ChangeEntity {
    /// Stable lowercase name used in the `changes.entity` column.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Node => "node",
            Self::Edge => "edge",
        }
    }
    /// Parse a column value.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "node" => Some(Self::Node),
            "edge" => Some(Self::Edge),
            _ => None,
        }
    }
}

/// What happened to the row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum ChangeOp {
    /// Row inserted.
    Insert,
    /// Row updated (any column; a purge is not an update).
    Update,
    /// Row deleted.
    Delete,
    /// Node purged: content erased, edges deleted, the id row kept (F5).
    Purge,
}

impl ChangeOp {
    /// Stable lowercase name used in the `changes.op` column.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Insert => "insert",
            Self::Update => "update",
            Self::Delete => "delete",
            Self::Purge => "purge",
        }
    }
    /// Parse a column value.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "insert" => Some(Self::Insert),
            "update" => Some(Self::Update),
            "delete" => Some(Self::Delete),
            "purge" => Some(Self::Purge),
            _ => None,
        }
    }
}

/// One `ChangeLog` row. Carries ids only, never content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Change {
    /// Position in the `ChangeLog` (strictly increasing while the newest row is never pruned).
    pub seq: u64,
    /// Which table.
    pub entity: ChangeEntity,
    /// What happened.
    pub op: ChangeOp,
    /// Row id (node or edge `UUIDv7`).
    pub id: String,
    /// Workspace of the row (F17 tenant key).
    pub workspace_id: String,
    /// Unix time (seconds) the row was written.
    pub ts: i64,
    /// Serialized paths touched (docs-ingest), when recorded.
    pub paths: Option<String>,
}
