//! The change-feed projector (spec 05 SS-09, SS-21): the `text-index` consumer that carries
//! committed record changes into the index in publications of several chunks and one commit,
//! the commit payload being the watermark and the resume point.
//!
//! ```text
//!   lease "text-index" (election)  =>  Writer::acquire (the directory lock: the one fence)
//!     =>  per publication, chunks of CHUNK rows:  Rows::replay(after = staged or payload.seq)
//!           resume_point: From | Rebuild                      [checked on every chunk's read]
//!           stage: Rows::current(ids, deduplicated) ; delete_term(id)* ; add_document*
//!         until the head, the target, a waited-for position, a bound (rows, decoded bytes)
//!         or the deadline, then commit{seq} once ; changes_ack(seq, fence)   [retention only]
//! ```
//!
//! The bounds (astra, projection research) are [`INTERACTIVE`] for a run that starts quiet and
//! [`BULK`] from the publication after one that filled its bound (a backlog larger than a
//! window: a large ingest) until a publication reaches the head (the final flush); no empty
//! commit is ever made. Three entry points share that loop: [`Projector::catch_up`] (a lone
//! writer bringing the index to the head, rebuilding when the payload cannot be trusted),
//! [`Projector::drain_until`] (the write barrier's bounded drain, SS-10: a target, a deadline,
//! never a rebuild) and [`Projector::hold`] (a serving process keeping lease and lock, renewing
//! the lease and relinquishing both on its loss). Whoever holds the lock is the only publisher;
//! a lease elects who tries, and a holder that lost its lease stops before its next commit. The
//! mode-B wake-up on `PRAGMA data_version` (lane T4) replaces [`Projector::hold`]'s tick.

pub mod capture;
pub mod poll;
mod recover;

use crate::payload::Payload;
use crate::writer::Writer;
use crate::{Error, TantivyIndex};
use kaban_core::SinkId;
use kaban_core::text_index::Canonical;
use kaban_core::{ChangeLogStore, Lease, LeaseStore, StoreError};
use poll::Signals;
use recover::{Resume, resume_point};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// The processing chunk: the feed rows read, deduplicated and staged at a time (design
/// amendment 7; a chunk is not a commit).
pub const CHUNK: usize = 256;
/// The bounds of a publication: it commits at the first of these (or the head, the target, a
/// waited-for position, the deadline).
#[derive(Debug, Clone, Copy)]
pub struct Bounds {
    /// Feed rows staged.
    pub rows: usize,
    /// Decoded indexed fields staged (text, summaries, tags, identifier values, anchor keys).
    pub bytes: usize,
}
/// The bounds of an interactive run: a publication window's worth.
pub const INTERACTIVE: Bounds = Bounds {
    rows: 2048,
    bytes: 4 << 20,
};
/// The bounds while a backlog larger than [`INTERACTIVE`] lasts (a large ingest): fewer,
/// larger commits, the last one at the head.
pub const BULK: Bounds = Bounds {
    rows: 10_000,
    bytes: 16 << 20,
};
/// How often a holder wakes to catch up and renew.
const HOLD_TICK: Duration = Duration::from_millis(100);

/// Why a run stopped short of the head. Pending writes stay pending; nothing was published
/// that the payload does not cover.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Halt {
    /// Another live process holds the `text-index` lease: it is the elected projector.
    LeaseHeld,
    /// Another writer holds the directory lock (a stopped holder included): no publication.
    WriterHeld,
    /// The lease was lost to a successor: the writer was dropped before any further commit.
    LeaseLost,
    /// The payload cannot be resumed from and this run may not rebuild.
    RebuildNeeded,
    /// The deadline passed.
    Deadline,
}

/// What a run achieved: the payload watermark afterwards and why it stopped, when it did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Progress {
    /// The committed payload's sequence (0 without a payload).
    pub seq: u64,
    /// `None` when the run reached the head (or its target).
    pub halt: Option<Halt>,
    /// The commits the run made (no empty commit is ever made).
    pub commits: u32,
}

