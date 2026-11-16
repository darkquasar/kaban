//! Change records: the sediment blob format and the input of `seal`.

use kaban_core::query_budget::{BudgetLease, QueryBudget};
use tantivy::directory::OwnedBytes;

use crate::charge::{Charge, Ticker};
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

/// One node membership change.
///
/// Adding a node changes only its membership. Removing a node is also a deletion barrier: every
/// edge with the node as an endpoint (either direction) and every fact of the node recorded
/// before the removal is deleted, so a later re-add starts with no edges and no facts. Within
/// one [`Records`] batch the node stream applies first, then the edge and fact streams: a batch
/// that removes and re-adds a node, or removes it, still keeps its own edges and facts of that
/// node (hidden while the node is absent). The barrier holds wherever sediment, [`crate::seal`]
/// and [`crate::fold`] boundaries fall between batches: identical histories give identical
/// views. A producer that re-emits a node must emit its current edges and facts again.
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

/// A batch of change records: the node stream applies first (removals are barriers, see
/// [`NodeOp`]), then the edge and fact streams, each in order; the newest record per key wins.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Records {
    /// Edge changes in order.
    pub edges: Vec<EdgeOp>,
    /// Node changes in order.
    pub nodes: Vec<NodeOp>,
    /// Fact changes in order.
    pub facts: Vec<FactOp>,
}

const RECORDS_MAGIC: &[u8; 4] = b"KBRC";
const RECORDS_VERSION: u8 = 1;
/// Encoded bytes of one [`EdgeOp`]: src 4, rel 2, dst 4, kind 1, weight 8, ts 8.
const EDGE_BYTES: usize = 27;
const NODE_BYTES: usize = 5;
/// The fixed part of one encoded [`FactOp`]: node 4, key 1, kind 1, payload length 8.
const FACT_FIXED_BYTES: usize = 14;
/// Payload bytes copied per unit of work.
pub(crate) const COPY_CHUNK: usize = 4096;

