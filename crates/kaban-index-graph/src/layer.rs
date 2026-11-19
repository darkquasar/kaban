//! The one extent format and reader shared by the base and every sealed layer (DEL-06).
//!
//! An extent is a header, a section table and the sections it lists, all little-endian:
//!
//! - header (24 bytes): magic `KBXG`, version `u8` (2), kind `u8` (0 base, 1 layer), reserved
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
//! - union of touched nodes over the chain, outgoing and incoming (tags 5, 6, layers only): the
//!   cumulative set of nodes whose list some layer of the chain touched, each written as an id
//!   index in the form its density picks when the layer is sealed, by the presence rule: form
//!   byte 0, a sorted list (count `u64`, ascending `u32` ids), when touched * 30 <= domain;
//!   form byte 1, the bitvector over the domain with its rank samples, otherwise. Any other
//!   form byte is corrupt. A list costs 9 + 4t bytes for t ids, a bitvector over d ids
//!   17 + 8 ceil(d / 64) + 8 ceil(d / 512), about 9d / 64; they break even at t = 9d / 256
//!   (about d / 28.4). The rule switches at t = d / 30, where the list (about 0.133d) is still
//!   the smaller, so the bitvector chosen between d / 30 and d / 28.4 is at most about 5.5%
//!   larger than the list, and everywhere else the chosen form is the smaller one. Membership
//!   is a binary search in a list and one bit probe in a bitvector.
//!
//! Version 2 replaced version 1's always-bitvector unions with these density-chosen indexes;
//! any other version is refused as corrupt.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;

use kaban_core::query_budget::QueryBudget;
use tantivy::directory::OwnedBytes;

use crate::charge::{Phase, Ticker};
use crate::error::GraphError;
use crate::facts::{FactEntry, Facts};
use crate::id::LocalId;
use crate::op::{Attr, OpKind, Reader, Records, column, corrupt, kind_byte, kind_from, put_len};
use crate::presence::{IdIndex, IdIter, Presence, check_kinds};
use crate::view::View;

const MAGIC: &[u8; 4] = b"KBXG";
const VERSION: u8 = 2;
pub(crate) const KIND_BASE: u8 = 0;
pub(crate) const KIND_LAYER: u8 = 1;
const TAG_PRESENCE: u32 = 1;
const TAG_OUT: u32 = 2;
const TAG_IN: u32 = 3;
const TAG_FACTS: u32 = 4;
const TAG_UNION_OUT: u32 = 5;
const TAG_UNION_IN: u32 = 6;
/// Section tags in their mandatory order; a base has the first four.
const SECTION_TAGS: [u32; 6] = [
    TAG_PRESENCE,
    TAG_OUT,
    TAG_IN,
    TAG_FACTS,
    TAG_UNION_OUT,
    TAG_UNION_IN,
];
/// Largest id domain: every `u32` id.
const MAX_DOMAIN: u64 = 1 << 32;

/// One edge entry of a list: neighbour, relation, change, attributes.
pub(crate) type EdgeEntry = (u32, u16, OpKind, Attr);

/// Edge changes keyed by (node, neighbour, relation), so iteration is list order.
pub(crate) type EdgeMap = BTreeMap<(u32, u32, u16), (OpKind, Attr)>;

/// One edge row of a section: (node, neighbour, relation), change, attributes.
pub(crate) type EdgeRow = ((u32, u32, u16), OpKind, Attr);

/// The rows of an extent's sections, each sorted by its key and key-unique: presence by id,
/// edges by (node, neighbour, relation), facts by (node, key).
#[derive(Debug, Default)]
pub(crate) struct Sections<'p> {
    pub(crate) nodes: Vec<(u32, OpKind)>,
    pub(crate) out: Vec<EdgeRow>,
    pub(crate) inc: Vec<EdgeRow>,
    pub(crate) facts: Vec<FactEntry<'p>>,
}

