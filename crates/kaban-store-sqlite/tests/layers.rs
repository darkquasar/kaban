//! Layer-run bookkeeping uses existing metadata and leaves records/indexes alone.
#![allow(clippy::unwrap_used)]

use kaban_core::{IngestStore, Layer, MemoryStore};
use kaban_store_sqlite::{LayerRunOutcome, LayerRunRecord, SqliteStore};

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
        .record_layer_run(Layer::Gazetteer, 42, LayerRunOutcome::Ok)
        .unwrap();
    assert!(store.layer_run(Layer::Gazetteer).unwrap().is_some());
    assert_eq!(tables(), before);
}

#[test]
fn fixed_runs_round_trip_without_changing_counters() {
    let store = SqliteStore::open_memory().unwrap();
    let before = store.stats(None).unwrap();
    assert_eq!(store.layer_run(Layer::Gazetteer).unwrap(), None);
    store
        .record_layer_run(Layer::Gazetteer, 42, LayerRunOutcome::Failed)
        .unwrap();
    assert_eq!(
        store.layer_run(Layer::Gazetteer).unwrap(),
        Some(LayerRunRecord {
            at: 42,
            outcome: LayerRunOutcome::Failed
        })
    );
    store
        .record_layer_run(Layer::Fts, 43, LayerRunOutcome::Ok)
        .unwrap();
    store
        .record_layer_run(Layer::Embed, 44, LayerRunOutcome::SkippedBudget)
        .unwrap();
    assert_eq!(store.layer_run(Layer::Fts).unwrap().unwrap().at, 43);
    assert_eq!(
        store.layer_run(Layer::Embed).unwrap().unwrap().outcome,
        LayerRunOutcome::SkippedBudget
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
        store.set_index_meta("layer_run:gazetteer", value).unwrap();
        let before = store.index_meta().unwrap().entries;
        assert_eq!(store.layer_run(Layer::Gazetteer).unwrap(), None);
        assert_eq!(store.index_meta().unwrap().entries, before);
    }
    let before = store.index_meta().unwrap().entries;
    assert!(
        store
            .record_layer_run(Layer::Gazetteer, -5, LayerRunOutcome::Ok)
            .is_err()
    );
    assert_eq!(store.index_meta().unwrap().entries, before);
    store.set_index_meta("layer_run:gazetteer", "12").unwrap();
    assert_eq!(
        store.layer_run(Layer::Gazetteer).unwrap(),
        Some(LayerRunRecord {
            at: 12,
            outcome: LayerRunOutcome::Ok
        })
    );
}
