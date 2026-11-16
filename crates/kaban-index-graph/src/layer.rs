//! The one extent format and reader shared by the base and every sealed layer (DEL-06).
//!
//! An extent is a header, a section table and the sections it lists, all little-endian:
//!
//! - header (24 bytes): magic `KBXG`, version `u8`, kind `u8` (0 base, 1 layer), reserved
//!   `u16`, depth `u32` (0 for a base, the chain position for a layer), id domain `u64`,
//!   section count `u32`;
//! - section table: per section a tag `u32` and a byte length `u64`; sections follow in table
//!   order and must fill the file exactly;
//! - presence (tag 1, mandatory): a density-chosen id index plus one kind byte per id;
//! - outgoing and incoming edges (tags 2, 3, mandatory): an id index of touched nodes,
//!   `u64` list offsets (touched + 1) and columns aligned by entry: neighbour `u32`, relation
//!   `u16`, kind `u8`, weight `i64`, ts `i64`; each list is sorted by (neighbour, relation);
//! - facts (tag 4, mandatory): count `u64`, columns node `u32`, key `u8`, kind `u8`, payload
//!   offsets `u64` (count + 1), payload bytes; sorted by (node, key);
//! - union of touched nodes over the chain, outgoing and incoming (tags 5, 6, layers only):
//!   bitvectors over the domain.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;

use tantivy::directory::OwnedBytes;

use crate::error::GraphError;
use crate::facts::{FactEntry, Facts};
use crate::op::{Attr, OpKind, Reader, Records, column, corrupt, kind_byte, kind_from, put_len};
use crate::presence::{IdIndex, Presence, parse_bits, test_bit, write_bits};
use crate::view::View;

const MAGIC: &[u8; 4] = b"KBXG";
const VERSION: u8 = 1;
pub(crate) const KIND_BASE: u8 = 0;
pub(crate) const KIND_LAYER: u8 = 1;
const TAG_PRESENCE: u32 = 1;
const TAG_OUT: u32 = 2;
const TAG_IN: u32 = 3;
const TAG_FACTS: u32 = 4;
const TAG_UNION_OUT: u32 = 5;
const TAG_UNION_IN: u32 = 6;
/// Largest id domain: every `u32` id.
const MAX_DOMAIN: u64 = 1 << 32;

/// One edge entry of a list: neighbour, relation, change, attributes.
pub(crate) type EdgeEntry = (u32, u16, OpKind, Attr);

/// Edge changes keyed by (node, neighbour, relation), so iteration is list order.
pub(crate) type EdgeMap = BTreeMap<(u32, u32, u16), (OpKind, Attr)>;

/// The net change of a batch of records: the last record per edge, node and fact wins.
#[derive(Debug, Default)]
pub(crate) struct Net<'p> {
    pub(crate) nodes: BTreeMap<u32, OpKind>,
    pub(crate) out: EdgeMap,
    pub(crate) inc: EdgeMap,
    pub(crate) facts: BTreeMap<(u32, u8), (OpKind, &'p [u8])>,
}

impl<'p> Net<'p> {
    /// Apply `r` in order on top of this net change.
    pub(crate) fn apply(&mut self, r: &'p Records) {
        for e in &r.edges {
            self.out
                .insert((e.src.0, e.dst.0, e.rel.0), (e.kind, e.attr));
            self.inc
                .insert((e.dst.0, e.src.0, e.rel.0), (e.kind, e.attr));
        }
        for n in &r.nodes {
            self.nodes.insert(n.node.0, n.kind);
        }
        for f in &r.facts {
            self.facts
                .insert((f.node.0, f.key), (f.kind, f.payload.as_slice()));
        }
    }

    /// The smallest domain covering every id named here.
    pub(crate) fn domain(&self) -> u64 {
        let top = [
            self.nodes.keys().next_back().copied(),
            self.out.keys().map(|&(s, _, _)| s).max(),
            self.inc.keys().map(|&(d, _, _)| d).max(),
            self.facts.keys().next_back().map(|&(n, _)| n),
        ];
        top.into_iter()
            .flatten()
            .max()
            .map_or(0, |id| u64::from(id) + 1)
    }
}

