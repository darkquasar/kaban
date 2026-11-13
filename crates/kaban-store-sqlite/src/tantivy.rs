//! The store's own text index (spec 05 OWN-M, lane T8a; reverses design amendment 1): the
//! Tantivy adapter is composed here, not by a binary. Opening a store yields one whose index
//! exists: a file store's index lives beside the database (`<stem>.index/tantivy/`) and is
//! admitted as it stands (compatible, within the canonical history) or rebuilt from SQLite
//! under the exclusive hold before the open returns; an in-memory store gets the adapter's RAM
//! index, built at open. A write never commits the index itself (fix16: a Tantivy commit per
//! transaction cost 7-16 ms and serialized every writer on fsync): [`Write`] records the
//! store's receipt position (the change-log head after its commit) once the guard is gone, and
//! the projection runs coalesced, with no connection guard held, (a) by a serving store's own
//! projector thread (`crate::projector_thread`, lane T8b: the writers signal it, the barrier
//! and the reads wait on its publication), (b) inside the write barrier toward the receipt's
//! position within the caller's one deadline ([`SqliteStore::publish_until`] through
//! `GraphStore::text_publish`), (c) before a recall, a `stats` or a prune of a store whose
//! receipt position the index does not cover yet (lane T8d: a session position, never "the
//! index lags the head"), bounded by [`PUBLISH_BUDGET`], and (d) once when a store whose
//! position is uncovered closes (a one-shot command's exit). The mmap readers of other
//! processes stay as T2/T4 left them.
//!
//! ```text
//!   open(db)  ==> canonical head + identity (under the store's lock, then released)
//!      |         admit_installed(<db>.index/tantivy, at): compatible and seq <= head?
//!      |           yes     => TantivyIndex::open ; catch_up (lease + lock, non-blocking;
//!      |                      held elsewhere: wait, bounded, for that publisher's head)
//!      |           missing => TantivyIndex::open (created) ; catch_up builds it (the lease
//!      |                      elects one builder among concurrent openers; the rest wait)
//!      |           unfit   => exclusive_hold(db), waited for, or TextIndexRebuildRefused ;
//!      |                      wipe ; open ; rebuild
//!      v
//!   write   ==> lock ; BEGIN IMMEDIATE .. COMMIT ; unlock      [the store's transaction]
//!             ==> head moved? receipt = head                      [cheap; nothing committed]
//!   publish_until(target, deadline) ==> Projector::drain_until   [barrier, recall, close:
//!                 lock released; a halt or a failure is logged, the receipt and the barrier
//!                 report the lag; a read drains only while receipt > the watermark]
//! ```
//!
//! Lock order (constraint 1): the store's connection lock, then the adapter, never the
//! reverse. An in-memory adapter reads the store back through the shared connection
//! ([`IndexRows::shared`]), so recall releases the lock before every adapter call
//! (`recall::Access`), the post-commit projection runs after the guard is gone, and nothing
//! here calls the adapter while holding `SqliteStore::lock`.

use crate::changelog;
use crate::index_rows::{IndexRows, canonical};
use crate::sqlite::SqliteStore;
use kaban_core::text_index::{TextIndex, Visibility, index_dir};
use kaban_core::{Result, StoreError};
use kaban_index_tantivy::TantivyIndex;
use kaban_index_tantivy::admission::DatabaseHold;
use kaban_index_tantivy::projector::Projector;
use rusqlite::Connection;
use std::any::Any;
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// How long a projection the store runs on its own account (before a recall, at close) may
/// take (spec 05 SS-10: the barrier's budget; the barrier itself passes its own deadline).
pub(crate) const PUBLISH_BUDGET: Duration = Duration::from_secs(5);
/// A hook's share (`kaban hook` answers within its watchdog whatever happens, M4-9): a lone
/// hook publishes its one page in well under it; a held lock leaves the write pending.
pub(crate) const HOOK_PUBLISH_BUDGET: Duration = Duration::from_millis(250);
/// The `text-index` lease of a post-commit projection: longer than the budget, so no
/// successor takes the lease from a live projection, and renewed past a third of it.
const PUBLISH_LEASE_TTL_S: u32 = 15;
/// How long an open whose index does not admit waits for the exclusive hold a rebuild needs
/// (another process may be building that very index: its commit admits here as soon as it
/// lands) before it is refused with the reason.
const REBUILD_WAIT: Duration = Duration::from_secs(10);

