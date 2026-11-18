//! Compaction claims and publication (spec 11 T6 Steps 1 and 3, KER-07, PC-9, 7.4): two
//! compactions never claim the same input, and a publication switches the view chain,
//! retires its inputs and hides the sediment it consumed from every later pin, while an
//! earlier pin keeps what it saw.
//!
//! Runs with the `failpoints` feature (the test class lives behind it).

#![cfg(feature = "failpoints")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::{Scratch, created, ctx, nid};
use kaban_core::MemoryStore;
use kaban_core::query_budget::QueryBudget;
use kaban_store_sqlite::SqliteStore;
use kaban_store_sqlite::catalog_testing;
use std::sync::{Arc, Barrier};
use std::time::Duration;

/// The bound of every wait on another thread.
const STEP: Duration = Duration::from_secs(10);

fn budget() -> QueryBudget {
    QueryBudget::new(u64::MAX, Some(STEP))
}

/// `len` bytes of the nonzero `fill`.
fn pattern(fill: u8, len: usize) -> Vec<u8> {
    vec![fill; len]
}

#[test]
fn two_compactions_over_the_same_inputs_get_one_claim_between_them() {
    // Arrange: two sealed extents and two stores of one database, one compaction thread each
    let s = Scratch::new("kaban-t6-compact", "claim-race");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let inputs =
        catalog_testing::append_and_publish_all(&store, &[pattern(3, 64), pattern(5, 64)]).unwrap();
    let start = Arc::new(Barrier::new(2));
    let (tx, rx) = std::sync::mpsc::channel();
    for _ in 0..2 {
        let (db, keys, inputs, start, tx) = (
            s.db(),
            s.keys(),
            inputs.clone(),
            Arc::clone(&start),
            tx.clone(),
        );
        std::thread::spawn(move || {
            let mine = SqliteStore::open(&db, &keys).unwrap();
            start.wait();
            let claimed = catalog_testing::claim(&mine, &inputs).unwrap();
            let _ = tx.send(claimed.map(|c| c.inputs()));
        });
    }

    // Act
    let outcomes: Vec<Option<Vec<u64>>> = (0..2)
        .map(|_| {
            rx.recv_timeout(STEP)
                .expect("a compaction did not answer in time")
        })
        .collect();

    // Assert: exactly one claimed every input; the other claimed nothing (its inputs skipped)
    let won: Vec<&Vec<u64>> = outcomes.iter().flatten().collect();
    assert_eq!(won, vec![&inputs], "{outcomes:?}");
    assert!(
        catalog_testing::claim(&store, &inputs[..1])
            .unwrap()
            .is_none(),
        "an input held by a claim was claimed again"
    );
}

#[test]
fn a_publication_switches_the_chain_and_hides_what_it_consumed_from_later_pins_only() {
    // Arrange: the test class's sediment from two links, and a chain of three extents
    let s = Scratch::new("kaban-t6-compact", "publish");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let _observed = catalog_testing::install_edge_sediment_class(&store);
    let a = created(&store, "Fact", "the lighthouse keeper logs every ship");
    let b = created(&store, "Fact", "the harbour master reads the log");
    for (from, to) in [(&a, &b), (&b, &a)] {
        store
            .link(
                &nid(&from.id),
                &nid(&to.id),
                "RELATES_TO",
                None,
                None,
                &ctx(),
            )
            .unwrap();
    }
    let ids = catalog_testing::append_and_publish_all(
        &store,
        &[pattern(7, 100), pattern(9, 100), pattern(11, 100)],
    )
    .unwrap();
    let sediment = catalog_testing::pin_sediment_rows(&store).unwrap();
    assert_eq!(sediment.len(), 2, "{sediment:?}");
    let earlier = catalog_testing::pin(&store, &budget()).unwrap();

    // Act: the first two extents and the sediment fold into one new extent
    let claim = catalog_testing::claim(&store, &ids[..2])
        .unwrap()
        .expect("the inputs were free");
    let seqs: Vec<u64> = sediment.iter().map(|(seq, _)| *seq).collect();
    let generation =
        catalog_testing::publish(&store, claim, &[pattern(13, 200)], &ids[..2], &seqs).unwrap();
    let later = catalog_testing::pin(&store, &budget()).unwrap();

    // Assert: the new extent takes the inputs' place in the chain, the consumed sediment is
    // gone from the later pin, and the earlier pin still holds and reads everything it saw
    assert_eq!(later.view().generation(), generation);
    let chain = later.view().extents();
    assert_eq!(chain.len(), 2, "{chain:?}");
    assert_eq!(chain[1], ids[2], "{chain:?}");
    assert_eq!(later.view().read(chain[0]).unwrap(), Some(pattern(13, 200)));
    assert_eq!(
        catalog_testing::pin_sediment_rows(&store).unwrap(),
        Vec::new()
    );
    assert_eq!(earlier.view().extents(), ids);
    assert_eq!(earlier.view().read(ids[0]).unwrap(), Some(pattern(7, 100)));
    // the claim ended with the switch: a retired input cannot be claimed, the new one can
    assert!(catalog_testing::claim(&store, &ids[..1]).unwrap().is_none());
    assert!(
        catalog_testing::claim(&store, &chain[..1])
            .unwrap()
            .is_some()
    );
}

#[test]
fn consumed_sediment_rows_are_deleted_after_the_switch_by_reclamation() {
    // Arrange: one consumed sediment row, its switch published
    let s = Scratch::new("kaban-t6-compact", "sediment-delete");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let _observed = catalog_testing::install_edge_sediment_class(&store);
    let a = created(&store, "Fact", "the tide turns at six");
    let b = created(&store, "Fact", "the boats leave on the turn");
    store
        .link(&nid(&a.id), &nid(&b.id), "RELATES_TO", None, None, &ctx())
        .unwrap();
    let seqs: Vec<u64> = catalog_testing::pin_sediment_rows(&store)
        .unwrap()
        .into_iter()
        .map(|(seq, _)| seq)
        .collect();
    let claim = catalog_testing::claim(&store, &[])
        .unwrap()
        .expect("a claim of no extents (a seal) was refused");
    catalog_testing::publish(&store, claim, &[pattern(17, 50)], &[], &seqs).unwrap();

    // Act
    let first = catalog_testing::reclaim_retired(&store).unwrap();
    let second = catalog_testing::reclaim_retired(&store).unwrap();

    // Assert: the switch only marked the row; the reclamation deleted it, once
    assert_eq!(first.sediment_deleted, 1, "{first:?}");
    assert_eq!(second.sediment_deleted, 0, "{second:?}");
}
