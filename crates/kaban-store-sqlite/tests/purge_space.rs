//! Purge and space on the catalog (spec 11 T7 Step 3, KER-05, TXT-06): a purge through the
//! public `forget` path leaves no row or text read that finds the record, after a reopen and
//! with a seal in flight too; a pin taken before the purge keeps its snapshot, and once it is
//! released compaction and the checkpoint give the purged records' space back.
//!
//! ```text
//!   fixture => sealed tier + pending packs (large records, live neighbours)
//!   old pin (a held text search) => forget(Purge) x n => Catalog::purge rewrites the tiers
//!   compact while held => reclaim pending;  release => compact => allocated bytes fall
//! ```
//!
//! Runs with the `failpoints` feature (the compaction window hooks live behind it).

#![cfg(feature = "failpoints")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::{Scratch, created, ctx, nid};
use kaban_core::config::{CatalogCfg, Recall, SealOps};
use kaban_core::query_budget::QueryBudget;
use kaban_core::text_index::{
    MatchMode, PurgeOutcome, Searched, SeedFilter, TextIndex, TextQuery, TextSnapshot,
};
use kaban_core::{ForgetMode, MemoryStore, Node, RecallQuery};
use kaban_index_tantivy::TantivyIndex;
use kaban_store_sqlite::catalog_testing::{self, Window};
use kaban_store_sqlite::{CompactBudget, SqliteStore};
use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The bound of every wait on another thread.
const STEP: Duration = Duration::from_secs(10);
/// The bound of the held reader's wait: the purges it spans rewrite a tier each (debug build).
const HOLD: Duration = Duration::from_secs(120);

/// The word every purged record holds and no live one does.
const DOOMED: &str = "zebradoomed";
/// The word every live neighbour holds and no purged one does.
const KEPT: &str = "quokkakept";

/// Text seals as soon as two packs are pending, and every compaction ticks the checkpoint.
fn config() -> CatalogCfg {
    CatalogCfg {
        seal_ops: SealOps {
            text: 1,
            ..SealOps::default()
        },
        checkpoint_interval_ms: 1,
        ..CatalogCfg::default()
    }
}

fn open(s: &Scratch) -> SqliteStore {
    SqliteStore::open(&s.db(), &s.keys())
        .unwrap()
        .with_catalog_config(config())
        .unwrap()
}

/// About 32 KiB of words no other record holds, after `word`: a large record whose text
/// tier and pack are large too.
fn body(word: &str, seed: u64) -> String {
    let mut out = format!("{word} ");
    let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    while out.len() < 32 << 10 {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        write!(out, "w{:012x} ", state & 0xFFFF_FFFF_FFFF).unwrap();
    }
    out
}

/// A lexical seed query for `text`, past the size of the fixture.
fn query<'a>(text: &'a str, q: &'a RecallQuery) -> TextQuery<'a> {
    TextQuery {
        text,
        mode: MatchMode::Lexical,
        filter: SeedFilter::of(q),
        limit: 100,
        sufficient: 100,
        skip: 0,
    }
}

/// How many seed hits the text index of `store` has for `text`.
fn seeds(store: &SqliteStore, text: &str) -> usize {
    let q = RecallQuery::default();
    match store.search_text(&query(text, &q)).unwrap() {
        Searched::Hits { hits, .. } => hits.len(),
        Searched::Unavailable => panic!("the text index is unavailable"),
    }
}

/// How many records a recall of `text` returns on `store`.
fn recalled(store: &SqliteStore, text: &str) -> usize {
    store
        .recall(&RecallQuery {
            query: text.into(),
            limit: 100,
            hops: 0,
            ..Default::default()
        })
        .unwrap()
        .len()
}

/// The allocated bytes (`st_blocks`) of the file at `path`, 0 when it is missing.
fn allocated(path: &Path) -> u64 {
    std::fs::metadata(path).map_or(0, |m| m.blocks() * 512)
}

/// The allocated bytes of the database, its WAL and the container.
#[derive(Debug, Clone, Copy)]
struct Space {
    db: u64,
    #[expect(
        dead_code,
        reason = "captured for the failure messages only: a WAL is reused"
    )]
    wal: u64,
    kbx: u64,
}

fn space(s: &Scratch) -> Space {
    let db = s.db();
    Space {
        db: allocated(&db),
        wal: allocated(&db.with_file_name("kaban.db-wal")),
        kbx: allocated(&db.with_file_name("kaban.kbx")),
    }
}

