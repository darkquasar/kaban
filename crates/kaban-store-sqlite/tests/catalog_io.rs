//! The catalog's I/O (spec 11 T5 Step 5, KER-11, KER-01, INV-5): a failed sync of the container
//! is fatal, attempted once and publishes nothing; creating the catalog's artefacts syncs them
//! and their directory entries, and a failed creation sync refuses the open, which a later open
//! completes; hole punching is probed at open, and without it reclamation zero-fills, so a
//! reclaimed range reads as zeros either way and the container never shrinks.
//!
//! An extent appended after a pin mapped the container is read by a later pin through the same
//! I/O path in `catalog_pin.rs`
//! (`an_extent_appended_after_a_pin_mapped_the_container_is_readable_by_a_later_pin`).
//!
//! Runs with the `failpoints` feature (the faults and the test class live behind it).

#![cfg(feature = "failpoints")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion
#![allow(clippy::print_stderr)] // the visible skip markers where the file system punches no holes

mod common;

use common::Scratch;
use kaban_core::StoreError;
use kaban_core::query_budget::QueryBudget;
use kaban_store_sqlite::SqliteStore;
use kaban_store_sqlite::catalog_testing::{self, IoFaults};
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::time::Duration;

/// The length of the reclamation tests' extents: whole blocks, so a punch frees them.
const EXTENT_LEN: usize = 64 * 1024;

fn budget() -> QueryBudget {
    QueryBudget::new(u64::MAX, Some(Duration::from_secs(10)))
}

/// `len` bytes of the nonzero `fill`.
fn pattern(fill: u8, len: usize) -> Vec<u8> {
    assert_ne!(
        fill, 0,
        "a pattern is never zero: zero is what reclamation leaves"
    );
    vec![fill; len]
}

/// The generation and the test-class extents a fresh pin of `store` sees.
fn pinned(store: &SqliteStore) -> (u64, Vec<u64>) {
    let pin = catalog_testing::pin(store, &budget()).unwrap();
    let view = pin.view();
    (view.generation(), view.extents())
}

/// The `op` and the cause of a `StorageFatal`, or a panic naming what `result` was instead.
fn storage_fatal<T>(result: kaban_core::Result<T>) -> (&'static str, String) {
    match result {
        Err(StoreError::StorageFatal { op, source }) => (op, source.to_string()),
        Err(other) => panic!("expected StorageFatal, got {other:?}"),
        Ok(_) => panic!("expected StorageFatal, got Ok"),
    }
}

#[test]
fn an_injected_sync_failure_during_a_publish_is_fatal_attempted_once_and_publishes_nothing() {
    // Arrange: a store whose container syncs fail
    let s = Scratch::new("kaban-t5-io", "fatal-append");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let faults = IoFaults::install(&s.0);
    let before = pinned(&store);
    faults.fail_sync("append_extent");

    // Act
    let failed = catalog_testing::append_and_publish(&store, &pattern(7, 4096));

    // Assert: StorageFatal naming the sync, one attempt, and the catalog unchanged
    let (op, cause) = storage_fatal(failed);
    assert_eq!(op, "append_extent");
    assert_eq!(
        faults.sync_calls("append_extent"),
        1,
        "a failed sync was retried ({cause})"
    );
    assert_eq!(pinned(&store), before, "a failed sync changed the catalog");

    // Assert: the message names the operation, and a later sync that succeeds publishes only
    // its own extent (the failed range stays unnamed)
    let err = catalog_testing::append_and_publish(&store, &[1]);
    assert!(err.is_err(), "the fault is still installed");
    let shown = err.unwrap_err().to_string();
    assert!(
        shown.contains("append_extent"),
        "Display hides the op: {shown}"
    );
    faults.heal();
    let id = catalog_testing::append_and_publish(&store, &pattern(9, 100)).unwrap();
    let (generation, extents) = pinned(&store);
    assert_eq!(extents, vec![id]);
    assert!(generation > before.0);
    assert_eq!(faults.sync_calls("append_extent"), 3);
}

/// Open a fresh project with the creation sync `op` failing: the open is refused as
/// `StorageFatal` after one attempt; a later open syncs the artefacts again and serves the
/// catalog.
fn a_failed_creation_sync_refuses_the_open_and_a_later_open_completes_it(op: &'static str) {
    // Arrange
    let s = Scratch::new("kaban-t5-io", op);
    let faults = IoFaults::install(&s.0);
    faults.fail_sync(op);

    // Act
    let refused = SqliteStore::open(&s.db(), &s.keys());

    // Assert: refused, once
    let (failed_op, _) = storage_fatal(refused);
    assert_eq!(failed_op, op);
    assert_eq!(
        faults.sync_calls(op),
        1,
        "a failed creation sync was retried"
    );

    // Act: a later open with the storage healthy again
    faults.heal();
    let store = SqliteStore::open(&s.db(), &s.keys()).expect("the later open was refused");

    // Assert: it synced the artefacts again (a found artefact is synced like a created one),
    // and the catalog it serves publishes and reads
    assert_eq!(faults.sync_calls(op), 2, "the later open did not sync {op}");
    for name in ["kaban.kbx", "kaban.kbx.alloc", "kaban.kbx.pins"] {
        assert!(
            s.0.join(name).exists(),
            "{name} is missing after the later open"
        );
    }
    let id = catalog_testing::append_and_publish(&store, &pattern(5, 512)).unwrap();
    let pin = catalog_testing::pin(&store, &budget()).unwrap();
    assert_eq!(pin.view().read(id).unwrap(), Some(pattern(5, 512)));
}

