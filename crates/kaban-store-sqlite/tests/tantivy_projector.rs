//! The projector at its seam (spec 05 SS-09): the payload is the resume point, never the
//! SQLite cursor; a page committed but not acknowledged is neither skipped nor replayed into a
//! duplicate hit; an index whose payload no longer joins the surviving history, or claims more
//! than the database has, is rebuilt rather than served; the writer lock is the one fence.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

#[path = "tantivy_common/mod.rs"]
mod common;

use common::{FakeRows, Scratch, index_row, note, rids, seed};
use kaban_core::config::Recall;
use kaban_core::text_index::{Canonical, IndexRow, MatchMode, Replay, Rows, TextIndex, Visibility};
use kaban_core::{ChangeLogStore, LeaseStore, RecallQuery, Result};
use kaban_index_tantivy::TantivyIndex;
use kaban_index_tantivy::projector::{Halt, Projector};
use kaban_store_sqlite::index_rows::IndexRows;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

const CONSUMER: &str = "text-index";

fn query(text: &str) -> RecallQuery {
    RecallQuery {
        query: text.into(),
        ..Default::default()
    }
}

/// The published watermark, `None` while the index serves no payload.
fn visible_through(index: &dyn TextIndex) -> Option<i64> {
    match index.visible().unwrap() {
        Visibility::Through(seq) => Some(seq),
        Visibility::Unavailable => None,
        Visibility::Current => panic!("an index with its own storage reports a watermark"),
    }
}

/// The published watermark of an index that serves a payload.
fn watermark(index: &dyn TextIndex) -> i64 {
    visible_through(index).expect("the index serves a payload")
}

/// A consumer registered after the index was built has its cursor at the head, ahead of the
/// payload. A projector resuming from the cursor would skip the rows in between (the row
/// written after the build would never become a hit); the projector resumes from the payload,
/// replays them, publishes the head and the acknowledgement catches the cursor up.
#[test]
fn the_payload_is_the_resume_point_not_a_cursor_ahead_of_it() {
    let s = Scratch::new("resume-payload");
    let store = s.store();
    let index = s.adapter();
    note(&store, "default", "Fact", "quokka one by the shore", &[]);
    index.rebuild().unwrap();
    let built_at = watermark(&index);
    note(&store, "default", "Fact", "quokka two under the trees", &[]);
    let installer = store
        .lease_acquire(CONSUMER, "installer", 5)
        .unwrap()
        .unwrap();
    let cursor = store.changes_register(CONSUMER, installer.fence).unwrap();
    store.lease_release(&installer).unwrap();
    assert!(
        i64::try_from(cursor).unwrap() > built_at,
        "the cursor is ahead"
    );

    let projector = Projector::new(&index, &store, &store, "test:successor", 2);
    let progress = projector.catch_up().unwrap();

    assert_eq!(progress.halt, None, "{progress:?}");
    let q = query("quokka");
    assert_eq!(
        rids(&index, &seed(&q, MatchMode::Lexical, 10, 0)).len(),
        2,
        "the row written after the build was replayed from the payload"
    );
    let head = store.changes_head().unwrap();
    assert_eq!(watermark(&index), i64::try_from(head).unwrap());
    assert_eq!(store.changes_lag_by_consumer().unwrap()[CONSUMER], 0);
}

/// The script of an [`Interleaved`] `Rows` (T2 review t2-8), shared with the test: a hook
/// run at the start of the first `replay` (after the projector read the payload, before the
/// page), a hook run at the start of every `replay` with its ordinal, a page cap, the
/// projection signature reported once the first page was served, and the counts of replays
/// and rebuilds (`active` calls).
#[derive(Default)]
struct Script {
    before_first_replay: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    on_replay: Option<Box<dyn Fn(usize) + Send + Sync>>,
    page_cap: Option<usize>,
    signature_after_first_page: Option<String>,
    replays: AtomicUsize,
    rebuilds: AtomicUsize,
}

/// The store's own [`IndexRows`] with [`Script`]'s interleavings.
struct Interleaved {
    inner: IndexRows,
    script: Arc<Script>,
}

impl Interleaved {
    fn patch(&self, at: &mut Canonical) {
        if self.script.replays.load(Ordering::SeqCst) > 1
            && let Some(signature) = &self.script.signature_after_first_page
        {
            at.identity.projection_signature.clone_from(signature);
        }
    }
}

impl Rows for Interleaved {
    fn current(&self, ids: &[String]) -> Result<Vec<IndexRow>> {
        self.inner.current(ids)
    }
    fn active(&self, visit: &mut dyn FnMut(IndexRow) -> Result<()>) -> Result<Canonical> {
        self.script.rebuilds.fetch_add(1, Ordering::SeqCst);
        let mut at = self.inner.active(visit)?;
        self.patch(&mut at);
        Ok(at)
    }
    fn canonical(&self) -> Result<Canonical> {
        let mut at = self.inner.canonical()?;
        self.patch(&mut at);
        Ok(at)
    }
    fn replay(&self, after: u64, limit: usize) -> Result<Replay> {
        let ordinal = self.script.replays.fetch_add(1, Ordering::SeqCst);
        if ordinal == 0
            && let Some(hook) = self.script.before_first_replay.lock().unwrap().take()
        {
            hook();
        }
        if let Some(hook) = &self.script.on_replay {
            hook(ordinal);
        }
        let limit = self.script.page_cap.map_or(limit, |cap| cap.min(limit));
        let mut replay = self.inner.replay(after, limit)?;
        self.patch(&mut replay.at);
        Ok(replay)
    }
}

