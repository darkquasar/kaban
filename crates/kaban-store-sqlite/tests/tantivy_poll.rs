//! The mode-B loop at its seam (spec 05 SS-21): a holder woken by `PRAGMA data_version`
//! publishes another connection's write within a tick without a barrier asking for it, a
//! write that lands between a run and the idle wait is not missed, the lease is renewed while
//! idle, and the loop returns with lease and lock released when told to stop.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

#[path = "tantivy_common/mod.rs"]
mod common;

use common::{Scratch, note, rids, seed};
use kaban_core::text_index::{MatchMode, TextIndex, Visibility};
use kaban_core::{ChangeLogStore, LeaseStore, RecallQuery};
use kaban_index_tantivy::projector::{Halt, Projector};
use kaban_store_sqlite::index_rows::IndexRows;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const CONSUMER: &str = "text-index";
const TTL_S: u32 = 2;

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

/// Wait until `index` publishes `store`'s head, within `timeout`.
fn published_within(
    index: &dyn TextIndex,
    store: &kaban_store_sqlite::SqliteStore,
    timeout: Duration,
) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if visible_through(index) == Some(i64::try_from(store.changes_head().unwrap()).unwrap()) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

/// The loop publishes writes it was never asked about: a note written on the store's own
/// connection while the holder idles is a hit within a tick or two; so is one written while
/// the holder is between its last page and its idle wait (the quiet check, not the version
/// alone); the lease is renewed past its ttl while nothing is written; told to stop, the loop
/// returns without a halt and the lease is free again.
#[test]
fn the_holder_wakes_on_data_version_and_publishes_within_a_tick() {
    let s = Scratch::new("poll-wakes");
    let store = s.store();
    let index = s.adapter();
    note(&store, "default", "Fact", "quokka before the holder", &[]);
    let observation = IndexRows::open(&s.db()).unwrap();
    let stop = AtomicBool::new(false);
    let projector = Projector::new(&index, &store, &store, "holder", TTL_S);

    std::thread::scope(|scope| {
        let holder = scope.spawn(|| projector.poll(&|| observation.data_version(), &stop));
        assert!(
            published_within(&index, &store, Duration::from_secs(5)),
            "the initial pending work"
        );
        // another connection's write, with nobody draining: the loop alone publishes it
        for i in 0..5 {
            note(
                &store,
                "default",
                "Fact",
                &format!("quokka polled {i}"),
                &[],
            );
            assert!(
                published_within(&index, &store, Duration::from_secs(2)),
                "write {i} published by the loop"
            );
        }
        let q = query("quokka");
        assert_eq!(rids(&index, &seed(&q, MatchMode::Lexical, 10, 0)).len(), 6);
        // idle past the ttl: the lease is renewed, nobody else can take it
        std::thread::sleep(Duration::from_secs(u64::from(TTL_S) + 1));
        assert!(
            store
                .lease_acquire(CONSUMER, "intruder", TTL_S)
                .unwrap()
                .is_none(),
            "the idle holder renewed its lease"
        );
        assert_eq!(store.changes_lag_by_consumer().unwrap()[CONSUMER], 0);
        stop.store(true, Ordering::Release);
        let progress = holder.join().unwrap().unwrap();
        assert_eq!(progress.halt, None, "{progress:?}");
        assert_eq!(progress.seq, store.changes_head().unwrap());
    });
    // released: lease and lock are free for the next holder
    let lease = store.lease_acquire(CONSUMER, "next", TTL_S).unwrap();
    assert!(lease.is_some(), "the lease was released");
    store.lease_release(&lease.unwrap()).unwrap();
    assert_eq!(
        index.rebuild().map_err(|e| e.to_string()),
        Ok(()),
        "the lock was released"
    );
}

/// A second holder on the same database is not elected (the lease is held) and publishes
/// nothing; a holder whose lease a successor took (expired while this one was unable to
/// renew) relinquishes with `LeaseLost` on its next tick instead of publishing further.
#[test]
fn a_second_holder_is_not_elected_and_a_superseded_one_relinquishes() {
    let s = Scratch::new("poll-election");
    let store = s.store();
    let index = s.adapter();
    note(&store, "default", "Fact", "quokka one", &[]);
    let observation = IndexRows::open(&s.db()).unwrap();
    let stop = AtomicBool::new(false);
    let projector = Projector::new(&index, &store, &store, "holder", TTL_S);
    std::thread::scope(|scope| {
        let holder = scope.spawn(|| projector.poll(&|| observation.data_version(), &stop));
        assert!(published_within(&index, &store, Duration::from_secs(5)));
        let second = s.adapter();
        let other = IndexRows::open(&s.db()).unwrap();
        let progress = Projector::new(&second, &store, &store, "second", TTL_S)
            .poll(&|| other.data_version(), &AtomicBool::new(true))
            .unwrap();
        assert_eq!(progress.halt, Some(Halt::LeaseHeld));
        // the successor: the holder's lease forced to expiry, taken over with a new fence
        rusqlite::Connection::open(s.db())
            .unwrap()
            .execute(
                "UPDATE leases SET expires_at = unixepoch() - 1 WHERE name = ?1",
                [CONSUMER],
            )
            .unwrap();
        let successor = store
            .lease_acquire(CONSUMER, "successor", 30)
            .unwrap()
            .expect("the expired lease is taken");
        let progress = holder.join().unwrap().unwrap();
        assert_eq!(progress.halt, Some(Halt::LeaseLost), "{progress:?}");
        store.lease_release(&successor).unwrap();
    });
}