impl Sections<'_> {
    /// The smallest domain covering every id named here.
    fn domain(&self) -> u64 {
        let top = [
            self.nodes.last().map(|&(n, _)| n),
            self.out.last().map(|&((s, _, _), _, _)| s),
            self.inc.last().map(|&((d, _, _), _, _)| d),
            self.facts.iter().map(|&((n, _), _, _)| n).max(),
        ];
        top.into_iter()
            .flatten()
            .max()
            .map_or(0, |id| u64::from(id) + 1)
    }
}

/// The net change of a batch of records: the last record per edge, node and fact wins. Node
/// removal barriers are applied by the callers that have older data ([`seal`]).
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

    /// The section rows of this net change, in key order (the maps iterate sorted).
    pub(crate) fn sections(&self) -> Sections<'p> {
        let edges = |map: &EdgeMap| map.iter().map(|(&k, &(o, a))| (k, o, a)).collect();
        Sections {
            nodes: self.nodes.iter().map(|(&n, &k)| (n, k)).collect(),
            out: edges(&self.out),
            inc: edges(&self.inc),
            facts: self.facts.iter().map(|(&k, &(o, p))| (k, o, p)).collect(),
        }
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
    /// Write the section of `rows` (sorted and key-unique).
    fn write(out: &mut Vec<u8>, rows: &[EdgeRow], domain: u64) {
        let mut nodes: Vec<u32> = Vec::new();
        let mut offsets: Vec<u64> = vec![0];
        for (i, &((node, _, _), _, _)) in rows.iter().enumerate() {
            if nodes.last() != Some(&node) {
                if !nodes.is_empty() {
                    offsets.push(i as u64);
                }
                nodes.push(node);
            }
        }
        if !nodes.is_empty() {
            offsets.push(rows.len() as u64);
        }
        IdIndex::write(out, &nodes, domain);
        for o in &offsets {
            out.extend_from_slice(&o.to_le_bytes());
        }
        for &((_, nbr, _), _, _) in rows {
            out.extend_from_slice(&nbr.to_le_bytes());
        }
        for &((_, _, rel), _, _) in rows {
            out.extend_from_slice(&rel.to_le_bytes());
        }
        out.extend(rows.iter().map(|&(_, kind, _)| kind_byte(kind)));
        for (_, _, attr) in rows {
            out.extend_from_slice(&attr.weight.to_le_bytes());
        }
        for (_, _, attr) in rows {
            out.extend_from_slice(&attr.ts.to_le_bytes());
        }
    }

    fn parse(
        r: &mut Reader<'_>,
        file: &OwnedBytes,
        domain: u64,
        t: &Ticker<'_>,
    ) -> Result<Self, GraphError> {
        let index = IdIndex::parse(r, file, domain, t)?;
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
        let edges = Self {
            nbrs: r.slice(file, len(4)?, "edge neighbours")?,
            rels: r.slice(file, len(2)?, "edge relations")?,
            kinds: r.slice(file, len(1)?, "edge kinds")?,
            weights: r.slice(file, len(8)?, "edge weights")?,
            tss: r.slice(file, len(8)?, "edge timestamps")?,
            index,
            offsets,
            entries,
        };
        edges.validate(domain, t)?;
        Ok(edges)
    }

    /// One pass over the section: offsets start at zero and never decrease, every kind is
    /// known, and every list strictly ascends by (neighbour, relation) with neighbours below
    /// `domain`.
    fn validate(&self, domain: u64, t: &Ticker<'_>) -> Result<(), GraphError> {
        check_kinds(&self.kinds, "edge kinds", t)?;
        let mut start = 0usize;
        for (i, &o) in self.offsets.as_chunks::<8>().0.iter().enumerate() {
            t.tick(Phase::Validate)?;
            let end =
                usize::try_from(u64::from_le_bytes(o)).map_err(|_| corrupt("edge offsets"))?;
            if (i == 0 && end != 0) || end < start || end > self.entries {
                return Err(corrupt("edge offsets"));
            }
            let mut prev: Option<(u32, u16)> = None;
            for e in start..end {
                t.tick(Phase::Validate)?;
                let key = column::<4>(&self.nbrs, e)
                    .map(u32::from_le_bytes)
                    .zip(column::<2>(&self.rels, e).map(u16::from_le_bytes))
                    .ok_or_else(|| corrupt("edge columns"))?;
                if u64::from(key.0) >= domain || prev.is_some_and(|p| p >= key) {
                    return Err(corrupt("edge list order or domain"));
                }
                prev = Some(key);
            }
            start = end;
        }
        Ok(())
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

    /// A metered cursor over the touched nodes, ascending.
    pub(crate) fn touched_cursor(&self) -> IdIter<'_> {
        self.index.cursor()
    }

    /// Touched nodes ascending (writer paths: unmetered).
    pub(crate) fn touched(&self) -> impl Iterator<Item = u32> + '_ {
        let mut ids = self.index.cursor();
        let t = Ticker::unmetered();
        std::iter::from_fn(move || ids.next(&t, Phase::Writer))
    }

    /// Whether `n` has a list here or is the neighbour of any entry.
    fn names(&self, n: u32) -> bool {
        self.index.rank(n).is_some()
            || (0..self.entries)
                .any(|i| column::<4>(&self.nbrs, i).map(u32::from_le_bytes) == Some(n))
    }
}