/// What an open does about the index beside its database.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Attach {
    /// The ordinary and library opens: the index is admitted and caught up, or rebuilt under
    /// the exclusive hold; the open fails when neither is possible.
    Own,
    /// A diagnosis, a hook or a verification: the index as it stands when it admits, nothing
    /// otherwise (nothing is created or rebuilt; a missing index is reported, and a write
    /// still commits with its receipt pending).
    Existing,
}

/// The seed index of a store (spec 05 SS-07): the adapter this module composed, or, under
/// `text-index-override`, one a test or bench injected through `SqliteStore::with_text_index`
/// (whose publication is the injector's business: the store projects nothing into it).
pub(crate) enum Attached {
    Tantivy(Arc<TantivyIndex>),
    #[cfg(feature = "text-index-override")]
    Override(Box<dyn TextIndex + Send + Sync>),
}

impl std::fmt::Debug for Attached {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Tantivy(index) => index.fmt(f),
            #[cfg(feature = "text-index-override")]
            Self::Override(_) => f.write_str("TextIndexOverride"),
        }
    }
}

impl Attached {
    /// The index behind the seam.
    pub(crate) fn text_index(&self) -> &(dyn TextIndex + Send + Sync) {
        match self {
            Self::Tantivy(index) => index.as_ref(),
            #[cfg(feature = "text-index-override")]
            Self::Override(index) => index.as_ref(),
        }
    }

    /// The composed adapter this store publishes into and configures, `None` for an injected
    /// index.
    #[cfg_attr(not(feature = "text-index-override"), allow(clippy::unnecessary_wraps))]
    pub(crate) fn tantivy(&self) -> Option<&Arc<TantivyIndex>> {
        match self {
            Self::Tantivy(index) => Some(index),
            #[cfg(feature = "text-index-override")]
            Self::Override(_) => None,
        }
    }
}

/// The database side of the exclusive maintenance condition (SS-12) for a purge's erasure:
/// this crate's owner lock on the database, as the adapter asks for it.
struct DatabaseOwner(PathBuf);

impl DatabaseHold for DatabaseOwner {
    fn try_exclusive(&self) -> Result<Option<Box<dyn Any + Send>>> {
        Ok(crate::connection::exclusive_hold(&self.0)?
            .map(|hold| Box::new(hold) as Box<dyn Any + Send>))
    }
}

static OWNERS: AtomicU64 = AtomicU64::new(0);

/// The index directory holds record text like the database file does (core-spec S5): the
/// directory and its parent are owner-only (0700) whatever the umask, so nothing under them is
/// readable by anyone else, and the files already there are owner-only too (0600; a lock file
/// created under a read-only umask must stay writable for the next acquisition). Files Tantivy
/// creates later take the umask inside the closed directory.
fn owner_only(dir: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let io = StoreError::FileMode;
    // parent first, owner-only before the child is created inside it (a read-only umask
    // would otherwise leave the parent unwritable)
    for directory in dir.parent().into_iter().chain(std::iter::once(dir)) {
        match std::fs::create_dir(directory) {
            Ok(()) => {}
            // another opener of the same database created it meanwhile
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(io(e)),
        }
        std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700)).map_err(io)?;
    }
    for entry in std::fs::read_dir(dir).map_err(io)? {
        let entry = entry.map_err(io)?;
        if entry.file_type().map_err(io)?.is_file() {
            match std::fs::set_permissions(entry.path(), std::fs::Permissions::from_mode(0o600)) {
                Ok(()) => {}
                // another opener's writer removed it meanwhile (a merged segment, a temp file
                // of its commit): a file that is gone needs no mode
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(io(e)),
            }
        }
    }
    Ok(())
}

/// One admission or rebuild at a time in this process: concurrent openers of one database
/// (threads of one process) would each find the index missing and race for the exclusive
/// hold, which admits one and refuses the rest; serialized, the second finds what the first
/// built. Another process's opener is still refused by the hold itself.
static ATTACHING: Mutex<()> = Mutex::new(());