/// The adapter of `s` over an [`Interleaved`] `Rows` running `script`.
fn interleaved(s: &Scratch, script: Arc<Script>) -> TantivyIndex {
    TantivyIndex::open(
        &s.index_dir(),
        &kaban_core::shipped::resolved().unwrap(),
        &Recall::default(),
        Box::new(Interleaved {
            inner: IndexRows::open(&s.db()).unwrap(),
            script,
        }),
    )
    .unwrap()
}

/// Amendment 6 (T2 review t2-8): the coverage check and the first page are one read. A prune
/// that lands between the projector's payload read and its first page (the hook runs at the
/// start of the first `replay`, after the consumer registered at the head) is seen by that
/// same read: the oldest surviving row is past the payload's successor, so the run rebuilds
/// instead of extending the index over the gap, and every row written in the gap is a hit.
#[test]
fn a_prune_between_the_coverage_check_and_the_first_page_is_a_rebuild_not_a_skip() {
    let s = Scratch::new("prune-between");
    let store = Arc::new(s.store());
    let script = Arc::new(Script::default());
    let index = interleaved(&s, Arc::clone(&script));
    note(&store, "default", "Fact", "quokka one by the shore", &[]);
    index.rebuild().unwrap();
    let built_at = watermark(&index);
    for i in 0..5 {
        note(
            &store,
            "default",
            "Fact",
            &format!("quokka {i} in the gap"),
            &[],
        );
    }
    let pruner = Arc::clone(&store);
    *script.before_first_replay.lock().unwrap() = Some(Box::new(move || {
        pruner.changes_prune(1).unwrap();
    }));
    let rebuilds = script.rebuilds.load(Ordering::SeqCst);

    let progress = Projector::new(&index, store.as_ref(), store.as_ref(), "test:gap", 2)
        .catch_up()
        .unwrap();

    assert_eq!(progress.halt, None, "{progress:?}");
    let oldest = store.changes_read(0, 1).unwrap()[0].seq;
    assert!(
        i64::try_from(oldest).unwrap() > built_at + 1,
        "the prune landed after the payload read: {oldest} > {built_at} + 1"
    );
    assert_eq!(
        script.rebuilds.load(Ordering::SeqCst),
        rebuilds + 1,
        "rebuilt, never extended over the gap"
    );
    let q = query("quokka");
    assert_eq!(rids(&index, &seed(&q, MatchMode::Lexical, 10, 0)).len(), 6);
    assert_eq!(
        watermark(&index),
        i64::try_from(store.changes_head().unwrap()).unwrap()
    );
}

/// A binding change between two projection pages (T2 review t2-8): the first page is
/// committed under the projection signature the database had; the next read reports another
/// one. The payload is incompatible with it (SS-09 `{seq, lineage, versions}`), so the run
/// rebuilds under the new binding rather than extending the index across it, and the index
/// serves at the head under the new signature with every row once.
#[test]
fn a_binding_change_between_pages_rebuilds_under_the_new_binding() {
    let s = Scratch::new("binding-between-pages");
    let store = s.store();
    let script = Arc::new(Script {
        page_cap: Some(1),
        signature_after_first_page: Some("sig-after".into()),
        ..Script::default()
    });
    let index = interleaved(&s, Arc::clone(&script));
    for text in [
        "quokka one by the shore",
        "quokka two under the trees",
        "quokka three at the ferry",
    ] {
        note(&store, "default", "Fact", text, &[]);
    }

    let progress = Projector::new(&index, &store, &store, "test:binding", 2)
        .catch_up()
        .unwrap();

    assert_eq!(progress.halt, None, "{progress:?}");
    assert!(
        script.replays.load(Ordering::SeqCst) > 1,
        "more than one page was read"
    );
    assert!(
        script.rebuilds.load(Ordering::SeqCst) >= 2,
        "the first build, then the rebuild under the new binding: {}",
        script.rebuilds.load(Ordering::SeqCst)
    );
    let q = query("quokka");
    assert_eq!(rids(&index, &seed(&q, MatchMode::Lexical, 10, 0)).len(), 3);
    assert_eq!(
        watermark(&index),
        i64::try_from(store.changes_head().unwrap()).unwrap(),
        "served under the new binding"
    );
}

/// A page committed but not acknowledged (the writer died between the two): the successor
/// resumes from the payload, so the page is neither replayed into a duplicate hit nor
/// skipped, the rows after it are published, and the acknowledgement catches up.
#[test]
fn a_committed_unacknowledged_page_is_neither_skipped_nor_duplicated() {
    let s = Scratch::new("after-commit-before-ack");
    // the store's own publication silenced: the transient writer below is the only publisher
    // and the only one who could acknowledge (review t8a-7)
    let store = s.store_unpublished();
    let index = s.adapter();
    note(&store, "default", "Fact", "quokka one by the shore", &[]);
    let projector = Projector::new(&index, &store, &store, "test:first", 2);
    assert_eq!(projector.catch_up().unwrap().halt, None);
    let acked = store.changes_head().unwrap();
    note(&store, "default", "Fact", "quokka two under the trees", &[]);
    // the commit without its acknowledgement: a transient writer publishes the page
    let page = store.changes_read(acked, 2048).unwrap();
    index.project(&page).unwrap();
    let committed = watermark(&index);
    assert_eq!(
        committed,
        i64::try_from(store.changes_head().unwrap()).unwrap()
    );
    assert!(
        store.changes_lag_by_consumer().unwrap()[CONSUMER] > 0,
        "the cursor lags the commit"
    );
    note(&store, "default", "Fact", "quokka three at the ferry", &[]);

    let progress = Projector::new(&index, &store, &store, "test:successor", 2)
        .catch_up()
        .unwrap();

    assert_eq!(progress.halt, None, "{progress:?}");
    let q = query("quokka");
    let found = rids(&index, &seed(&q, MatchMode::Lexical, 10, 0));
    assert_eq!(found.len(), 3, "each row once: {found:?}");
    assert_eq!(
        watermark(&index),
        i64::try_from(store.changes_head().unwrap()).unwrap()
    );
    assert_eq!(store.changes_lag_by_consumer().unwrap()[CONSUMER], 0);
}

