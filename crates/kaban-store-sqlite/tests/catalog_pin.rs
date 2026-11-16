//! Pins and the container (spec 11 T5 Step 4, KER-03, KER-11, INV-3, INV-5, PC-8): extents
//! appended outside the writer lock and published by a commit read back through a pin, a
//! later pin reads an extent appended after an earlier pin mapped the container, and a reader
//! racing a reclamation either retries or reads intact bytes, never a zeroed range.
//!
//! Runs with the `failpoints` feature (the test class and the failpoints live behind it).

#![cfg(feature = "failpoints")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::Scratch;
use kaban_core::query_budget::QueryBudget;
use kaban_store_sqlite::SqliteStore;
use kaban_store_sqlite::catalog_testing;
use std::time::Duration;

/// The deadline of every pin a test takes: acquisition and execution share it (PC-17).
const PIN_DEADLINE: Duration = Duration::from_secs(10);

fn budget() -> QueryBudget {
    QueryBudget::new(u64::MAX, Some(PIN_DEADLINE))
}

/// `len` bytes of the nonzero `fill`.
fn pattern(fill: u8, len: usize) -> Vec<u8> {
    assert_ne!(fill, 0, "a pattern is never zero: zero is what reclamation writes");
    vec![fill; len]
}

#[test]
fn an_extent_appended_and_published_reads_back_through_a_later_pin() {
    // Arrange
    let s = Scratch::new("kaban-t5-pin", "append-publish");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let before = catalog_testing::pin(&store, &budget()).unwrap();

    // Act
    let first = catalog_testing::append_and_publish(&store, &pattern(7, 4096))
        .expect("the extent was not appended and published");
    let second = catalog_testing::append_and_publish(&store, &pattern(9, 100))
        .expect("the extent was not appended and published");
    let pin = catalog_testing::pin(&store, &budget()).unwrap();

    // Assert: the earlier pin sees neither, the later one both, intact and in id order
    assert_eq!(before.view().extents(), Vec::<u64>::new());
    let view = pin.view();
    assert_eq!(view.extents(), vec![first, second]);
    assert!(
        view.generation() > before.view().generation(),
        "a publication did not publish a new generation"
    );
    assert_eq!(view.read(first).unwrap(), Some(pattern(7, 4096)));
    assert_eq!(view.read(second).unwrap(), Some(pattern(9, 100)));
}

#[test]
fn an_extent_appended_after_a_pin_mapped_the_container_is_readable_by_a_later_pin() {
    // Arrange: a pin that has mapped the container to read the first extent, still held
    let s = Scratch::new("kaban-t5-pin", "append-after-map");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let first = catalog_testing::append_and_publish(&store, &pattern(3, 8192)).unwrap();
    let mapped = catalog_testing::pin(&store, &budget()).unwrap();
    assert_eq!(mapped.view().read(first).unwrap(), Some(pattern(3, 8192)));

    // Act: the container grows past that mapping
    let second = catalog_testing::append_and_publish(&store, &pattern(5, 70_000)).unwrap();
    let later = catalog_testing::pin(&store, &budget()).unwrap();

    // Assert: the later pin reads the new extent, the earlier one still reads its own
    assert_eq!(later.view().read(second).unwrap(), Some(pattern(5, 70_000)));
    assert_eq!(later.view().read(first).unwrap(), Some(pattern(3, 8192)));
    assert_eq!(mapped.view().read(first).unwrap(), Some(pattern(3, 8192)));
    assert_eq!(mapped.view().read(second).unwrap(), None);
}

#[test]
fn concurrent_appends_reserve_disjoint_ranges_and_the_container_never_shrinks() {
    // Arrange: four writers on their own stores of one database
    let s = Scratch::new("kaban-t5-pin", "concurrent-append");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let kbx = s.0.join("kaban.kbx");
    let writers: Vec<_> = (1..=4_u8)
        .map(|w| {
            let (db, keys) = (s.db(), s.keys());
            std::thread::spawn(move || {
                let store = SqliteStore::open(&db, &keys).unwrap();
                (0..25_u8)
                    .map(|i| {
                        let fill = w * 50 + i + 1;
                        let len = 1000 + usize::from(i) * 37;
                        let id = catalog_testing::append_and_publish(&store, &pattern(fill, len))
                            .unwrap();
                        (id, fill, len)
                    })
                    .collect::<Vec<_>>()
            })
        })
        .collect();
    let mut written = Vec::new();
    for w in writers {
        written.extend(w.join().expect("a writer panicked"));
    }

    // Act
    let pin = catalog_testing::pin(&store, &budget()).unwrap();

    // Assert: every extent reads back as written (no two ranges overlap), and the container
    // is at least as long as everything appended
    let view = pin.view();
    assert_eq!(view.extents().len(), written.len());
    for (id, fill, len) in &written {
        assert_eq!(
            view.read(*id).unwrap(),
            Some(pattern(*fill, *len)),
            "extent {id} was overwritten"
        );
    }
    let total: usize = written.iter().map(|(_, _, len)| len).sum();
    let on_disk = std::fs::metadata(&kbx).unwrap().len();
    assert!(on_disk >= total as u64, "{on_disk} < {total}");
}
