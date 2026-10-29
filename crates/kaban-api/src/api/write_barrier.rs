//! The write barrier (spec 05 SS-10, design-6, amendment 7): after a public write committed,
//! the service waits one bounded barrier for the serving search reader to cover the receipt's
//! position, and reports `committed_not_yet_searchable` when it does not. The barrier runs
//! after every canonical transaction and lock of the call is released, once per public call.
//!
//! ```text
//!   receipt{committed, seq}
//!     => deadline = now + budget                       (taken first)
//!     => text_visible() covers seq?  ==> committed
//!     => local drain: drain_until(seq, deadline)       (pages of 256, deadline-checked,
//!                                                       never a rebuild; a serve process may
//!                                                       be the publisher instead)
//!     => poll text_visible() every 20 ms to the deadline
//!     => covered ==> committed ; else ==> committed_not_yet_searchable
//! ```
//!
//! A post-commit failure anywhere here is never a rejection: the write stands, the outcome
//! says not to replay it. A receipt without a position is pending by definition (nothing can
//! be verified for it). FTS5 reports `Current`, so on FTS5 every committed write stays
//! `committed` at the first check.

use crate::types::{WriteOutcome, WriteReceipt};
use kaban_core::text_index::Visibility;
use std::time::{Duration, Instant};

/// The barrier of a public write (SS-10 "one five-second barrier per public call").
pub(crate) const SEARCH_BARRIER: Duration = Duration::from_secs(5);
/// The barrier of a hook's write: a small share of the hook watchdog (`kaban hook` answers
/// within 1.8 s whatever happens, M4-9, and its own work takes most of that in a debug
/// build). A lone hook publishes its one page in well under it; when another writer holds
/// the lock the hook reports pending without waiting for it, so it never claims readiness
/// it did not verify and never misses its deadline waiting for it.
pub(crate) const HOOK_SEARCH_BARRIER: Duration = Duration::from_millis(250);
/// How often the serving watermark is polled while the barrier waits.
const POLL: Duration = Duration::from_millis(20);

/// The clock the barrier reads and sleeps on; a fake in tests.
pub(crate) trait Clock: Send + Sync {
    fn now(&self) -> Instant;
    fn sleep(&self, d: Duration);
}

/// The process clock.
#[derive(Debug, Default)]
pub(crate) struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
    fn sleep(&self, d: Duration) {
        std::thread::sleep(d);
    }
}

/// The barrier's drain toward a receipt's position: the store's own publication (OWN-M;
/// [`StoreDrain`]), a scripted one in the tests.
pub(crate) trait LocalDrain: Send + Sync {
    /// Publish committed changes toward `target` until `deadline`, in small pages, checking
    /// the deadline before each page and before the reader refresh, never rebuilding, and
    /// returning at once when another writer holds the lock or the index must be rebuilt
    /// (the barrier then waits on the serving watermark alone).
    ///
    /// # Errors
    /// A store or index failure; the barrier logs it and keeps polling.
    fn drain_until(&self, target: u64, deadline: Instant) -> kaban_core::Result<()>;
}

/// The store as its own local drain (spec 05 OWN-M): a write marks its commit pending and the
/// barrier publishes it within its one deadline through [`GraphStore::text_publish`].
pub(crate) struct StoreDrain<'a>(pub(crate) &'a dyn kaban_core::GraphStore);

impl LocalDrain for StoreDrain<'_> {
    fn drain_until(&self, target: u64, deadline: Instant) -> kaban_core::Result<()> {
        self.0.text_publish(target, deadline)
    }
}