/// An index restored behind pruned history (the oldest surviving row is past the payload's
/// successor) cannot be resumed from: it is rebuilt, never extended over the gap, and the
/// rows written in the gap are hits afterwards.
#[test]
fn an_index_behind_pruned_history_is_rebuilt_not_resumed() {
    let s = Scratch::new("pruned-history");
    let store = s.store();
    let index = s.adapter();
    note(&store, "default", "Fact", "quokka one by the shore", &[]);
    index.rebuild().unwrap();
    let built_at = watermark(&index);
    for i in 0..5 {
        note(
            &store,
            "default",
            "Fact",
            &format!("quokka {i} in the gap"),
            &[],
        );
    }
    // no registered consumer: a prune keeps the newest `tail` rows only
    store.changes_prune(1).unwrap();
    let oldest = store.changes_read(0, 1).unwrap()[0].seq;
    assert!(
        i64::try_from(oldest).unwrap() > built_at + 1,
        "a gap past the payload"
    );

    let progress = Projector::new(&index, &store, &store, "test:restored", 2)
        .catch_up()
        .unwrap();

    assert_eq!(progress.halt, None, "{progress:?}");
    let q = query("quokka");
    assert_eq!(rids(&index, &seed(&q, MatchMode::Lexical, 10, 0)).len(), 6);
    assert_eq!(
        watermark(&index),
        i64::try_from(store.changes_head().unwrap()).unwrap()
    );
}

/// An index whose payload claims more than the database has (a restored index beside an
/// older database) is unavailable until rebuilt; the projector rebuilds it at the position the
/// database actually has and it serves again.
#[test]
fn an_index_ahead_of_the_canonical_history_is_rebuilt() {
    let s = Scratch::new("ahead");
    let store = s.store(); // the change log and leases only; the rows are scripted
    let (rows, canonical) = FakeRows::new(vec![index_row(1, "default", "Fact", "quokka")], 10);
    let index = TantivyIndex::in_ram(
        &kaban_core::shipped::resolved().unwrap(),
        &Recall::default(),
        Box::new(rows),
    )
    .unwrap();
    index.rebuild().unwrap();
    assert_eq!(watermark(&index), 10);
    canonical.lock().unwrap().seq = 4;
    assert_eq!(
        visible_through(&index),
        None,
        "ahead of the history: unavailable"
    );

    let progress = Projector::new(&index, &store, &store, "test:ahead", 2)
        .catch_up()
        .unwrap();

    assert_eq!(progress.halt, None, "{progress:?}");
    assert_eq!(watermark(&index), 4);
    let q = query("quokka");
    assert_eq!(rids(&index, &seed(&q, MatchMode::Lexical, 10, 0)), vec![1]);
}

/// Two lone writers: the second finds the lease held by a live first and does not attempt
/// the lock (nothing published by it); once the first released, the second proceeds.
#[test]
fn a_live_lease_holder_is_the_elected_projector() {
    let s = Scratch::new("election");
    let store = s.store();
    let index = s.adapter();
    note(&store, "default", "Fact", "quokka one by the shore", &[]);
    let elected = store.lease_acquire(CONSUMER, "serve", 5).unwrap().unwrap();
    let projector = Projector::new(&index, &store, &store, "test:cli", 2);
    let progress = projector.catch_up().unwrap();
    assert_eq!(progress.halt, Some(Halt::LeaseHeld));
    assert_eq!(visible_through(&index), None, "nothing published");
    store.lease_release(&elected).unwrap();
    assert_eq!(projector.catch_up().unwrap().halt, None);
    assert_eq!(
        watermark(&index),
        i64::try_from(store.changes_head().unwrap()).unwrap()
    );
}

/// SS-11: a second process's first search after a successful acknowledgement loads at least
/// that watermark. The second handle (its own `Index` and reader, opened before the first
/// one published) serves the acknowledged position and the row behind it at its first
/// search, with no reload asked for.
#[test]
fn a_second_handles_first_search_after_an_ack_covers_that_watermark() {
    let s = Scratch::new("second-handle");
    let store = s.store();
    let first = s.adapter();
    let second = s.adapter();
    note(&store, "default", "Fact", "quokka one by the shore", &[]);
    assert_eq!(visible_through(&second), None, "nothing published yet");
    let progress = Projector::new(&first, &store, &store, "test:first", 2)
        .catch_up()
        .unwrap();
    assert_eq!(progress.halt, None);
    let acked = store.changes_lag_by_consumer().unwrap()[CONSUMER];
    assert_eq!(acked, 0, "the position is acknowledged");

    let q = query("quokka");
    let (found, _) = common::hits(&second, &seed(&q, MatchMode::Lexical, 10, 0));
    assert_eq!(found.len(), 1);
    assert!(watermark(&second) >= i64::try_from(progress.seq).unwrap());
}

