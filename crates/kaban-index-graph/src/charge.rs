//! Budgeted allocation (PC-17): every query-owned collection of this crate is reserved against
//! the caller's [`QueryBudget`] before it is allocated, and the charge lives as long as the
//! collection that owns it.

use std::cell::Cell;
use std::convert::Infallible;
use std::sync::atomic::{AtomicU64, Ordering};

use kaban_core::query_budget::{BudgetExceeded, BudgetLease, GrowError, QueryBudget};

use crate::error::GraphError;
use crate::view::BUDGET_CHECK_INTERVAL;

/// One growing charge: each [`Charge::vec`] reserves its capacity first, then allocates.
#[derive(Debug)]
pub(crate) struct Charge {
    lease: BudgetLease,
}

impl Charge {
    /// An empty charge on `budget`; refused when the budget already refuses (a recorded memory
    /// breach, the deadline or a cancellation).
    pub(crate) fn new(budget: &QueryBudget) -> Result<Self, GraphError> {
        budget.check()?;
        Ok(Self {
            lease: budget.reserve(0)?,
        })
    }

    /// A vector of exactly `capacity` elements, charged as its allocation (capacity rounded to
    /// the element alignment plus the allocation metadata) before it is allocated.
    pub(crate) fn vec<T>(&mut self, capacity: usize) -> Result<Vec<T>, GraphError> {
        let size = size_of::<T>();
        if capacity == 0 || size == 0 {
            return Ok(Vec::new());
        }
        let bytes = size
            .checked_mul(capacity)
            .and_then(|b| u64::try_from(b).ok())
            .ok_or(GraphError::BudgetExceeded {
                limit: "memory",
                value: u64::MAX,
            })?;
        let align = u64::try_from(align_of::<T>()).unwrap_or(u64::MAX);
        let charge = QueryBudget::allocation_charge(bytes, align)?;
        self.lease
            .grow_with(charge, || Ok::<_, Infallible>(Vec::with_capacity(capacity)))
            .map_err(|e| match e {
                GrowError::Budget(b) => GraphError::from(b),
                GrowError::Alloc(never) => match never {},
            })
    }

    /// The lease holding every charge made so far.
    pub(crate) fn into_lease(self) -> BudgetLease {
        self.lease
    }
}

/// A function result together with the budget charge of the memory it owns (PC-17).
///
/// The charge stays live while the `Charged` value (or the [`Hold`] taken from it with
/// [`Charged::into_parts`]) lives, and is released when it is dropped. Callers that convert the
/// rows keep the hold alive until the converted output is released, and reserve their own
/// conversions on the same budget.
#[derive(Debug)]
pub struct Charged<T> {
    value: T,
    hold: Hold,
}

/// The budget charge of a [`Charged`] value; dropping it releases the bytes.
#[derive(Debug)]
pub struct Hold {
    leases: [Option<BudgetLease>; 2],
}

impl Hold {
    pub(crate) fn new(first: BudgetLease, second: Option<BudgetLease>) -> Self {
        Self {
            leases: [Some(first), second],
        }
    }

    /// The bytes this hold keeps charged.
    #[must_use]
    pub fn bytes(&self) -> u64 {
        self.leases
            .iter()
            .flatten()
            .map(BudgetLease::bytes)
            .fold(0, u64::saturating_add)
    }
}

impl<T> Charged<T> {
    pub(crate) fn new(value: T, hold: Hold) -> Self {
        Self { value, hold }
    }

    /// The result.
    #[must_use]
    pub fn value(&self) -> &T {
        &self.value
    }

    /// The bytes charged for the result.
    #[must_use]
    pub fn bytes(&self) -> u64 {
        self.hold.bytes()
    }

    /// The result and its charge, to be kept alive while the result (or a copy made from it)
    /// is in use.
    #[must_use]
    pub fn into_parts(self) -> (T, Hold) {
        (self.value, self.hold)
    }
}

impl<T> std::ops::Deref for Charged<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.value
    }
}

/// A growing query-owned vector (PC-17): every growth reserves the new buffer on the budget
/// before allocating it, and the old buffer's charge is released once that buffer is freed.
/// The charge always covers the live capacity. A growth moves the elements in chunks of
/// [`MOVE_CHUNK`] bytes, one unit of work each, so a long move is checked from inside.
#[derive(Debug)]
pub(crate) struct Grow<'b, T> {
    budget: &'b QueryBudget,
    items: Vec<T>,
    lease: BudgetLease,
    ticker: Ticker<'b>,
}

