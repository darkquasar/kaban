//! The density-chosen id index of an extent section, and presence itself.
//!
//! An index maps the touched ids of a section to their rank (their position in the section's
//! aligned columns). It is a sorted `u32` id array when touched/n is at most 1/30, otherwise a
//! bitvector over the id domain with one cumulative rank sample per 512 bits (S7). The same
//! index stores a layer's union sections, where only membership is asked.

use tantivy::directory::OwnedBytes;

use crate::charge::{Phase, Ticker};
use crate::error::GraphError;
use crate::op::OpKind;
use crate::op::{Reader, column, corrupt, kind_byte, kind_from, put_len};

const SORTED: u8 = 0;
const BITS: u8 = 1;
/// 64-bit words per rank sample (512 bits).
const WORDS_PER_SAMPLE: usize = 8;
/// The bitvector is chosen when touched * DENSITY > n.
const DENSITY: u64 = 30;

/// A parsed id index.
#[derive(Debug, Clone)]
pub(crate) enum IdIndex {
    /// Ascending `u32` ids.
    Sorted { ids: OwnedBytes, count: usize },
    /// A bitvector over the domain with cumulative `u64` rank samples.
    Bits {
        words: OwnedBytes,
        samples: OwnedBytes,
        nbits: u64,
        count: usize,
    },
}

/// Words needed for `nbits` bits.
fn words_for(nbits: u64) -> Result<usize, GraphError> {
    usize::try_from(nbits.div_ceil(64)).map_err(|_| corrupt("bitvector size"))
}

impl IdIndex {
    /// Write the index of `ids` (ascending, unique, each below `domain`).
    pub(crate) fn write(out: &mut Vec<u8>, ids: &[u32], domain: u64) {
        let count = ids.len() as u64;
        if count.saturating_mul(DENSITY) > domain {
            out.push(BITS);
            put_len(out, ids.len());
            let words = write_bits(out, ids, domain);
            let mut total = 0u64;
            for block in words.chunks(WORDS_PER_SAMPLE) {
                out.extend_from_slice(&total.to_le_bytes());
                total += block.iter().map(|w| u64::from(w.count_ones())).sum::<u64>();
            }
        } else {
            out.push(SORTED);
            put_len(out, ids.len());
            for id in ids {
                out.extend_from_slice(&id.to_le_bytes());
            }
        }
    }

    /// Parse an index over `domain` from `r`, slicing columns out of `file`, and validate it in
    /// one pass over its bytes: sorted ids strictly ascend below `domain`; a bitvector has no
    /// bit set past `domain`, its population equals `count` and every rank sample equals the
    /// population of the words before its block. Lookups on a parsed index are therefore exact.
    pub(crate) fn parse(
        r: &mut Reader<'_>,
        file: &OwnedBytes,
        domain: u64,
        t: &Ticker<'_>,
    ) -> Result<Self, GraphError> {
        let mode = r.u8("index mode")?;
        match mode {
            SORTED => {
                let count = r.count(4, "sorted index")?;
                let len = count
                    .checked_mul(4)
                    .ok_or_else(|| corrupt("sorted index"))?;
                let ids = r.slice(file, len, "sorted index")?;
                let mut prev: Option<u32> = None;
                for &chunk in ids.as_chunks::<4>().0 {
                    t.tick(Phase::Validate)?;
                    let id = u32::from_le_bytes(chunk);
                    if u64::from(id) >= domain || prev.is_some_and(|p| p >= id) {
                        return Err(corrupt("sorted index order or domain"));
                    }
                    prev = Some(id);
                }
                Ok(Self::Sorted { ids, count })
            }
            BITS => {
                let count = usize::try_from(r.u64("bit index count")?)
                    .map_err(|_| corrupt("bit index count"))?;
                let (words, nbits) = parse_bits(r, file, domain)?;
                let nwords = words.len() / 8;
                let nsamples = nwords.div_ceil(WORDS_PER_SAMPLE);
                let len = nsamples
                    .checked_mul(8)
                    .ok_or_else(|| corrupt("rank samples"))?;
                let samples = r.slice(file, len, "rank samples")?;
                let mut total = 0u64;
                for (block, sample) in words
                    .chunks(8 * WORDS_PER_SAMPLE)
                    .zip(samples.as_chunks::<8>().0)
                {
                    t.tick(Phase::Validate)?;
                    let sample = u64::from_le_bytes(*sample);
                    if sample != total {
                        return Err(corrupt("rank sample"));
                    }
                    let ones: u64 = block
                        .as_chunks::<8>()
                        .0
                        .iter()
                        .map(|&w| u64::from(u64::from_le_bytes(w).count_ones()))
                        .sum();
                    total = total
                        .checked_add(ones)
                        .ok_or_else(|| corrupt("rank sample"))?;
                }
                if u64::try_from(count).ok() != Some(total) {
                    return Err(corrupt("bit index count"));
                }
                Ok(Self::Bits {
                    words,
                    samples,
                    nbits,
                    count,
                })
            }
            _ => Err(corrupt("index mode")),
        }
    }

