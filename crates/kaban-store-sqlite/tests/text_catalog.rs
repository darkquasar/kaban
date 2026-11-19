//! Text on the catalog (spec 11 T7, KER-01, TXT-06): a commit's text is searchable by every
//! store of the database as soon as the commit returns, and a record deleted while a merge is
//! in flight never comes back once the merge publishes.
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
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The bound of every wait on another thread.
const STEP: Duration = Duration::from_secs(10);

/// The text index's seed hits for `text` on `store`, before any revalidation against rows.
fn seeds(store: &SqliteStore, text: &str) -> Searched {
    let q = RecallQuery::default();
    store
        .search_text(&TextQuery {
            text,
            mode: MatchMode::Lexical,
            filter: SeedFilter::of(&q),
            limit: 10,
            sufficient: 10,
            skip: 0,
        })
        .unwrap()
}

/// The number of seed hits for `text` on `store`.
fn seed_count(store: &SqliteStore, text: &str) -> usize {
    match seeds(store, text) {
        Searched::Hits { hits, .. } => hits.len(),
        Searched::Unavailable => panic!("the text index is unavailable"),
    }
}

#[test]
fn a_remember_is_found_by_a_second_store_of_the_database_at_once() {
    // Arrange: two stores of one database, the second opened before the write
    let s = Scratch::new("kaban-t7-text", "visible");
    let writer = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let reader = SqliteStore::open(&s.db(), &s.keys()).unwrap();

    // Act: remember through the first, recall through the second right after it returns
    let node = created(&writer, "Fact", "zebra-sentinel");
    let hits = reader
        .recall(&RecallQuery {
            query: "zebra".into(),
            ..Default::default()
        })
        .unwrap();

    // Assert: no wait, no poll, no barrier
    assert!(
        hits.iter().any(|h| h.node.id == node.id),
        "the second store did not find the record its peer just committed: {hits:?}"
    );
}

#[test]
fn a_record_deleted_while_a_seal_is_in_flight_stays_deleted_once_it_publishes() {
    // Arrange: text seals at once; three records pending; the first compaction unit (the text
    // seal) stops after its claim, its inputs already pinned, until the test lets it go
    let s = Scratch::new("kaban-t7-text", "delete-during-merge");
    let store = Arc::new(
        SqliteStore::open(&s.db(), &s.keys())
            .unwrap()
            .with_catalog_config(CatalogCfg {
                seal_ops: SealOps {
                    text: 1,
                    ..SealOps::default()
                },
                ..CatalogCfg::default()
            })
            .unwrap(),
    );
    let doomed = created(&store, "Fact", "the zebra-doomed record");
    created(&store, "Fact", "the zebra-kept record one");
    created(&store, "Fact", "the zebra-kept record two");
    let (claimed, on_claim) = std::sync::mpsc::channel();
    let (go, wait) = std::sync::mpsc::channel::<()>();
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

    // Act: delete the record while the merge is paused, then let the merge publish
    store
        .forget(&nid(&doomed.id), ForgetMode::Tombstone, &ctx())
        .unwrap();
    go.send(()).unwrap();
    let report = compacting.join().unwrap().unwrap();

    // Assert: the seal published a tier, and the index never returns the deleted record
    assert!(
        report.published.iter().any(|p| p.class == "text_tier"),
        "the text seal did not publish: {report:?}"
    );
    assert_eq!(
        seed_count(&store, "doomed"),
        0,
        "{:?}",
        seeds(&store, "doomed")
    );
    assert_eq!(seed_count(&store, "kept"), 2, "{:?}", seeds(&store, "kept"));
}

/// The catalog's text adapter behind a recorder: every page a recall asks for is recorded
/// (its skip and its rids), and while the first page is served a second store of the database
/// forgets twenty of that page's records, commits a record that ranks first and seals the
/// text, so a new generation lands between the pages.
struct CommitBetweenPages {
    index: TantivyIndex,
    db: PathBuf,
    keys: PathBuf,
    /// The first twenty records the first page holds, forgotten between the pages.
    doomed: Vec<Node>,
    pages: Mutex<Vec<(usize, Vec<i64>)>>,
}