/// A parsed edge section of one direction.
#[derive(Debug, Clone)]
pub(crate) struct Edges {
    index: IdIndex,
    offsets: OwnedBytes,
    entries: usize,
    nbrs: OwnedBytes,
    rels: OwnedBytes,
    kinds: OwnedBytes,
    weights: OwnedBytes,
    tss: OwnedBytes,
}

impl Edges {
    fn write(out: &mut Vec<u8>, map: &EdgeMap, domain: u64) {
        let mut nodes: Vec<u32> = Vec::new();
        let mut offsets: Vec<u64> = vec![0];
        for (i, &(node, _, _)) in map.keys().enumerate() {
            if nodes.last() != Some(&node) {
                if !nodes.is_empty() {
                    offsets.push(i as u64);
                }
                nodes.push(node);
            }
        }
        if !nodes.is_empty() {
            offsets.push(map.len() as u64);
        }
        IdIndex::write(out, &nodes, domain);
        for o in &offsets {
            out.extend_from_slice(&o.to_le_bytes());
        }
        for &(_, nbr, _) in map.keys() {
            out.extend_from_slice(&nbr.to_le_bytes());
        }
        for &(_, _, rel) in map.keys() {
            out.extend_from_slice(&rel.to_le_bytes());
        }
        out.extend(map.values().map(|&(kind, _)| kind_byte(kind)));
        for (_, attr) in map.values() {
            out.extend_from_slice(&attr.weight.to_le_bytes());
        }
        for (_, attr) in map.values() {
            out.extend_from_slice(&attr.ts.to_le_bytes());
        }
    }

    fn parse(r: &mut Reader<'_>, file: &OwnedBytes, domain: u64) -> Result<Self, GraphError> {
        let index = IdIndex::parse(r, file, domain)?;
        let touched = index.len();
        let noffsets = touched
            .checked_add(1)
            .ok_or_else(|| corrupt("edge offsets"))?;
        let olen = noffsets
            .checked_mul(8)
            .ok_or_else(|| corrupt("edge offsets"))?;
        let offsets = r.slice(file, olen, "edge offsets")?;
        let entries = column::<8>(&offsets, touched)
            .map(u64::from_le_bytes)
            .and_then(|m| usize::try_from(m).ok())
            .ok_or_else(|| corrupt("edge offsets"))?;
        let len = |width: usize| {
            entries
                .checked_mul(width)
                .ok_or_else(|| corrupt("edge columns"))
        };
        Ok(Self {
            nbrs: r.slice(file, len(4)?, "edge neighbours")?,
            rels: r.slice(file, len(2)?, "edge relations")?,
            kinds: r.slice(file, len(1)?, "edge kinds")?,
            weights: r.slice(file, len(8)?, "edge weights")?,
            tss: r.slice(file, len(8)?, "edge timestamps")?,
            index,
            offsets,
            entries,
        })
    }

    /// The entry range of `node`'s list; `None` when untouched or inconsistent.
    pub(crate) fn list(&self, node: u32) -> Option<Range<usize>> {
        let rank = self.index.rank(node)?;
        let off = |i| {
            column::<8>(&self.offsets, i)
                .map(u64::from_le_bytes)
                .and_then(|o| usize::try_from(o).ok())
        };
        let (lo, hi) = (off(rank)?, off(rank + 1)?);
        (lo <= hi && hi <= self.entries).then_some(lo..hi)
    }

    /// Entry `i`.
    pub(crate) fn entry(&self, i: usize) -> Option<EdgeEntry> {
        Some((
            u32::from_le_bytes(column::<4>(&self.nbrs, i)?),
            u16::from_le_bytes(column::<2>(&self.rels, i)?),
            kind_from(*self.kinds.get(i)?).ok()?,
            Attr {
                weight: i64::from_le_bytes(column::<8>(&self.weights, i)?),
                ts: i64::from_le_bytes(column::<8>(&self.tss, i)?),
            },
        ))
    }

    /// Touched nodes ascending.
    pub(crate) fn touched(&self) -> impl Iterator<Item = u32> + '_ {
        self.index.iter()
    }
}

/// A union bitvector of touched nodes.
#[derive(Debug, Clone)]
pub(crate) struct Union {
    words: OwnedBytes,
    nbits: u64,
}

