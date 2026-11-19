//! The shared per-query resource budget (spec 11 QRY-04, QRY-06, PC-17).
//!
//! A [`QueryBudget`] holds an `Arc`-backed atomic live-byte counter, a deadline and a
//! cancellation flag, and remembers the first limit that was exceeded. Every allocation a query
//! makes is charged through [`QueryBudget::reserve`] before it happens; the returned
//! [`BudgetLease`] gives the bytes back exactly once when it is dropped. All arithmetic is
//! checked. The module has no SQL and no `unsafe` code.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

/// Which limit a query exceeded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Limit {
    /// The wall-clock deadline passed; the value is the elapsed milliseconds.
    Time,
    /// The live-byte ceiling would be passed; the value is the attempted live total in bytes.
    Memory,
    /// The query was cancelled; the value is the elapsed milliseconds.
    Cancelled,
}

/// A budget limit that was exceeded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("query budget exceeded: {limit:?} ({value})")]
pub struct BudgetExceeded {
    /// The limit that was exceeded first.
    pub limit: Limit,
    /// The measured value at the moment of the breach (see [`Limit`]).
    pub value: u64,
}

/// Accounting metadata charged beside every allocation (the allocator header and the lease).
pub const ALLOCATION_METADATA_BYTES: u64 = 16;

#[derive(Debug)]
struct Inner {
    max_bytes: u64,
    live: AtomicU64,
    deadline: Option<Duration>,
    started: Instant,
    cancelled: AtomicBool,
    first: OnceLock<BudgetExceeded>,
}

/// The shared budget of one query; cloning shares the same counters.
#[derive(Debug, Clone)]
pub struct QueryBudget {
    inner: Arc<Inner>,
}

/// Bytes charged to a [`QueryBudget`]; dropping the lease releases them exactly once.
#[derive(Debug)]
pub struct BudgetLease {
    budget: QueryBudget,
    bytes: u64,
}

impl QueryBudget {
    /// A budget with a live-byte ceiling and an optional deadline measured from now.
    #[must_use]
    pub fn new(max_bytes: u64, deadline: Option<Duration>) -> Self {
        Self {
            inner: Arc::new(Inner {
                max_bytes,
                live: AtomicU64::new(0),
                deadline,
                started: Instant::now(),
                cancelled: AtomicBool::new(false),
                first: OnceLock::new(),
            }),
        }
    }

    /// The bytes currently charged.
    #[must_use]
    pub fn live_bytes(&self) -> u64 {
        self.inner.live.load(Ordering::SeqCst)
    }

    /// Charge `bytes` before the allocation they pay for.
    ///
    /// # Errors
    /// [`BudgetExceeded`] when the charge would pass the ceiling or overflow, or when the
    /// deadline passed or the query was cancelled.
    pub fn reserve(&self, bytes: u64) -> Result<BudgetLease, BudgetExceeded> {
        self.check_clock()?;
        let max = self.inner.max_bytes;
        let mut current = self.inner.live.load(Ordering::SeqCst);
        loop {
            let overflow = current.checked_add(bytes).is_none();
            let attempted = current.saturating_add(bytes); // reported only, never stored
            if overflow || attempted > max {
                let own = BudgetExceeded {
                    limit: Limit::Memory,
                    value: attempted,
                };
                self.record(own.limit, own.value); // only the first breach is remembered
                return Err(own);
            }
            match self.inner.live.compare_exchange_weak(
                current,
                attempted,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => break,
                Err(seen) => current = seen,
            }
        }
        Ok(BudgetLease {
            budget: self.clone(),
            bytes,
        })
    }

    /// The first exceeded limit, or `Ok` while the query is within budget.
    ///
    /// # Errors
    /// [`BudgetExceeded`] for time, memory or cancellation.
    pub fn check(&self) -> Result<(), BudgetExceeded> {
        match self.inner.first.get() {
            Some(first) => Err(*first),
            None => self.check_clock(),
        }
    }

    /// Time and cancellation only: a recorded memory breach does not stop smaller charges.
    fn check_clock(&self) -> Result<(), BudgetExceeded> {
        if let Some(first) = self.inner.first.get()
            && first.limit != Limit::Memory
        {
            return Err(*first);
        }
        let elapsed = self.inner.started.elapsed();
        let millis = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
        if self.inner.cancelled.load(Ordering::SeqCst) {
            return Err(self.record(Limit::Cancelled, millis));
        }
        if self
            .inner
            .deadline
            .is_some_and(|deadline| elapsed > deadline)
        {
            return Err(self.record(Limit::Time, millis));
        }
        Ok(())
    }

    /// Remember a breach unless one was recorded first; return the first one.
    fn record(&self, limit: Limit, value: u64) -> BudgetExceeded {
        *self
            .inner
            .first
            .get_or_init(|| BudgetExceeded { limit, value })
    }

    /// The bytes to charge for one allocation: its capacity rounded up to `align`, plus the
    /// accounting metadata [`ALLOCATION_METADATA_BYTES`].
    ///
    /// # Errors
    /// [`BudgetExceeded`] when `align` is zero or the arithmetic overflows.
    pub fn allocation_charge(capacity: u64, align: u64) -> Result<u64, BudgetExceeded> {
        let overflow = BudgetExceeded {
            limit: Limit::Memory,
            value: u64::MAX,
        };
        capacity
            .checked_next_multiple_of(align)
            .and_then(|rounded| rounded.checked_add(ALLOCATION_METADATA_BYTES))
            .ok_or(overflow)
    }

    /// Cancel the query; later checks and reservations refuse.
    pub fn cancel(&self) {
        self.inner.cancelled.store(true, Ordering::SeqCst);
    }
}

/// Why a [`BudgetLease::grow_with`] failed.
#[derive(Debug, PartialEq, Eq)]
pub enum GrowError<E> {
    /// The budget refused the extra bytes; nothing was allocated.
    Budget(BudgetExceeded),
    /// The allocation failed after the extra bytes were reserved; they were released.
    Alloc(E),
}

impl BudgetLease {
    /// Grow this lease by `delta` bytes around an allocation: the delta is reserved first, then
    /// `allocate` runs. If it fails, the attempted growth is released and the old charge stays.
    ///
    /// # Errors
    /// [`GrowError::Budget`] when the delta is refused, [`GrowError::Alloc`] when `allocate` fails.
    pub fn grow_with<T, E>(
        &mut self,
        delta: u64,
        allocate: impl FnOnce() -> Result<T, E>,
    ) -> Result<T, GrowError<E>> {
        let mut extra = self.budget.reserve(delta).map_err(GrowError::Budget)?;
        let value = allocate().map_err(GrowError::Alloc)?; // `extra` drops here and releases
        let total = self
            .bytes
            .checked_add(extra.bytes)
            .ok_or(GrowError::Budget(BudgetExceeded {
                limit: Limit::Memory,
                value: u64::MAX,
            }))?;
        self.bytes = total;
        extra.bytes = 0; // the delta now belongs to this lease; dropping `extra` releases nothing
        Ok(value)
    }

    /// The bytes this lease holds.
    #[must_use]
    pub fn bytes(&self) -> u64 {
        self.bytes
    }
}

impl Drop for BudgetLease {
    fn drop(&mut self) {
        self.budget
            .inner
            .live
            .fetch_sub(self.bytes, Ordering::SeqCst);
    }
}