/// The holder role of [`multi_process_stopped_holder_blocks_publication_until_it_exits`]: in
/// a child process (`KABAN_T2_HOLDER_DB` set) hold lease and lock over that database until
/// the lease is lost or the process is killed. A no-op elsewhere.
#[test]
#[ignore = "a role of the multi-process case"]
fn multi_process_holder_role() {
    let Ok(db) = std::env::var("KABAN_T2_HOLDER_DB") else {
        return;
    };
    let db = std::path::PathBuf::from(db);
    let store = kaban_store_sqlite::SqliteStore::open(&db, &db.with_file_name("keys")).unwrap();
    let index = TantivyIndex::open(
        &db.with_file_name("adapter").join("tantivy"),
        &kaban_core::shipped::resolved().unwrap(),
        &Recall::default(),
        Box::new(kaban_store_sqlite::index_rows::IndexRows::open(&db).unwrap()),
    )
    .unwrap();
    let projector = Projector::new(&index, &store, &store, "holder", HOLDER_TTL_S);
    // a writer's own post-commit projection holds the lease for a moment (the store publishes
    // its commits): the role retries its election like the serving loop does
    let progress = loop {
        let progress = projector
            .hold(&std::sync::atomic::AtomicBool::new(false))
            .unwrap();
        if !matches!(progress.halt, Some(Halt::LeaseHeld | Halt::WriterHeld)) {
            break progress;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    };
    assert_eq!(progress.halt, Some(Halt::LeaseLost), "{progress:?}");
}

const HOLDER_TTL_S: u32 = 2;

/// The holder of the lock, in a child process.
struct Holder(std::process::Child);

impl Holder {
    fn start(s: &Scratch) -> Self {
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "multi_process_holder_role", "--ignored"])
            .env("KABAN_T2_HOLDER_DB", s.db())
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        Self(child)
    }
    fn signal(&self, signal: &str) {
        let status = std::process::Command::new("kill")
            .args([signal, &self.0.id().to_string()])
            .status()
            .unwrap();
        assert!(status.success(), "kill {signal}");
    }
    /// The exit status within `timeout`, `None` when the child is still running.
    fn exit_within(&mut self, timeout: std::time::Duration) -> Option<std::process::ExitStatus> {
        let start = std::time::Instant::now();
        while start.elapsed() < timeout {
            if let Some(status) = self.0.try_wait().unwrap() {
                return Some(status);
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        None
    }
}

impl Drop for Holder {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Wait until the index publishes the head of the log (the holder caught up).
fn wait_for_head(index: &dyn TextIndex, store: &kaban_store_sqlite::SqliteStore) {
    let start = std::time::Instant::now();
    while start.elapsed() < std::time::Duration::from_secs(15) {
        if visible_through(index) == Some(i64::try_from(store.changes_head().unwrap()).unwrap()) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    panic!("the holder never published the head");
}

/// SS-09's stopped-holder case across two processes. A holder is stopped (SIGSTOP) while it
/// holds the lock; its lease expires; the successor takes the lease but cannot take the lock,
/// so it publishes nothing and the new write stays unpublished. Continued (SIGCONT), the
/// holder relinquishes on detecting the lease loss and the successor publishes. A second
/// holder, stopped the same way and killed, releases the lock with its death; the successor
/// resumes from the payload and every row is a hit exactly once.
#[test]
#[ignore = "two processes and lease expiry: run with --run-ignored only -E 'test(multi_process)'"]
fn multi_process_stopped_holder_blocks_publication_until_it_exits() {
    let s = Scratch::new("stopped-holder");
    let store = s.store();
    let index = s.adapter();
    note(&store, "default", "Fact", "quokka one by the shore", &[]);
    let q = query("quokka");
    let successor = Projector::new(&index, &store, &store, "successor", HOLDER_TTL_S);

    // 1. a stopped holder: the successor has the lease, not the lock
    let mut holder = Holder::start(&s);
    wait_for_head(&index, &store);
    holder.signal("-STOP");
    std::thread::sleep(std::time::Duration::from_secs(u64::from(HOLDER_TTL_S) + 1));
    note(&store, "default", "Fact", "quokka two under the trees", &[]);
    let before = watermark(&index);
    let progress = successor.catch_up().unwrap();
    assert_eq!(progress.halt, Some(Halt::WriterHeld), "{progress:?}");
    assert_eq!(
        watermark(&index),
        before,
        "nothing published while the holder is stopped"
    );
    assert_eq!(rids(&index, &seed(&q, MatchMode::Lexical, 10, 0)).len(), 1);

    // 2. continued: the holder sees the lease lost and relinquishes; the successor publishes
    holder.signal("-CONT");
    let status = holder.exit_within(std::time::Duration::from_secs(10));
    assert!(
        status.is_some_and(|st| st.success()),
        "the holder relinquished on lease loss: {status:?}"
    );
    let progress = successor.catch_up().unwrap();
    assert_eq!(progress.halt, None, "{progress:?}");
    assert_eq!(rids(&index, &seed(&q, MatchMode::Lexical, 10, 0)).len(), 2);

    // 3. a killed holder: the lock dies with it and the successor resumes from the payload
    let mut holder = Holder::start(&s);
    note(&store, "default", "Fact", "quokka three at the ferry", &[]);
    wait_for_head(&index, &store);
    holder.signal("-STOP");
    std::thread::sleep(std::time::Duration::from_secs(u64::from(HOLDER_TTL_S) + 1));
    note(
        &store,
        "default",
        "Fact",
        "quokka four near the settlement",
        &[],
    );
    assert_eq!(successor.catch_up().unwrap().halt, Some(Halt::WriterHeld));
    holder.signal("-KILL");
    assert!(
        holder
            .exit_within(std::time::Duration::from_secs(10))
            .is_some()
    );
    let progress = successor.catch_up().unwrap();
    assert_eq!(progress.halt, None, "{progress:?}");
    assert_eq!(
        watermark(&index),
        i64::try_from(store.changes_head().unwrap()).unwrap()
    );
    let found = rids(&index, &seed(&q, MatchMode::Lexical, 10, 0));
    assert_eq!(found.len(), 4, "each row once: {found:?}");
    assert_eq!(store.changes_lag_by_consumer().unwrap()[CONSUMER], 0);
}

// ---- lane T8d.1: publication bounds (astra, projection research) ----

/// The feed rows one note writes (a node row and whatever else `remember` logs).
fn rows_per_note(store: &kaban_store_sqlite::SqliteStore) -> u64 {
    let before = store.changes_head().unwrap();
    note(
        store,
        "default",
        "Fact",
        "a note measured for its rows",
        &[],
    );
    store.changes_head().unwrap() - before
}

/// T8d.1: a 5,000-note burst behind one barrier drains in a few commits (a publication of up
/// to 2,048 feed rows, then bulk publications of up to 10,000 while the backlog lasts), its
/// 256-row chunks staged without a commit each, and the target is searchable at the end of
/// the one drain. Before: one commit per 256-row page (forty and more for this burst).
#[test]
fn a_burst_behind_a_barrier_drains_in_a_few_commits_not_one_per_chunk() {
    let s = Scratch::new("burst-few-commits");
    let store = s.store_unpublished();
    let index = s.adapter();
    note(&store, "default", "Fact", "quokka before the burst", &[]);
    index.rebuild().unwrap();
    for i in 0..5_000 {
        note(&store, "default", "Fact", &format!("wombat burst {i}"), &[]);
    }
    let head = store.changes_head().unwrap();

    let progress = Projector::new(&index, &store, &store, "barrier", 5)
        .drain_until(
            head,
            std::time::Instant::now() + std::time::Duration::from_secs(60),
        )
        .unwrap();

    assert_eq!(progress.halt, None, "{progress:?}");
    assert_eq!(progress.seq, head);
    assert!(
        progress.commits <= 3,
        "{} commits for the burst: {progress:?}",
        progress.commits
    );
    assert_eq!(watermark(&index), i64::try_from(head).unwrap());
    // every note of the burst is searchable, each once (T8d review t8d-6)
    let hits = all_rids(&index, "wombat", 6_000);
    let mut distinct = hits.clone();
    distinct.dedup();
    assert_eq!(hits.len(), 5_000, "every note of the burst");
    assert_eq!(distinct.len(), 5_000, "each once");
    assert_eq!(store.changes_lag_by_consumer().unwrap()[CONSUMER], 0);
}

/// The serving process's side of the loop, scripted: a barrier that wants a position a
/// thousand rows past the first commit, and the positions each commit reported.
struct BarrierAfterFirstCommit {
    published: Mutex<Vec<u64>>,
    head: u64,
    stop: std::sync::atomic::AtomicBool,
}

impl kaban_index_tantivy::projector::poll::Signals for BarrierAfterFirstCommit {
    fn wait(&self, _tick: std::time::Duration) -> kaban_index_tantivy::projector::poll::Wake {
        self.stop.store(true, Ordering::SeqCst);
        kaban_index_tantivy::projector::poll::Wake::Tick
    }
    fn published(&self, seq: u64) {
        self.published.lock().unwrap().push(seq);
        if seq >= self.head {
            self.stop.store(true, Ordering::SeqCst);
        }
    }
    fn wanted(&self) -> u64 {
        self.published
            .lock()
            .unwrap()
            .first()
            .map_or(0, |first| first + 1_000)
    }
}

/// T8d.1: a barrier during bulk mode is not made to wait for the bulk bound. The holder's
/// backlog is a 5,000-note burst; its first publication fills the interactive bound and the
/// loop goes bulk; a barrier then wants a position a thousand rows further, and the next
/// commit lands within a chunk of that position, well before the 10,000-row bound, with the
/// backlog drained after it.
#[test]
fn a_barrier_during_bulk_mode_is_committed_within_a_chunk_of_its_position() {
    let s = Scratch::new("bulk-barrier");
    let store = s.store_unpublished();
    let index = s.adapter();
    note(&store, "default", "Fact", "quokka before the burst", &[]);
    index.rebuild().unwrap();
    let per_note = rows_per_note(&store);
    for i in 0..5_000 {
        note(&store, "default", "Fact", &format!("wombat burst {i}"), &[]);
    }
    let head = store.changes_head().unwrap();
    let observation = IndexRows::open(&s.db()).unwrap();
    let signals = BarrierAfterFirstCommit {
        published: Mutex::new(Vec::new()),
        head,
        stop: std::sync::atomic::AtomicBool::new(false),
    };

    let progress = Projector::new(&index, &store, &store, "holder", 5)
        .poll_with(&|| observation.data_version(), &signals.stop, &signals)
        .unwrap();

    assert_eq!(progress.halt, None, "{progress:?}");
    let published = signals.published.lock().unwrap().clone();
    let first = published[0];
    let wanted = first + 1_000;
    assert!(
        published.len() >= 3 && published.len() <= 5,
        "a few commits: {published:?}"
    );
    let second = published[1];
    assert!(
        second >= wanted && second < wanted + 256 + per_note,
        "the barrier's position {wanted} is committed within a chunk: {published:?}"
    );
    assert_eq!(*published.last().unwrap(), head, "the final flush");
    assert_eq!(watermark(&index), i64::try_from(head).unwrap());
}

/// The rids of every `text` hit, sorted (a limit past the fixture: the whole set).
fn all_rids(index: &dyn TextIndex, text: &str, limit: usize) -> Vec<i64> {
    let q = query(text);
    let mut rids = rids(index, &seed(&q, MatchMode::Lexical, limit, 0));
    rids.sort_unstable();
    rids
}

/// T8d review t8d-1 (SS-09, SS-11): a rebuild forced by a later chunk's compatibility check
/// starts from an index that already staged an earlier chunk of the same publication (a note
/// deleted and re-added, uncommitted). The rebuild publishes exactly the snapshot's rows: the
/// staged additions never land beside their snapshot copies (Tantivy's `delete_all_documents`
/// clears segments, not the indexing queue), and nothing of the staged deletions reaches the
/// snapshot's documents.
#[test]
fn a_rebuild_after_a_staged_chunk_publishes_exactly_the_snapshot() {
    let s = Scratch::new("rebuild-after-staged-chunk");
    let store = s.store_unpublished();
    note(&store, "default", "Fact", "quokka one by the shore", &[]);
    let per_note = usize::try_from(rows_per_note(&store)).unwrap();
    let script = Arc::new(Script {
        // one note per chunk: the first chunk stages note two before the binding changes
        page_cap: Some(per_note),
        signature_after_first_page: Some("sig-after".into()),
        ..Script::default()
    });
    let index = interleaved(&s, Arc::clone(&script));
    index.rebuild().unwrap();
    let built = all_rids(&index, "quokka", 100);
    assert_eq!(built.len(), 1);
    let mut expected = built;
    for text in ["quokka two under the trees", "quokka three at the ferry"] {
        let n = note(&store, "default", "Fact", text, &[]);
        expected.push(common::row(&s.db(), &n).0);
    }
    expected.sort_unstable();
    let rebuilds = script.rebuilds.load(Ordering::SeqCst);

    let progress = Projector::new(&index, &store, &store, "test:staged-then-rebuild", 2)
        .catch_up()
        .unwrap();

    assert_eq!(progress.halt, None, "{progress:?}");
    assert_eq!(
        script.rebuilds.load(Ordering::SeqCst),
        rebuilds + 1,
        "the binding change rebuilt"
    );
    assert_eq!(
        all_rids(&index, "quokka", 100),
        expected,
        "exactly the snapshot's rows, each once"
    );
    // the raw document count: the three quokka notes and the measured one, nothing staged
    // before the rebuild beside its snapshot copy
    let raw = tantivy::Index::open_in_dir(s.index_dir()).unwrap();
    let docs: u64 = raw
        .reader()
        .unwrap()
        .searcher()
        .segment_readers()
        .iter()
        .map(|r| u64::from(r.num_docs()))
        .sum();
    assert_eq!(docs, 4, "every active row once");
    assert_eq!(
        watermark(&index),
        i64::try_from(store.changes_head().unwrap()).unwrap()
    );
}

/// T8d review t8d-3 (SS-10): a drain whose deadline passes while it stages commits what it
/// staged (one commit, the payload at that position), then halts with `Deadline` without
/// bringing the index to the head; the watermark reads back as the committed position (the
/// reader refresh past the budget is the next read's).
#[test]
fn a_deadline_during_staging_commits_what_is_staged_then_halts() {
    let s = Scratch::new("deadline-during-staging");
    let store = s.store_unpublished();
    note(&store, "default", "Fact", "quokka before", &[]);
    let per_note = usize::try_from(rows_per_note(&store)).unwrap();
    let budget = std::time::Duration::from_millis(150);
    let script = Arc::new(Script {
        page_cap: Some(per_note),
        on_replay: Some(Box::new(move |ordinal| {
            if ordinal == 1 {
                std::thread::sleep(budget * 2);
            }
        })),
        ..Script::default()
    });
    let index = interleaved(&s, Arc::clone(&script));
    index.rebuild().unwrap();
    let mut positions = Vec::new();
    for text in ["quokka two", "quokka three", "quokka four"] {
        note(&store, "default", "Fact", text, &[]);
        positions.push(store.changes_head().unwrap());
    }
    let head = *positions.last().unwrap();

    let started = std::time::Instant::now();
    let progress = Projector::new(&index, &store, &store, "test:deadline", 2)
        .drain_until(head, started + budget)
        .unwrap();

    assert_eq!(progress.halt, Some(Halt::Deadline), "{progress:?}");
    assert_eq!(progress.commits, 1, "{progress:?}");
    assert_eq!(
        progress.seq, positions[1],
        "the two staged notes: {progress:?}"
    );
    assert!(progress.seq < head);
    assert_eq!(watermark(&index), i64::try_from(positions[1]).unwrap());
    assert_eq!(
        all_rids(&index, "quokka", 100).len(),
        3,
        "the note before and the two staged ones, not the fourth"
    );
}

/// The holder's side scripted for one session: the first idle wait writes a burst and asks
/// for publication, the second stops the loop; every published position is recorded.
struct BurstOnFirstWait<'s> {
    store: &'s kaban_store_sqlite::SqliteStore,
    notes: usize,
    waits: AtomicUsize,
    published: Mutex<Vec<u64>>,
    stop: std::sync::atomic::AtomicBool,
}

impl kaban_index_tantivy::projector::poll::Signals for BurstOnFirstWait<'_> {
    fn wait(&self, _tick: std::time::Duration) -> kaban_index_tantivy::projector::poll::Wake {
        if self.waits.fetch_add(1, Ordering::SeqCst) == 0 {
            for i in 0..self.notes {
                note(
                    self.store,
                    "default",
                    "Fact",
                    &format!("wombat later {i}"),
                    &[],
                );
            }
            return kaban_index_tantivy::projector::poll::Wake::Publish;
        }
        self.stop.store(true, Ordering::SeqCst);
        kaban_index_tantivy::projector::poll::Wake::Tick
    }
    fn published(&self, seq: u64) {
        self.published.lock().unwrap().push(seq);
    }
}

/// T8d review t8d-4: bulk mode ends with the publication that reaches the head, the one that
/// finds nothing to stage included. A backlog of exactly 2,048 feed rows fills the interactive
/// bound (bulk from here), the next publication finds the head with nothing staged; a later,
/// independent burst of 2,048 + 512 rows in the same session then publishes under the
/// interactive bound again: two commits, the first at most 2,048 rows past the previous head.
#[test]
fn bulk_mode_ends_when_an_empty_publication_finds_the_head() {
    let s = Scratch::new("bulk-ends-at-head");
    let store = s.store_unpublished();
    let index = s.adapter();
    note(&store, "default", "Fact", "quokka before the burst", &[]);
    index.rebuild().unwrap();
    let per_note = usize::try_from(rows_per_note(&store)).unwrap();
    let interactive = kaban_index_tantivy::projector::INTERACTIVE.rows;
    assert_eq!(
        interactive % per_note,
        0,
        "{per_note} rows per note cannot land exactly on the bound"
    );
    index.rebuild().unwrap();
    let before = store.changes_head().unwrap();
    for i in 0..interactive / per_note {
        note(&store, "default", "Fact", &format!("wombat exact {i}"), &[]);
    }
    let exact = store.changes_head().unwrap();
    assert_eq!(exact - before, u64::try_from(interactive).unwrap());
    let observation = IndexRows::open(&s.db()).unwrap();
    let signals = BurstOnFirstWait {
        store: &store,
        notes: (interactive + 512).div_ceil(per_note),
        waits: AtomicUsize::new(0),
        published: Mutex::new(Vec::new()),
        stop: std::sync::atomic::AtomicBool::new(false),
    };

    let progress = Projector::new(&index, &store, &store, "holder", 5)
        .poll_with(&|| observation.data_version(), &signals.stop, &signals)
        .unwrap();

    assert_eq!(progress.halt, None, "{progress:?}");
    let head = store.changes_head().unwrap();
    let published = signals.published.lock().unwrap().clone();
    let later: Vec<u64> = published.iter().copied().filter(|p| *p > exact).collect();
    assert_eq!(*published.last().unwrap(), head, "{published:?}");
    assert_eq!(
        progress.commits, 3,
        "the exact bound, then two interactive commits for the later burst: {published:?}"
    );
    assert!(
        later[0] <= exact + u64::try_from(interactive).unwrap() && later[0] < head,
        "the later burst's first commit is within the interactive bound: {published:?}"
    );
}

/// The number of `Node` changes of `store` in `(after, through]`.
fn node_changes_between(
    store: &kaban_store_sqlite::SqliteStore,
    after: u64,
    through: u64,
) -> usize {
    store
        .changes_read(after, 100_000)
        .unwrap()
        .iter()
        .filter(|c| c.seq <= through && c.entity == kaban_core::ChangeEntity::Node)
        .count()
}

/// The holder's side scripted for the bulk-barrier case, checking every publication's
/// contents as it is reported (T8d review t8d-6): the hits at each published position are
/// exactly the burst notes the change log holds through that position, so no intermediate
/// payload claims more than the rows applied and no row is skipped.
struct CheckedBarrier<'s> {
    store: &'s kaban_store_sqlite::SqliteStore,
    index: &'s TantivyIndex,
    /// The query every burst note answers.
    text: &'static str,
    built_at: u64,
    published: Mutex<Vec<u64>>,
    head: u64,
    stop: std::sync::atomic::AtomicBool,
}

