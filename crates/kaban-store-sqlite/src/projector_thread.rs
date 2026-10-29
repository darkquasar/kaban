//! The serving store's projector thread (spec 05 SS-21, OWN-M; lane T8b moved the binary's
//! mode-B loop in here): one persistent thread per serving store competes for the
//! `text-index` lease and, holding it and the writer lock, owns the index writer and publishes
//! every process's commits as they land. This process's writers signal it after their
//! transaction (a coalescing notification, never a queue of rows: the change log stays the
//! feed), the barrier and the reads that wait for a position route through it (a wait on its
//! publication, never a second writer), and `PRAGMA data_version` every 100 ms stays as the
//! discovery of other processes' writes. A store that does not serve has no thread: a
//! one-shot command drains inline once (`crate::tantivy`).
//!
//! ```text
//!   Write::drop ~> signal_commit()            the window opens at the first signal (100 ms)
//!   publish_until ~> await_published(target)  wanted = max(wanted, target): publication now
//!        |                                      waits on `published` (20 ms slices, its deadline)
//!        v
//!   thread: Projector::poll_with(observe = data_version, signals = this)
//!             wait(tick): Publish when the window closed or wanted > published, else Tick
//!             published(seq) after every run that reached the head
//!           not the publisher (lease or lock held elsewhere): retry after a pause
//! ```
//!
//! The thread talks to the database through its own [`ChangeLogStore`]/[`LeaseStore`] over
//! the store's shared connection ([`Db`]): the lease and the acknowledgement are short
//! transactions, never held across a wait, and nothing here takes the connection while an
//! adapter call runs (constraint 1).

use crate::changelog;
use crate::index_rows::IndexRows;
use crate::leases;
use crate::sql_error::SqlResultExt;
use crate::tantivy::SharedConnection;
use kaban_core::text_index::{TextIndex, Visibility};
use kaban_core::{BackendSource, Change, ChangeLogStore, Lease, LeaseStore, Result, StoreError};
use kaban_index_tantivy::TantivyIndex;
use kaban_index_tantivy::projector::Projector;
use kaban_index_tantivy::projector::poll::{POLL_TICK, Signals, Wake};
use rusqlite::Connection;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// The `text-index` lease of the thread: renewed every tick while it runs, so it is short,
/// and the window in which a stopped or dead holder keeps every other writer pending
/// (`committed_not_yet_searchable`) before a successor is elected (SS-21).
pub(crate) const LEASE_TTL_S: u32 = 5;
/// The pause before a thread that found the lease or the lock held, or lost its lease, tries
/// again (a barrier's signal ends it early).
const RETRY: Duration = Duration::from_secs(1);
/// The publication window of a local commit (astra, projection research): non-resetting,
/// from the first commit after the last publication; a barrier overrides it.
const WINDOW: Duration = Duration::from_millis(100);
/// How often a waiter re-reads the index while it waits (another process may publish).
const WAIT_SLICE: Duration = Duration::from_millis(20);

/// A test's injected worker death (T8d review t8d-5): the next acquisition panics, once, with
/// lease and lock held.
#[cfg(test)]
static PANIC_AT_ACQUISITION: AtomicBool = AtomicBool::new(false);

/// See the module doc. Stopped and joined on drop (with the store), which releases the lease
/// and the lock; a process that dies instead leaves the lock to the kernel and the lease to
/// its expiry.
pub(crate) struct ProjectorThread {
    shared: Arc<Shared>,
    thread: Option<std::thread::JoinHandle<()>>,
}

struct Shared {
    stop: AtomicBool,
    state: Mutex<State>,
    /// The loop waits on it: a commit, a barrier, the stop.
    changed: Condvar,
    /// Waiters wait on it: a run published.
    published: Condvar,
}

#[derive(Default)]
struct State {
    /// When the first unpublished local commit was signalled; `None` while quiet.
    window_from: Option<Instant>,
    /// The highest position a barrier or a read waits for.
    wanted: u64,
    /// The payload's sequence after the thread's last run.
    published: u64,
    /// Whether the thread holds lease and lock: `Some(true)` from its acquisition until the
    /// run returns, `Some(false)` after a run that was not the publisher (or failed) until the
    /// next election, `None` while an election is in flight (a waiter waits it out).
    holding: Option<bool>,
}

