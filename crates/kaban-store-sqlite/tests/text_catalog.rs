//! Text on the catalog (spec 11 T7, KER-01, TXT-06): a commit's text is searchable by every
//! store of the database as soon as the commit returns, and a record deleted while a merge is
//! in flight never comes back once the merge publishes.
//!
//! Runs with the `failpoints` feature (the compaction window hooks live behind it).

#![cfg(feature = "failpoints")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::{Scratch, created, ctx, nid};
use kaban_core::config::{CatalogCfg, SealOps};
use kaban_core::text_index::{MatchMode, Searched, SeedFilter, TextQuery};
use kaban_core::{ForgetMode, MemoryStore, RecallQuery};
use kaban_store_sqlite::catalog_testing::{self, Window};
use kaban_store_sqlite::{CompactBudget, SqliteStore};
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
