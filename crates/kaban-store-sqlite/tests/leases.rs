//! Leases with fencing tokens (core-spec F15; core-plan C2.5; MD-12, MD-14): one `BEGIN
//! IMMEDIATE` acquisition that increments `fence`, DB-side expiry with `unixepoch()`, renewal
//! guarded by the fence, and the `ChangeLog` ack of a superseded holder rejected.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::Scratch;
use kaban_core::{ChangeLogStore, LeaseStore, MemoryStore};
use kaban_store_sqlite::SqliteStore;

/// C2.5 slice 1 (F15 "acquisition increments `fence` in one `BEGIN IMMEDIATE` transaction"):
/// of two acquirers of a free lease exactly one wins and receives fence 1; the loser gets
/// `None`; the holder's own second acquisition of its unexpired lease is also `None` (the
/// fence does not move: the holder renews instead).
#[test]
fn two_acquirers_of_one_lease_one_wins() {
    let store = SqliteStore::open_memory().unwrap();
    let first = store.lease_acquire("export", "host:1:aaaa", 30).unwrap();
    let second = store.lease_acquire("export", "host:2:bbbb", 30).unwrap();
    let (winner, loser) = match (first, second) {
        (Some(w), None) => (w, "host:2:bbbb"),
        (None, Some(w)) => (w, "host:1:aaaa"),
        other => panic!("exactly one acquirer wins: {other:?}"),
    };
    assert_eq!(winner.name, "export");
    assert_ne!(winner.owner, loser);
    assert_eq!(
        winner.fence, 1,
        "the first acquisition of a lease is fence 1"
    );
    assert!(winner.expires_at > 0, "{winner:?}");
    // a second acquisition by the holder of an unexpired lease does not move the fence
    assert_eq!(
        store.lease_acquire("export", &winner.owner, 30).unwrap(),
        None
    );
    // a different lease name is independent
    let other = store.lease_acquire("vectors", loser, 30).unwrap().unwrap();
    assert_eq!((other.name.as_str(), other.fence), ("vectors", 1));
}

/// Clock skew or a dead holder: move the lease's expiry into the past directly (the database
/// clock decides, so the test does not wait for a real ttl to pass).
fn expire(db: &std::path::Path, name: &str) {
    let raw = rusqlite::Connection::open(db).unwrap();
    raw.execute(
        "UPDATE leases SET expires_at = unixepoch() - 1 WHERE name = ?1",
        [name],
    )
    .unwrap();
}

fn remember(store: &SqliteStore, text: &str) {
    use kaban_core::{Actor, NewNode, WorkspaceId, WriteCtx};
    store
        .remember(
            NewNode {
                kind: "Fact".into(),
                text: text.into(),
                ..Default::default()
            },
            &[],
            &WriteCtx::new("pv", WorkspaceId::default(), Actor::Agent),
        )
        .unwrap();
}

/// C2.5 slice 2 (F15 SR-2 "an expired worker cannot resume"; MD-12 re-proof of the C2.4 ack
/// guard through `lease_acquire`): a holder renews while its fence is current; once its lease
/// has expired a contender takes it over with fence + 1, after which the old holder's renew
/// and `ChangeLog` ack are both `LeaseLost` and its cursor does not move; the new holder acks.
#[test]
fn an_expired_lease_is_taken_over_with_the_next_fence_and_the_old_holder_is_fenced_out() {
    let s = Scratch::new("kaban-leases", "takeover");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    remember(&store, "wombats dig burrows");
    remember(&store, "wombat cubes");
    let old = store
        .lease_acquire("export", "host:1:old", 30)
        .unwrap()
        .unwrap();
    assert_eq!(old.fence, 1);
    // renewal extends the expiry while the fence is current
    store.lease_renew(&old, 60).unwrap();
    let after_renew: i64 = rusqlite::Connection::open(s.db())
        .unwrap()
        .query_row(
            "SELECT expires_at FROM leases WHERE name = 'export'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        after_renew >= old.expires_at + 29,
        "{after_renew} vs {old:?}"
    );
    store.changes_ack("export", 1, old.fence).unwrap();
    // the holder dies (or its clock lies): the row expires; a contender takes over
    assert_eq!(
        store.lease_acquire("export", "host:2:new", 30).unwrap(),
        None,
        "an unexpired lease is not taken"
    );
    expire(&s.db(), "export");
    let new = store
        .lease_acquire("export", "host:2:new", 30)
        .unwrap()
        .unwrap();
    assert_eq!((new.fence, new.owner.as_str()), (2, "host:2:new"));
    assert!(new.expires_at > old.expires_at - 30, "{new:?}");
    // the old holder is fenced out of renewal and of the `ChangeLog` cursor
    let err = store.lease_renew(&old, 30).unwrap_err();
    assert!(
        matches!(&err, kaban_core::StoreError::LeaseLost { name, fence: 1 } if name == "export"),
        "{err:?}"
    );
    let err = store.changes_ack("export", 2, old.fence).unwrap_err();
    assert!(
        matches!(err, kaban_core::StoreError::LeaseLost { fence: 1, .. }),
        "{err:?}"
    );
    let cursor: i64 = rusqlite::Connection::open(s.db())
        .unwrap()
        .query_row(
            "SELECT last_seq FROM projections WHERE name = 'export'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(cursor, 1, "the fenced-out holder moved nothing");
    // the new holder works normally
    store.lease_renew(&new, 30).unwrap();
    store.changes_ack("export", 2, new.fence).unwrap();
    // a lease that was never acquired cannot be renewed
    let ghost = kaban_core::Lease {
        name: "nobody".into(),
        owner: "x".into(),
        fence: 0,
        expires_at: 0,
    };
    assert!(matches!(
        store.lease_renew(&ghost, 30),
        Err(kaban_core::StoreError::LeaseLost { .. })
    ));
}

/// R5-11 (F15): a holder releases its lease and the next acquirer takes it at once with the
/// next fence (the fence is kept, not reset); a release with a stale fence is `LeaseLost` and
/// leaves the current holder's lease alone.
#[test]
fn a_released_lease_is_acquired_at_once_with_the_next_fence_and_a_stale_release_is_fenced_out() {
    let store = SqliteStore::open_memory().unwrap();
    let first = store
        .lease_acquire("export", "host:1:aaaa", 60)
        .unwrap()
        .expect("free");
    assert_eq!(first.fence, 1);
    assert_eq!(
        store.lease_acquire("export", "host:2:bbbb", 60).unwrap(),
        None
    );
    store.lease_release(&first).unwrap();
    let second = store
        .lease_acquire("export", "host:2:bbbb", 60)
        .unwrap()
        .expect("released: free");
    assert_eq!(second.fence, 2, "the fence is monotonic across a release");
    // the old holder's stale release neither expires nor moves the successor's lease
    let err = store.lease_release(&first).unwrap_err();
    assert!(
        matches!(err, kaban_core::StoreError::LeaseLost { fence: 1, .. }),
        "{err:?}"
    );
    assert_eq!(
        store.lease_acquire("export", "host:3:cccc", 60).unwrap(),
        None
    );
    assert!(store.lease_renew(&second, 60).is_ok());
}