#[test]
fn a_failed_sync_of_the_new_container_refuses_the_open_and_a_later_open_completes_it() {
    a_failed_creation_sync_refuses_the_open_and_a_later_open_completes_it("create_container");
}

#[test]
fn a_failed_sync_of_the_new_allocation_lock_refuses_the_open_and_a_later_open_completes_it() {
    a_failed_creation_sync_refuses_the_open_and_a_later_open_completes_it("create_alloc_lock");
}

#[test]
fn a_failed_sync_of_the_catalog_directory_entries_refuses_the_open_and_a_later_open_completes_it() {
    a_failed_creation_sync_refuses_the_open_and_a_later_open_completes_it("create_dir_entries");
}

/// What one reclamation of a retired extent did to the container.
#[derive(Debug)]
struct Reclaim {
    /// The container's length before and after.
    len: (u64, u64),
    /// Its allocated 512-byte blocks before and after.
    blocks: (u64, u64),
    /// The doomed extent's bytes after.
    doomed: Vec<u8>,
    /// The keeper's bytes after.
    keeper: Vec<u8>,
    /// What the reclamation reported.
    outcome: catalog_testing::Reclaimed,
}

/// Publish a keeper and a doomed extent, retire and reclaim the doomed one, and read the
/// container back raw.
fn reclaim_one(store: &SqliteStore, kbx: &Path) -> Reclaim {
    let ids = catalog_testing::append_and_publish_all(
        store,
        &[pattern(4, EXTENT_LEN), pattern(6, EXTENT_LEN)],
    )
    .unwrap();
    let (keeper, doomed) = (ids[0], ids[1]);
    let pin = catalog_testing::pin(store, &budget()).unwrap();
    let keeper_at = pin.view().span(keeper).unwrap();
    let doomed_at = pin.view().span(doomed).unwrap();
    drop(pin);
    let before = std::fs::metadata(kbx).unwrap();
    catalog_testing::retire(store, &[doomed]).unwrap();
    let reclaimed = catalog_testing::reclaim(store).unwrap();
    assert_eq!(reclaimed.punched, vec![doomed], "{reclaimed:?}");
    let after = std::fs::metadata(kbx).unwrap();
    let raw = std::fs::read(kbx).unwrap();
    let range = |(offset, len): (u64, u64)| {
        raw[usize::try_from(offset).unwrap()..usize::try_from(offset + len).unwrap()].to_vec()
    };
    Reclaim {
        len: (before.len(), after.len()),
        blocks: (before.blocks(), after.blocks()),
        doomed: range(doomed_at),
        keeper: range(keeper_at),
        outcome: reclaimed,
    }
}

#[test]
fn a_forced_no_punch_probe_degrades_reclamation_to_zero_fill_and_the_container_never_shrinks() {
    // Arrange: a filesystem that refuses keep-size hole punching, as the open's probe sees it
    let s = Scratch::new("kaban-t5-io", "no-punch");
    let faults = IoFaults::install(&s.0);
    faults.no_punch();
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();

    // Act
    let degraded = catalog_testing::degraded_reclamation(&store);
    let r = reclaim_one(&store, &s.0.join("kaban.kbx"));

    // Assert: degraded, the range zero-filled in place (its blocks stay allocated, so
    // nothing counts as reclaimed space) and the container neither shrank nor grew
    assert!(degraded, "a no-punch probe did not degrade reclamation");
    assert_eq!(
        (r.outcome.punched_bytes, r.outcome.zero_filled_bytes),
        (0, EXTENT_LEN as u64),
        "the zero-filled bytes were not reported as such"
    );
    assert_eq!(r.doomed, vec![0; EXTENT_LEN], "{:?}", r.len);
    assert_eq!(r.keeper, pattern(4, EXTENT_LEN));
    assert_eq!(
        r.len.0, r.len.1,
        "reclamation changed the container's length"
    );
    assert!(
        r.blocks.1 >= r.blocks.0,
        "zero-fill freed blocks: {:?}",
        r.blocks
    );
}

/// Whether the file system of `dir` punches keep-size holes, asked of it directly (a scratch
/// file, not the catalog's probe): the hole-punch test proves block release where it does.
fn punches_holes(dir: &Path) -> bool {
    let path = dir.join("punch-probe");
    let file = std::fs::File::create(&path).unwrap();
    std::io::Write::write_all(&mut &file, &pattern(1, EXTENT_LEN)).unwrap();
    file.sync_all().unwrap();
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let punched = rustix::fs::fallocate(
        &file,
        rustix::fs::FallocateFlags::PUNCH_HOLE | rustix::fs::FallocateFlags::KEEP_SIZE,
        0,
        EXTENT_LEN as u64,
    )
    .is_ok();
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    let punched = false;
    drop(file);
    std::fs::remove_file(&path).unwrap();
    punched
}