impl Shared {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Signals for Shared {
    fn wait(&self, tick: Duration) -> Wake {
        let deadline = Instant::now() + tick;
        let mut state = self.state();
        loop {
            if self.stop.load(Ordering::Acquire) {
                return Wake::Tick;
            }
            if state.wanted > state.published {
                return Wake::Publish;
            }
            let now = Instant::now();
            let until = match state.window_from {
                Some(from) if now >= from + WINDOW => {
                    state.window_from = None;
                    return Wake::Publish;
                }
                Some(from) => deadline.min(from + WINDOW),
                None => deadline,
            };
            if now >= until {
                return Wake::Tick;
            }
            state = self
                .changed
                .wait_timeout(state, until - now)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
    }

    fn acquired(&self) {
        // the injected worker death of the t8d-5 test
        #[cfg(test)]
        assert!(
            !PANIC_AT_ACQUISITION.swap(false, Ordering::AcqRel),
            "injected: the projector worker died at its acquisition"
        );
        self.state().holding = Some(true);
    }

    fn published(&self, seq: u64) {
        let mut state = self.state();
        state.published = seq;
        self.published.notify_all();
    }

    fn wanted(&self) -> u64 {
        self.state().wanted
    }
}

impl std::fmt::Debug for ProjectorThread {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self.shared.state();
        f.debug_struct("ProjectorThread")
            .field("holding", &state.holding)
            .field("published", &state.published)
            .finish_non_exhaustive()
    }
}

impl ProjectorThread {
    /// Start the thread of a serving store over `index`, `conn` (the store's connection,
    /// shared) and the database at `db` (its observation connection).
    pub(crate) fn start(
        index: Arc<TantivyIndex>,
        conn: SharedConnection,
        db: &Path,
    ) -> Result<Self> {
        let observation = IndexRows::open(db)?;
        let shared = Arc::new(Shared {
            stop: AtomicBool::new(false),
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
            published: Condvar::new(),
        });
        let loop_shared = Arc::clone(&shared);
        let owner = format!("kaban:{}:serve", std::process::id());
        let thread = std::thread::Builder::new()
            .name("text-index-projector".into())
            .spawn(move || {
                let db = Db(conn);
                let shared = loop_shared;
                // t8b-4: a panic unwinds the session (lease given back, lock released) and
                // this guard leaves no waiter on a holder that is gone
                let _gone = Gone(&shared);
                let observe = || observation.data_version();
                while !shared.stop.load(Ordering::Acquire) {
                    shared.state().holding = None;
                    let projector = Projector::new(&index, &db, &db, &owner, LEASE_TTL_S);
                    match projector.poll_with(&observe, &shared.stop, shared.as_ref()) {
                        Ok(progress) => match progress.halt {
                            None => {}
                            Some(halt) => tracing::debug!(
                                ?halt,
                                seq = progress.seq,
                                "the projector thread is not the publisher; retrying"
                            ),
                        },
                        Err(e) => {
                            tracing::warn!(error = %e, "the projector thread failed; retrying");
                        }
                    }
                    shared.settle(|| match index.visible() {
                        Ok(Visibility::Through(seq)) => u64::try_from(seq).ok(),
                        _ => None,
                    });
                    shared.pause(RETRY);
                }
            })
            .map_err(|e| StoreError::Backend {
                source: BackendSource::new(e),
            })?;
        Ok(Self {
            shared,
            thread: Some(thread),
        })
    }

    /// Whether the worker still runs (t8b-4): a worker that panicked holds nothing, and the
    /// store publishes inline, fenced, as a store without a thread does.
    pub(crate) fn alive(&self) -> bool {
        self.thread.as_ref().is_some_and(|t| !t.is_finished())
    }

    /// A commit of this store landed: open the window (the first signal starts it; later
    /// ones fall into it).
    pub(crate) fn signal_commit(&self) {
        self.shared.signal_commit();
    }

    /// Wait until the thread published `target` or `covered` says the index covers it (another
    /// process published), or `deadline` passes: `true` when covered. Returns at once when the
    /// thread found itself not the publisher (the caller then serves, or its barrier waits
    /// on, the watermark as it stands, exactly as a halted inline drain left it); an election
    /// in flight is waited out.
    pub(crate) fn await_published(
        &self,
        target: u64,
        deadline: Instant,
        covered: &dyn Fn() -> bool,
    ) -> bool {
        self.shared.await_published(target, deadline, covered)
    }
}

impl Shared {
    /// See [`ProjectorThread::signal_commit`].
    fn signal_commit(&self) {
        let mut state = self.state();
        if state.window_from.is_none() {
            state.window_from = Some(Instant::now());
            self.changed.notify_one();
        }
    }

