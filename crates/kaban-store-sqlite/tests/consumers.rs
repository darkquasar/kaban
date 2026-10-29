//! Independent `ChangeLog` consumers (spec 05 SS-06): installing a consumer registers its
//! retention cursor before its initial snapshot or consumption begins, so pruning keeps every
//! row after it however far the other consumers have advanced; a second consumer registers and
//! advances on its own `projections` cursor and lease without touching another consumer's
//! code; pruning never evicts a slow consumer and `seq` is never reused.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::created;
use kaban_core::{Ack, Change, ChangeLogStore, LeaseStore, Result, Sink, SinkId, run_sink};
use kaban_store_sqlite::SqliteStore;

/// A minimal consumer: records the seqs it was handed and acks the last one.
struct Probe {
    id: SinkId,
    seen: Vec<u64>,
}

impl Probe {
    fn new(name: &'static str) -> Self {
        Self {
            id: SinkId(name),
            seen: Vec::new(),
        }
    }
}

impl Sink for Probe {
    fn id(&self) -> SinkId {
        self.id
    }
    fn consume(&mut self, changes: &[Change]) -> Result<Ack> {
        self.seen.extend(changes.iter().map(|c| c.seq));
        Ok(Ack {
            upto: changes.last().map_or(0, |c| c.seq),
        })
    }
}

fn head(store: &SqliteStore) -> u64 {
    store
        .changes_read(0, 10_000)
        .unwrap()
        .last()
        .map_or(0, |c| c.seq)
}

/// Install `sink` (its cursor is registered under its lease before it consumes, SS-06) and
/// drain it from `after` to the head; returns its final cursor.
fn drain(store: &SqliteStore, sink: &mut Probe, mut after: u64) -> u64 {
    let lease = store
        .lease_acquire(sink.id().0, "probe", 60)
        .unwrap()
        .unwrap();
    store.changes_register(sink.id().0, lease.fence).unwrap();
    loop {
        let report = run_sink(store, sink, &lease, after, 3).unwrap();
        match report.acked {
            Some(upto) => after = upto,
            None => break,
        }
    }
    store.lease_release(&lease).unwrap();
    after
}

#[test]
fn a_consumer_registered_before_its_bootstrap_is_never_pruned_past() {
    let store = SqliteStore::open_memory().unwrap();
    for i in 0..3 {
        created(
            &store,
            "Fact",
            &format!("history before the slow consumer {i}"),
        );
    }
    // the fast consumer has seen everything so far
    let mut fast = Probe::new("fast-projector");
    let fast_at = drain(&store, &mut fast, 0);
    assert_eq!(fast_at, head(&store));

    // the slow consumer is installed: its cursor is registered before its snapshot is taken
    let lease = store
        .lease_acquire("slow-probe", "probe", 60)
        .unwrap()
        .unwrap();
    let start = store.changes_register("slow-probe", lease.fence).unwrap();
    assert_eq!(start, head(&store), "a new consumer starts at the head");
    store.lease_release(&lease).unwrap();

    // more writes while the slow consumer bootstraps; the fast one keeps up and prunes hard
    for i in 0..4 {
        created(&store, "Fact", &format!("written during the bootstrap {i}"));
    }
    let after_bootstrap: Vec<u64> = store
        .changes_read(start, 10_000)
        .unwrap()
        .iter()
        .map(|c| c.seq)
        .collect();
    assert_ne!(after_bootstrap, Vec::<u64>::new());
    drain(&store, &mut fast, fast_at);
    store.changes_prune(0).unwrap();

    // every row after the slow cursor survived, and the slow consumer reads it all
    let mut slow = Probe::new("slow-probe");
    let slow_at = drain(&store, &mut slow, start);
    assert_eq!(
        slow.seen, after_bootstrap,
        "nothing after the slow cursor was pruned"
    );
    assert_eq!(slow_at, head(&store));
}

#[test]
fn consumers_advance_independently_and_registration_keeps_an_existing_cursor() {
    let store = SqliteStore::open_memory().unwrap();
    let lease = store.lease_acquire("second", "probe", 60).unwrap().unwrap();
    assert_eq!(
        store.changes_register("second", lease.fence).unwrap(),
        0,
        "an empty log registers at 0"
    );
    store.lease_release(&lease).unwrap();
    for i in 0..5 {
        created(&store, "Fact", &format!("shared feed row {i}"));
    }
    let mut first = Probe::new("first");
    let first_at = drain(&store, &mut first, 0);
    let mut second = Probe::new("second");
    let second_at = drain(&store, &mut second, 0);
    assert_eq!(first.seen, second.seen, "both see the same feed");
    assert_eq!(first_at, second_at);

    // a re-registration never moves a cursor (forward or back)
    created(&store, "Fact", "one more row");
    let lease = store.lease_acquire("second", "probe", 60).unwrap().unwrap();
    assert_eq!(
        store.changes_register("second", lease.fence).unwrap(),
        second_at
    );
    // a stale fence registers nothing
    assert!(matches!(
        store.changes_register("second", lease.fence + 1),
        Err(kaban_core::StoreError::LeaseLost { .. })
    ));
    store.lease_release(&lease).unwrap();
}

#[test]
fn pruning_keeps_the_newest_row_so_seq_is_never_reused() {
    let store = SqliteStore::open_memory().unwrap();
    for i in 0..3 {
        created(&store, "Fact", &format!("row {i}"));
    }
    let mut only = Probe::new("only");
    let at = drain(&store, &mut only, 0);
    store.changes_prune(0).unwrap();
    let left = store.changes_read(0, 10).unwrap();
    assert_eq!(left.len(), 1, "the newest row survives");
    assert_eq!(left[0].seq, at);
    created(&store, "Fact", "after the prune");
    assert!(head(&store) > at, "seq continues past every cursor");
}
