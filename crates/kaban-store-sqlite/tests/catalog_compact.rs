//! Compaction claims and publication (spec 11 T6 Steps 1 and 3, KER-07, PC-9, 7.4): two
//! compactions never claim the same input, and a publication switches the view chain,
//! retires its inputs and hides the sediment it consumed from every later pin, while an
//! earlier pin keeps what it saw.
//!
//! Compaction by policy (Step 4 of the T6 brief, 7.6, 7.8): due sediment seals, a deep chain
//! folds, a unit refused by its maintenance budget is retried smaller and reported, and a
//! purge rewrites the extents that hold a purged record.
//!
//! Runs with the `failpoints` feature (the test class lives behind it).

#![cfg(feature = "failpoints")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::{Scratch, created, ctx, nid};
use kaban_core::config::{CatalogCfg, SealOps};
use kaban_core::query_budget::{Limit, QueryBudget};
use kaban_core::{MemoryStore, StoreError};
use kaban_store_sqlite::catalog_testing::{self, Window};
use kaban_store_sqlite::{CompactBudget, SkipReason, Skipped, SqliteStore, WorkKind};
use std::sync::{Arc, Barrier, Mutex};
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
    let generation = catalog_testing::publish(
        &store,
        claim,
        &[pattern(13, 200)],
        &[("main", &ids[..2])],
        &ids[..2],
        &seqs,
    )
    .unwrap();
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
    catalog_testing::publish(
        &store,
        claim,
        &[pattern(17, 50)],
        &[("main", &[])],
        &[],
        &seqs,
    )
    .unwrap();

    // Act
    let first = catalog_testing::reclaim_retired(&store).unwrap();
    let second = catalog_testing::reclaim_retired(&store).unwrap();

    // Assert: the switch only marked the row; the reclamation deleted it, once
    assert_eq!(first.sediment_deleted, 1, "{first:?}");
    assert_eq!(second.sediment_deleted, 0, "{second:?}");
}

/// The driver's text of a backend error (what names the refusal), else the error's own.
fn driver_text(err: &StoreError) -> String {
    match err {
        StoreError::Backend { source } => source.driver().to_string(),
        other => other.to_string(),
    }
}

/// A code view's name (`code@<40 hex>`).
fn code_view() -> String {
    format!("code@{}", "7".repeat(40))
}

#[test]
fn a_fold_in_one_view_leaves_another_view_over_the_same_base_as_it_was() {
    // Arrange: `main` is [B, A] and a code view is [B, C]: they share the base B
    let s = Scratch::new("kaban-t6-compact", "shared-base");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let code = code_view();
    let b = catalog_testing::append_and_publish_into(&store, &pattern(2, 64), &["main", &code])
        .unwrap();
    let a = catalog_testing::append_and_publish_into(&store, &pattern(4, 64), &["main"]).unwrap();
    let c = catalog_testing::append_and_publish_into(&store, &pattern(6, 64), &[&code]).unwrap();
    let claim = catalog_testing::claim(&store, &[b, a]).unwrap().unwrap();

    // Act: fold main's [B, A] into F, first retiring B too (the code view still needs it)
    let (refused, claim) = *catalog_testing::publish(
        &store,
        claim,
        &[pattern(8, 128)],
        &[("main", &[b, a])],
        &[b, a],
        &[],
    )
    .expect_err("a base another view still holds was retired");
    let unchanged = (
        catalog_testing::chain(&store, "main").unwrap(),
        catalog_testing::chain(&store, &code).unwrap(),
    );
    catalog_testing::publish(
        &store,
        claim,
        &[pattern(8, 128)],
        &[("main", &[b, a])],
        &[a],
        &[],
    )
    .unwrap();

    // Assert: the refusal named the base and changed nothing; the fold changed main only
    let text = driver_text(&refused);
    assert!(text.contains(&format!("extent {b} is retired")), "{text}");
    assert_eq!(unchanged, (vec![b, a], vec![b, c]));
    let main = catalog_testing::chain(&store, "main").unwrap();
    assert_eq!(main.len(), 1, "{main:?}");
    assert!(![a, b, c].contains(&main[0]), "{main:?}");
    assert_eq!(catalog_testing::chain(&store, &code).unwrap(), vec![b, c]);
}