    /// See [`ProjectorThread::await_published`].
    fn await_published(&self, target: u64, deadline: Instant, covered: &dyn Fn() -> bool) -> bool {
        let mut state = self.state();
        if state.wanted < target {
            state.wanted = target;
        }
        self.changed.notify_one();
        loop {
            if state.published >= target {
                return true;
            }
            if state.holding == Some(false) {
                return false;
            }
            drop(state);
            // t8b-2: the deadline first; past it nothing reloads and nothing is taken
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            if covered() {
                return true;
            }
            state = self
                .published
                .wait_timeout(self.state(), WAIT_SLICE.min(deadline - now))
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
    }

    /// A run returned without holding (not the publisher, or failed): record it, with the
    /// external `watermark` (t8b-1: a follower's view of the published position is the
    /// winner's, so a waiter it covers stops asking for elections), and release the waiters
    /// of this election to the watermark. The watermark is read with the state released
    /// (t8d-2: it may reload the index and wait on Tantivy's meta lock; a writer's commit
    /// signal and a waiter's deadline never wait on it).
    fn settle(&self, watermark: impl FnOnce() -> Option<u64>) {
        let seq = watermark();
        let mut state = self.state();
        state.holding = Some(false);
        if let Some(seq) = seq {
            state.published = state.published.max(seq);
        }
        self.published.notify_all();
    }
}

/// The worker's exit, a panic included (t8b-4): no election is in flight and nothing is held,
/// so a waiter returns to the watermark instead of waiting on a dead thread.
struct Gone<'s>(&'s Shared);

impl Drop for Gone<'_> {
    fn drop(&mut self) {
        self.0.state().holding = Some(false);
        self.0.published.notify_all();
    }
}

impl Shared {
    /// The pause between elections, ended early by a signal or the stop, but never before a
    /// tick passed (t8b-1: a follower under a winning publisher asks for the lease, a write
    /// transaction, at most once per tick while a waiter waits, once per pause otherwise).
    fn pause(&self, pause: Duration) {
        let started = Instant::now();
        let until = started + pause;
        let mut state = self.state();
        while !self.stop.load(Ordering::Acquire) {
            if Instant::now() >= started + POLL_TICK
                && (state.wanted > state.published || state.window_from.is_some())
            {
                // a barrier or a commit waits: try the election now, and clear the window so
                // the run that follows is not asked for twice
                state.window_from = None;
                return;
            }
            let now = Instant::now();
            if now >= until {
                return;
            }
            state = self
                .changed
                .wait_timeout(state, (until - now).min(POLL_TICK))
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
    }
}

impl Drop for ProjectorThread {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        self.shared.changed.notify_all();
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            tracing::error!("the projector thread panicked");
        }
    }
}

/// The thread's side of the database: the change log and the leases over the store's shared
/// connection, each call one short transaction (`SqliteStore` implements the same traits for
/// its callers; the thread cannot borrow the store it lives in).
struct Db(SharedConnection);

impl Db {
    fn lock(&self) -> MutexGuard<'_, Connection> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn immediate<T>(
        &self,
        body: impl FnOnce(&rusqlite::Transaction<'_>) -> Result<T>,
    ) -> Result<T> {
        let mut conn = self.lock();
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .sql()?;
        let out = body(&tx)?;
        tx.commit().sql()?;
        Ok(out)
    }
}

impl ChangeLogStore for Db {
    fn changes_read(&self, after: u64, limit: usize) -> Result<Vec<Change>> {
        changelog::read(&self.lock(), after, limit)
    }
    fn changes_head(&self) -> Result<u64> {
        changelog::head(&self.lock())
    }
    fn changes_ack(&self, consumer: &str, upto: u64, fence: u64) -> Result<()> {
        self.immediate(|tx| changelog::ack(tx, consumer, upto, fence))
    }
    fn changes_register(&self, consumer: &str, fence: u64) -> Result<u64> {
        self.immediate(|tx| changelog::register(tx, consumer, fence))
    }
    fn changes_lag_by_consumer(&self) -> Result<BTreeMap<String, u64>> {
        changelog::lag_by_consumer(&self.lock())
    }
    fn changes_prune(&self, tail: u64) -> Result<u64> {
        self.immediate(|tx| changelog::prune(tx, tail))
    }
}