/// The receipt after the barrier (see the module doc): unchanged unless it was `committed`
/// and the serving reader did not cover its position within `budget`.
pub(crate) fn settle(
    receipt: WriteReceipt,
    budget: Duration,
    visible: &dyn Fn() -> kaban_core::Result<Visibility>,
    drain: Option<&dyn LocalDrain>,
    clock: &dyn Clock,
) -> WriteReceipt {
    if receipt.outcome != WriteOutcome::Committed {
        return receipt;
    }
    let deadline = clock.now() + budget;
    let Some(seq) = receipt.seq else {
        return receipt.not_yet_searchable();
    };
    let covers = |visibility: Visibility| match visibility {
        Visibility::Current => true,
        Visibility::Through(through) => i64::try_from(seq).is_ok_and(|seq| through >= seq),
        Visibility::Unavailable => false,
    };
    match visible() {
        Ok(visibility) if covers(visibility) => return receipt,
        Ok(_) => {}
        Err(e) => {
            tracing::warn!(error = %e, "the search watermark could not be read after a commit");
            return receipt.not_yet_searchable();
        }
    }
    if let Some(drain) = drain
        && let Err(e) = drain.drain_until(seq, deadline)
    {
        tracing::warn!(error = %e, "the local search drain failed after a commit");
    }
    loop {
        // expiry before coverage (amendment 7): a drain or a poll that ran past the deadline
        // is pending even when the watermark covers the position by now, and nothing reads
        // (or reloads) the serving reader once the barrier is over
        if clock.now() >= deadline {
            return receipt.not_yet_searchable();
        }
        match visible() {
            Ok(visibility) if covers(visibility) => return receipt,
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(error = %e, "the search watermark could not be read after a commit");
                return receipt.not_yet_searchable();
            }
        }
        clock.sleep(POLL);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion
mod tests {
    use super::{Clock, LocalDrain, SEARCH_BARRIER, settle};
    use crate::types::{ReceiptIdentities, WriteOutcome, WriteReceipt};
    use kaban_core::StoreError;
    use kaban_core::text_index::Visibility;
    use std::cell::Cell;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    /// A clock that advances only when slept on.
    struct FakeClock {
        start: Instant,
        elapsed: Mutex<Duration>,
    }

    impl FakeClock {
        fn new() -> Self {
            Self {
                start: Instant::now(),
                elapsed: Mutex::new(Duration::ZERO),
            }
        }
        fn elapsed(&self) -> Duration {
            *self.elapsed.lock().unwrap()
        }
    }

    impl Clock for FakeClock {
        fn now(&self) -> Instant {
            self.start + self.elapsed()
        }
        fn sleep(&self, d: Duration) {
            *self.elapsed.lock().unwrap() += d;
        }
    }

    /// A drain that records its call and reports what it was told to.
    struct ScriptedDrain {
        called: Mutex<Option<(u64, Instant)>>,
        result: fn() -> kaban_core::Result<()>,
    }

    impl LocalDrain for ScriptedDrain {
        fn drain_until(&self, target: u64, deadline: Instant) -> kaban_core::Result<()> {
            *self.called.lock().unwrap() = Some((target, deadline));
            (self.result)()
        }
    }

    fn committed(seq: Option<u64>) -> WriteReceipt {
        WriteReceipt::committed(
            &ReceiptIdentities {
                instance_id: "db".into(),
                pack_lineage_id: "lineage".into(),
            },
            seq,
        )
    }

    /// Expiry under a fake clock: a watermark that never covers the position leaves the
    /// outcome pending once the budget elapsed, and never earlier (the clock advanced through
    /// the polls to the deadline, no further); a `seq: None` receipt is pending at once.
    #[test]
    fn a_watermark_that_never_covers_the_receipt_is_pending_at_the_deadline() {
        let clock = FakeClock::new();
        let settled = settle(
            committed(Some(7)),
            SEARCH_BARRIER,
            &|| Ok(Visibility::Through(6)),
            None,
            &clock,
        );
        assert_eq!(settled.outcome, WriteOutcome::CommittedNotYetSearchable);
        assert_eq!(settled.seq, Some(7), "the position stays on the receipt");
        assert!(clock.elapsed() >= SEARCH_BARRIER, "{:?}", clock.elapsed());
        assert!(clock.elapsed() < SEARCH_BARRIER + Duration::from_millis(40));

        let clock = FakeClock::new();
        let settled = settle(
            committed(None),
            SEARCH_BARRIER,
            &|| Ok(Visibility::Current),
            None,
            &clock,
        );
        assert_eq!(settled.outcome, WriteOutcome::CommittedNotYetSearchable);
        assert_eq!(
            clock.elapsed(),
            Duration::ZERO,
            "pending by definition, no wait"
        );
    }

    /// Coverage: `Current` (FTS5) and a watermark at or past the position are `committed`
    /// without a wait; a watermark that catches up while the barrier polls is `committed`
    /// before the deadline; a rejected receipt is left alone.
    #[test]
    fn a_covering_watermark_keeps_the_receipt_committed() {
        for visibility in [
            Visibility::Current,
            Visibility::Through(7),
            Visibility::Through(9),
        ] {
            let clock = FakeClock::new();
            let settled = settle(
                committed(Some(7)),
                SEARCH_BARRIER,
                &|| Ok(visibility),
                None,
                &clock,
            );
            assert_eq!(settled.outcome, WriteOutcome::Committed, "{visibility:?}");
            assert_eq!(clock.elapsed(), Duration::ZERO);
        }
        let clock = FakeClock::new();
        let polls = Cell::new(0);
        let catching_up = || {
            polls.set(polls.get() + 1);
            Ok(if polls.get() > 3 {
                Visibility::Through(7)
            } else {
                Visibility::Through(5)
            })
        };
        let settled = settle(
            committed(Some(7)),
            SEARCH_BARRIER,
            &catching_up,
            None,
            &clock,
        );
        assert_eq!(settled.outcome, WriteOutcome::Committed);
        assert!(
            clock.elapsed() < Duration::from_millis(100),
            "{:?}",
            clock.elapsed()
        );
        let rejected = WriteReceipt::rejected(&ReceiptIdentities {
            instance_id: "db".into(),
            pack_lineage_id: "lineage".into(),
        });
        let settled = settle(
            rejected.clone(),
            SEARCH_BARRIER,
            &|| Ok(Visibility::Through(0)),
            None,
            &FakeClock::new(),
        );
        assert_eq!(settled, rejected);
    }

    /// The local drain is asked for the receipt's position with the deadline taken before
    /// anything else; its failure is logged, never a rejection: the barrier polls on and the
    /// outcome is pending at the deadline. A watermark read failure is pending at once.
    #[test]
    fn a_failing_drain_or_watermark_read_is_pending_never_rejected() {
        let clock = FakeClock::new();
        let drain = ScriptedDrain {
            called: Mutex::new(None),
            result: || Err(StoreError::Audit("drain failed".into())),
        };
        let settled = settle(
            committed(Some(7)),
            SEARCH_BARRIER,
            &|| Ok(Visibility::Through(0)),
            Some(&drain),
            &clock,
        );
        assert_eq!(settled.outcome, WriteOutcome::CommittedNotYetSearchable);
        let (target, deadline) = drain.called.lock().unwrap().unwrap();
        assert_eq!(target, 7);
        assert_eq!(
            deadline,
            clock.start + SEARCH_BARRIER,
            "the deadline was taken first"
        );

        let clock = FakeClock::new();
        let settled = settle(
            committed(Some(7)),
            SEARCH_BARRIER,
            &|| Err(StoreError::Audit("no watermark".into())),
            None,
            &clock,
        );
        assert_eq!(settled.outcome, WriteOutcome::CommittedNotYetSearchable);
        assert_eq!(clock.elapsed(), Duration::ZERO);
    }

    /// A drain that runs on the clock for as long as it is told (the pages overran the
    /// deadline), after which the watermark covers the position.
    struct SlowDrain<'a> {
        clock: &'a FakeClock,
        takes: Duration,
    }

    impl LocalDrain for SlowDrain<'_> {
        fn drain_until(&self, _target: u64, _deadline: Instant) -> kaban_core::Result<()> {
            self.clock.sleep(self.takes);
            Ok(())
        }
    }

    /// Expiry is checked before coverage (T2 review t2-4, SS-10, amendment 7): a drain whose
    /// last page completed at t = 5.1 s leaves the outcome pending even though the watermark
    /// covers the receipt by then, and the watermark is not read again after expiry (no
    /// reader refresh once the barrier is over).
    #[test]
    fn coverage_reached_after_the_deadline_is_pending_and_reads_no_watermark() {
        let clock = FakeClock::new();
        let reads = Cell::new(0);
        let covered_after_the_drain = || {
            reads.set(reads.get() + 1);
            Ok(if reads.get() == 1 {
                Visibility::Through(5)
            } else {
                Visibility::Through(7)
            })
        };
        let drain = SlowDrain {
            clock: &clock,
            takes: SEARCH_BARRIER + Duration::from_millis(100),
        };
        let settled = settle(
            committed(Some(7)),
            SEARCH_BARRIER,
            &covered_after_the_drain,
            Some(&drain),
            &clock,
        );
        assert_eq!(settled.outcome, WriteOutcome::CommittedNotYetSearchable);
        assert_eq!(reads.get(), 1, "no watermark read after expiry");
        assert_eq!(settled.seq, Some(7));
    }
}