impl CommitBetweenPages {
    fn commit_and_seal(&self) {
        let peer = SqliteStore::open(&self.db, &self.keys)
            .unwrap()
            .with_catalog_config(CatalogCfg {
                seal_ops: SealOps {
                    text: 1,
                    ..SealOps::default()
                },
                ..CatalogCfg::default()
            })
            .unwrap();
        for node in &self.doomed {
            peer.forget(&nid(&node.id), ForgetMode::Tombstone, &ctx())
                .unwrap();
        }
        created(&peer, "Fact", "quokka quokka shore shore the late arrival");
        let report = peer.compact().unwrap();
        assert!(
            report.published.iter().any(|p| p.class == "text_tier"),
            "the text seal did not publish between the pages: {report:?}"
        );
    }
}

impl TextIndex for CommitBetweenPages {
    fn search(
        &self,
        at: &TextSnapshot<'_>,
        query: &TextQuery<'_>,
        budget: &QueryBudget,
    ) -> kaban_core::Result<Searched> {
        let first = self.pages.lock().unwrap().is_empty();
        let page = self.index.search(at, query, budget)?;
        if let Searched::Hits { hits, .. } = &page {
            self.pages
                .lock()
                .unwrap()
                .push((query.skip, hits.iter().map(|h| h.rid).collect()));
        }
        if first {
            self.commit_and_seal();
        }
        Ok(page)
    }
    fn purge(&self, rids: &[i64]) -> kaban_core::Result<PurgeOutcome> {
        self.index.purge(rids)
    }
}

/// SS-11 on the catalog (T7 midpoint review, the regression a deleted projector-era test
/// protected): a recall whose first page comes back short (twenty of its records are
/// forgotten before it is revalidated) refills from the same pinned generation while a peer
/// commits and seals between the pages. The pages continue exactly where the last stopped
/// and together name every record of the pinned generation once (no skip, no duplicate); the
/// record committed in between is not among them, and a fresh read finds it.
#[test]
fn a_commit_and_a_seal_between_two_pages_of_one_recall_neither_duplicate_nor_skip() {
    // Arrange: sixty matching records, more than one page of fifty seeds at limit ten
    let s = Scratch::new("kaban-t7-text", "commit-between-pages");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let notes: Vec<Node> = (0..60)
        .map(|i| created(&store, "Fact", &format!("quokka number {i} by the shore")))
        .collect();
    let hook = Arc::new(CommitBetweenPages {
        index: TantivyIndex::new(
            &kaban_core::shipped::resolved().unwrap(),
            &Recall::default(),
        ),
        db: s.db(),
        keys: s.keys(),
        doomed: notes[..20].to_vec(),
        pages: Mutex::new(Vec::new()),
    });
    let store = store.with_text_index(Box::new(Arc::clone(&hook)));

    // Act
    let found = store
        .recall(&RecallQuery {
            query: "quokka shore".into(),
            limit: 10,
            hops: 0,
            ..Default::default()
        })
        .unwrap();

    // Assert: the pages of the one pinned generation, each once
    let pages = hook.pages.lock().unwrap().clone();
    assert!(pages.len() >= 2, "a refill happened: {pages:?}");
    let mut next = 0;
    let mut seen: Vec<i64> = Vec::new();
    for (skip, rids) in &pages {
        assert_eq!(
            *skip, next,
            "each page continues where the last stopped: {pages:?}"
        );
        next += rids.len();
        seen.extend(rids);
    }
    seen.sort_unstable();
    seen.dedup();
    assert_eq!(
        (next, seen.len()),
        (60, 60),
        "every record of the pinned generation once: {pages:?}"
    );
    assert_eq!(found.len(), 10);
    assert!(
        found.iter().all(|h| !h.node.text.contains("late arrival")),
        "the record committed between the pages is not served by the pinned generation"
    );
    // a fresh read sees the record committed between the pages
    let fresh = store
        .recall(&RecallQuery {
            query: "arrival".into(),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(fresh.len(), 1, "{fresh:?}");
}