/// Every file under `dir`, recursively.
fn files(dir: &Path) -> BTreeSet<PathBuf> {
    let mut out = BTreeSet::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            out.extend(files(&path));
        } else {
            out.insert(path);
        }
    }
    out
}

/// Compact `store` until a pass publishes nothing, leaves nothing pending and empties
/// SQLite's free list (it vacuums fewer than its 1024-page step; its own commits free a page
/// or so each time), bounded.
fn compact_to_completion(store: &SqliteStore) {
    let mut last = None;
    for _ in 0..16 {
        let report = store.compact().unwrap();
        if report.reclaim.pending.is_empty()
            && report.vacuumed_pages < 1024
            && report.published.is_empty()
        {
            return;
        }
        last = Some(report);
    }
    panic!("compaction never completed: {last:?}");
}

/// A reader holding its pin: the first text search counts its hits, waits until the test lets
/// it go, then searches the same pinned snapshot again.
struct HeldReader {
    index: TantivyIndex,
    held: Mutex<Sender<()>>,
    go: Mutex<Receiver<()>>,
    counts: Mutex<Vec<usize>>,
}

impl HeldReader {
    fn count(&self, at: &TextSnapshot<'_>, q: &TextQuery<'_>, budget: &QueryBudget) -> usize {
        match self.index.search(at, q, budget).unwrap() {
            Searched::Hits { hits, .. } => hits.len(),
            Searched::Unavailable => panic!("the held snapshot is unavailable"),
        }
    }
}

impl TextIndex for HeldReader {
    fn search(
        &self,
        at: &TextSnapshot<'_>,
        q: &TextQuery<'_>,
        budget: &QueryBudget,
    ) -> kaban_core::Result<Searched> {
        let before = self.count(at, q, budget);
        self.held.lock().unwrap().send(()).unwrap();
        let released = self.go.lock().unwrap().recv_timeout(HOLD);
        let after = self.count(at, q, budget);
        self.counts.lock().unwrap().extend([before, after]);
        released.expect("the test never released the held reader");
        self.index.search(at, q, budget)
    }
    fn purge(&self, rids: &[i64]) -> kaban_core::Result<PurgeOutcome> {
        self.index.purge(rids)
    }
}

/// KER-05: large purged records in a sealed tier and in pending packs, an old reader held
/// across the purge, then compaction to completion once it lets go.
#[test]
fn a_purge_hides_the_records_from_every_read_and_gives_their_space_back_once_readers_let_go() {
    // Arrange: 8 doomed records and 4 neighbours sealed into a tier, then 4 doomed and 2
    // neighbours left pending
    let s = Scratch::new("kaban-t7-purge", "space");
    let writer = open(&s);
    let mut doomed: Vec<Node> = Vec::new();
    let mut seed = 0;
    let mut add = |doomed: &mut Vec<Node>, n_doomed: usize, n_kept: usize| {
        for i in 0..n_doomed + n_kept {
            seed += 1;
            if i < n_doomed {
                doomed.push(created(&writer, "Fact", &body(DOOMED, seed)));
            } else {
                created(&writer, "Fact", &body(KEPT, seed));
            }
        }
    };
    add(&mut doomed, 8, 4);
    compact_to_completion(&writer);
    add(&mut doomed, 4, 2);
    // the baseline with the WAL folded into the database file
    writer.checkpoint_if_due().unwrap();
    let before = space(&s);
    let files_before = files(&s.0);
    assert_eq!(seeds(&writer, DOOMED), 12);

    // the old reader: a second store whose text search holds its pin
    let (held, on_held) = channel();
    let (go, wait) = channel();
    let hook = Arc::new(HeldReader {
        index: TantivyIndex::new(
            &kaban_core::shipped::resolved().unwrap(),
            &Recall::default(),
        ),
        held: Mutex::new(held),
        go: Mutex::new(wait),
        counts: Mutex::new(Vec::new()),
    });
    let reader = open(&s).with_text_index(Box::new(Arc::clone(&hook)));
    let reading = std::thread::spawn(move || {
        let q = RecallQuery::default();
        reader.search_text(&query(DOOMED, &q)).map(|_| ())
    });
    on_held.recv_timeout(STEP).expect("the reader never pinned");

    // Act: purge every doomed record through the public path, compact while the reader holds
    let mut outcomes = Vec::new();
    for node in &doomed {
        outcomes.push(
            writer
                .forget(&nid(&node.id), ForgetMode::Purge, &ctx())
                .unwrap(),
        );
    }
    let held_report = writer.compact().unwrap();

    // Assert: no new read finds a purged record; the held snapshot still does, and its
    // extents are reported pending
    for outcome in &outcomes {
        assert!(
            outcome.revoked && !outcome.physically_cleaned,
            "{outcome:?}"
        );
        let cleanup = outcome.cleanup.clone().unwrap_or_default();
        for scope in ["database/WAL:", "index:", "temporary outputs:"] {
            assert!(cleanup.contains(scope), "{cleanup}");
        }
    }
    assert_eq!(seeds(&writer, DOOMED), 0);
    assert_eq!(recalled(&writer, DOOMED), 0);
    for node in &doomed {
        assert!(writer.get(&nid(&node.id), false).unwrap().is_none());
    }
    assert_eq!(
        recalled(&writer, KEPT),
        6,
        "the neighbours keep their answers"
    );
    assert!(
        !held_report.reclaim.pending.is_empty(),
        "the held pin keeps the old extents pending: {held_report:?}"
    );
    go.send(()).unwrap();
    reading.join().unwrap().unwrap();
    assert_eq!(
        *hook.counts.lock().unwrap(),
        vec![12, 12],
        "the old pin's snapshot is intact"
    );

    // Act: the reader let go; compact and checkpoint to completion
    compact_to_completion(&writer);
    let after = space(&s);

    // Assert: the database and the container hold less, no scratch file is left behind
    assert!(after.kbx < before.kbx, "container: {before:?} -> {after:?}");
    assert!(after.db < before.db, "database: {before:?} -> {after:?}");
    let leftover: Vec<_> = files(&s.0).difference(&files_before).cloned().collect();
    assert_eq!(leftover, Vec::<PathBuf>::new(), "{before:?} -> {after:?}");
    assert_eq!(writer.verify_catalog().unwrap().inconsistencies(), 0);

    // Assert: after a reopen too
    drop(writer);
    let reopened = open(&s);
    assert_eq!(seeds(&reopened, DOOMED), 0);
    assert_eq!(recalled(&reopened, KEPT), 6);
    for node in &doomed {
        assert!(reopened.get(&nid(&node.id), false).unwrap().is_none());
    }
}