/// The lease owner of one store's projections: this process and this store, so two stores of
/// one process on one database never share a lease.
pub(crate) fn owner() -> String {
    format!(
        "kaban:{}:store:{}",
        std::process::id(),
        OWNERS.fetch_add(1, Ordering::Relaxed)
    )
}

impl SqliteStore {
    /// The index of a file store at `db`, as `attach` says (module doc). `conn` is the store's
    /// connection, lent for the canonical read only: it is released before the adapter is
    /// opened, so the adapter's own reads (a rebuild walks every active row through its
    /// read-only connection) never meet the store's lock.
    pub(crate) fn open_file_index(
        &self,
        db: &Path,
        attach: Attach,
    ) -> Result<Option<Arc<TantivyIndex>>> {
        let _one_at_a_time = ATTACHING
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = index_dir(db);
        let admission = |at: &kaban_core::text_index::Canonical| match TantivyIndex::admit_installed(
            &dir, at,
        ) {
            Ok(Some(seq)) if seq <= at.seq => None,
            Ok(Some(_)) => Some("ahead"),
            Ok(None) => Some("missing"),
            Err(class) => Some(class),
        };
        let admitted = admission(&canonical(&self.lock())?);
        let rows = || -> Result<Box<dyn kaban_core::text_index::Rows>> {
            Ok(Box::new(IndexRows::open(db)?))
        };
        // only a writable attach prepares the directory and tightens modes (review t8a-6): a
        // diagnosis or a verification reads the admitted index exactly as it stands
        let open = |rows| -> Result<Arc<TantivyIndex>> {
            if attach == Attach::Own {
                owner_only(&dir)?;
            }
            let index = TantivyIndex::open(&dir, self.ontology(), self.recall_config(), rows)?
                .with_database_hold(Box::new(DatabaseOwner(db.to_path_buf())));
            if attach == Attach::Own {
                owner_only(&dir)?;
            }
            Ok(Arc::new(index))
        };
        match (admitted, attach) {
            // admitted as it stands; or no index yet (a fresh database, or one this binary
            // opens first) or one created but never committed (another opener is building it,
            // or a build was interrupted): opened here and built by the catch-up that follows
            // under the projector's lease and lock, which elect one builder among concurrent
            // openers (nothing to wipe, no hold needed)
            (None, _) | (Some("missing" | "payload"), Attach::Own) => Ok(Some(open(rows()?)?)),
            (Some(why), Attach::Existing) => {
                tracing::debug!(why, "the text index is not served at this open");
                Ok(None)
            }
            (Some(mut why), Attach::Own) => {
                // an index this build cannot serve: the rebuild replaces the directory other
                // processes may be serving from, so it runs only with the database to this
                // process and the directory to this opener (review t8a-1: the writer lock and
                // the readers' mark, taken before anything is removed, and the lock files
                // themselves are never unlinked, so no holder's lock is stolen). An opener
                // waits a bounded time for both, re-admitting meanwhile (another opener may be
                // rebuilding this index), then is refused with the reason, never served stale.
                // While it waits it is not using the database: its own presence is lifted
                // (per handle, never an admitted store's of this process; T8c review t8c-2,
                // t8c-4), so that of N concurrent openers of one unfit index one can take the
                // hold and rebuild for all, and taken again before it goes on
                let deadline = Instant::now() + REBUILD_WAIT;
                let mut lifted = None;
                let hold = loop {
                    if let Some(hold) = crate::connection::exclusive_hold(db)? {
                        if !dir.is_dir() || kaban_index_tantivy::admission::clear_for_rebuild(&dir)?
                        {
                            break hold;
                        }
                        tracing::debug!(
                            "another adapter holds the index directory; the rebuild waits"
                        );
                    }
                    if lifted.is_none() {
                        lifted = Some(crate::connection::lift_presence(db)?);
                    }
                    std::thread::sleep(Duration::from_millis(50));
                    match admission(&canonical(&self.lock())?) {
                        None => {
                            if let Some(lifted) = lifted.take() {
                                lifted.restore()?;
                            }
                            return Ok(Some(open(rows()?)?));
                        }
                        Some(class) => why = class,
                    }
                    if Instant::now() >= deadline {
                        return Err(StoreError::TextIndexRebuildRefused(
                            "another session holds the database or its text index; close the other kaban processes and retry",
                        ));
                    }
                };
                // the hold holds the exclusive lock: the waiter's claim is only bookkeeping now
                if let Some(lifted) = lifted.take() {
                    lifted.restore()?;
                }
                tracing::info!(why, "rebuilding the text index from the database");
                let index = open(rows()?)?;
                index.rebuild()?;
                owner_only(&dir)?;
                drop(hold);
                Ok(Some(index))
            }
        }
    }