#[test]
fn a_replacement_of_layers_that_are_not_consecutive_is_refused() {
    // Arrange: main is [L1, L2, L3]
    let s = Scratch::new("kaban-t6-compact", "non-contiguous");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let ids = catalog_testing::append_and_publish_all(
        &store,
        &[pattern(1, 32), pattern(2, 32), pattern(3, 32)],
    )
    .unwrap();
    let claim = catalog_testing::claim(&store, &[ids[0], ids[2]])
        .unwrap()
        .unwrap();

    // Act: fold L1 and L3 around the surviving L2
    let outcome = catalog_testing::publish(
        &store,
        claim,
        &[pattern(9, 64)],
        &[("main", &[ids[0], ids[2]])],
        &[ids[0], ids[2]],
        &[],
    );

    // Assert: refused, and L3's updates never move before L2
    let (refused, _claim) = *outcome.expect_err("a non-consecutive run was replaced");
    let text = driver_text(&refused);
    assert!(text.contains("not consecutive"), "{text}");
    assert_eq!(catalog_testing::chain(&store, "main").unwrap(), ids);
}

#[test]
fn a_refused_publication_keeps_its_claim_for_release_and_its_inputs_can_be_claimed_again() {
    // Arrange: a claim on A, and a publication that retires B, which it does not hold
    let s = Scratch::new("kaban-t6-compact", "claim-release");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let ids =
        catalog_testing::append_and_publish_all(&store, &[pattern(5, 32), pattern(7, 32)]).unwrap();
    let claim = catalog_testing::claim(&store, &ids[..1]).unwrap().unwrap();
    let (_refused, claim) = *catalog_testing::publish(
        &store,
        claim,
        &[pattern(9, 64)],
        &[("main", &ids[..1])],
        &ids,
        &[],
    )
    .expect_err("an unclaimed extent was retired");

    // Act
    catalog_testing::release(&store, claim).unwrap();
    let again = catalog_testing::claim(&store, &ids[..1]).unwrap();

    // Assert: the refusal rolled the switch back, and the released inputs are free again
    assert_eq!(catalog_testing::chain(&store, "main").unwrap(), ids);
    assert_eq!(again.map(|c| c.inputs()), Some(vec![ids[0]]));
}

/// The test policy: the test class seals at 2 pending rows and folds past 2 layers; a scheduled
/// checkpoint at most once a minute.
fn policy() -> CatalogCfg {
    CatalogCfg {
        seal_ops: SealOps {
            graph: 2,
            ..SealOps::default()
        },
        fold_depth: 2,
        checkpoint_interval_ms: 60_000,
        ..CatalogCfg::default()
    }
}

/// A store of `s`'s database under [`policy`], with the test class installed.
fn open(s: &Scratch) -> SqliteStore {
    let store = SqliteStore::open(&s.db(), &s.keys())
        .unwrap()
        .with_catalog_config(policy())
        .unwrap();
    let _observed = catalog_testing::install_edge_sediment_class(&store);
    store
}

/// A unit budget of `memory_bytes` and [`STEP`].
fn unit(memory_bytes: u64) -> CompactBudget {
    CompactBudget {
        memory_bytes,
        deadline: STEP,
    }
}

/// The bytes of the test-class extents in `main`'s chain, in chain order.
fn chain_bytes(store: &SqliteStore) -> Vec<Vec<u8>> {
    let pin = catalog_testing::pin(store, &budget()).unwrap();
    let view = pin.view();
    view.extents()
        .into_iter()
        .map(|id| view.read(id).unwrap().unwrap())
        .collect()
}