impl kaban_index_tantivy::projector::poll::Signals for CheckedBarrier<'_> {
    fn wait(&self, _tick: std::time::Duration) -> kaban_index_tantivy::projector::poll::Wake {
        self.stop.store(true, Ordering::SeqCst);
        kaban_index_tantivy::projector::poll::Wake::Tick
    }
    fn published(&self, seq: u64) {
        let applied = node_changes_between(self.store, self.built_at, seq);
        let hits = all_rids(self.index, self.text, 6_000);
        let mut distinct = hits.clone();
        distinct.dedup();
        assert_eq!(distinct.len(), hits.len(), "a row once at {seq}");
        assert_eq!(
            hits.len(),
            applied,
            "the hits at {seq} are the rows applied through it"
        );
        assert_eq!(watermark(self.index), i64::try_from(seq).unwrap());
        self.published.lock().unwrap().push(seq);
        if seq >= self.head {
            self.stop.store(true, Ordering::SeqCst);
        }
    }
    fn wanted(&self) -> u64 {
        self.published
            .lock()
            .unwrap()
            .first()
            .map_or(0, |first| first + 1_000)
    }
}

/// T8d review t8d-6: the bulk-barrier run of
/// [`a_barrier_during_bulk_mode_is_committed_within_a_chunk_of_its_position`] with every
/// published position verified against the change log as it is reported: the hits are the
/// rows applied through that position, each once, and the watermark names it.
#[test]
fn every_intermediate_publication_serves_exactly_the_rows_applied_through_it() {
    let s = Scratch::new("bulk-checked");
    let store = s.store_unpublished();
    let index = s.adapter();
    note(&store, "default", "Fact", "quokka before the burst", &[]);
    index.rebuild().unwrap();
    let built_at = store.changes_head().unwrap();
    for i in 0..5_000 {
        note(&store, "default", "Fact", &format!("wombat burst {i}"), &[]);
    }
    let head = store.changes_head().unwrap();
    let observation = IndexRows::open(&s.db()).unwrap();
    let signals = CheckedBarrier {
        store: &store,
        index: &index,
        text: "wombat",
        built_at,
        published: Mutex::new(Vec::new()),
        head,
        stop: std::sync::atomic::AtomicBool::new(false),
    };

    let progress = Projector::new(&index, &store, &store, "holder", 5)
        .poll_with(&|| observation.data_version(), &signals.stop, &signals)
        .unwrap();

    assert_eq!(progress.halt, None, "{progress:?}");
    let published = signals.published.lock().unwrap().clone();
    assert!(
        published.len() >= 3,
        "a few commits, each checked: {published:?}"
    );
    assert_eq!(*published.last().unwrap(), head);
    assert_eq!(all_rids(&index, "wombat", 6_000).len(), 5_000);
}