/// See the module doc.
pub struct Projector<'a> {
    index: &'a TantivyIndex,
    log: &'a dyn ChangeLogStore,
    leases: &'a dyn LeaseStore,
    owner: String,
    ttl_s: u32,
}

impl<'a> Projector<'a> {
    /// A projector of `index` over the change log and leases of its database; `owner` names
    /// this process on the lease, which lives `ttl_s` seconds between renewals.
    pub fn new(
        index: &'a TantivyIndex,
        log: &'a dyn ChangeLogStore,
        leases: &'a dyn LeaseStore,
        owner: &str,
        ttl_s: u32,
    ) -> Self {
        Self {
            index,
            log,
            leases,
            owner: owner.to_owned(),
            ttl_s,
        }
    }

    /// Bring the index to the head of the log, rebuilding first when the payload cannot be
    /// resumed from; lease and lock are released at the end.
    ///
    /// # Errors
    /// A store or index failure; a held lease or lock and a lost lease are a [`Halt`].
    pub fn catch_up(&self) -> kaban_core::Result<Progress> {
        let mut session = match self.open()? {
            Ok(session) => session,
            Err(halt) => return self.progress(Some(halt), 0),
        };
        let halt = session.run(&Run::to_head(true))?;
        let commits = session.commits;
        drop(session);
        self.progress(halt, commits)
    }

    /// The write barrier's drain (SS-10, design amendment 7): publications until the payload
    /// covers `target` or `deadline` passes, never a rebuild.
    ///
    /// # Errors
    /// A store or index failure; every other stop is a [`Halt`].
    pub fn drain_until(&self, target: u64, deadline: Instant) -> kaban_core::Result<Progress> {
        let mut session = match self.open()? {
            Ok(session) => session,
            Err(halt) => return self.progress(Some(halt), 0),
        };
        let halt = session.run(&Run {
            target: Some(target),
            deadline: Some(deadline),
            may_rebuild: false,
            signals: None,
        })?;
        let commits = session.commits;
        drop(session);
        self.progress(halt, commits)
    }

    /// Hold lease and lock, catching up on every tick and renewing the lease, until `stop`
    /// is set or the lease is lost (a successor acquired it while this process was stopped):
    /// then the writer is dropped before any further commit and the run returns.
    ///
    /// # Errors
    /// A store or index failure; a held lease or lock at the start is a [`Halt`].
    pub fn hold(&self, stop: &AtomicBool) -> kaban_core::Result<Progress> {
        let mut session = match self.open()? {
            Ok(session) => session,
            Err(halt) => return self.progress(Some(halt), 0),
        };
        let halt = loop {
            if let Some(halt) = session.run(&Run::to_head(true))? {
                break Some(halt);
            }
            if stop.load(Ordering::Acquire) {
                break None;
            }
            std::thread::sleep(HOLD_TICK);
            match session.renew() {
                Ok(()) => {}
                Err(Stop::Halt(halt)) => break Some(halt),
                Err(Stop::Failed(e)) => return Err(e),
            }
        };
        let commits = session.commits;
        drop(session);
        self.progress(halt, commits)
    }

    fn progress(&self, halt: Option<Halt>, commits: u32) -> kaban_core::Result<Progress> {
        let seq = match self.index.payload() {
            Ok(payload) => payload.map_or(0, |p| p.seq),
            // a malformed payload claims nothing (a run that may rebuild has replaced it)
            Err(Error::Payload(_)) => 0,
            Err(e) => return Err(e.into()),
        };
        Ok(Progress { seq, halt, commits })
    }