    /// The RAM index of an in-memory store, built over the store's own connection (empty at
    /// open: the payload sits at the canonical head, 0 or the fixture's).
    pub(crate) fn open_memory_index(&self) -> Result<Arc<TantivyIndex>> {
        let rows = Box::new(IndexRows::shared(Arc::clone(self.connection())));
        let index = Arc::new(TantivyIndex::in_ram(
            self.ontology(),
            self.recall_config(),
            rows,
        )?);
        index.rebuild()?;
        Ok(index)
    }

    /// Bring an admitted index to the head at open: the projector's catch-up under lease and
    /// lock, rebuilding under the lock when the payload cannot be resumed from (SS-09's
    /// recovery). Lease and lock are taken non-blocking: an index that serves a payload is
    /// admitted as it stands (a stopped holder leaves committed writes pending, SS-09, and the
    /// watermark reports the lag); one that serves nothing (missing, never committed) while
    /// another process holds lease or lock (a serving process building it, or the unexpired
    /// lease of a crashed projector) makes this opener wait up to [`REBUILD_WAIT`] for that
    /// publisher, retrying the catch-up meanwhile, and refuses with
    /// [`StoreError::TextIndexUnavailable`] when nothing came to be served (review t8a-2): an
    /// open never yields an index that silently answers empty.
    pub(crate) fn catch_up(&self) -> Result<()> {
        let Some(index) = self.text_index.as_ref().and_then(Attached::tantivy) else {
            return Ok(());
        };
        let deadline = Instant::now() + REBUILD_WAIT;
        loop {
            let progress =
                Projector::new(index, self, self, &self.owner, PUBLISH_LEASE_TTL_S).catch_up()?;
            let Some(halt) = progress.halt else {
                return Ok(());
            };
            if let Visibility::Through(seq) = index.visible()? {
                tracing::debug!(?halt, seq, "the text index is served as it stands at open");
                return Ok(());
            }
            if Instant::now() >= deadline {
                tracing::warn!(
                    ?halt,
                    seq = progress.seq,
                    "the text index serves nothing at open: another publisher holds it"
                );
                return Err(StoreError::TextIndexUnavailable);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Publish the pending commits (constraint 2) toward `target` until `deadline`, under
    /// lease and lock taken non-blocking, in bounded publications, never a rebuild. Called
    /// with no connection guard held. A halt (the lease or the lock held by a serving process,
    /// the deadline) leaves the position uncovered; nothing here fails a write: the receipt
    /// keeps its position and the barrier reports what the watermark says.
    ///
    /// # Errors
    /// A store or index failure (the caller logs it; the write stands).
    pub(crate) fn publish_until(&self, target: u64, deadline: Instant) -> Result<()> {
        let Some(index) = self.text_index.as_ref().and_then(Attached::tantivy) else {
            return Ok(());
        };
        self.publish_toward(index, target, deadline)
    }

    /// [`Self::publish_until`] over `index`.
    fn publish_toward(&self, index: &TantivyIndex, target: u64, deadline: Instant) -> Result<()> {
        // a serving store routes through its projector thread (`crate::projector_thread`):
        // the wait ends at the thread's publication of the position, at another process's
        // (the watermark covers it), at the deadline, or at once when the thread is not the
        // publisher now (then the barrier waits on the watermark alone, as after a halt)
        if let Some(thread) = self.projector.as_ref().filter(|t| t.alive()) {
            let target = if target == u64::MAX {
                changelog::head(&self.lock())?
            } else {
                target
            };
            thread.await_published(target, deadline, &|| {
                Self::index_covers(index, target).unwrap_or(false)
            });
            return Ok(());
        }
        let progress = Projector::new(index, self, self, &self.owner, PUBLISH_LEASE_TTL_S)
            .drain_until(target, deadline)?;
        if let Some(halt) = progress.halt {
            tracing::debug!(
                ?halt,
                seq = progress.seq,
                target,
                "the projection stopped short; the barrier waits on the watermark"
            );
        }
        Ok(())
    }

    /// Whether the index's payload covers `position` now.
    fn index_covers(index: &TantivyIndex, position: u64) -> Result<bool> {
        Ok(match index.visible()? {
            Visibility::Through(seq) => u64::try_from(seq).is_ok_and(|seq| seq >= position),
            Visibility::Current => true,
            Visibility::Unavailable => false,
        })
    }

    /// Publish toward the store's receipt position on its own account, within
    /// [`PUBLISH_BUDGET`] (a hook's share for a hook open), at close; a failure is logged,
    /// never raised.
    pub(crate) fn publish_pending(&self) {
        if let Some((index, receipt)) = self.uncovered_receipt() {
            self.publish_now(index, receipt);
        }
    }

    /// Before a read that expects the committed writes of this store (a recall, the cursors
    /// `stats` and a prune floor read): publish, within the same budget, toward this store's
    /// session position when the index does not cover it yet (astra: a read never drains for
    /// a position it did not write; another session's pending commits are that session's,
    /// the serving thread's or the barrier's to publish); a lease held elsewhere (a serving
    /// process is the publisher) halts it at once and the read serves the index as it stands.
    pub(crate) fn publish_before_read(&self) {
        if let Some((index, receipt)) = self.uncovered_receipt() {
            self.publish_now(index, receipt);
        }
    }

    /// The store's receipt position while the index does not cover it (one payload read; a
    /// store that wrote nothing reads nothing).
    fn uncovered_receipt(&self) -> Option<(&TantivyIndex, u64)> {
        let index = self.text_index.as_ref().and_then(Attached::tantivy)?;
        let receipt = self.receipt.load(Ordering::Acquire);
        (receipt > 0 && !Self::index_covers(index, receipt).unwrap_or(true))
            .then_some((index, receipt))
    }

    fn publish_now(&self, index: &TantivyIndex, receipt: u64) {
        let deadline = Instant::now() + self.publish_budget;
        if let Err(error) = self.publish_toward(index, receipt, deadline) {
            tracing::warn!(%error, "the pending projection failed; the writes stand");
        }
    }

    /// The connection for one write (module doc): locked like [`SqliteStore::lock`], and when
    /// the guard goes, with the lock released, the change log's head becomes this store's
    /// receipt position if it moved (`SqliteStore::receipt`; the serving thread is signalled).
    /// Every write of a canonical row takes its connection here; reads keep `lock`.
    pub(crate) fn write(&self) -> Write<'_> {
        let guard = self.lock();
        let head = changelog::head(&guard).unwrap_or(0);
        Write {
            store: self,
            guard: Some(guard),
            head,
        }
    }
}

/// See [`SqliteStore::write`].
pub(crate) struct Write<'s> {
    store: &'s SqliteStore,
    /// `Some` until the drop releases it, before the commit is marked pending.
    guard: Option<MutexGuard<'s, Connection>>,
    /// The change log's head when the lock was taken.
    head: u64,
}

impl Deref for Write<'_> {
    type Target = Connection;
    #[allow(clippy::expect_used)] // the one invariant of the field: taken by the drop only
    fn deref(&self) -> &Connection {
        self.guard
            .as_deref()
            .expect("the write's connection is taken only by its drop")
    }
}

impl DerefMut for Write<'_> {
    #[allow(clippy::expect_used)] // see `deref`
    fn deref_mut(&mut self) -> &mut Connection {
        self.guard
            .as_deref_mut()
            .expect("the write's connection is taken only by its drop")
    }
}

impl Drop for Write<'_> {
    fn drop(&mut self) {
        let Some(guard) = self.guard.take() else {
            return;
        };
        let head = changelog::head(&guard).unwrap_or(0);
        drop(guard);
        if head > self.head {
            // the receipt position: a read of this store waits for it, a close publishes it
            self.store.receipt.fetch_max(head, Ordering::AcqRel);
            // a serving store's thread publishes it within the window (lane T8b)
            if let Some(thread) = &self.store.projector {
                thread.signal_commit();
            }
        }
    }
}

/// The store's connection as the in-memory adapter shares it.
pub(crate) type SharedConnection = Arc<Mutex<Connection>>;
