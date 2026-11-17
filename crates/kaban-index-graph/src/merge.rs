//! The k-way merge shared by reads and by run merges: sorted, key-unique sources, oldest first,
//! merged by key through a binary heap of the source heads, the newest source winning per key.
//!
//! A heap keeps the cost of one merged entry at O(log k) comparisons for k sources, instead of
//! comparing every source per key; this matters where many sources feed one list (a long layer
//! chain under several sediment runs, or presence enumerated over all of them).

use std::cmp::Reverse;
use std::collections::BinaryHeap;

use crate::charge::{Charge, Phase, Ticker};
use crate::error::GraphError;

/// A sorted keyed source with strictly ascending keys; every advance is metered on the ticker.
pub(crate) trait Source {
    type Key: Ord + Copy;
    type Value;

    /// The next entry, one unit of `t` in `phase`; `None` at the end or once `t` refused.
    fn advance(&mut self, t: &Ticker<'_>, phase: Phase) -> Option<(Self::Key, Self::Value)>;
}

/// A k-way merge of sorted keyed sources, oldest first: per key the newest source wins.
pub(crate) struct NewestWins<S: Source> {
    sources: Vec<S>,
    /// The value of each source's head; the heap holds its key.
    heads: Vec<Option<S::Value>>,
    heap: BinaryHeap<Reverse<(S::Key, usize)>>,
    primed: bool,
}

/// A vector of `capacity`, charged when a charge is given (query paths).
pub(crate) fn charged<T>(
    charge: Option<&mut Charge>,
    capacity: usize,
) -> Result<Vec<T>, GraphError> {
    match charge {
        Some(charge) => charge.vec(capacity),
        None => Ok(Vec::with_capacity(capacity)),
    }
}

impl<S: Source> NewestWins<S> {
    /// A merge of `sources` (oldest first); its head and heap vectors are charged to `charge`
    /// when given.
    pub(crate) fn new(
        sources: Vec<S>,
        mut charge: Option<&mut Charge>,
    ) -> Result<Self, GraphError> {
        let mut heads = charged(charge.as_deref_mut(), sources.len())?;
        heads.resize_with(sources.len(), || None);
        let heap = BinaryHeap::from(charged(charge, sources.len())?);
        Ok(Self {
            sources,
            heads,
            heap,
            primed: false,
        })
    }

    /// A merge of nothing.
    pub(crate) fn empty() -> Self {
        Self {
            sources: Vec::new(),
            heads: Vec::new(),
            heap: BinaryHeap::new(),
            primed: true,
        }
    }

    /// Advance source `i` and put its next head on the heap; one unit of `t` for the push.
    fn refill(&mut self, i: usize, t: &Ticker<'_>, phase: Phase) {
        if let Some((key, value)) = self.sources[i].advance(t, phase)
            && t.step(phase)
        {
            self.heads[i] = Some(value);
            self.heap.push(Reverse((key, i)));
        }
    }

    /// The next merged entry and the index of the source it came from. Every source holding
    /// the same key is passed to `each` oldest first and advanced; the newest one's value is
    /// returned. Each heap pop and push is one unit of `t` (a sift of at most log2(k)
    /// comparisons), each source advance another; after `t` refused the result is meaningless
    /// and the caller reports the refusal.
    pub(crate) fn next_with(
        &mut self,
        t: &Ticker<'_>,
        mut each: impl FnMut(usize, &S::Value),
    ) -> Option<(S::Key, S::Value, usize)> {
        if !self.primed {
            self.primed = true;
            for i in 0..self.sources.len() {
                self.refill(i, t, Phase::MergePrime);
            }
        }
        let mut won: Option<(S::Key, S::Value, usize)> = None;
        loop {
            match (self.heap.peek(), &won) {
                (Some(Reverse((key, _))), Some((at, _, _))) if key != at => break,
                (None, _) => break,
                _ => {}
            }
            if !t.step(Phase::Merge) {
                return None;
            }
            let Reverse((key, i)) = self.heap.pop()?;
            let value = self.heads[i].take()?;
            each(i, &value);
            self.refill(i, t, Phase::Merge);
            won = Some((key, value, i));
        }
        won
    }

    /// [`NewestWins::next_with`] without looking at the older sources of a key.
    pub(crate) fn next(&mut self, t: &Ticker<'_>) -> Option<(S::Key, S::Value, usize)> {
        self.next_with(t, |_, _| {})
    }
}

/// A sorted slice as a source, keyed by `key`; its values borrow the slice.
pub(crate) struct Slice<'a, T, K> {
    items: std::slice::Iter<'a, T>,
    key: fn(&T) -> K,
}

impl<'a, T, K> Slice<'a, T, K> {
    pub(crate) fn new(items: &'a [T], key: fn(&T) -> K) -> Self {
        Self {
            items: items.iter(),
            key,
        }
    }
}

impl<'a, T, K: Ord + Copy> Source for Slice<'a, T, K> {
    type Key = K;
    type Value = &'a T;

    fn advance(&mut self, t: &Ticker<'_>, phase: Phase) -> Option<(K, &'a T)> {
        if !t.step(phase) {
            return None;
        }
        self.items.next().map(|item| ((self.key)(item), item))
    }
}