impl LeaseStore for Db {
    fn lease_acquire(&self, name: &str, owner: &str, ttl_s: u32) -> Result<Option<Lease>> {
        self.immediate(|tx| leases::acquire(tx, name, owner, ttl_s))
    }
    fn lease_renew(&self, lease: &Lease, ttl_s: u32) -> Result<()> {
        self.immediate(|tx| leases::renew(tx, lease, ttl_s))
    }
    fn lease_release(&self, lease: &Lease) -> Result<()> {
        self.immediate(|tx| leases::release(tx, lease))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion
mod tests {
    use super::{PANIC_AT_ACQUISITION, Shared, State};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::{Duration, Instant};

    fn shared() -> Arc<Shared> {
        Arc::new(Shared {
            stop: AtomicBool::new(false),
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
            published: Condvar::new(),
        })
    }

    /// T8d review t8d-2 (SS-10, SS-21): the follower's watermark read after a run (a reload,
    /// Tantivy's meta lock, a canonical read) runs with the state released. A waiter past its
    /// deadline returns at its deadline and a writer's commit signal returns at once while
    /// that read is slow; the watermark still reaches the waiters once it is read.
    #[test]
    fn a_slow_watermark_read_delays_neither_an_expired_waiter_nor_a_commit_signal() {
        let shared = shared();
        shared.state().holding = Some(true);
        let slow = Duration::from_millis(400);
        let reader = {
            let shared = Arc::clone(&shared);
            std::thread::spawn(move || {
                shared.settle(|| {
                    std::thread::sleep(slow);
                    Some(7)
                });
            })
        };
        std::thread::sleep(Duration::from_millis(50));
        let started = Instant::now();
        let deadline = started + Duration::from_millis(60);
        let covered = shared.await_published(7, deadline, &|| false);
        let waited = started.elapsed();
        assert!(!covered, "nothing published yet");
        assert!(
            waited < slow / 2,
            "the waiter returned at its deadline, not after the read: {waited:?}"
        );
        let started = Instant::now();
        shared.signal_commit();
        assert!(
            started.elapsed() < slow / 2,
            "the commit signal did not wait for the read"
        );
        reader.join().unwrap();
        let state = shared.state();
        assert_eq!(state.holding, Some(false));
        assert_eq!(state.published, 7, "the watermark reached the state");
    }

    /// T8d review t8d-5 (t8b-4, SS-21): a worker that panics while it holds lease and lock
    /// (injected at the acquisition) leaves neither behind: the unwinding releases both, the
    /// store finds its thread gone and publishes inline, fenced, as a store without a thread
    /// does, so the next write is searchable through the barrier and the serve lease is free.
    #[test]
    fn a_worker_panic_releases_lease_and_lock_and_the_store_publishes_inline() {
        let dir = std::env::temp_dir().join(format!("t8d5-worker-panic-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        PANIC_AT_ACQUISITION.store(true, Ordering::Release);
        let store = crate::SqliteStore::open(&dir.join("kaban.db"), &dir.join("keys"))
            .unwrap()
            .serving(true)
            .unwrap();
        let thread = store
            .projector
            .as_ref()
            .expect("a serving store has a thread");
        let deadline = Instant::now() + Duration::from_secs(5);
        while thread.alive() {
            assert!(Instant::now() < deadline, "the worker never died");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            !PANIC_AT_ACQUISITION.load(Ordering::Acquire),
            "the worker died at its acquisition, holding lease and lock"
        );
        assert_eq!(thread.shared.state().holding, Some(false), "nothing held");
        let ctx = kaban_core::WriteCtx::new(
            "v1",
            kaban_core::WorkspaceId::default(),
            kaban_core::Actor::Agent,
        );
        let note = kaban_core::NewNode {
            kind: "Fact".into(),
            text: "quokka after the worker died".into(),
            ..Default::default()
        };
        kaban_core::MemoryStore::remember(&store, note, &[], &ctx).unwrap();
        let head = kaban_core::ChangeLogStore::changes_head(&store).unwrap();
        kaban_core::MemoryStore::text_publish(
            &store,
            head,
            Instant::now() + Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(
            kaban_core::MemoryStore::text_visible(&store).unwrap(),
            kaban_core::text_index::Visibility::Through(i64::try_from(head).unwrap()),
            "the inline takeover published through the barrier"
        );
        // lease and lock were released by the unwinding: the inline publication took both
        let lease =
            kaban_core::LeaseStore::lease_acquire(&store, "text-index", "probe", 5).unwrap();
        assert!(lease.is_some(), "the serve lease is free");
        kaban_core::LeaseStore::lease_release(&store, &lease.unwrap()).unwrap();
        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