    /// Election and fence: the lease (non-blocking), the consumer's retention cursor, the
    /// directory lock (non-blocking). A lock held elsewhere gives the lease back at once, so
    /// a stopped holder blocks publication, not every other process's attempt.
    fn open(&self) -> kaban_core::Result<Result<Session<'_, 'a>, Halt>> {
        let Some(lease) =
            self.leases
                .lease_acquire(SinkId::TEXT_INDEX.0, &self.owner, self.ttl_s)?
        else {
            return Ok(Err(Halt::LeaseHeld));
        };
        let held = HeldLease {
            lease,
            leases: self.leases,
            lost: false,
        };
        match self
            .log
            .changes_register(SinkId::TEXT_INDEX.0, held.lease.fence)
        {
            Ok(_) => {}
            Err(StoreError::LeaseLost { .. }) => return Ok(Err(Halt::LeaseLost)),
            Err(e) => return Err(e),
        }
        let writer = match self.index.acquire_writer() {
            Ok(writer) => writer,
            Err(Error::WriterHeld) => return Ok(Err(Halt::WriterHeld)),
            Err(e) => return Err(e.into()),
        };
        Ok(Ok(Session {
            projector: self,
            writer,
            held,
            renewed: Instant::now(),
            bulk: false,
            commits: 0,
        }))
    }
}

/// A held lease, given back when dropped unless it was lost (a release would be refused).
struct HeldLease<'a> {
    lease: Lease,
    leases: &'a dyn LeaseStore,
    lost: bool,
}

impl Drop for HeldLease<'_> {
    fn drop(&mut self) {
        if !self.lost
            && let Err(e) = self.leases.lease_release(&self.lease)
        {
            tracing::debug!(error = %e, "the text-index lease was not released");
        }
    }
}

/// One acquisition: the writer (dropped first, so the lock is gone before the lease is given
/// back) and the lease.
struct Session<'p, 'a> {
    projector: &'p Projector<'a>,
    writer: Writer,
    held: HeldLease<'a>,
    renewed: Instant,
    /// Whether the next publication takes the [`BULK`] bounds: set by a publication that
    /// filled its bound, cleared by one that reached the head.
    bulk: bool,
    /// The commits this session made.
    commits: u32,
}

/// One run's brief: where it stops and whom it reports to.
pub(crate) struct Run<'r> {
    /// Stop once the payload covers it (a barrier's drain); `None`: the head.
    pub(crate) target: Option<u64>,
    pub(crate) deadline: Option<Instant>,
    /// Whether a payload that cannot be resumed from is rebuilt here or reported.
    pub(crate) may_rebuild: bool,
    /// The holder's own process (`poll_with`): every commit is reported to it, and a position
    /// it waits for is committed as soon as it is staged, whatever the bounds say.
    pub(crate) signals: Option<&'r dyn Signals>,
}

impl Run<'_> {
    /// To the head, no deadline.
    pub(crate) const fn to_head(may_rebuild: bool) -> Self {
        Self {
            target: None,
            deadline: None,
            may_rebuild,
            signals: None,
        }
    }
}

/// Why a publication stopped staging.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Closed {
    /// Nothing more in the feed.
    Head,
    /// The target or a waited-for position is staged.
    Position,
    /// A bound is reached (a backlog remains).
    Full,
    /// The deadline passed; what is staged commits, then the run halts.
    Deadline,
}

/// Why a session's loop ended early: a [`Halt`] (reported) or a failure (propagated).
enum Stop {
    Halt(Halt),
    Failed(StoreError),
}

impl From<Halt> for Stop {
    fn from(halt: Halt) -> Self {
        Self::Halt(halt)
    }
}

impl From<StoreError> for Stop {
    fn from(e: StoreError) -> Self {
        Self::Failed(e)
    }
}

impl From<Error> for Stop {
    fn from(e: Error) -> Self {
        Self::Failed(e.into())
    }
}

impl Session<'_, '_> {
    /// The loop of the module doc under `run`'s brief.
    pub(crate) fn run(&mut self, run: &Run<'_>) -> kaban_core::Result<Option<Halt>> {
        match self.publications(run) {
            Ok(()) => Ok(None),
            Err(Stop::Halt(halt)) => Ok(Some(halt)),
            Err(Stop::Failed(e)) => Err(e),
        }
    }

