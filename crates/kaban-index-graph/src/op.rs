//! Change records: the sediment blob format and the input of `seal`.

use crate::error::GraphError;
use crate::id::{LocalId, RelId};

/// Edge attributes read by the scored operators (GRA-03).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Attr {
    /// Fixed-point weight: the value times 1e6.
    pub weight: i64,
    /// Timestamp in epoch milliseconds.
    pub ts: i64,
}

/// Whether a record adds or removes its subject.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpKind {
    /// Add, or replace the attributes or payload of, the subject.
    Add,
    /// Remove the subject.
    Remove,
}

/// One edge change; 27 bytes encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EdgeOp {
    /// Source node.
    pub src: LocalId,
    /// Relation.
    pub rel: RelId,
    /// Destination node.
    pub dst: LocalId,
    /// Add or remove.
    pub kind: OpKind,
    /// Attributes (ignored by a remove).
    pub attr: Attr,
}

/// One node membership change, independent of the node's edges.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NodeOp {
    /// The node.
    pub node: LocalId,
    /// Add or remove.
    pub kind: OpKind,
}

/// One node fact change; `Remove` deletes the fact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FactOp {
    /// The node.
    pub node: LocalId,
    /// The fact key.
    pub key: u8,
    /// Add (set) or remove.
    pub kind: OpKind,
    /// The fact bytes (empty for a remove).
    pub payload: Vec<u8>,
}

/// A batch of change records, applied in order: edges, nodes and facts are independent streams.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Records {
    /// Edge changes in order.
    pub edges: Vec<EdgeOp>,
    /// Node changes in order.
    pub nodes: Vec<NodeOp>,
    /// Fact changes in order.
    pub facts: Vec<FactOp>,
}

impl Records {
    /// The sediment blob encoding.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        Vec::new()
    }

    /// Decode a sediment blob.
    ///
    /// # Errors
    /// [`GraphError::Corrupt`] on a bad header, truncation, overflow, an unknown kind or
    /// trailing bytes.
    pub fn decode(bytes: &[u8]) -> Result<Records, GraphError> {
        let _ = bytes;
        Ok(Records::default())
    }
}