#[test]
fn a_reclaimed_range_reads_as_zeros_the_size_stays_and_where_holes_punch_its_blocks_are_freed() {
    // Arrange: whatever the scratch file system is (ext4, xfs, btrfs and tmpfs punch holes)
    let s = Scratch::new("kaban-t5-io", "punch");
    let punches = punches_holes(&s.0);
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();

    // Act
    let degraded = catalog_testing::degraded_reclamation(&store);
    let r = reclaim_one(&store, &s.0.join("kaban.kbx"));

    // Assert, everywhere: the catalog's probe agrees with the file system, the range reads as
    // zeros, the keeper is intact and the container's length is unchanged
    assert_eq!(
        degraded, !punches,
        "the catalog's probe disagrees with the file system (punches holes: {punches})"
    );
    assert_eq!(r.doomed, vec![0; EXTENT_LEN]);
    assert_eq!(r.keeper, pattern(4, EXTENT_LEN));
    assert_eq!(
        r.len.0, r.len.1,
        "reclamation changed the container's length"
    );
    if !punches {
        assert_eq!(
            (r.outcome.punched_bytes, r.outcome.zero_filled_bytes),
            (0, EXTENT_LEN as u64),
            "zero-filled bytes were reported as reclaimed space"
        );
        eprintln!(
            "skipped the block-release assertion: the scratch file system at {} does not punch \
             holes (a punching file system proves it)",
            s.0.display()
        );
        return;
    }
    // where holes punch: its blocks were freed (a punch, not a zero-fill) and reported so
    assert!(
        r.blocks.1 < r.blocks.0,
        "the reclaimed range was not punched: blocks {:?}",
        r.blocks
    );
    assert_eq!(
        (r.outcome.punched_bytes, r.outcome.zero_filled_bytes),
        (EXTENT_LEN as u64, 0),
        "the punched bytes were not reported as such"
    );
}

#[test]
fn a_punch_refused_after_a_successful_probe_zero_fills_syncs_and_reports_degraded_reclamation() {
    // Arrange: the open's probe finds hole punching, then the file system refuses the punch of
    // a release (as one remounted or replaced under a live catalog)
    let s = Scratch::new("kaban-t5-io", "late-no-punch");
    let faults = IoFaults::install(&s.0);
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    if catalog_testing::degraded_reclamation(&store) {
        eprintln!(
            "skipped: the scratch file system does not punch holes, so no probe can succeed \
             before the refused punch (the forced-fallback test covers zero-fill)"
        );
        return;
    }
    faults.no_release_punch();

    // Act
    let r = reclaim_one(&store, &s.0.join("kaban.kbx"));

    // Assert: the range was zero-filled in place and synced, the keeper and the length kept,
    // no byte reported as punched, and the catalog now reports degraded reclamation
    assert_eq!(r.doomed, vec![0; EXTENT_LEN]);
    assert_eq!(r.keeper, pattern(4, EXTENT_LEN));
    assert_eq!(
        r.len.0, r.len.1,
        "reclamation changed the container's length"
    );
    assert!(
        r.blocks.1 >= r.blocks.0,
        "zero-fill freed blocks: {:?}",
        r.blocks
    );
    assert_eq!(
        faults.sync_calls("reclaim_ranges"),
        1,
        "the zero-fill was not synced"
    );
    assert_eq!(
        (r.outcome.punched_bytes, r.outcome.zero_filled_bytes),
        (0, EXTENT_LEN as u64),
        "zero-filled bytes were reported as reclaimed space"
    );
    assert!(
        catalog_testing::degraded_reclamation(&store),
        "a refused punch left reclamation reported as not degraded"
    );
}

#[test]
fn an_injected_sync_failure_after_reclaiming_ranges_is_fatal_and_keeps_their_catalog_rows() {
    // Arrange: a retired extent and a container whose reclamation sync fails
    let s = Scratch::new("kaban-t5-io", "fatal-reclaim");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let faults = IoFaults::install(&s.0);
    let id = catalog_testing::append_and_publish(&store, &pattern(8, EXTENT_LEN)).unwrap();
    catalog_testing::retire(&store, &[id]).unwrap();
    faults.fail_sync("reclaim_ranges");

    // Act
    let failed = catalog_testing::reclaim(&store);

    // Assert: fatal after one attempt; the extent's row survives for a later reclamation,
    // which completes once the storage is healthy
    let (op, _) = storage_fatal(failed);
    assert_eq!(op, "reclaim_ranges");
    assert_eq!(faults.sync_calls("reclaim_ranges"), 1);
    faults.heal();
    let again = catalog_testing::reclaim(&store).unwrap();
    assert_eq!(again.punched, vec![id], "{again:?}");
}