impl Union {
    pub(crate) fn contains(&self, id: u32) -> bool {
        test_bit(&self.words, self.nbits, id)
    }
}

/// A parsed extent: a base or a layer.
#[derive(Debug, Clone)]
pub(crate) struct Extent {
    pub(crate) depth: u32,
    pub(crate) domain: u64,
    pub(crate) presence: Presence,
    pub(crate) out: Edges,
    pub(crate) inc: Edges,
    pub(crate) facts: Facts,
    /// Outgoing and incoming unions (layers only).
    pub(crate) union: Option<(Union, Union)>,
}

impl Extent {
    /// The edge section of one direction (`true` for outgoing).
    pub(crate) fn edges(&self, out: bool) -> &Edges {
        if out { &self.out } else { &self.inc }
    }

    /// Write an extent of `kind` holding `net`; layers also carry the chain unions.
    pub(crate) fn write(
        kind: u8,
        depth: u32,
        domain: u64,
        net: &Net<'_>,
        union: Option<(&[u32], &[u32])>,
    ) -> Vec<u8> {
        let mut sections: Vec<(u32, Vec<u8>)> = Vec::new();
        let mut buf = Vec::new();
        let nodes: Vec<(u32, OpKind)> = net.nodes.iter().map(|(&n, &k)| (n, k)).collect();
        Presence::write(&mut buf, &nodes, domain);
        sections.push((TAG_PRESENCE, std::mem::take(&mut buf)));
        Edges::write(&mut buf, &net.out, domain);
        sections.push((TAG_OUT, std::mem::take(&mut buf)));
        Edges::write(&mut buf, &net.inc, domain);
        sections.push((TAG_IN, std::mem::take(&mut buf)));
        let facts: Vec<FactEntry<'_>> = net.facts.iter().map(|(&k, &(o, p))| (k, o, p)).collect();
        Facts::write(&mut buf, &facts);
        sections.push((TAG_FACTS, std::mem::take(&mut buf)));
        if let Some((out, inc)) = union {
            write_bits(&mut buf, out, domain);
            sections.push((TAG_UNION_OUT, std::mem::take(&mut buf)));
            write_bits(&mut buf, inc, domain);
            sections.push((TAG_UNION_IN, std::mem::take(&mut buf)));
        }
        let body: usize = sections.iter().map(|(_, s)| s.len()).sum();
        let mut file = Vec::with_capacity(24 + 12 * sections.len() + body);
        file.extend_from_slice(MAGIC);
        file.push(VERSION);
        file.push(kind);
        file.extend_from_slice(&0u16.to_le_bytes());
        file.extend_from_slice(&depth.to_le_bytes());
        file.extend_from_slice(&domain.to_le_bytes());
        file.extend_from_slice(
            &u32::try_from(sections.len())
                .unwrap_or(u32::MAX)
                .to_le_bytes(),
        );
        for (tag, s) in &sections {
            file.extend_from_slice(&tag.to_le_bytes());
            put_len(&mut file, s.len());
        }
        for (_, s) in &sections {
            file.extend_from_slice(s);
        }
        file
    }

