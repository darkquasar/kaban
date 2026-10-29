//! Effective budgets and the five-row default matrix.
#![allow(clippy::unwrap_used)]

use kaban_api::layers::{
    BudgetRow, BudgetSource, LayerOutcome, LayerRegistry, MatrixLayer, RunFlags, budgets,
};
use kaban_api::{DoctorContext, DoctorReport, KabanApi};
use kaban_core::config::EngineConfig;
use kaban_core::{DatabaseFileModes, IngestStore};
use kaban_core::{IndexMeta, Layer};
use kaban_policy::Policy;
use kaban_store_sqlite::{LATEST_SCHEMA_VERSION, SqliteStore};
use std::sync::Arc;

#[test]
fn defaults_overrides_and_flag_provenance() {
    let mut cfg = EngineConfig::default();
    let defaults = budgets(&cfg, &RunFlags::default());
    assert_eq!(defaults.len(), 8);
    assert!(
        defaults
            .iter()
            .all(|row| row.source == BudgetSource::Default
                && !row.cap_lifted
                && row.default == row.effective)
    );
    cfg.budget.l0_rss_mb += 1;
    cfg.gazetteer.max_automaton_mb += 1;
    cfg.embed.max_scan_rss_mb += 1;
    cfg.ingest.lru_mib += 1;
    cfg.index.deadline_secs += 1;
    cfg.index.quota.file_bytes = u64::MAX;
    cfg.index.quota.run_dir_bytes = u64::MAX - 1;
    cfg.index.quota.tmp_bytes = u64::MAX - 2;
    let overrides = budgets(&cfg, &RunFlags::default());
    assert!(
        overrides
            .iter()
            .all(|row| row.source == BudgetSource::Config && !row.cap_lifted)
    );
    assert_eq!(overrides[5].effective, u64::MAX);
    assert_eq!(overrides[6].effective, u64::MAX - 1);
    assert_eq!(overrides[7].effective, u64::MAX - 2);
    let forced = budgets(&cfg, &RunFlags { symbols: true });
    for (actual, prior) in forced.iter().zip(overrides) {
        assert_eq!(actual.effective, prior.effective);
        assert_eq!(actual.default, prior.default);
        if actual.key == "gazetteer.max_automaton_mb" {
            assert_eq!(actual.source, BudgetSource::Flag("--symbols"));
            assert!(actual.cap_lifted);
        } else {
            assert_eq!(*actual, prior);
        }
        let wire = BudgetRow::from(*actual);
        let encoded = serde_json::to_string(&wire).unwrap();
        assert_eq!(serde_json::from_str::<BudgetRow>(&encoded).unwrap(), wire);
    }
}

#[test]
fn default_matrix_is_four_rows_and_extractors_stay_explicit() {
    let mut cfg = EngineConfig::default();
    let rows = LayerRegistry.enabled(&cfg);
    assert_eq!(
        rows.iter().map(|row| row.layer).collect::<Vec<_>>(),
        [
            MatrixLayer::Fts,
            MatrixLayer::Gazetteer,
            MatrixLayer::Embed,
            MatrixLayer::Extractors
        ]
    );
    assert!(rows[0].on);
    assert!(!rows[1].on);
    assert!(!rows[2].on);
    assert!(!rows[3].on);
    assert_eq!(
        rows[3].producer_layers,
        [Layer::Llm, Layer::Gliner, Layer::Script]
    );
    cfg.embed.model = Some(std::collections::BTreeMap::from([(
        "name".to_owned(),
        "reserved-model".to_owned(),
    )]));
    assert!(!LayerRegistry.enabled(&cfg)[2].on);
    assert_eq!(
        LayerRegistry.enabled(&cfg)[2].reason_off.as_deref(),
        Some("not_available_in_this_release")
    );
    assert!(!LayerRegistry.enabled(&cfg)[3].on);
}

#[test]
fn valid_latest_runs_survive_malformed_siblings() {
    let mut meta = IndexMeta::default();
    meta.entries.insert("layer_run:fts".into(), "12".into());
    meta.entries.insert(
        "layer_run:gazetteer".into(),
        r#"{"at":20,"outcome":"failed"}"#.into(),
    );
    meta.entries
        .insert("layer_run:embed".into(), "malformed".into());
    meta.entries
        .insert("layer_run:llm".into(), r#"{"at":30,"outcome":"ok"}"#.into());
    meta.entries.insert(
        "layer_run:gliner".into(),
        r#"{"at":31,"outcome":"skipped_budget"}"#.into(),
    );
    meta.entries.insert(
        "layer_run:script".into(),
        r#"{"at":99,"outcome":"arbitrary"}"#.into(),
    );
    let rows = LayerRegistry.with_runs(&EngineConfig::default(), &meta);
    assert_eq!(rows[0].last_run, Some(12));
    assert_eq!(rows[0].last_outcome, Some(LayerOutcome::Ok));
    assert_eq!(rows[1].last_outcome, Some(LayerOutcome::Failed));
    assert_eq!(rows[2].last_run, None);
    assert_eq!(rows[3].last_run, Some(31));
    assert_eq!(rows[3].last_outcome, Some(LayerOutcome::SkippedBudget));
    assert!(!rows[3].on);
}

#[test]
fn doctor_reports_skipped_layer_without_mutating_metadata_or_hiding_siblings() {
    let store = Arc::new(SqliteStore::open_memory().unwrap());
    store
        .set_index_meta(
            "layer_run:gazetteer",
            r#"{"at":42,"outcome":"skipped_budget"}"#,
        )
        .unwrap();
    store
        .set_index_meta("layer_run:embed", "malformed")
        .unwrap();
    let before = store.index_meta().unwrap().entries;
    let signer = store.signer();
    let api = KabanApi::new(store.clone(), Policy::empty(), signer, None);
    let report = api.doctor(&DoctorContext {
        files: DatabaseFileModes {
            db: DatabaseFileModes::OWNER_ONLY,
            wal: None,
            shm: None,
        },
        schema_version: LATEST_SCHEMA_VERSION,
        keys_dir: std::env::temp_dir().join("kaban-layer-diagnostic-no-keys"),
        vacuum: false,
        priors: false,
    });
    let layer = report
        .checks
        .iter()
        .find(|check| check.name == "layer_gazetteer")
        .unwrap();
    assert!(!layer.ok);
    assert_eq!(layer.detail, "skipped_budget");
    assert_eq!(
        report
            .checks
            .iter()
            .filter(|check| check.name.starts_with("layer_"))
            .count(),
        4
    );
    assert_eq!(report.budgets.len(), 8);
    assert_eq!(before, store.index_meta().unwrap().entries);
    let legacy = serde_json::json!({"ok":true,"checks":[]});
    assert_eq!(
        serde_json::from_value::<DoctorReport>(legacy)
            .unwrap()
            .budgets,
        Vec::new()
    );
}