/// T8d review t8d-6: the decoded-byte bound closes a publication below the row bound. Notes
/// of 20 KiB fill 4 MiB at the first 256-row chunk, so a 300-note backlog (far below 2,048
/// rows) drains in two commits, the first at 256 rows; every note is a hit at the end.
#[test]
fn the_decoded_byte_bound_closes_a_publication_below_the_row_bound() {
    let s = Scratch::new("byte-bound");
    let store = s.store_unpublished();
    let index = s.adapter();
    note(&store, "default", "Fact", "quokka before the burst", &[]);
    assert_eq!(rows_per_note(&store), 1, "one feed row per note");
    index.rebuild().unwrap();
    let before = store.changes_head().unwrap();
    let body = "wombat heavy ".repeat((20 << 10) / 13 + 1);
    assert!(body.len() >= 20 << 10);
    for i in 0..300 {
        note(&store, "default", "Fact", &format!("{body} {i}"), &[]);
    }
    let head = store.changes_head().unwrap();
    assert_eq!(head - before, 300);
    let observation = IndexRows::open(&s.db()).unwrap();
    let signals = CheckedBarrier {
        store: &store,
        index: &index,
        text: "wombat heavy",
        built_at: before,
        published: Mutex::new(Vec::new()),
        head,
        stop: std::sync::atomic::AtomicBool::new(false),
    };

    let progress = Projector::new(&index, &store, &store, "holder", 5)
        .poll_with(&|| observation.data_version(), &signals.stop, &signals)
        .unwrap();

    assert_eq!(progress.halt, None, "{progress:?}");
    assert_eq!(progress.commits, 2, "{progress:?}");
    let published = signals.published.lock().unwrap().clone();
    assert_eq!(
        published.first().copied(),
        Some(before + u64::try_from(kaban_index_tantivy::projector::CHUNK).unwrap()),
        "the first chunk alone fills the byte bound: {published:?}"
    );
    assert_eq!(all_rids(&index, "wombat heavy", 1_000).len(), 300);
}