/// TXT-06 for a purge: records purged while a text seal is in flight (paused after its claim,
/// its inputs already pinned) stay absent once the seal publishes, and after a reopen.
#[test]
fn records_purged_while_a_seal_is_in_flight_stay_absent_once_it_publishes() {
    // Arrange: three doomed records and two neighbours pending; the first compaction unit
    // (the text seal) stops after its claim until the test lets it go
    let s = Scratch::new("kaban-t7-purge", "seal-in-flight");
    let store = Arc::new(open(&s));
    let doomed: Vec<Node> = (0..3)
        .map(|i| created(&store, "Fact", &format!("the {DOOMED} record {i}")))
        .collect();
    for i in 0..2 {
        created(&store, "Fact", &format!("the {KEPT} record {i}"));
    }
    let (claimed, on_claim) = channel();
    let (go, wait) = channel::<()>();
    let wait = Mutex::new(wait);
    let first = AtomicBool::new(true);
    catalog_testing::set_failpoint(
        &store,
        Window::CompactClaimed,
        Some(Arc::new(move || {
            if first.swap(false, Ordering::AcqRel) {
                let _ = claimed.send(());
                let _ = wait.lock().unwrap().recv_timeout(STEP);
            }
        })),
    );
    let compacting = {
        let store = Arc::clone(&store);
        std::thread::spawn(move || {
            catalog_testing::compact(
                &store,
                CompactBudget {
                    memory_bytes: u64::MAX,
                    deadline: STEP,
                },
            )
        })
    };
    on_claim
        .recv_timeout(STEP)
        .expect("the compaction never claimed");

    // Act: purge while the seal is paused, then let it publish
    for node in &doomed {
        store
            .forget(&nid(&node.id), ForgetMode::Purge, &ctx())
            .unwrap();
    }
    go.send(()).unwrap();
    let report = compacting.join().unwrap().unwrap();

    // Assert
    assert!(
        report.published.iter().any(|p| p.class == "text_tier"),
        "the text seal did not publish: {report:?}"
    );
    assert_eq!(seeds(&store, DOOMED), 0);
    assert_eq!(recalled(&store, KEPT), 2);
    drop(report);
    let store = Arc::into_inner(store).expect("the compaction thread let go");
    drop(store);
    let reopened = open(&s);
    assert_eq!(seeds(&reopened, DOOMED), 0);
    assert_eq!(recalled(&reopened, KEPT), 2);
    for node in &doomed {
        assert!(reopened.get(&nid(&node.id), false).unwrap().is_none());
    }
}