/// Link `from` to `to` on `store`.
fn link(store: &SqliteStore, from: &kaban_core::Node, to: &kaban_core::Node) {
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

#[test]
fn compaction_seals_due_sediment_into_one_extent_of_main() {
    // Arrange: two pending sediment rows, the test class's seal trigger
    let s = Scratch::new("kaban-t6-compact", "seal");
    let store = open(&s);
    let a = created(&store, "Fact", "the net is mended at dawn");
    let b = created(&store, "Fact", "the boat sails at noon");
    link(&store, &a, &b);
    link(&store, &b, &a);
    let rows = catalog_testing::pin_sediment_rows(&store).unwrap();
    assert_eq!(rows.len(), 2, "{rows:?}");

    // Act
    let report = catalog_testing::compact(&store, unit(u64::MAX)).unwrap();

    // Assert: one seal of both rows; main now holds their payloads and no pending row
    assert_eq!(report.skipped, Vec::new(), "{report:?}");
    assert_eq!(report.published.len(), 1, "{report:?}");
    let sealed = &report.published[0];
    assert_eq!(
        (sealed.class, sealed.kind, sealed.sediment_rows),
        ("test", WorkKind::Seal, 2)
    );
    assert!(sealed.switch_hold > Duration::ZERO, "{sealed:?}");
    let expected: Vec<u8> = rows
        .iter()
        .flat_map(|(_, p)| format!("{p}\n").into_bytes())
        .collect();
    assert_eq!(chain_bytes(&store), vec![expected]);
    assert_eq!(
        catalog_testing::pin_sediment_rows(&store).unwrap(),
        Vec::new()
    );
}

#[test]
fn a_publication_reports_how_long_it_held_the_write_transaction() {
    // Arrange
    let s = Scratch::new("kaban-t6-compact", "publish-hold");
    let store = open(&s);

    // Act
    let (_, hold) = catalog_testing::append_and_publish_held(&store, &pattern(1, 64)).unwrap();

    // Assert: some time inside the transaction, far less than the whole test
    assert!(hold > Duration::ZERO && hold < STEP, "{hold:?}");
}

#[test]
fn compaction_folds_a_chain_deeper_than_the_fold_depth() {
    // Arrange: a base and two layers, past the fold depth of 2
    let s = Scratch::new("kaban-t6-compact", "fold");
    let store = open(&s);
    let parts = [pattern(1, 100), pattern(2, 100), pattern(3, 100)];
    let ids = catalog_testing::append_and_publish_all(&store, &parts).unwrap();

    // Act
    let report = catalog_testing::compact(&store, unit(u64::MAX)).unwrap();

    // Assert: one fold of all three into one extent with their bytes in chain order
    assert_eq!(report.skipped, Vec::new(), "{report:?}");
    assert_eq!(report.published.len(), 1, "{report:?}");
    assert_eq!(report.published[0].kind, WorkKind::Fold);
    assert_eq!(report.published[0].extents, ids);
    assert_eq!(chain_bytes(&store), vec![parts.concat()]);
    assert!(catalog_testing::claim(&store, &ids[..1]).unwrap().is_none());
}

#[test]
fn two_compactions_over_the_same_inputs_fold_once_and_the_other_reports_them_skipped() {
    // Arrange: a foldable chain, and two stores of the database; the first stops right after
    // its claim until the second has run
    let s = Scratch::new("kaban-t6-compact", "compact-race");
    let first = open(&s);
    let second = open(&s);
    let ids = catalog_testing::append_and_publish_all(
        &first,
        &[pattern(1, 64), pattern(2, 64), pattern(3, 64)],
    )
    .unwrap();
    let (claimed, on_claim) = std::sync::mpsc::channel();
    let (go, wait) = std::sync::mpsc::channel::<()>();
    let wait = Mutex::new(wait);
    catalog_testing::set_failpoint(
        &first,
        Window::CompactClaimed,
        Some(Arc::new(move || {
            let _ = claimed.send(());
            let _ = wait.lock().unwrap().recv_timeout(STEP);
        })),
    );
    let (done, finished) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = done.send(catalog_testing::compact(&first, unit(u64::MAX)));
    });
    on_claim
        .recv_timeout(STEP)
        .expect("the first compaction never claimed");

    // Act
    let skipped = catalog_testing::compact(&second, unit(u64::MAX)).unwrap();
    go.send(()).unwrap();
    let folded = finished
        .recv_timeout(STEP)
        .expect("the first compaction did not finish")
        .unwrap();

    // Assert: the first folded the inputs; the second published nothing and listed them
    assert_eq!(folded.published.len(), 1, "{folded:?}");
    assert_eq!(folded.published[0].extents, ids);
    assert_eq!(skipped.published, Vec::new(), "{skipped:?}");
    assert_eq!(
        skipped.skipped,
        vec![Skipped {
            class: "test",
            kind: WorkKind::Fold,
            extents: ids,
            sediment_rows: 0,
            reason: SkipReason::Claimed,
        }]
    );
}

/// Whether `reason` is a refusal of the memory ceiling.
fn memory_refused(reason: &SkipReason) -> bool {
    matches!(reason, SkipReason::Budget(e) if e.limit == Limit::Memory)
}

#[test]
fn a_fold_refused_by_its_budget_is_retried_with_fewer_layers() {
    // Arrange: four extents of 1000 bytes; the budget holds two of them, not four
    let s = Scratch::new("kaban-t6-compact", "fold-retry");
    let store = open(&s);
    let parts: Vec<Vec<u8>> = (1..=4).map(|i| pattern(i, 1000)).collect();
    let ids = catalog_testing::append_and_publish_all(&store, &parts).unwrap();

    // Act
    let report = catalog_testing::compact(&store, unit(2500)).unwrap();

    // Assert: the fold of four was refused and listed; the fold of the newest two published
    assert_eq!(report.skipped.len(), 1, "{report:?}");
    assert_eq!(report.skipped[0].extents, ids);
    assert!(memory_refused(&report.skipped[0].reason), "{report:?}");
    assert_eq!(report.published.len(), 1, "{report:?}");
    assert_eq!(report.published[0].extents, ids[2..]);
    let chain = chain_bytes(&store);
    assert_eq!(
        chain,
        vec![parts[0].clone(), parts[1].clone(), parts[2..].concat()]
    );
}

