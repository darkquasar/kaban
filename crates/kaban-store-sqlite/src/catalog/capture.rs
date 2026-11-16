//! Change capture without writer cooperation (spec 11 KER-03, INV-5): TEMP insert, update and
//! delete triggers on `node` and `edge` queue each change's identity, operation and the
//! before/after values the index classes need (existence, label, lifecycle), deletion values
//! before they disappear, FK cascade deletes included. [`super::Catalog::commit`] freezes and
//! drains the queue once after the writer closure; every consumer sees that one immutable set.

use kaban_core::Result;
use rusqlite::Connection;

/// What happened to a captured row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CaptureOp {
    /// The row was inserted.
    Insert,
    /// The row was updated.
    Update,
    /// The row was deleted.
    Delete,
}

/// The captured values of a node row on one side of a change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NodeImage {
    /// The node's label (`kind`).
    pub(crate) kind: String,
    /// The node's lifecycle.
    pub(crate) lifecycle: String,
}

/// One captured node change; a side that does not exist (before an insert, after a delete) is
/// `None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NodeChange {
    /// The node's local id (`node.rid`, PC-1).
    pub(crate) rid: i64,
    /// The operation.
    pub(crate) op: CaptureOp,
    /// The row before the change.
    pub(crate) before: Option<NodeImage>,
    /// The row after the change.
    pub(crate) after: Option<NodeImage>,
}

/// One captured edge change, by the edge's identity `(src, dst, rel)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EdgeChange {
    /// The source node's `rid`.
    pub(crate) src: i64,
    /// The target node's `rid`.
    pub(crate) dst: i64,
    /// The relationship type.
    pub(crate) rel: String,
    /// The operation.
    pub(crate) op: CaptureOp,
}

/// The frozen change set of one commit, in capture order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct BatchChanges {
    nodes: Vec<NodeChange>,
    edges: Vec<EdgeChange>,
}

impl BatchChanges {
    /// The captured node changes.
    pub(crate) fn nodes(&self) -> &[NodeChange] {
        &self.nodes
    }
    /// The captured edge changes.
    pub(crate) fn edges(&self) -> &[EdgeChange] {
        &self.edges
    }
    /// Whether the commit changed no captured row.
    pub(crate) fn is_empty(&self) -> bool {
        self.nodes.is_empty() && self.edges.is_empty()
    }
}

/// Install the TEMP capture triggers and their queue on a writer connection at open.
///
/// # Errors
/// SQLite errors (T5 Step 3).
pub(crate) fn install(conn: &Connection) -> Result<()> {
    let _ = conn;
    Ok(())
}

/// Freeze and drain the queue of the open transaction on `conn`: called once per commit.
///
/// # Errors
/// SQLite errors (T5 Step 3).
pub(crate) fn drain(conn: &Connection) -> Result<BatchChanges> {
    let _ = conn;
    Ok(BatchChanges::default())
}
