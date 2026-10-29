//! Startup under contention (core-spec F15, N4; M2 review R2-5 and the BUSY-on-open peer review
//! `reviews/m2-flake-busy-open-review-codex.md`): `PRAGMA journal_mode = WAL` upgrades an
//! internal read transaction to a write transaction, a path on which SQLite returns plain
//! `SQLITE_BUSY` without invoking the busy handler, so `SqliteStore::open` must supply the
//! bounded retry itself.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::Scratch;
use kaban_store_sqlite::SqliteStore;
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// R2-5 deterministic regression: an initialized database in rollback-journal mode, connection
/// A holds `BEGIN IMMEDIATE` (the RESERVED lock) while B runs `SqliteStore::open`; A releases
/// after about 200 ms. B must succeed (within the 5 s startup deadline) and leave the file in
/// WAL mode. Before the fix B failed at once with `SQLITE_BUSY` (code 5) from the WAL pragma.
#[test]
fn open_waits_for_a_reserved_lock_held_during_wal_conversion_instead_of_failing_busy() {
    let s = Scratch::new("kaban-busy", "reserved");
    // an initialized database, then back to a rollback journal (as a pre-WAL file would be)
    drop(SqliteStore::open(&s.db(), &s.0.join("keys")).unwrap());
    {
        let a = rusqlite::Connection::open(s.db()).unwrap();
        let mode: String = a
            .query_row("PRAGMA journal_mode = DELETE", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode, "delete");
    }

    let (held_tx, held_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let db = s.db();
    let holder = std::thread::spawn(move || {
        let a = rusqlite::Connection::open(db).unwrap();
        a.execute_batch("BEGIN IMMEDIATE").unwrap();
        held_tx.send(()).unwrap();
        // released by the test after B has been blocked for a while (or on test failure)
        let _ = release_rx.recv_timeout(Duration::from_secs(10));
        a.execute_batch("COMMIT").unwrap();
    });
    held_rx.recv().unwrap();

    let db = s.db();
    let keys = s.0.join("keys");
    let opener = std::thread::spawn(move || {
        let started = Instant::now();
        let r = SqliteStore::open(&db, &keys);
        (r.map(|_| ()), started.elapsed())
    });
    std::thread::sleep(Duration::from_millis(200));
    release_tx.send(()).unwrap();
    holder.join().unwrap();
    let (result, took) = opener.join().unwrap();
    assert!(
        result.is_ok(),
        "open must wait for the RESERVED lock, not fail: {result:?} after {took:?}"
    );
    assert!(
        took >= Duration::from_millis(150) && took < Duration::from_secs(5),
        "open waited for the holder: {took:?}"
    );
    let b = rusqlite::Connection::open(s.db()).unwrap();
    let mode: String = b
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        mode, "wal",
        "the opener established WAL once the lock was released"
    );
}