#[test]
fn a_smallest_unit_refused_is_reported_and_its_inputs_stay_claimable() {
    // Arrange: four extents of 1000 bytes; the budget holds less than two
    let s = Scratch::new("kaban-t6-compact", "fold-refused");
    let store = open(&s);
    let parts: Vec<Vec<u8>> = (1..=4).map(|i| pattern(i, 1000)).collect();
    let ids = catalog_testing::append_and_publish_all(&store, &parts).unwrap();

    // Act: no error, whatever the budget refuses
    let report = catalog_testing::compact(&store, unit(1500)).unwrap();

    // Assert: four, then two refused and listed; nothing published, every claim released
    let refused: Vec<Vec<u64>> = report.skipped.iter().map(|k| k.extents.clone()).collect();
    assert_eq!(refused, vec![ids.clone(), ids[2..].to_vec()], "{report:?}");
    assert!(report.skipped.iter().all(|k| memory_refused(&k.reason)));
    assert_eq!(report.published, Vec::new());
    assert_eq!(chain_bytes(&store), parts);
    assert!(catalog_testing::claim(&store, &ids).unwrap().is_some());
    // the one unit refused at its smallest size is recorded for doctor (7.6)
    assert_eq!(store.catalog_status().unwrap().refused_units, 1);
}

#[test]
fn inputs_beyond_the_maintenance_memory_skip_the_pass_instead_of_loading() {
    // Arrange: twenty pending rows, whose copy outweighs a ceiling that one seal of them fits
    let s = Scratch::new("kaban-t6-compact", "pass-ceiling");
    let store = open(&s);
    let hub = created(&store, "Fact", "the lighthouse");
    for i in 0..20 {
        let ship = created(&store, "Fact", &format!("ship number {i} passes"));
        link(&store, &hub, &ship);
    }

    // Act: no error, whatever the ceiling refuses
    let report = catalog_testing::compact(&store, unit(1200)).unwrap();

    // Assert: the pass is reported skipped for its memory, nothing ran or published, and every
    // pending row is still pending
    assert!(
        report
            .pass_skipped
            .is_some_and(|e| e.limit == Limit::Memory),
        "{report:?}"
    );
    assert_eq!(report.published, Vec::new(), "{report:?}");
    assert_eq!(report.skipped, Vec::new(), "{report:?}");
    assert_eq!(catalog_testing::pin_sediment_rows(&store).unwrap().len(), 20);
}

/// A code view's name, every hex digit `c`.
fn code_of(c: char) -> String {
    format!("code@{}", c.to_string().repeat(40))
}

#[test]
fn compaction_retires_the_least_recently_used_views_past_the_retention_but_keeps_a_parent() {
    // Arrange: code views P, A, B and C, last used in that order, C's parent P, a retention of
    // two; A and B share one extent, A holds one of its own
    let s = Scratch::new("kaban-t6-compact", "retention");
    let store = SqliteStore::open(&s.db(), &s.keys())
        .unwrap()
        .with_catalog_config(CatalogCfg {
            view_retention: 2,
            ..policy()
        })
        .unwrap();
    let _observed = catalog_testing::install_edge_sediment_class(&store);
    let (p, a, b, c) = (code_of('1'), code_of('2'), code_of('3'), code_of('4'));
    catalog_testing::append_and_publish_into(&store, &pattern(1, 32), &[&p]).unwrap();
    let shared =
        catalog_testing::append_and_publish_into(&store, &pattern(2, 32), &[&a, &b]).unwrap();
    let own = catalog_testing::append_and_publish_into(&store, &pattern(3, 32), &[&a]).unwrap();
    catalog_testing::append_and_publish_into(&store, &pattern(4, 32), &[&c]).unwrap();
    catalog_testing::corrupt(
        &store,
        &format!(
            "UPDATE cat_view SET last_used = 50 WHERE name = '{p}';
             UPDATE cat_view SET last_used = 100 WHERE name = '{a}';
             UPDATE cat_view SET last_used = 200 WHERE name = '{b}';
             UPDATE cat_view SET last_used = 300, parent = '{p}' WHERE name = '{c}';"
        ),
    )
    .unwrap();

    // Act
    let report = catalog_testing::compact(&store, unit(u64::MAX)).unwrap();

    // Assert: A is retired (needs reimport, its chain gone, its own extent retired); P stays as
    // C's parent; the shared extent stays live in B
    assert_eq!(report.views_retired, vec![a.clone()], "{report:?}");
    let verified = store.verify_catalog().unwrap();
    assert_eq!(verified.needs_reimport, vec![a.clone()], "{verified:?}");
    assert_eq!(verified.chain_inconsistencies, Vec::<String>::new());
    assert_eq!(catalog_testing::chain(&store, &a).unwrap(), Vec::<u64>::new());
    assert_eq!(catalog_testing::chain(&store, &b).unwrap(), vec![shared]);
    assert!(
        catalog_testing::claim(&store, &[own]).unwrap().is_none(),
        "A's own extent is still live"
    );
    assert!(catalog_testing::claim(&store, &[shared]).unwrap().is_some());
}

