//! Effective budgets grouped under their producer or index, and the seven producer rows
//! (PC-16).
#![allow(clippy::unwrap_used)]

use kaban_api::producers::{
    BudgetRow, BudgetSource, ProducerOutcome, ProducerRegistry, ProducerRow, RunFlags, budgets,
};
use kaban_api::{DoctorContext, DoctorReport, KabanApi};
use kaban_core::IndexMeta;
use kaban_core::config::EngineConfig;
use kaban_core::{DatabaseFileModes, IngestStore};
use kaban_policy::Policy;
use kaban_store_sqlite::{LATEST_SCHEMA_VERSION, SqliteStore};
use std::sync::Arc;

#[test]
fn defaults_overrides_and_flag_provenance() {
    let mut cfg = EngineConfig::default();
    let defaults = budgets(&cfg, &RunFlags::default());
    assert_eq!(defaults.len(), 7);
    assert!(
        defaults
            .iter()
            .all(|row| row.source == BudgetSource::Default
                && !row.cap_lifted
                && row.default == row.effective)
    );
    cfg.text.build_rss_mb += 1;
    cfg.gazetteer.max_automaton_mb += 1;
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
    assert_eq!(overrides[4].effective, u64::MAX);
    assert_eq!(overrides[5].effective, u64::MAX - 1);
    assert_eq!(overrides[6].effective, u64::MAX - 2);
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
fn seven_producer_rows_with_their_budgets_and_extractors_stay_explicit() {
    let cfg = EngineConfig::default();
    let rows = ProducerRegistry.enabled(&cfg);
    let names: Vec<&str> = rows.iter().map(|row| row.producer.as_str()).collect();
    assert_eq!(
        names,
        [
            "authored",
            "hook_events",
            "document_ingest",
            "gazetteer",
            "extractor",
            "scip_import",
            "git_history"
        ]
    );
    let extractor = &rows[4];
    assert!(!extractor.on);
    assert_eq!(
        extractor.reason_off.as_deref(),
        Some("explicit_selection_only")
    );
    let keys = |row: &ProducerRow| -> Vec<String> {
        row.budgets
            .iter()
            .map(|budget| budget.key.clone())
            .collect()
    };
    assert_eq!(keys(&rows[3]), ["gazetteer.max_automaton_mb"]);
    assert_eq!(keys(&rows[2]), ["ingest.lru_mib"]);
    assert_eq!(keys(&rows[5]).len(), 4, "the SCIP deadline and quotas");
    assert_eq!(keys(&rows[0]), Vec::<String>::new());
}

#[test]
fn valid_latest_runs_survive_malformed_siblings() {
    // a bare integer is not a run record: no compatibility reading of the old bookkeeping
    let mut meta = IndexMeta::default();
    meta.entries
        .insert("producer_run:authored".into(), "12".into());
    meta.entries.insert(
        "producer_run:gazetteer".into(),
        r#"{"at":20,"outcome":"failed"}"#.into(),
    );
    meta.entries
        .insert("producer_run:hook_events".into(), "malformed".into());
    meta.entries.insert(
        "producer_run:extractor:llm".into(),
        r#"{"at":30,"outcome":"ok"}"#.into(),
    );
    meta.entries.insert(
        "producer_run:extractor:gliner".into(),
        r#"{"at":31,"outcome":"skipped_budget"}"#.into(),
    );
    meta.entries.insert(
        "producer_run:extractor:script".into(),
        r#"{"at":99,"outcome":"arbitrary"}"#.into(),
    );
    let rows = ProducerRegistry.with_runs(&EngineConfig::default(), &meta);
    assert_eq!(rows[0].last_run, None);
    assert_eq!(rows[0].last_outcome, None);
    assert_eq!(rows[1].last_run, None);
    assert_eq!(rows[3].last_outcome, Some(ProducerOutcome::Failed));
    assert_eq!(rows[4].last_run, Some(31));
    assert_eq!(rows[4].last_outcome, Some(ProducerOutcome::SkippedBudget));
    assert!(!rows[4].on);
}

#[test]
fn doctor_reports_skipped_layer_without_mutating_metadata_or_hiding_siblings() {
    let store = Arc::new(SqliteStore::open_memory().unwrap());
    store
        .set_index_meta(
            "producer_run:gazetteer",
            r#"{"at":42,"outcome":"skipped_budget"}"#,
        )
        .unwrap();
    store
        .set_index_meta("producer_run:hook_events", "malformed")
        .unwrap();
    let before = store.index_meta().unwrap().entries;
    let signer = store.signer();
    let api = KabanApi::new(store.clone(), Policy::empty(), signer);
    let report = api.doctor(&DoctorContext {
        files: DatabaseFileModes {
            db: DatabaseFileModes::OWNER_ONLY,
            wal: None,
            shm: None,
        },
        schema_version: LATEST_SCHEMA_VERSION,
        keys_dir: std::env::temp_dir().join("kaban-layer-diagnostic-no-keys"),
        vacuum: false,
        allocator_adapter: None,
    });
    let layer = report
        .checks
        .iter()
        .find(|check| check.name == "producer_gazetteer")
        .unwrap();
    assert!(!layer.ok);
    assert_eq!(layer.detail, "skipped_budget");
    assert_eq!(
        report
            .checks
            .iter()
            .filter(|check| check.name.starts_with("producer_"))
            .count(),
        7
    );
    assert_eq!(report.budgets.len(), 7);
    assert_eq!(before, store.index_meta().unwrap().entries);
    let legacy = serde_json::json!({"ok":true,"checks":[]});
    assert_eq!(
        serde_json::from_value::<DoctorReport>(legacy)
            .unwrap()
            .budgets,
        Vec::new()
    );
}