    /// Whether `id` is touched: a binary search over a sorted index, one bit probe over a
    /// bitvector.
    pub(crate) fn contains(&self, id: u32) -> bool {
        match self {
            Self::Sorted { .. } => self.rank(id).is_some(),
            Self::Bits { words, nbits, .. } => test_bit(words, *nbits, id),
        }
    }

    /// Touched ids.
    pub(crate) fn len(&self) -> usize {
        match self {
            Self::Sorted { count, .. } | Self::Bits { count, .. } => *count,
        }
    }

    /// The rank of `id`, or `None` when untouched. Arithmetic is checked; the index was
    /// validated at parse, so a checked failure cannot hide a recorded id.
    pub(crate) fn rank(&self, id: u32) -> Option<usize> {
        match self {
            Self::Sorted { ids, count } => {
                let (mut lo, mut hi) = (0usize, *count);
                while lo < hi {
                    let mid = lo + (hi - lo) / 2;
                    let at = u32::from_le_bytes(column::<4>(ids, mid)?);
                    match at.cmp(&id) {
                        std::cmp::Ordering::Less => lo = mid + 1,
                        std::cmp::Ordering::Greater => hi = mid,
                        std::cmp::Ordering::Equal => return Some(mid),
                    }
                }
                None
            }
            Self::Bits {
                words,
                samples,
                nbits,
                count,
            } => {
                if u64::from(id) >= *nbits {
                    return None;
                }
                let w = usize::try_from(id / 64).ok()?;
                let word = u64::from_le_bytes(column::<8>(words, w)?);
                let bit = 1u64 << (id % 64);
                if word & bit == 0 {
                    return None;
                }
                let block = w / WORDS_PER_SAMPLE;
                let mut rank = u64::from_le_bytes(column::<8>(samples, block)?);
                for i in block * WORDS_PER_SAMPLE..w {
                    let ones = u64::from_le_bytes(column::<8>(words, i)?).count_ones();
                    rank = rank.checked_add(u64::from(ones))?;
                }
                rank = rank.checked_add(u64::from((word & (bit - 1)).count_ones()))?;
                let rank = usize::try_from(rank).ok()?;
                (rank < *count).then_some(rank)
            }
        }
    }

    /// Touched ids in ascending order with their ranks.
    pub(crate) fn cursor(&self) -> IdIter<'_> {
        IdIter {
            index: self,
            pos: 0,
            word: 0,
            word_bits: 0,
            rank: 0,
        }
    }
}

/// Ascending ids of an [`IdIndex`].
#[derive(Debug)]
pub(crate) struct IdIter<'i> {
    index: &'i IdIndex,
    /// Next id position (sorted) or next word (bits).
    pos: usize,
    /// Base id of the current word.
    word: u64,
    /// Remaining set bits of the current word.
    word_bits: u64,
    rank: usize,
}

impl IdIter<'_> {
    /// The next id; every id of a sorted index and every bitvector word visited is one unit of
    /// `t`, so a long run of empty words is metered and checked word by word; `None` once `t`
    /// refused.
    pub(crate) fn next(&mut self, t: &Ticker<'_>, phase: Phase) -> Option<u32> {
        match self.index {
            IdIndex::Sorted { ids, count } => {
                if self.pos >= *count {
                    return None;
                }
                if !t.step(phase) {
                    return None;
                }
                let id = u32::from_le_bytes(column::<4>(ids, self.pos)?);
                self.pos += 1;
                Some(id)
            }
            IdIndex::Bits { words, count, .. } => loop {
                if self.rank >= *count {
                    return None;
                }
                if self.word_bits != 0 {
                    let bit = u64::from(self.word_bits.trailing_zeros());
                    self.word_bits &= self.word_bits - 1;
                    self.rank += 1;
                    return u32::try_from(self.word + bit).ok();
                }
                if !t.step(phase) {
                    return None;
                }
                self.word_bits = u64::from_le_bytes(column::<8>(words, self.pos)?);
                self.word = (self.pos as u64) * 64;
                self.pos += 1;
            },
        }
    }
}