    /// Publications until the head (or the target), each several chunks and one commit. A
    /// quiet resume (nothing to replay) still acknowledges the payload, so retention never
    /// lags a quiet index.
    fn publications(&mut self, run: &Run<'_>) -> Result<(), Stop> {
        let index = self.projector.index;
        let expired = || run.deadline.is_some_and(|d| Instant::now() >= d);
        let mut consumed = false;
        'publication: loop {
            if expired() {
                return Err(Halt::Deadline.into());
            }
            let payload = match index.payload() {
                Ok(payload) => payload,
                // corrupt index state (SS-09; T2 review t2-6): classified under the lock as a
                // rebuild like an incompatible payload, never an error on every run
                Err(Error::Payload(error)) => {
                    if !run.may_rebuild {
                        return Err(Halt::RebuildNeeded.into());
                    }
                    tracing::info!(%error, "rebuilding the text index: malformed payload");
                    self.rebuild()?;
                    consumed = true;
                    continue;
                }
                Err(e) => return Err(e.into()),
            };
            let after = payload.as_ref().map_or(0, |p| p.seq);
            if payload.is_some() && run.target.is_some_and(|t| after >= t) {
                return Ok(());
            }
            let bounds = if self.bulk { BULK } else { INTERACTIVE };
            // one publication: chunks staged under the writer, then one commit
            let mut staged: Option<(u64, Canonical)> = None;
            let (mut rows, mut bytes) = (0usize, 0usize);
            let closed = loop {
                let from = staged.as_ref().map_or(after, |(seq, _)| *seq);
                let replay = index.rows().replay(from, CHUNK)?;
                if let Resume::Rebuild(why) = resume_point(payload.as_ref(), &replay) {
                    if !run.may_rebuild {
                        return Err(Halt::RebuildNeeded.into());
                    }
                    tracing::info!(why, "rebuilding the text index");
                    self.rebuild()?;
                    consumed = true;
                    continue 'publication;
                }
                if replay.changes.is_empty() {
                    break Closed::Head;
                }
                self.renew()?;
                let chunk = index.stage(&mut self.writer, &replay.changes)?;
                rows += replay.changes.len();
                bytes += chunk.bytes;
                staged = Some((chunk.seq, replay.at));
                let wanted = run.signals.map_or(0, Signals::wanted);
                if run.target.is_some_and(|t| chunk.seq >= t)
                    || (wanted > after && chunk.seq >= wanted)
                {
                    break Closed::Position;
                }
                if rows >= bounds.rows || bytes >= bounds.bytes {
                    break Closed::Full;
                }
                if expired() {
                    break Closed::Deadline;
                }
            };
            let Some((seq, at)) = staged else {
                // the head, with nothing staged: the backlog is gone (t8d-4: after a
                // publication that filled its bound exactly, or one a waiter closed at the
                // head), so the next independent burst takes the interactive bounds
                self.bulk = false;
                if !consumed && let Some(payload) = &payload {
                    // the cursor follows the payload (never backwards)
                    self.ack(payload.seq)?;
                }
                return Ok(());
            };
            self.writer.commit(&payload_at(seq, &at))?;
            self.commits += 1;
            self.ack(seq)?;
            consumed = true;
            match closed {
                Closed::Full => self.bulk = true,
                Closed::Head => self.bulk = false,
                Closed::Position | Closed::Deadline => {}
            }
            // past the budget nothing more starts, the reader refresh included (t8d-3: the
            // next read reloads; the commit stands and the payload names it)
            if expired() {
                return Err(Halt::Deadline.into());
            }
            index.refresh()?;
            if let Some(signals) = run.signals {
                signals.published(seq);
            }
            if closed == Closed::Head {
                return Ok(());
            }
        }
    }

    /// Register retention first (an acknowledgement at 0 never lowers a cursor and creates a
    /// missing one), rebuild and publish at the snapshot's position, then acknowledge it.
    fn rebuild(&mut self) -> Result<(), Stop> {
        self.ack(0)?;
        let at = self.projector.index.rebuild_with(&mut self.writer)?;
        self.commits += 1;
        self.ack(at.seq)
    }

    /// The lease-fenced acknowledgement (retention only): refused means a successor holds
    /// the lease, and this writer stops here.
    fn ack(&mut self, upto: u64) -> Result<(), Stop> {
        match self
            .projector
            .log
            .changes_ack(SinkId::TEXT_INDEX.0, upto, self.held.lease.fence)
        {
            Ok(()) => Ok(()),
            Err(StoreError::LeaseLost { .. }) => {
                self.held.lost = true;
                Err(Halt::LeaseLost.into())
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Renew the lease once a third of its life passed; a refused renewal is a loss.
    fn renew(&mut self) -> Result<(), Stop> {
        let due = Duration::from_millis(u64::from(self.projector.ttl_s) * 1000 / 3);
        if self.renewed.elapsed() < due {
            return Ok(());
        }
        match self
            .projector
            .leases
            .lease_renew(&self.held.lease, self.projector.ttl_s)
        {
            Ok(()) => {
                self.renewed = Instant::now();
                Ok(())
            }
            Err(StoreError::LeaseLost { .. }) => {
                self.held.lost = true;
                Err(Halt::LeaseLost.into())
            }
            Err(e) => Err(e.into()),
        }
    }
}

/// The payload of a commit at `seq` under `at`'s identity.
pub(crate) fn payload_at(seq: u64, at: &Canonical) -> Payload {
    Payload::new(&Canonical {
        seq,
        identity: at.identity.clone(),
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion
mod tests {
    use super::{Halt, Projector};
    use crate::TantivyIndex;
    use kaban_core::config::Recall;
    use kaban_core::text_index::{MatchMode, Searched, SeedFilter, TextIndex, TextQuery};
    use kaban_core::{
        Actor, ChangeEntity, ChangeLogStore, MemoryStore, NewNode, RecallQuery, WorkspaceId,
        WriteCtx,
    };
    use kaban_store_sqlite::SqliteStore;
    use kaban_store_sqlite::index_rows::IndexRows;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    /// A private scratch directory removed on drop.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let d = std::env::temp_dir().join(format!("ss09-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&d);
            std::fs::create_dir_all(&d).unwrap();
            Self(d)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn remember(store: &SqliteStore, text: &str) {
        let ctx = WriteCtx::new("v1", WorkspaceId::default(), Actor::Agent);
        let note = NewNode {
            kind: "Fact".into(),
            text: text.into(),
            ..Default::default()
        };
        store.remember(note, &[], &ctx).unwrap();
    }

    fn hits(index: &TantivyIndex, text: &str) -> usize {
        let q = RecallQuery {
            query: text.into(),
            ..Default::default()
        };
        let query = TextQuery {
            text: &q.query,
            mode: MatchMode::Lexical,
            filter: SeedFilter::of(&q),
            limit: 10,
            sufficient: 10,
            skip: 0,
        };
        match index.search(&query).unwrap() {
            Searched::Hits { hits, .. } => hits.len(),
            Searched::Unavailable => panic!("the index serves"),
        }
    }

    /// A writer that staged a page (deletions and additions) and died before its commit
    /// published nothing: the payload still names the earlier position, the staged rows are
    /// not hits, and the successor replays the page from the payload, each row once.
    #[test]
    fn a_crash_before_the_commit_publishes_nothing_and_the_successor_replays() {
        let s = Scratch::new("crash-before-commit");
        let db = s.0.join("kaban.db");
        let store = SqliteStore::open(&db, &s.0.join("keys")).unwrap();
        let index = TantivyIndex::open(
            &db.with_file_name("adapter"),
            &kaban_core::shipped::resolved().unwrap(),
            &Recall::default(),
            Box::new(IndexRows::open(&db).unwrap()),
        )
        .unwrap();
        remember(&store, "quokka one by the shore");
        let projector = Projector::new(&index, &store, &store, "test:first", 2);
        assert_eq!(projector.catch_up().unwrap().halt, None);
        let published = index.payload().unwrap().unwrap().seq;
        remember(&store, "quokka two under the trees");
        // the crash: staged, never committed, the lock released with the process
        {
            let writer = index.acquire_writer().unwrap();
            let page = store.changes_read(published, 2048).unwrap();
            let ids: Vec<String> = page
                .iter()
                .filter(|c| c.entity == ChangeEntity::Node)
                .map(|c| c.id.clone())
                .collect();
            let rows = index.rows().current(&ids).unwrap();
            assert_eq!(rows.len(), 1);
            writer.apply(&ids, &rows).unwrap();
        }
        assert_eq!(index.payload().unwrap().unwrap().seq, published);
        assert_eq!(
            hits(&index, "quokka"),
            1,
            "nothing of the staged page shows"
        );

        let progress = Projector::new(&index, &store, &store, "test:successor", 2)
            .catch_up()
            .unwrap();

        assert_eq!(progress.halt, None, "{progress:?}");
        assert_eq!(progress.seq, store.changes_head().unwrap());
        assert_eq!(hits(&index, "quokka"), 2, "each row once");
    }

    /// A payload that does not deserialize (T2 review t2-6; SS-09 "corrupt index state
    /// requires rebuild"): a commit whose payload is not this crate's JSON is classified under
    /// the writer lock as a rebuild, never a propagated error on every run. The bounded drain
    /// (never a rebuild) reports the need; the catch-up rebuilds, stamps a fresh payload at
    /// the head and serves every row once.
    #[test]
    fn a_malformed_payload_is_rebuilt_under_the_lock_not_an_error_on_every_catch_up() {
        let s = Scratch::new("malformed-payload");
        let db = s.0.join("kaban.db");
        let store = SqliteStore::open(&db, &s.0.join("keys")).unwrap();
        let index = TantivyIndex::open(
            &db.with_file_name("adapter"),
            &kaban_core::shipped::resolved().unwrap(),
            &Recall::default(),
            Box::new(IndexRows::open(&db).unwrap()),
        )
        .unwrap();
        remember(&store, "quokka one by the shore");
        assert_eq!(
            Projector::new(&index, &store, &store, "test:first", 2)
                .catch_up()
                .unwrap()
                .halt,
            None
        );
        // a commit under a payload this crate did not write
        {
            let mut writer: tantivy::IndexWriter<tantivy::TantivyDocument> =
                index.index.writer_with_num_threads(1, 32 << 20).unwrap();
            let mut prepared = writer.prepare_commit().unwrap();
            prepared.set_payload("{not the payload");
            prepared.commit().unwrap();
            writer.wait_merging_threads().unwrap();
        }
        assert!(index.payload().is_err(), "malformed");
        remember(&store, "quokka two under the trees");
        let head = store.changes_head().unwrap();

        let drained = Projector::new(&index, &store, &store, "test:drain", 2)
            .drain_until(head, Instant::now() + Duration::from_secs(1))
            .unwrap();
        assert_eq!(drained.halt, Some(Halt::RebuildNeeded), "{drained:?}");
        let progress = Projector::new(&index, &store, &store, "test:recover", 2)
            .catch_up()
            .unwrap();

        assert_eq!(progress.halt, None, "{progress:?}");
        assert_eq!(progress.seq, head);
        assert_eq!(
            index.payload().unwrap().unwrap().seq,
            head,
            "a fresh payload"
        );
        assert_eq!(hits(&index, "quokka"), 2, "each row once");
    }
}
