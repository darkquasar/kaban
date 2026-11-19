//! Failpoints of the pin protocol (spec 11 T5 Step 4, KER-03, PC-8): the windows a race test
//! stops a reader or a reclamation in. Test builds (`cfg(test)`, `failpoints`) run the hook a
//! test installs for a window; other builds compile every window to nothing.

/// A window of the pin protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(
    not(feature = "failpoints"),
    expect(
        unreachable_pub,
        reason = "public for the race tests (`catalog_testing::Window`, `failpoints` only)"
    )
)]
pub enum Window {
    /// A reader has read its candidate generation and holds no pin lock yet.
    ReaderCandidate,
    /// A reader holds the pin lock of its candidate and has not begun its read transaction.
    ReaderLocked,
    /// A reader's read transaction is open and passed the floor check; nothing is copied yet
    /// (a snapshot keeps it open while its closure runs).
    ReaderInTransaction,
    /// A reclamation has committed its floor and not re-scanned the pin locks yet.
    ReclaimFloorCommitted,
    /// A compaction unit's claim is committed and its merge work has not begun.
    CompactClaimed,
    /// A verification has read its rows and not hashed the live extents yet.
    VerifyHashing,
}

/// Maintenance boundaries at which a test can cancel or expire the unit budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(
    not(feature = "failpoints"),
    expect(unreachable_pub, reason = "test seam")
)]
pub enum BudgetWindow {
    /// A canonical graph row is about to be loaded.
    GraphLoading,
    /// An old graph extent is about to be hashed.
    GraphHashing,
    /// Engine output is ready, before appending or publishing it.
    BeforePublish,
}

#[cfg(any(test, feature = "failpoints"))]
pub(crate) use hooks::Failpoints;

#[cfg(any(test, feature = "failpoints"))]
mod hooks {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex, PoisonError};

    use super::{BudgetWindow, Window};
    use kaban_core::query_budget::QueryBudget;

    pub(crate) type BudgetHook = Arc<dyn Fn(BudgetWindow, &mut QueryBudget) + Send + Sync>;

    /// A test's hook for one window: it may block (bounded by the test) to hold the window open.
    pub(crate) type Hook = Arc<dyn Fn() + Send + Sync>;

    /// The installed hooks of one catalog, and how often its readers retried (PC-8 step 4).
    #[derive(Default)]
    pub(crate) struct Failpoints {
        hooks: Mutex<[Option<Hook>; 6]>,
        retries: AtomicU64,
        budget: Mutex<Option<BudgetHook>>,
    }

    impl std::fmt::Debug for Failpoints {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("Failpoints")
                .field("retries", &self.retries)
                .finish_non_exhaustive()
        }
    }

    fn slot(window: Window) -> usize {
        match window {
            Window::ReaderCandidate => 0,
            Window::ReaderLocked => 1,
            Window::ReaderInTransaction => 2,
            Window::ReclaimFloorCommitted => 3,
            Window::CompactClaimed => 4,
            Window::VerifyHashing => 5,
        }
    }

    impl Failpoints {
        pub(crate) fn set_budget(&self, hook: Option<BudgetHook>) {
            *self.budget.lock().unwrap_or_else(PoisonError::into_inner) = hook;
        }
        pub(crate) fn budget(&self, window: BudgetWindow, budget: &mut QueryBudget) {
            let hook = self
                .budget
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone();
            if let Some(hook) = hook {
                hook(window, budget);
            }
        }
        /// Install (`Some`) or remove (`None`) the hook of `window`.
        pub(crate) fn set(&self, window: Window, hook: Option<Hook>) {
            self.hooks.lock().unwrap_or_else(PoisonError::into_inner)[slot(window)] = hook;
        }
        /// Run the hook of `window`, if any, without holding the registry while it runs.
        pub(crate) fn hit(&self, window: Window) {
            let hook =
                self.hooks.lock().unwrap_or_else(PoisonError::into_inner)[slot(window)].clone();
            if let Some(hook) = hook {
                hook();
            }
        }
        /// Count one reader retry.
        pub(crate) fn retried(&self) {
            // a statistic: no other memory depends on it
            self.retries.fetch_add(1, Ordering::Relaxed);
        }
        /// The readers' retries so far.
        pub(crate) fn retries(&self) -> u64 {
            self.retries.load(Ordering::Relaxed)
        }
    }
}