#[test]
fn a_pin_records_when_it_used_its_view_and_a_label_names_a_view() {
    // Arrange
    let s = Scratch::new("kaban-t6-compact", "last-used");
    let store = open(&s);
    let view = code_of('5');
    catalog_testing::append_and_publish_into(&store, &pattern(1, 32), &[&view]).unwrap();
    let before = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();

    // Act: a pin of the view, then a label
    catalog_testing::chain(&store, &view).unwrap();
    catalog_testing::set_view_label(&store, &view, Some("the feature branch")).unwrap();

    // Assert
    let conn = rusqlite::Connection::open(s.db()).unwrap();
    let (last_used, label): (Option<i64>, Option<String>) = conn
        .query_row(
            "SELECT last_used, label FROM cat_view WHERE name = ?1",
            [&view],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert!(
        last_used.is_some_and(|t| u64::try_from(t).unwrap() >= before),
        "{last_used:?}"
    );
    assert_eq!(label.as_deref(), Some("the feature branch"));
}

/// The `rid`s of a sealed edge payload line (`<op> <src> <dst> <rel>`).
fn rids_of(payload: &str) -> (i64, i64) {
    let mut fields = payload.split_whitespace().skip(1);
    let mut next = || fields.next().unwrap().parse().unwrap();
    (next(), next())
}

#[test]
fn a_purge_rewrites_the_extents_that_hold_a_rid_and_retires_them() {
    // Arrange: three nodes, a -> b and b -> c sealed into one extent
    let s = Scratch::new("kaban-t6-compact", "purge");
    let store = open(&s);
    let a = created(&store, "Fact", "the anchor is raised");
    let b = created(&store, "Fact", "the sail is set");
    let c = created(&store, "Fact", "the harbour falls behind");
    link(&store, &a, &b);
    link(&store, &b, &c);
    let rows = catalog_testing::pin_sediment_rows(&store).unwrap();
    let (rid_a, _) = rids_of(&rows[0].1);
    catalog_testing::compact(&store, unit(u64::MAX)).unwrap();
    let sealed = catalog_testing::pin(&store, &budget())
        .unwrap()
        .view()
        .extents();
    assert_eq!(sealed.len(), 1, "{sealed:?}");

    // Act: a rid no extent holds, then a's
    let untouched = catalog_testing::purge(&store, &[i64::MAX]).unwrap();
    let purged = catalog_testing::purge(&store, &[rid_a]).unwrap();

    // Assert: nothing rewritten for the unknown rid; a's line is gone, b -> c is kept, and the
    // old extent is retired
    assert_eq!(untouched.published, Vec::new(), "{untouched:?}");
    assert_eq!(untouched.skipped, Vec::new(), "{untouched:?}");
    assert_eq!(purged.published.len(), 1, "{purged:?}");
    assert_eq!(purged.published[0].kind, WorkKind::Purge);
    assert_eq!(purged.published[0].extents, sealed);
    assert_eq!(
        chain_bytes(&store),
        vec![format!("{}\n", rows[1].1).into_bytes()]
    );
    assert!(catalog_testing::claim(&store, &sealed).unwrap().is_none());
}

#[test]
fn compaction_ticks_the_checkpoint_only_once_its_interval_elapsed() {
    // Arrange: an interval of a minute
    let s = Scratch::new("kaban-t6-compact", "tick");
    let store = open(&s);
    catalog_testing::append_and_publish(&store, &pattern(1, 64)).unwrap();

    // Act
    let first = catalog_testing::compact(&store, unit(u64::MAX)).unwrap();
    let second = catalog_testing::compact(&store, unit(u64::MAX)).unwrap();

    // Assert: the first compaction ticked, the second, within the interval, did not
    assert!(first.checkpoint.is_some(), "{first:?}");
    assert_eq!(second.checkpoint, None, "{second:?}");
}