/// The union of touched nodes over a chain, in one direction: a density-chosen id index.
#[derive(Debug, Clone)]
pub(crate) struct Union(IdIndex);

impl Union {
    /// Whether some layer of the chain touched `id`'s list.
    pub(crate) fn contains(&self, id: u32) -> bool {
        self.0.contains(id)
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
    /// Whether any entry of this extent names `n`: its presence, an edge list of it or an edge
    /// to it, a fact of it, or a chain union.
    pub(crate) fn mentions(&self, n: u32) -> bool {
        self.presence.get(n).is_some()
            || self.out.names(n)
            || self.inc.names(n)
            || self.facts.of_node(n).next().is_some()
            || self
                .union
                .as_ref()
                .is_some_and(|(out, inc)| out.contains(n) || inc.contains(n))
    }

    /// The edge section of one direction (`true` for outgoing).
    pub(crate) fn edges(&self, out: bool) -> &Edges {
        if out { &self.out } else { &self.inc }
    }

    /// Write an extent of `kind` holding `rows`; layers also carry the chain unions.
    pub(crate) fn write(
        kind: u8,
        depth: u32,
        domain: u64,
        rows: &Sections<'_>,
        union: Option<(&[u32], &[u32])>,
    ) -> Vec<u8> {
        let mut sections: Vec<(u32, Vec<u8>)> = Vec::new();
        let mut buf = Vec::new();
        Presence::write(&mut buf, &rows.nodes, domain);
        sections.push((TAG_PRESENCE, std::mem::take(&mut buf)));
        Edges::write(&mut buf, &rows.out, domain);
        sections.push((TAG_OUT, std::mem::take(&mut buf)));
        Edges::write(&mut buf, &rows.inc, domain);
        sections.push((TAG_IN, std::mem::take(&mut buf)));
        Facts::write(&mut buf, &rows.facts);
        sections.push((TAG_FACTS, std::mem::take(&mut buf)));
        if let Some((out, inc)) = union {
            IdIndex::write(&mut buf, out, domain);
            sections.push((TAG_UNION_OUT, std::mem::take(&mut buf)));
            IdIndex::write(&mut buf, inc, domain);
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
    pub(crate) fn open(
        bytes: OwnedBytes,
        kind: u8,
        ticker: &Ticker<'_>,
    ) -> Result<Self, GraphError> {
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
        // This version has exactly four base sections or six layer sections, in tag order;
        // the count is checked before anything is read or allocated from it.
        let expected = if kind == KIND_BASE {
            4
        } else {
            SECTION_TAGS.len()
        };
        if usize::try_from(r.u32("section count")?).ok() != Some(expected) {
            return Err(corrupt("section count"));
        }
        let mut lens = [0usize; SECTION_TAGS.len()];
        for (len, &want) in lens.iter_mut().zip(&SECTION_TAGS).take(expected) {
            if r.u32("section tag")? != want {
                return Err(corrupt("section tag or order"));
            }
            *len =
                usize::try_from(r.u64("section length")?).map_err(|_| corrupt("section length"))?;
        }
        let mut section = |i: usize| r.sub(lens[i], "section");
        let mut s = section(0)?;
        let presence = Presence::parse(&mut s, file, domain, ticker)?;
        s.finish("section length")?;
        let mut s = section(1)?;
        let out = Edges::parse(&mut s, file, domain, ticker)?;
        s.finish("section length")?;
        let mut s = section(2)?;
        let inc = Edges::parse(&mut s, file, domain, ticker)?;
        s.finish("section length")?;
        let mut s = section(3)?;
        let facts = Facts::parse(&mut s, file, domain, ticker)?;
        s.finish("section length")?;
        let union = if kind == KIND_LAYER {
            let mut s = section(4)?;
            let o = parse_union(&mut s, file, domain, ticker)?;
            s.finish("section length")?;
            let mut s = section(5)?;
            let i = parse_union(&mut s, file, domain, ticker)?;
            s.finish("section length")?;
            Some((o, i))
        } else {
            None
        };
        r.finish("extent length")?;
        Ok(Self {
            depth,
            domain,
            presence,
            out,
            inc,
            facts,
            union,
        })
    }
}

/// Parse and validate a union section, either form, as every other id index.
fn parse_union(
    r: &mut Reader<'_>,
    file: &OwnedBytes,
    domain: u64,
    t: &Ticker<'_>,
) -> Result<Union, GraphError> {
    IdIndex::parse(r, file, domain, t).map(Union)
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
    /// a base, a section count other than six, sections out of tag order, or a broken section
    /// invariant. Opening validates every section in one pass over its bytes (O(bytes)): an
    /// unknown kind byte; sorted ids, neighbour lists or fact keys that do not strictly ascend;
    /// list or payload offsets that do not start at zero or decrease; ids at or past the
    /// domain; bitvector bits set past the domain; a bit-index count or rank sample that does
    /// not match the bits; a union form byte other than list or bitvector. After a successful
    /// open every lookup is exact, so corruption is never read as absence.
    ///
    /// Validation is per section: it does not check that the outgoing and incoming sections
    /// are transposes of each other, and nothing in a layer names the parent it was sealed
    /// over, so ancestry is the caller's to record; [`View::new`] checks only that the newest
    /// union covers every layer it is given.
    ///
    /// Validation runs under `budget` (the loading query's deadline and cancellation): every
    /// validated id, entry, offset, rank-sample block and 256 kind bytes is one unit of work,
    /// checked every [`crate::BUDGET_CHECK_INTERVAL`] units. Opening allocates nothing.
    ///
    /// [`GraphError::BudgetExceeded`] when `budget` already refuses at entry or is cancelled or
    /// past its deadline during validation.
    pub fn open(bytes: OwnedBytes, budget: &QueryBudget) -> Result<Layer, GraphError> {
        budget.check()?;
        let t = Ticker::new(budget);
        Ok(Layer {
            extent: Extent::open(bytes, KIND_LAYER, &t)?,
        })
    }

    /// Whether any entry of this layer names `n` (presence, edges in either direction, facts or
    /// the chain unions, a removal included): how a purge is checked.
    #[must_use]
    pub fn mentions(&self, n: LocalId) -> bool {
        self.extent.mentions(n.0)
    }
}

/// Seal `r` as a new layer over the chain of `parent`: its base and layers. The sediment of
/// `parent` is not part of that chain and is ignored, so a caller may seal a batch from a loaded
/// view that still holds the same batch (or any other) as sediment. The layer stores the net
/// change per edge, node and fact, every list sorted, plus the cumulative union of touched nodes
/// over the chain in each direction, each written as a sorted list or a bitvector by density
/// (see the format above).
///
/// A node removal in `r` is a deletion barrier ([`crate::NodeOp`]): the layer materialises a
/// tombstone for every edge of the node (both directions, presence not applied) and every fact
/// of the node that survives in the chain, before `r`'s own edge and fact records apply. So a
/// later re-add never shows data from before the removal. The id domain is the chain's domain
/// widened by `r`.
///
/// This is the single-batch entry, kept for writers that hold one decoded batch; it writes
/// through the same tombstone and section path as [`crate::PreparedSediment::seal`], which
/// seals several batches (runs) in order and is what a writer holding prepared sediment uses.
#[must_use]
pub fn seal(parent: &View<'_>, r: &Records) -> Vec<u8> {
    let removed: BTreeSet<u32> = r
        .nodes
        .iter()
        .filter(|op| op.kind == OpKind::Remove)
        .map(|op| op.node.0)
        .collect();
    let removed: Vec<u32> = removed.into_iter().collect();
    let mut net = Net::default();
    net.apply(r);
    seal_over(parent, &removed, net.sections())
}

/// Overlay `over` on `under` (both sorted and key-unique by `key`) by one linear merge: at an
/// equal key the `over` row wins.
fn overlay<T, K: Ord>(under: Vec<T>, over: Vec<T>, key: fn(&T) -> K) -> Vec<T> {
    let mut merged = Vec::with_capacity(under.len() + over.len());
    let mut under = under.into_iter().peekable();
    for row in over {
        while let Some(old) = under.next_if(|old| key(old) < key(&row)) {
            merged.push(old);
        }
        under.next_if(|old| key(old) == key(&row));
        merged.push(row);
    }
    merged.extend(under);
    merged
}

/// Write `own` (the net change being sealed, each section sorted, newest record per key,
/// barriers among its own records applied) as a layer over the chain of `parent`. Every
/// `removed` node (ascending) is a barrier over the chain: its chain edges (both directions,
/// presence not applied) and facts get tombstones, over which `own`'s rows apply. Only the
/// tombstones are sorted (they come from scattered lists); `own` is merged as it is.
pub(crate) fn seal_over(parent: &View<'_>, removed: &[u32], own: Sections<'_>) -> Vec<u8> {
    let parent = parent.chain();
    let gone = |key| (key, OpKind::Remove, Attr::default());
    let (mut out, mut inc, mut facts) = (Vec::new(), Vec::new(), Vec::new());
    for &n in removed {
        for (m, rel, _) in parent.alive(n, true) {
            out.push(gone((n, m, rel)));
            inc.push(gone((m, n, rel)));
        }
        for (m, rel, _) in parent.alive(n, false) {
            inc.push(gone((n, m, rel)));
            out.push(gone((m, n, rel)));
        }
        for key in parent.alive_fact_keys(n) {
            facts.push(((n, key), OpKind::Remove, &[][..]));
        }
    }
    for tombs in [&mut out, &mut inc] {
        tombs.sort_unstable_by_key(|&(k, _, _)| k);
        tombs.dedup_by_key(|&mut (k, _, _)| k);
    }
    let rows = Sections {
        nodes: own.nodes,
        out: overlay(out, own.out, |&(k, _, _)| k),
        inc: overlay(inc, own.inc, |&(k, _, _)| k),
        facts: overlay(facts, own.facts, |&(k, _, _)| k),
    };
    let domain = parent.domain().max(rows.domain());
    let depth = u32::try_from(parent.layers().len() + 1).unwrap_or(u32::MAX);
    let union = |side: &[EdgeRow], out: bool| -> Vec<u32> {
        let mut set: BTreeSet<u32> = parent
            .layers()
            .iter()
            .flat_map(|l| l.extent.edges(out).touched())
            .collect();
        set.extend(side.iter().map(|&((n, _, _), _, _)| n));
        set.into_iter().collect()
    };
    let (out, inc) = (union(&rows.out, true), union(&rows.inc, false));
    Extent::write(KIND_LAYER, depth, domain, &rows, Some((&out, &inc)))
}