/// Bytes of a growing vector moved per unit of work.
const MOVE_CHUNK: usize = 4096;

/// The charge of a buffer of `capacity` elements of `T`: its bytes rounded to the alignment plus
/// the allocation metadata.
fn buffer_charge<T>(capacity: usize) -> Result<u64, GraphError> {
    let overflow = GraphError::BudgetExceeded {
        limit: "memory",
        value: u64::MAX,
    };
    let bytes = size_of::<T>()
        .checked_mul(capacity)
        .and_then(|b| u64::try_from(b).ok())
        .ok_or(overflow)?;
    let align = u64::try_from(align_of::<T>()).unwrap_or(u64::MAX);
    Ok(QueryBudget::allocation_charge(bytes, align)?)
}

impl<'b, T> Grow<'b, T> {
    /// An empty vector on `budget`; refused when the budget already refuses (a recorded memory
    /// breach, the deadline or a cancellation).
    pub(crate) fn new(budget: &'b QueryBudget) -> Result<Self, GraphError> {
        budget.check()?;
        Ok(Self {
            budget,
            items: Vec::new(),
            lease: budget.reserve(0)?,
            ticker: Ticker::new(budget),
        })
    }

    /// An empty vector with room for `capacity` elements, reserved first.
    pub(crate) fn with_capacity(
        budget: &'b QueryBudget,
        capacity: usize,
    ) -> Result<Self, GraphError> {
        let mut grow = Self::new(budget)?;
        grow.reserve(capacity)?;
        Ok(grow)
    }

    /// Room for `extra` more elements; grows by doubling, reserving before allocating. After an
    /// error the vector may be emptied: callers propagate the error and drop it.
    pub(crate) fn reserve(&mut self, extra: usize) -> Result<(), GraphError> {
        let need = self
            .items
            .len()
            .checked_add(extra)
            .ok_or(GraphError::BudgetExceeded {
                limit: "memory",
                value: u64::MAX,
            })?;
        if need <= self.items.capacity() {
            return Ok(());
        }
        let capacity = need.max(self.items.capacity().saturating_mul(2)).max(4);
        let lease = self.budget.reserve(buffer_charge::<T>(capacity)?)?;
        let mut items = Vec::with_capacity(capacity);
        let chunk = (MOVE_CHUNK / size_of::<T>().max(1)).max(1);
        let mut old = std::mem::take(&mut self.items).into_iter();
        while old.len() > 0 {
            self.ticker.tick(Phase::Grow)?;
            items.extend(old.by_ref().take(chunk));
        }
        // The old buffer is freed here, then its charge is released with the old lease.
        drop(old);
        self.items = items;
        self.lease = lease;
        Ok(())
    }

    /// Append `item`, growing first when full.
    pub(crate) fn push(&mut self, item: T) -> Result<(), GraphError> {
        self.reserve(1)?;
        self.items.push(item);
        Ok(())
    }

    /// Keep the first `len` elements (the capacity and its charge stay).
    pub(crate) fn truncate(&mut self, len: usize) {
        self.items.truncate(len);
    }

    /// The elements and the lease charging their buffer.
    pub(crate) fn into_parts(self) -> (Vec<T>, BudgetLease) {
        (self.items, self.lease)
    }
}

impl<T: PartialEq + Copy> Grow<'_, T> {
    /// Remove consecutive equal elements, one unit of `t` per element.
    pub(crate) fn dedup(&mut self, t: &Ticker<'_>) -> Result<(), GraphError> {
        let mut kept = 0;
        for i in 0..self.items.len() {
            t.tick(Phase::Dedup)?;
            let item = self.items[i];
            if kept == 0 || self.items[kept - 1] != item {
                self.items[kept] = item;
                kept += 1;
            }
        }
        self.items.truncate(kept);
        Ok(())
    }
}

/// Slices at most this long are sorted in one unit of work.
const SMALL_SORT: usize = 32;

/// Sort `v` by `key` in place, interruptibly: a heapsort whose every sift is one unit of `t`
/// (a sift is at most 2 log2(len) comparisons), so a cancellation or deadline stops an
/// input-sized sort within [`BUDGET_CHECK_INTERVAL`] sifts. Unstable; no allocation. Slices of
/// at most [`SMALL_SORT`] elements are sorted in one unit.
pub(crate) fn sort_checked<T, K: Ord>(
    v: &mut [T],
    key: impl Fn(&T) -> K,
    t: &Ticker<'_>,
) -> Result<(), GraphError> {
    if v.len() <= SMALL_SORT {
        t.tick(Phase::Sort)?;
        v.sort_unstable_by_key(key);
        return Ok(());
    }
    let n = v.len();
    for root in (0..n / 2).rev() {
        t.tick(Phase::Sort)?;
        sift_down(v, root, n, &key);
    }
    for end in (1..n).rev() {
        t.tick(Phase::Sort)?;
        v.swap(0, end);
        sift_down(v, 0, end, &key);
    }
    Ok(())
}

