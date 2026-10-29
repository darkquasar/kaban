//! The default sink (engine-spec EN-E10b `Verify:` "the `Sink` trait object test from EN-F15
//! runs the default sink and a no-op test sink over the same change sequence with identical
//! acks"; engine-plan E2.0 item 4): `kaban_api::sync::DefaultSink` and a no-op sink, both driven
//! by `kaban_core::run_sink` over one SQLite store, acknowledge the same positions page by page.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

use kaban_api::sync::{DefaultSink, EXPORT_CONSUMER};
use kaban_api::{KabanApi, RememberRequest};
use kaban_core::{Ack, Change, ChangeLogStore, LeaseStore, MemoryStore, Sink, SinkId, run_sink};
use kaban_policy::Policy;
use kaban_store_sqlite::SqliteStore;
use std::sync::Arc;

/// Looks at nothing, acks the page's last seq.
struct Noop;

impl Sink for Noop {
    fn id(&self) -> SinkId {
        SinkId::EXPORT
    }
    fn consume(&mut self, changes: &[Change]) -> kaban_core::Result<Ack> {
        Ok(Ack {
            upto: changes.last().map_or(0, |c| c.seq),
        })
    }
}

/// Every ack of `sink` over the whole log from 0 in pages of `limit`.
fn acks(
    store: &SqliteStore,
    sink: &mut dyn Sink,
    lease: &kaban_core::Lease,
    limit: usize,
) -> Vec<u64> {
    let mut out = Vec::new();
    let mut after = 0;
    loop {
        let report = run_sink(store, sink, lease, after, limit).unwrap();
        assert!(!report.lease_lost, "{report:?}");
        let Some(acked) = report.acked else {
            break;
        };
        out.push(acked);
        after = acked;
    }
    out
}

#[test]
fn the_default_sink_and_a_no_op_sink_acknowledge_identical_positions() {
    // Arrange: five writes through the service, one lease
    let store = Arc::new(SqliteStore::open_memory().unwrap());
    let api = KabanApi::new(store.clone(), Policy::empty(), store.signer(), None);
    for text in ["one", "two", "three", "four", "five"] {
        api.remember(RememberRequest {
            kind: "Fact".into(),
            text: format!("quokka {text}"),
            ..Default::default()
        })
        .unwrap();
    }
    let rows = store.changes_read(0, 1_000).unwrap();
    assert!(rows.len() >= 5, "{rows:?}");
    let lease = store
        .lease_acquire(EXPORT_CONSUMER, "test:1", 60)
        .unwrap()
        .unwrap();
    assert_eq!(DefaultSink::new(&api).id(), SinkId::EXPORT);

    // Act
    let by_default = acks(&store, &mut DefaultSink::new(&api), &lease, 2);
    let by_noop = acks(&store, &mut Noop, &lease, 2);

    // Assert
    assert_eq!(by_default, by_noop);
    assert_eq!(by_default.last().copied(), Some(rows.last().unwrap().seq));
    assert_eq!(by_default.len(), rows.len().div_ceil(2), "one ack per page");
    assert_eq!(
        store.stats(None).unwrap()["changes_lag"],
        0,
        "the cursor reached the newest row"
    );
}