impl Records {
    /// The sediment blob encoding: a magic and version, then the edge, node and fact streams,
    /// each a `u64` count followed by fixed-width little-endian records.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let payload: usize = self.facts.iter().map(|f| f.payload.len()).sum();
        let mut out = Vec::with_capacity(
            RECORDS_MAGIC.len()
                + 1
                + 24
                + self.edges.len() * EDGE_BYTES
                + self.nodes.len() * NODE_BYTES
                + self.facts.len() * FACT_FIXED_BYTES
                + payload,
        );
        out.extend_from_slice(RECORDS_MAGIC);
        out.push(RECORDS_VERSION);
        put_len(&mut out, self.edges.len());
        for e in &self.edges {
            out.extend_from_slice(&e.src.0.to_le_bytes());
            out.extend_from_slice(&e.rel.0.to_le_bytes());
            out.extend_from_slice(&e.dst.0.to_le_bytes());
            out.push(kind_byte(e.kind));
            out.extend_from_slice(&e.attr.weight.to_le_bytes());
            out.extend_from_slice(&e.attr.ts.to_le_bytes());
        }
        put_len(&mut out, self.nodes.len());
        for n in &self.nodes {
            out.extend_from_slice(&n.node.0.to_le_bytes());
            out.push(kind_byte(n.kind));
        }
        put_len(&mut out, self.facts.len());
        for f in &self.facts {
            out.extend_from_slice(&f.node.0.to_le_bytes());
            out.push(f.key);
            out.push(kind_byte(f.kind));
            put_len(&mut out, f.payload.len());
            out.extend_from_slice(&f.payload);
        }
        out
    }

    /// Decode a sediment blob into query-owned records. Every vector and fact payload is
    /// reserved against `budget` (its capacity plus allocation metadata) before it is
    /// allocated; the charge stays live as long as the returned [`DecodedRecords`] and is
    /// released on drop or on error. Every record and every [`COPY_CHUNK`] payload bytes is one
    /// unit of work, checked every [`crate::BUDGET_CHECK_INTERVAL`] units.
    ///
    /// # Errors
    /// [`GraphError::Corrupt`] on a bad header, truncation, overflow, an unknown kind or
    /// trailing bytes; [`GraphError::BudgetExceeded`] when `budget` refuses a reservation, already
    /// refuses at entry (a recorded breach) or is cancelled or past its deadline while decoding.
    pub fn decode(bytes: &[u8], budget: &QueryBudget) -> Result<DecodedRecords, GraphError> {
        let mut charge = Charge::new(budget)?;
        let t = Ticker::new(budget);
        let mut r = Reader::new(bytes);
        if r.take(RECORDS_MAGIC.len(), "records magic")? != RECORDS_MAGIC {
            return Err(corrupt("records magic"));
        }
        if r.u8("records version")? != RECORDS_VERSION {
            return Err(corrupt("records version"));
        }
        let count = r.count(EDGE_BYTES, "edge records")?;
        let mut edges = charge.vec(count)?;
        for _ in 0..count {
            t.tick()?;
            edges.push(EdgeOp {
                src: LocalId(r.u32("edge src")?),
                rel: RelId(r.u16("edge rel")?),
                dst: LocalId(r.u32("edge dst")?),
                kind: kind_from(r.u8("edge kind")?)?,
                attr: Attr {
                    weight: r.i64("edge weight")?,
                    ts: r.i64("edge ts")?,
                },
            });
        }
        let count = r.count(NODE_BYTES, "node records")?;
        let mut nodes = charge.vec(count)?;
        for _ in 0..count {
            t.tick()?;
            nodes.push(NodeOp {
                node: LocalId(r.u32("node id")?),
                kind: kind_from(r.u8("node kind")?)?,
            });
        }
        let count = r.count(FACT_FIXED_BYTES, "fact records")?;
        let mut facts = charge.vec(count)?;
        for _ in 0..count {
            t.tick()?;
            let node = LocalId(r.u32("fact node")?);
            let key = r.u8("fact key")?;
            let kind = kind_from(r.u8("fact kind")?)?;
            let len = r.count(1, "fact payload")?;
            let mut payload = charge.vec(len)?;
            for chunk in r.take(len, "fact payload")?.chunks(COPY_CHUNK) {
                t.tick()?;
                payload.extend_from_slice(chunk);
            }
            facts.push(FactOp {
                node,
                key,
                kind,
                payload,
            });
        }
        r.finish("records")?;
        Ok(DecodedRecords {
            records: Records {
                edges,
                nodes,
                facts,
            },
            _lease: charge.into_lease(),
        })
    }
}

impl AsRef<Records> for Records {
    fn as_ref(&self) -> &Records {
        self
    }
}

/// Records decoded for a query, holding their budget charge while they live.
#[derive(Debug)]
pub struct DecodedRecords {
    records: Records,
    _lease: BudgetLease,
}

impl DecodedRecords {
    /// The decoded records.
    #[must_use]
    pub fn records(&self) -> &Records {
        &self.records
    }
}

impl AsRef<Records> for DecodedRecords {
    fn as_ref(&self) -> &Records {
        &self.records
    }
}

/// A [`GraphError::Corrupt`] naming what failed to decode.
pub(crate) fn corrupt(what: &str) -> GraphError {
    GraphError::Corrupt(format!("{what}: truncated, overflowing or invalid"))
}

pub(crate) fn kind_byte(kind: OpKind) -> u8 {
    match kind {
        OpKind::Add => 0,
        OpKind::Remove => 1,
    }
}

pub(crate) fn kind_from(byte: u8) -> Result<OpKind, GraphError> {
    match byte {
        0 => Ok(OpKind::Add),
        1 => Ok(OpKind::Remove),
        _ => Err(corrupt("op kind")),
    }
}