/// T8d review t8d-6 (SS-09 recovery): a publication of several chunks interrupted between
/// its chunks (the writer dies with two chunks staged, nothing committed) publishes nothing,
/// and the successor resumes from the committed payload: every note once, none skipped.
#[test]
fn an_interrupted_multi_chunk_publication_recovers_from_the_committed_payload() {
    let s = Scratch::new("interrupted-between-chunks");
    let store = s.store_unpublished();
    note(&store, "default", "Fact", "quokka one by the shore", &[]);
    let per_note = usize::try_from(rows_per_note(&store)).unwrap();
    let script = Arc::new(Script {
        page_cap: Some(per_note),
        on_replay: Some(Box::new(|ordinal| {
            assert!(
                ordinal != 2,
                "killed between the second and the third chunk"
            );
        })),
        ..Script::default()
    });
    let index = interleaved(&s, Arc::clone(&script));
    index.rebuild().unwrap();
    let built_at = watermark(&index);
    let mut expected = all_rids(&index, "quokka", 100);
    for text in [
        "quokka two under the trees",
        "quokka three at the ferry",
        "quokka four on the hill",
        "quokka five in the reeds",
    ] {
        let n = note(&store, "default", "Fact", text, &[]);
        expected.push(common::row(&s.db(), &n).0);
    }
    expected.sort_unstable();

    let killed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        Projector::new(&index, &store, &store, "test:killed", 2).catch_up()
    }));

    assert!(killed.is_err(), "the writer died between chunks");
    assert_eq!(watermark(&index), built_at, "nothing published");
    assert_eq!(all_rids(&index, "quokka", 100).len(), 1);
    let progress = Projector::new(&index, &store, &store, "test:successor", 2)
        .catch_up()
        .unwrap();
    assert_eq!(progress.halt, None, "{progress:?}");
    assert_eq!(progress.seq, store.changes_head().unwrap());
    assert_eq!(all_rids(&index, "quokka", 100), expected, "each row once");
}