/// Write a bitvector of `nbits` bits with `ids` set; returns the words written.
fn write_bits(out: &mut Vec<u8>, ids: &[u32], nbits: u64) -> Vec<u64> {
    let mut words = vec![0u64; usize::try_from(nbits.div_ceil(64)).unwrap_or(0)];
    for &id in ids {
        if let Some(w) = words.get_mut((id / 64) as usize) {
            *w |= 1u64 << (id % 64);
        }
    }
    out.extend_from_slice(&nbits.to_le_bytes());
    for w in &words {
        out.extend_from_slice(&w.to_le_bytes());
    }
    words
}

/// Parse a bitvector written by [`write_bits`]; its size must equal `domain` and the bits past
/// it in the last word must be clear.
fn parse_bits(
    r: &mut Reader<'_>,
    file: &OwnedBytes,
    domain: u64,
) -> Result<(OwnedBytes, u64), GraphError> {
    let nbits = r.u64("bitvector bits")?;
    if nbits != domain {
        return Err(corrupt("bitvector domain"));
    }
    let nwords = words_for(nbits)?;
    let len = nwords
        .checked_mul(8)
        .ok_or_else(|| corrupt("bitvector size"))?;
    let words = r.slice(file, len, "bitvector")?;
    let used = nbits % 64;
    if used != 0
        && let Some(last) = words
            .len()
            .checked_sub(8)
            .and_then(|at| column::<8>(&words[at..], 0))
        && u64::from_le_bytes(last) >> used != 0
    {
        return Err(corrupt("bitvector padding"));
    }
    Ok((words, nbits))
}

/// Whether bit `id` is set in a bitvector of `nbits` bits.
fn test_bit(words: &[u8], nbits: u64, id: u32) -> bool {
    u64::from(id) < nbits
        && usize::try_from(id / 64)
            .ok()
            .and_then(|w| column::<8>(words, w))
            .is_some_and(|w| u64::from_le_bytes(w) & (1u64 << (id % 64)) != 0)
}

/// The presence section: touched nodes and whether each is added or removed.
#[derive(Debug, Clone)]
pub(crate) struct Presence {
    index: IdIndex,
    kinds: OwnedBytes,
}

impl Presence {
    /// Write presence for `entries` (ascending, unique ids).
    pub(crate) fn write(out: &mut Vec<u8>, entries: &[(u32, OpKind)], domain: u64) {
        let ids: Vec<u32> = entries.iter().map(|&(id, _)| id).collect();
        IdIndex::write(out, &ids, domain);
        out.extend(entries.iter().map(|&(_, k)| kind_byte(k)));
    }

    pub(crate) fn parse(
        r: &mut Reader<'_>,
        file: &OwnedBytes,
        domain: u64,
        t: &Ticker<'_>,
    ) -> Result<Self, GraphError> {
        let index = IdIndex::parse(r, file, domain, t)?;
        let kinds = r.slice(file, index.len(), "presence kinds")?;
        check_kinds(&kinds, "presence kinds", t)?;
        Ok(Self { index, kinds })
    }

    /// The membership change this extent records for `id`, if any.
    pub(crate) fn get(&self, id: u32) -> Option<OpKind> {
        let rank = self.index.rank(id)?;
        kind_from(*self.kinds.get(rank)?).ok()
    }

    /// Touched ids ascending with their change.
    pub(crate) fn cursor(&self) -> PresenceIter<'_> {
        PresenceIter {
            ids: self.index.cursor(),
            kinds: &self.kinds,
            rank: 0,
        }
    }
}

/// Touched ids of a presence section ascending with their change.
#[derive(Debug)]
pub(crate) struct PresenceIter<'p> {
    ids: IdIter<'p>,
    kinds: &'p [u8],
    rank: usize,
}

impl PresenceIter<'_> {
    /// The next touched id and its change, metered on `t` as [`IdIter::next`].
    pub(crate) fn next(&mut self, t: &Ticker<'_>, phase: Phase) -> Option<(u32, OpKind)> {
        let id = self.ids.next(t, phase)?;
        let kind = kind_from(*self.kinds.get(self.rank)?).ok()?;
        self.rank += 1;
        Some((id, kind))
    }
}

/// Kind bytes validated per unit of work.
const KINDS_PER_UNIT: usize = 256;

/// Refuse any byte that is not a known [`OpKind`]; every [`KINDS_PER_UNIT`] bytes are one unit
/// of `t`.
pub(crate) fn check_kinds(kinds: &[u8], what: &str, t: &Ticker<'_>) -> Result<(), GraphError> {
    for chunk in kinds.chunks(KINDS_PER_UNIT) {
        t.tick(Phase::Validate)?;
        for &k in chunk {
            kind_from(k).map_err(|_| corrupt(what))?;
        }
    }
    Ok(())
}