/// Restore the max-heap order of `v[..end]` below `root`.
fn sift_down<T, K: Ord>(v: &mut [T], mut root: usize, end: usize, key: &impl Fn(&T) -> K) {
    loop {
        let mut child = 2 * root + 1;
        if child >= end {
            return;
        }
        if child + 1 < end && key(&v[child]) < key(&v[child + 1]) {
            child += 1;
        }
        if key(&v[root]) >= key(&v[child]) {
            return;
        }
        v.swap(root, child);
        root = child;
    }
}

impl<T> std::ops::Deref for Grow<'_, T> {
    type Target = [T];

    fn deref(&self) -> &[T] {
        &self.items
    }
}

impl<T> std::ops::DerefMut for Grow<'_, T> {
    fn deref_mut(&mut self) -> &mut [T] {
        &mut self.items
    }
}

/// Metered work units done in this process so far (instrumentation): every unit counted by a
/// budgeted [`Ticker`], published at its interval boundaries and when it is dropped.
static WORK_UNITS: AtomicU64 = AtomicU64::new(0);

/// Metered units of query work done in this process so far, for tests and diagnostics of the
/// bounded-work contract (PC-17).
///
/// A unit is one bounded step of query work: one source entry advanced or compared in a merge,
/// one bitvector word visited, one cursor prepared, one loop iteration of a structural function,
/// one decoded record, one validated entry or one sort step. Every budgeted loop of this crate
/// counts its units here; the count is published every [`BUDGET_CHECK_INTERVAL`] units and when
/// the counting loop ends, so it can lag the work by less than one interval per live loop. The
/// counter is process-wide: concurrent queries add to the same count.
#[must_use]
pub fn work_units() -> u64 {
    WORK_UNITS.load(Ordering::SeqCst)
}

/// The metered loop a unit of work belongs to. Every unit names its loop, so test
/// instrumentation can request a cancellation inside one named loop and see which loop's check
/// refused; production code ignores it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Phase {
    /// Decoding sediment records and copying their payloads.
    Decode,
    /// Validating an extent's sections at open.
    Validate,
    /// Counting a stream's records over the batches of a new run.
    CountBatches,
    /// Summing the fact payload bytes of the batches of a new run.
    SizePayload,
    /// Copying the batches' records and payloads into a new run.
    Copy,
    /// One sift of an interruptible sort.
    Sort,
    /// Compacting a sorted stream to its newest record per key.
    Compact,
    /// Counting the nodes of a run that carry a barrier.
    CountBarriers,
    /// Copying the ids of a run's barriers.
    Barriers,
    /// Writing a run's incoming edges from its outgoing ones.
    Transpose,
    /// Setting up a k-way merge's per-source heads.
    MergeSetup,
    /// Priming a k-way merge's heap with every source's first entry.
    MergePrime,
    /// One step of a k-way merge (a heap pop or push, or a source advanced by one entry).
    Merge,
    /// Preparing one cursor per extent or run.
    Cursor,
    /// Checking that the newest layer's union covers every layer of the chain.
    UnionCoverage,
    /// Moving a growing vector's elements into its new buffer.
    Grow,
    /// Removing consecutive duplicates.
    Dedup,
    /// The loops of a breadth-first search.
    Search,
    /// Initialising a structural function's per-node state.
    Init,
    /// The in-degree, seeding and release loops of a topological order.
    Topo,
    /// Finding and walking to a node on a cycle.
    Cycle,
    /// One step of a union-find walk.
    UnionFind,
    /// The node and output loops of the components.
    Components,
    /// Marking the nodes on a shortest path.
    Paths,
    /// Merging two seen sets for meeting points.
    Meet,
    /// Galloping through two neighbour lists.
    Common,
    /// Unmetered writer paths (a ticker without a budget never checks).
    Writer,
}

