//! The node fact section of an extent: changes sorted by (node, key) with aligned kinds and
//! payload offsets.

use std::cmp::Ordering;

use tantivy::directory::OwnedBytes;

use crate::error::GraphError;
use crate::op::{OpKind, Reader, column, corrupt, kind_byte, kind_from, put_len};

/// One fact change as written.
pub(crate) type FactEntry<'p> = ((u32, u8), OpKind, &'p [u8]);

/// A parsed fact section.
#[derive(Debug, Clone)]
pub(crate) struct Facts {
    count: usize,
    nodes: OwnedBytes,
    keys: OwnedBytes,
    kinds: OwnedBytes,
    offsets: OwnedBytes,
    payload: OwnedBytes,
}

impl Facts {
    /// Write `entries`, sorted by (node, key) and unique.
    pub(crate) fn write(out: &mut Vec<u8>, entries: &[FactEntry<'_>]) {
        put_len(out, entries.len());
        for ((node, _), _, _) in entries {
            out.extend_from_slice(&node.to_le_bytes());
        }
        out.extend(entries.iter().map(|((_, key), _, _)| *key));
        out.extend(entries.iter().map(|(_, kind, _)| kind_byte(*kind)));
        let mut offset = 0u64;
        out.extend_from_slice(&offset.to_le_bytes());
        for (_, _, payload) in entries {
            offset += payload.len() as u64;
            out.extend_from_slice(&offset.to_le_bytes());
        }
        for (_, _, payload) in entries {
            out.extend_from_slice(payload);
        }
    }

    pub(crate) fn parse(r: &mut Reader<'_>, file: &OwnedBytes) -> Result<Self, GraphError> {
        let count = r.count(4 + 1 + 1 + 8, "facts")?;
        let nodes = r.slice(file, count * 4, "fact nodes")?;
        let keys = r.slice(file, count, "fact keys")?;
        let kinds = r.slice(file, count, "fact kinds")?;
        let offsets = r.slice(file, (count + 1) * 8, "fact offsets")?;
        let total = column::<8>(&offsets, count)
            .map(u64::from_le_bytes)
            .and_then(|t| usize::try_from(t).ok())
            .ok_or_else(|| corrupt("fact payload"))?;
        let payload = r.slice(file, total, "fact payload")?;
        Ok(Self {
            count,
            nodes,
            keys,
            kinds,
            offsets,
            payload,
        })
    }

    fn key_at(&self, i: usize) -> Option<(u32, u8)> {
        Some((
            u32::from_le_bytes(column::<4>(&self.nodes, i)?),
            *self.keys.get(i)?,
        ))
    }

    fn entry(&self, i: usize) -> Option<FactEntry<'_>> {
        let off = |j| {
            column::<8>(&self.offsets, j)
                .map(u64::from_le_bytes)
                .and_then(|o| usize::try_from(o).ok())
        };
        let (lo, hi) = (off(i)?, off(i + 1)?);
        let payload = self.payload.get(lo..hi)?;
        Some((
            self.key_at(i)?,
            kind_from(*self.kinds.get(i)?).ok()?,
            payload,
        ))
    }

    /// The change this extent records for fact `key` of `node`, if any.
    pub(crate) fn get(&self, node: u32, key: u8) -> Option<(OpKind, &[u8])> {
        let want = (node, key);
        let (mut lo, mut hi) = (0usize, self.count);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            match self.key_at(mid)?.cmp(&want) {
                Ordering::Less => lo = mid + 1,
                Ordering::Greater => hi = mid,
                Ordering::Equal => return self.entry(mid).map(|(_, k, p)| (k, p)),
            }
        }
        None
    }

    /// Every change in (node, key) order.
    pub(crate) fn iter(&self) -> impl Iterator<Item = FactEntry<'_>> + '_ {
        (0..self.count).map_while(|i| self.entry(i))
    }
}