/// Append a length as a little-endian `u64`.
pub(crate) fn put_len(out: &mut Vec<u8>, len: usize) {
    out.extend_from_slice(&(len as u64).to_le_bytes());
}

/// A bounds-checked little-endian reader: every read past the end is [`GraphError::Corrupt`].
#[derive(Debug)]
pub(crate) struct Reader<'b> {
    bytes: &'b [u8],
    pos: usize,
}

impl<'b> Reader<'b> {
    pub(crate) fn new(bytes: &'b [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    /// The next `len` bytes.
    pub(crate) fn take(&mut self, len: usize, what: &str) -> Result<&'b [u8], GraphError> {
        let end = self.pos.checked_add(len).ok_or_else(|| corrupt(what))?;
        let slice = self.bytes.get(self.pos..end).ok_or_else(|| corrupt(what))?;
        self.pos = end;
        Ok(slice)
    }

    /// The next `len` bytes as a sub-slice of `file` (the buffer this reader reads).
    pub(crate) fn slice(
        &mut self,
        file: &OwnedBytes,
        len: usize,
        what: &str,
    ) -> Result<OwnedBytes, GraphError> {
        let start = self.pos;
        self.take(len, what)?;
        if self.pos > file.len() {
            return Err(corrupt(what));
        }
        Ok(file.slice(start..self.pos))
    }

    /// A reader over the next `len` bytes only; positions stay absolute.
    pub(crate) fn sub(&mut self, len: usize, what: &str) -> Result<Reader<'b>, GraphError> {
        let start = self.pos;
        self.take(len, what)?;
        let bytes = self.bytes.get(..self.pos).ok_or_else(|| corrupt(what))?;
        Ok(Reader { bytes, pos: start })
    }

    fn array<const N: usize>(&mut self, what: &str) -> Result<[u8; N], GraphError> {
        self.take(N, what)?.try_into().map_err(|_| corrupt(what))
    }

    pub(crate) fn u8(&mut self, what: &str) -> Result<u8, GraphError> {
        Ok(self.array::<1>(what)?[0])
    }

    pub(crate) fn u16(&mut self, what: &str) -> Result<u16, GraphError> {
        self.array(what).map(u16::from_le_bytes)
    }

    pub(crate) fn u32(&mut self, what: &str) -> Result<u32, GraphError> {
        self.array(what).map(u32::from_le_bytes)
    }

    pub(crate) fn u64(&mut self, what: &str) -> Result<u64, GraphError> {
        self.array(what).map(u64::from_le_bytes)
    }

    pub(crate) fn i64(&mut self, what: &str) -> Result<i64, GraphError> {
        self.array(what).map(i64::from_le_bytes)
    }

    /// A `u64` count of items of at least `item_bytes` each, refused when the remaining bytes
    /// cannot hold them (so no allocation is sized from a corrupt count).
    pub(crate) fn count(&mut self, item_bytes: usize, what: &str) -> Result<usize, GraphError> {
        let count = usize::try_from(self.u64(what)?).map_err(|_| corrupt(what))?;
        let need = count.checked_mul(item_bytes).ok_or_else(|| corrupt(what))?;
        if need > self.remaining() {
            return Err(corrupt(what));
        }
        Ok(count)
    }

    pub(crate) fn remaining(&self) -> usize {
        self.bytes.len().saturating_sub(self.pos)
    }

    /// Refuse trailing bytes.
    pub(crate) fn finish(&self, what: &str) -> Result<(), GraphError> {
        if self.remaining() == 0 {
            Ok(())
        } else {
            Err(corrupt(what))
        }
    }
}

/// Element `i` of a packed little-endian column of `N`-byte values.
pub(crate) fn column<const N: usize>(bytes: &[u8], i: usize) -> Option<[u8; N]> {
    let start = i.checked_mul(N)?;
    bytes.get(start..start.checked_add(N)?)?.try_into().ok()
}