/// Counts units of work and checks the budget once every [`BUDGET_CHECK_INTERVAL`] units,
/// starting before the first unit. A ticker without a budget (writer paths) neither checks nor
/// publishes.
///
/// Iterator sources that cannot return an error use [`Ticker::step`]: once a check fails the
/// ticker remembers the breach, every later step refuses, and the owning loop reports the breach
/// from [`Ticker::stopped`].
#[derive(Debug)]
pub(crate) struct Ticker<'b> {
    budget: Option<&'b QueryBudget>,
    units: Cell<u64>,
    published: Cell<u64>,
    failed: Cell<Option<BudgetExceeded>>,
}

impl<'b> Ticker<'b> {
    pub(crate) fn new(budget: &'b QueryBudget) -> Self {
        Self::maybe(Some(budget))
    }

    /// A ticker on `budget`, or an unmetered one for writer paths.
    pub(crate) fn maybe(budget: Option<&'b QueryBudget>) -> Self {
        Self {
            budget,
            units: Cell::new(0),
            published: Cell::new(0),
            failed: Cell::new(None),
        }
    }

    /// An unmetered ticker (writer paths): it never refuses.
    pub(crate) fn unmetered() -> Ticker<'static> {
        Ticker::maybe(None)
    }

    /// One unit of work of the loop `phase`; at every interval boundary (and before the first
    /// unit) the budget is checked.
    pub(crate) fn tick(&self, phase: Phase) -> Result<(), GraphError> {
        if let Some(e) = self.failed.get() {
            #[cfg(test)]
            crate::probe::after_refusal(self.budget, phase);
            return Err(e.into());
        }
        let units = self.units.get();
        if units.is_multiple_of(BUDGET_CHECK_INTERVAL) {
            self.publish();
            if let Some(budget) = self.budget
                && let Err(e) = budget.check()
            {
                self.failed.set(Some(e));
                #[cfg(test)]
                crate::probe::refused(budget, phase);
                return Err(e.into());
            }
        }
        self.units.set(units + 1);
        #[cfg(test)]
        crate::probe::unit(self.budget, phase);
        #[cfg(not(test))]
        let _ = phase;
        Ok(())
    }

    /// [`Ticker::tick`] for sources that cannot return an error: `false` once the budget refused.
    pub(crate) fn step(&self, phase: Phase) -> bool {
        self.tick(phase).is_ok()
    }

    /// Whether a check of this ticker has refused: the loop must stop at once.
    pub(crate) fn refused(&self) -> bool {
        self.failed.get().is_some()
    }

    /// `items`, one unit of `phase` each: the one way a query path walks input-sized data.
    /// The budget is checked before the first item and at every interval; once a check refuses
    /// the error is yielded and the walk ends. A loop that does not walk this way either ticks
    /// `phase` itself at the top of every iteration or is bounded by a constant.
    pub(crate) fn each<I: IntoIterator>(
        &self,
        phase: Phase,
        items: I,
    ) -> Each<'_, 'b, I::IntoIter> {
        Each {
            ticker: self,
            phase,
            items: items.into_iter(),
            done: false,
        }
    }

    /// The saturating sum of `len` over `items`, one unit of `phase` per item.
    pub(crate) fn sum<I: IntoIterator>(
        &self,
        phase: Phase,
        items: I,
        len: impl Fn(I::Item) -> usize,
    ) -> Result<usize, GraphError> {
        let mut total = 0usize;
        for item in self.each(phase, items) {
            total = total.saturating_add(len(item?));
        }
        Ok(total)
    }

    /// The breach a failed step recorded, if any.
    pub(crate) fn stopped(&self) -> Result<(), GraphError> {
        match self.failed.get() {
            Some(e) => Err(e.into()),
            None => Ok(()),
        }
    }

    fn publish(&self) {
        if self.budget.is_some() {
            let units = self.units.get();
            WORK_UNITS.fetch_add(units - self.published.get(), Ordering::SeqCst);
            self.published.set(units);
        }
    }
}

/// The iterator of [`Ticker::each`].
#[derive(Debug)]
pub(crate) struct Each<'t, 'b, I> {
    ticker: &'t Ticker<'b>,
    phase: Phase,
    items: I,
    done: bool,
}

impl<I: Iterator> Iterator for Each<'_, '_, I> {
    type Item = Result<I::Item, GraphError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        let item = self.items.next()?;
        if let Err(e) = self.ticker.tick(self.phase) {
            self.done = true;
            return Some(Err(e));
        }
        Some(Ok(item))
    }
}

impl Drop for Ticker<'_> {
    fn drop(&mut self) {
        self.publish();
    }
}
