//! `stats` v3 (core-spec F6; core-plan C3.6 as amended by M3-26): a superset of the spike's
//! counters with the engine, the schema version, the conflicts count, the stores list and the
//! policy's effective hash beside its file hash.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

use kaban_api::{ApiConfig, KabanApi, RememberRequest, StatsRequest, StatsResponse};
use kaban_core::IngestStore;
use kaban_core::config::EngineConfig;
use kaban_policy::Policy;
use kaban_store_sqlite::SqliteStore;
use serde_json::{Value, json};
use std::sync::Arc;

#[test]
fn stats_is_a_superset_with_engine_schema_version_conflicts_stores_and_effective_hash() {
    let policy = Policy::from_file(Policy::parse(b"").unwrap(), "f".repeat(64)).unwrap();
    let effective = policy.effective_hash().to_owned();
    let store = Arc::new(SqliteStore::open_memory().unwrap());
    let signer = store.signer();
    let api = KabanApi::new(store, policy, signer, None);
    api.remember(RememberRequest {
        kind: "Fact".into(),
        text: "quokka counted".into(),
        ..Default::default()
    })
    .unwrap();

    let v: Value = serde_json::to_value(api.stats(StatsRequest::default()).unwrap()).unwrap();
    // the spike's counters stay
    assert_eq!(v["nodes"], json!(1), "{v}");
    assert_eq!(v["by_kind"]["Fact"], json!(1));
    assert!(v["db_bytes"].as_u64().is_some_and(|b| b > 0));
    // EN-E10b: `changes_lag` with its deprecated alias `outbox_lag` (same value) until 3.2
    assert_eq!(v["changes_lag"], json!(0));
    assert_eq!(v["outbox_lag"], json!(0));
    assert_eq!(v["scope"]["changes_lag"], json!("global"), "{v}");
    assert_eq!(
        v["policy"]["version"],
        json!("f".repeat(64)),
        "file hash kept"
    );
    // the v3 superset
    assert_eq!(v["engine"], json!("sqlite"), "{v}");
    assert_eq!(
        v["schema_version"],
        json!(kaban_store_sqlite::LATEST_SCHEMA_VERSION),
        "{v}"
    );
    assert_eq!(v["conflicts"], json!(0), "{v}");
    assert_eq!(v["stores"], json!(["project"]), "{v}");
    assert_eq!(v["layers"].as_array().unwrap().len(), 4);
    assert_eq!(v["layers"][0]["layer"], "fts");
    assert_eq!(v["layers"][0]["on"], true);
    assert_eq!(v["layers"][2]["on"], false);
    assert_eq!(v["layers"][3]["on"], false);
    let mut legacy = v.clone();
    legacy.as_object_mut().unwrap().remove("layers");
    assert_eq!(
        serde_json::from_value::<StatsResponse>(legacy)
            .unwrap()
            .layers,
        Vec::new()
    );
    assert_eq!(v["policy"]["effective_hash"], json!(effective), "{v}");
    assert_ne!(
        effective,
        "f".repeat(64),
        "effective hash covers rules, not file bytes"
    );
}

#[test]
fn stats_reports_configured_layers_and_isolates_bad_run_metadata() {
    let store = Arc::new(SqliteStore::open_memory().unwrap());
    store
        .set_index_meta("layer_run:gazetteer", r#"{"at":42,"outcome":"failed"}"#)
        .unwrap();
    store
        .set_index_meta("layer_run:embed", "malformed")
        .unwrap();
    let signer = store.signer();
    let engine = EngineConfig::default();
    let api = KabanApi::with_config(
        store,
        Policy::from_file(Policy::parse(b"").unwrap(), "f".repeat(64)).unwrap(),
        signer,
        None,
        ApiConfig {
            engine: Arc::new(engine),
            ..ApiConfig::default()
        },
    );
    let stats = api.stats(StatsRequest::default()).unwrap();
    assert_eq!(stats.layers.len(), 4);
    assert!(stats.layers[0].on);
    assert!(!stats.layers[1].on);
    assert_eq!(stats.layers[1].last_run, Some(42));
    assert_eq!(
        stats.layers[1].last_outcome,
        Some(kaban_api::layers::LayerOutcome::Failed)
    );
    assert!(!stats.layers[2].on);
    assert_eq!(stats.layers[2].last_run, None);
    assert!(!stats.layers[3].on);
}

#[test]
fn matrix_enum_strings_remain_content_scanned_at_the_response_gate() {
    let policy = Policy::from_file(
        Policy::parse(b"words:\n  - {term: gazetteer, action: reject, id: matrix-content}\n")
            .unwrap(),
        "f".repeat(64),
    )
    .unwrap();
    let store = Arc::new(SqliteStore::open_memory().unwrap());
    let signer = store.signer();
    let api = KabanApi::new(store, policy, signer, None);
    let value = serde_json::to_value(api.stats(StatsRequest::default()).unwrap()).unwrap();
    assert!(matches!(
        api.gate_response("stats", &value),
        Err(kaban_api::ApiError::Rejected { .. })
    ));
}
