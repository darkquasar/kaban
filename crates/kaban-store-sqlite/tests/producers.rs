//! Producer-run bookkeeping uses existing metadata and leaves records/indexes alone.
#![allow(clippy::unwrap_used)]

use kaban_core::{IngestStore, MemoryStore, Producer};
use kaban_store_sqlite::{ProducerRunOutcome, ProducerRunRecord, SqliteStore};

mod common;

#[test]
fn bookkeeping_creates_no_optional_tables() {
    let scratch = common::Scratch::new("kaban-layer-runs", "schema");
    let store = SqliteStore::open(&scratch.db(), &scratch.keys()).unwrap();
    let connection = rusqlite::Connection::open(scratch.db()).unwrap();
    let tables = || {
        let mut stmt = connection
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .unwrap();
        stmt.query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    };
    let before = tables();
    assert!(
        !before
            .iter()
            .any(|name| name == "node_fts_tri" || name == "embedding")
    );
    store
        .record_producer_run(Producer::Gazetteer, 42, ProducerRunOutcome::Ok)
        .unwrap();
    assert!(store.producer_run(Producer::Gazetteer).unwrap().is_some());
    assert_eq!(tables(), before);
}

#[test]
fn fixed_runs_round_trip_without_changing_counters() {
    let store = SqliteStore::open_memory().unwrap();
    let before = store.stats(None).unwrap();
    assert_eq!(store.producer_run(Producer::Gazetteer).unwrap(), None);
    store
        .record_producer_run(Producer::Gazetteer, 42, ProducerRunOutcome::Failed)
        .unwrap();
    assert_eq!(
        store.producer_run(Producer::Gazetteer).unwrap(),
        Some(ProducerRunRecord {
            at: 42,
            outcome: ProducerRunOutcome::Failed
        })
    );
    store
        .record_producer_run(Producer::DocumentIngest, 43, ProducerRunOutcome::Ok)
        .unwrap();
    store
        .record_producer_run(Producer::HookEvents, 44, ProducerRunOutcome::SkippedBudget)
        .unwrap();
    assert_eq!(
        store
            .producer_run(Producer::DocumentIngest)
            .unwrap()
            .unwrap()
            .at,
        43
    );
    assert_eq!(
        store
            .producer_run(Producer::HookEvents)
            .unwrap()
            .unwrap()
            .outcome,
        ProducerRunOutcome::SkippedBudget
    );
    assert_eq!(store.stats(None).unwrap(), before);
}

#[test]
fn malformed_and_negative_runs_are_ignored_without_writes() {
    let store = SqliteStore::open_memory().unwrap();
    for value in [
        "malformed",
        "-1",
        r#"{"at":4,"outcome":"arbitrary"}"#,
        r#"{"at":4,"outcome":"ok","message":"caller text"}"#,
    ] {
        store
            .set_index_meta("producer_run:gazetteer", value)
            .unwrap();
        let before = store.index_meta().unwrap().entries;
        assert_eq!(store.producer_run(Producer::Gazetteer).unwrap(), None);
        assert_eq!(store.index_meta().unwrap().entries, before);
    }
    let before = store.index_meta().unwrap().entries;
    assert!(
        store
            .record_producer_run(Producer::Gazetteer, -5, ProducerRunOutcome::Ok)
            .is_err()
    );
    assert_eq!(store.index_meta().unwrap().entries, before);
    // the record is the JSON object only: a bare integer (the old bookkeeping) is no run
    store
        .set_index_meta("producer_run:gazetteer", "12")
        .unwrap();
    assert_eq!(store.producer_run(Producer::Gazetteer).unwrap(), None);
}