    /// Parse extent bytes of `kind`; the sections keep sub-slices of `bytes`.
    #[allow(
        clippy::needless_pass_by_value,
        reason = "the public `open` signatures take ownership of the mapped bytes"
    )]
    pub(crate) fn open(bytes: OwnedBytes, kind: u8) -> Result<Self, GraphError> {
        let file = &bytes;
        let mut r = Reader::new(file.as_slice());
        if r.take(MAGIC.len(), "extent magic")? != MAGIC {
            return Err(corrupt("extent magic"));
        }
        if r.u8("extent version")? != VERSION {
            return Err(corrupt("extent version"));
        }
        if r.u8("extent kind")? != kind {
            return Err(corrupt("extent kind"));
        }
        if r.u16("extent reserved")? != 0 {
            return Err(corrupt("extent reserved"));
        }
        let depth = r.u32("extent depth")?;
        let domain = r.u64("extent domain")?;
        if domain > MAX_DOMAIN || (kind == KIND_BASE) != (depth == 0) {
            return Err(corrupt("extent header"));
        }
        let nsections = usize::try_from(r.u32("section count")?)
            .ok()
            .filter(|&n| n.saturating_mul(12) <= r.remaining())
            .ok_or_else(|| corrupt("section table"))?;
        let mut table = Vec::with_capacity(nsections);
        for _ in 0..nsections {
            let tag = r.u32("section tag")?;
            let len =
                usize::try_from(r.u64("section length")?).map_err(|_| corrupt("section length"))?;
            table.push((tag, len));
        }
        let mut presence = None;
        let mut out = None;
        let mut inc = None;
        let mut facts = None;
        let mut union_out = None;
        let mut union_in = None;
        for (tag, len) in table {
            let mut s = r.sub(len, "section")?;
            let dup = match tag {
                TAG_PRESENCE => presence
                    .replace(Presence::parse(&mut s, file, domain)?)
                    .is_some(),
                TAG_OUT => out.replace(Edges::parse(&mut s, file, domain)?).is_some(),
                TAG_IN => inc.replace(Edges::parse(&mut s, file, domain)?).is_some(),
                TAG_FACTS => facts.replace(Facts::parse(&mut s, file)?).is_some(),
                TAG_UNION_OUT if kind == KIND_LAYER => union_out
                    .replace(parse_union(&mut s, file, domain)?)
                    .is_some(),
                TAG_UNION_IN if kind == KIND_LAYER => union_in
                    .replace(parse_union(&mut s, file, domain)?)
                    .is_some(),
                _ => return Err(corrupt("section tag")),
            };
            if dup {
                return Err(corrupt("duplicate section"));
            }
            s.finish("section length")?;
        }
        r.finish("extent length")?;
        let missing = |what: &str| corrupt(&format!("missing {what} section"));
        let union = match (union_out, union_in) {
            (Some(o), Some(i)) => Some((o, i)),
            (None, None) if kind == KIND_BASE => None,
            _ => return Err(missing("union")),
        };
        Ok(Self {
            depth,
            domain,
            presence: presence.ok_or_else(|| missing("presence"))?,
            out: out.ok_or_else(|| missing("outgoing edge"))?,
            inc: inc.ok_or_else(|| missing("incoming edge"))?,
            facts: facts.ok_or_else(|| missing("fact"))?,
            union,
        })
    }
}

fn parse_union(r: &mut Reader<'_>, file: &OwnedBytes, domain: u64) -> Result<Union, GraphError> {
    let (words, nbits) = parse_bits(r, file, domain)?;
    Ok(Union { words, nbits })
}

/// An immutable sealed layer over its parent view.
#[derive(Debug, Clone)]
pub struct Layer {
    pub(crate) extent: Extent,
}

impl Layer {
    /// Open sealed layer bytes.
    ///
    /// # Errors
    /// [`GraphError::Corrupt`] when the bytes are not a well-formed layer: truncated, padded,
    /// a base, an unknown or duplicate section, or a missing presence, edge, fact or union
    /// section.
    pub fn open(bytes: OwnedBytes) -> Result<Layer, GraphError> {
        Ok(Layer {
            extent: Extent::open(bytes, KIND_LAYER)?,
        })
    }
}

/// Seal `r` as a new layer over `parent`; the layer stores the net change per edge, node and
/// fact, every list sorted, plus the cumulative union of touched nodes over the chain.
#[must_use]
pub fn seal(parent: &View<'_>, r: &Records) -> Vec<u8> {
    let mut net = Net::default();
    net.apply(r);
    let domain = parent.domain().max(net.domain());
    let depth = u32::try_from(parent.layers().len() + 1).unwrap_or(u32::MAX);
    let union = |out: bool| -> Vec<u32> {
        let mut set: BTreeSet<u32> = parent
            .layers()
            .iter()
            .flat_map(|l| l.extent.edges(out).touched())
            .collect();
        let own = if out { &net.out } else { &net.inc };
        set.extend(own.keys().map(|&(n, _, _)| n));
        set.into_iter().collect()
    };
    let (out, inc) = (union(true), union(false));
    Extent::write(KIND_LAYER, depth, domain, &net, Some((&out, &inc)))
}
