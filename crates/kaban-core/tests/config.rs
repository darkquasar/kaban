//! The one `EngineConfig` and its loader (engine-spec §15.1; engine-plan E1.1, SO-11).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

use std::path::PathBuf;

use kaban_core::ValidatorLimits;
use kaban_core::config::{ConfigError, EngineConfig, load};

#[test]
fn claim_defaults_and_nonzero_bounds_are_registered() {
    let cfg = EngineConfig::default();
    assert_eq!(
        (
            cfg.ingest.claim_ttl_s,
            cfg.ingest.claim_max_attempts,
            cfg.ingest.claim_scan_max
        ),
        (300, 3, 1000)
    );
    for field in ["claim_ttl_s", "claim_max_attempts", "claim_scan_max"] {
        let path = std::env::temp_dir().join(format!(
            "kaban-claim-config-{}-{field}.yaml",
            std::process::id()
        ));
        std::fs::write(&path, format!("ingest:\n  {field}: 0\n")).unwrap();
        assert!(matches!(load(&path), Err(ConfigError::Range { .. })));
        std::fs::write(&path, format!("ingest:\n  {field}: 1\n")).unwrap();
        assert!(load(&path).is_ok());
        std::fs::remove_file(path).unwrap();
    }
}

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/config")
        .join(name)
}

#[test]
fn valid_config_fixtures_round_trip_without_changing_effective_values() {
    for name in [
        "defaults.yaml",
        "override_one.yaml",
        "maps_replace.yaml",
        "index_overrides.yaml",
    ] {
        let text = std::fs::read_to_string(fixture(name)).unwrap();
        let value: yaml_serde::Value = yaml_serde::from_str(&text).unwrap();
        let rendered = yaml_serde::to_string(&value).unwrap();
        let reparsed: yaml_serde::Value = yaml_serde::from_str(&rendered).unwrap();
        assert_eq!(reparsed, value, "{name}");
        assert_eq!(
            yaml_serde::to_string(&reparsed).unwrap(),
            rendered,
            "{name}"
        );
        let cfg: EngineConfig = yaml_serde::from_str(&rendered).unwrap();
        assert_eq!(cfg, load(&fixture(name)).unwrap(), "{name}");
    }
    // Comments are not emitted, but the one-key fixture's persisted YAML shape is unchanged.
    let value: yaml_serde::Value =
        yaml_serde::from_str(include_str!("../fixtures/config/override_one.yaml")).unwrap();
    assert_eq!(
        yaml_serde::to_string(&value).unwrap(),
        "ontology:\n  pattern_size_limit_kib: 2048\n"
    );
}

#[test]
fn malformed_yaml_config_is_a_parse_error_without_echoing_input() {
    let error = load(&fixture("malformed_yaml.yaml")).unwrap_err();
    assert!(matches!(error, ConfigError::Parse { ref path } if path.is_empty()));
    assert!(!error.to_string().contains("unfinished"));
}

#[test]
fn one_changed_key_loads_and_is_the_only_effective_override() {
    let cfg = load(&fixture("override_one.yaml")).unwrap();
    assert_eq!(cfg.ontology.pattern_size_limit_kib, 2048);
    assert_eq!(
        cfg.effective_overrides(),
        vec![(
            "ontology.pattern_size_limit_kib".to_owned(),
            "2048".to_owned()
        )]
    );
    // the validator limits come from the section
    assert_eq!(
        ValidatorLimits::from(&cfg.ontology),
        ValidatorLimits {
            pattern_size_limit_kib: 2048
        }
    );
    let _: Result<EngineConfig, ConfigError> = Ok(cfg);
}

#[test]
fn an_absent_file_and_a_file_spelling_every_register_default_both_equal_default() {
    let absent = load(&fixture("does-not-exist.yaml")).unwrap();
    assert_eq!(absent, EngineConfig::default());
    assert_eq!(absent.effective_overrides(), Vec::new());
    let spelled = load(&fixture("defaults.yaml")).unwrap();
    assert_eq!(spelled, EngineConfig::default());
    assert_eq!(spelled.effective_overrides(), Vec::new());
    // the §15.1 register, key by key (the table the plan asks for)
    let d = EngineConfig::default();
    let table: Vec<(&str, String)> = vec![
        ("reconcile.tau_type", d.reconcile.tau_type.to_string()),
        ("reconcile.tau_id", d.reconcile.tau_id.to_string()),
        ("reconcile.priors", format!("{:?}", d.reconcile.priors)),
        ("discovered.cap", d.discovered.cap.to_string()),
        (
            "discovered.prune_after_days",
            d.discovered.prune_after_days.to_string(),
        ),
        (
            "discovered.promote_when.min_count",
            d.discovered.promote_when.min_count.to_string(),
        ),
        (
            "discovered.promote_when.min_sources",
            d.discovered.promote_when.min_sources.to_string(),
        ),
        (
            "discovered.promote_when.min_mean_score",
            d.discovered.promote_when.min_mean_score.to_string(),
        ),
        (
            "ontology.pattern_size_limit_kib",
            d.ontology.pattern_size_limit_kib.to_string(),
        ),
        (
            "ontology.golden_strict",
            d.ontology.golden_strict.to_string(),
        ),
        (
            "gazetteer.min_pattern_len",
            d.gazetteer.min_pattern_len.to_string(),
        ),
        (
            "gazetteer.common_word_max_len",
            d.gazetteer.common_word_max_len.to_string(),
        ),
        (
            "gazetteer.corpus_freq_cutoff",
            d.gazetteer.corpus_freq_cutoff.to_string(),
        ),
        ("gazetteer.max_targets", d.gazetteer.max_targets.to_string()),
        (
            "gazetteer.max_automaton_mb",
            d.gazetteer.max_automaton_mb.to_string(),
        ),
        (
            "gazetteer.max_chunk_ms",
            d.gazetteer.max_chunk_ms.to_string(),
        ),
        ("gazetteer.v4_symbols", d.gazetteer.v4_symbols.to_string()),
        ("embed.model", format!("{:?}", d.embed.model)),
        ("embed.dtype", d.embed.dtype.clone()),
        ("embed.knn_k", d.embed.knn_k.to_string()),
        ("embed.tau.default", d.embed.tau["default"].to_string()),
        (
            "embed.scan_window_rows",
            d.embed.scan_window_rows.to_string(),
        ),
        ("embed.max_scan_rss_mb", d.embed.max_scan_rss_mb.to_string()),
        (
            "embed.min_topk_overlap",
            d.embed.min_topk_overlap.to_string(),
        ),
        ("l3.holdout_min_f1", d.l3.holdout_min_f1.to_string()),
        ("l3.holdout_min_spans", d.l3.holdout_min_spans.to_string()),
        ("recall.fusion", d.recall.fusion.clone()),
        ("recall.rrf_k", d.recall.rrf_k.to_string()),
        (
            "recall.judged_min_queries",
            d.recall.judged_min_queries.to_string(),
        ),
        (
            "recall.column_weights.text",
            d.recall.column_weights["text"].to_string(),
        ),
        (
            "recall.column_weights.summary",
            d.recall.column_weights["summary"].to_string(),
        ),
        (
            "recall.column_weights.tags",
            d.recall.column_weights["tags"].to_string(),
        ),
        (
            "recall.column_weights.ident_terms",
            d.recall.column_weights["ident_terms"].to_string(),
        ),
        ("ask.template_depth", d.ask.template_depth.to_string()),
        (
            "ask.template_depth_max",
            d.ask.template_depth_max.to_string(),
        ),
        ("er.m.title", d.er.m["title"].to_string()),
        ("er.u.title", d.er.u["title"].to_string()),
        ("er.m.anchors", d.er.m["anchors"].to_string()),
        ("er.u.anchors", d.er.u["anchors"].to_string()),
        ("er.match_threshold", d.er.match_threshold.to_string()),
        ("er.possible_threshold", d.er.possible_threshold.to_string()),
        ("ingest.finish_cap", d.ingest.finish_cap.to_string()),
        ("ingest.retention_days", d.ingest.retention_days.to_string()),
        ("ingest.claim_ttl_s", d.ingest.claim_ttl_s.to_string()),
        (
            "ingest.claim_max_attempts",
            d.ingest.claim_max_attempts.to_string(),
        ),
        ("ingest.claim_scan_max", d.ingest.claim_scan_max.to_string()),
        (
            "ingest.metadata_allowance_kib",
            d.ingest.metadata_allowance_kib.to_string(),
        ),
        ("ingest.lru_entries", d.ingest.lru_entries.to_string()),
        ("ingest.lru_mib", d.ingest.lru_mib.to_string()),
        (
            "ingest.default_with",
            format!("{:?}", d.ingest.default_with),
        ),
        (
            "pipeline.retire_batch_rows",
            d.pipeline.retire_batch_rows.to_string(),
        ),
        ("link.default", d.link.default.clone()),
        (
            "briefing.max_candidates",
            d.briefing.max_candidates.to_string(),
        ),
        ("budget.l0_rss_mb", d.budget.l0_rss_mb.to_string()),
        ("bench.min_samples", d.bench.min_samples.to_string()),
        (
            "bench.why_regression_pct",
            d.bench.why_regression_pct.to_string(),
        ),
        ("why.anchor_prefilter", d.why.anchor_prefilter.to_string()),
    ];
    let expected = [
        ("reconcile.tau_type", "0.3"),
        ("reconcile.tau_id", "0.4"),
        ("reconcile.priors", "None"),
        ("discovered.cap", "500"),
        ("discovered.prune_after_days", "90"),
        ("discovered.promote_when.min_count", "20"),
        ("discovered.promote_when.min_sources", "3"),
        ("discovered.promote_when.min_mean_score", "0.7"),
        ("ontology.pattern_size_limit_kib", "1024"),
        ("ontology.golden_strict", "false"),
        ("gazetteer.min_pattern_len", "3"),
        ("gazetteer.common_word_max_len", "4"),
        ("gazetteer.corpus_freq_cutoff", "0.05"),
        ("gazetteer.max_targets", "5"),
        ("gazetteer.max_automaton_mb", "25"),
        ("gazetteer.max_chunk_ms", "1"),
        ("gazetteer.v4_symbols", "true"),
        ("embed.model", "None"),
        ("embed.dtype", "int8"),
        ("embed.knn_k", "5"),
        ("embed.tau.default", "0.8"),
        ("embed.scan_window_rows", "4096"),
        ("embed.max_scan_rss_mb", "32"),
        ("embed.min_topk_overlap", "0.9"),
        ("l3.holdout_min_f1", "0.5"),
        ("l3.holdout_min_spans", "200"),
        ("recall.fusion", "rrf(60)"),
        ("recall.rrf_k", "60"),
        ("recall.judged_min_queries", "100"),
        ("recall.column_weights.text", "1"),
        ("recall.column_weights.summary", "1.5"),
        ("recall.column_weights.tags", "1"),
        ("recall.column_weights.ident_terms", "2"),
        ("ask.template_depth", "1"),
        ("ask.template_depth_max", "3"),
        ("er.m.title", "0.9"),
        ("er.u.title", "0.01"),
        ("er.m.anchors", "0.95"),
        ("er.u.anchors", "0.001"),
        ("er.match_threshold", "8"),
        ("er.possible_threshold", "3"),
        ("ingest.finish_cap", "10000"),
        ("ingest.retention_days", "7"),
        ("ingest.claim_ttl_s", "300"),
        ("ingest.claim_max_attempts", "3"),
        ("ingest.claim_scan_max", "1000"),
        ("ingest.metadata_allowance_kib", "4"),
        ("ingest.lru_entries", "4"),
        ("ingest.lru_mib", "8"),
        ("ingest.default_with", "None"),
        ("pipeline.retire_batch_rows", "1000"),
        ("link.default", "none"),
        ("briefing.max_candidates", "5"),
        ("budget.l0_rss_mb", "64"),
        ("bench.min_samples", "30"),
        ("bench.why_regression_pct", "10"),
        ("why.anchor_prefilter", "true"),
    ];
    assert_eq!(table.len(), expected.len());
    for ((key, got), (ekey, want)) in table.iter().zip(expected.iter()) {
        assert_eq!(key, ekey);
        assert_eq!(got, want, "{key}");
    }
    assert_eq!(d.er.m.len(), 7);
    assert_eq!(d.er.u.len(), 7);
}

#[test]
fn an_unknown_key_at_any_depth_fails_parse_with_the_field_path_and_never_the_value() {
    let cases = [
        ("unknown_key.yaml", "ontology.pattern_size_limit_kb"),
        ("unknown_key_depth3.yaml", "discovered.promote_when.min_cnt"),
        ("unknown_section.yaml", "ontologies"),
        ("wrong_type.yaml", "embed.tau.Decision"),
        ("not_a_mapping.yaml", ""),
    ];
    for (file, path) in cases {
        let err = load(&fixture(file)).unwrap_err();
        assert_eq!(
            err,
            ConfigError::Parse {
                path: path.to_owned()
            },
            "{file}"
        );
        let text = err.to_string();
        assert!(!text.contains("2048"), "{file}: {text}");
        assert!(!text.contains("preferred"), "{file}: {text}");
        assert!(!text.contains("secret"), "{file}: {text}");
    }
}

#[test]
fn an_opt_in_budget_of_zero_fails_range_and_a_directory_fails_io_by_class() {
    assert_eq!(
        load(&fixture("budget_zero.yaml")).unwrap_err(),
        ConfigError::Range {
            path: "gazetteer.max_automaton_mb".to_owned()
        }
    );
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/config");
    let err = load(&dir).unwrap_err();
    assert!(matches!(err, ConfigError::Io { .. }), "{err}");
}

#[test]
fn default_extractors_are_unavailable_but_missing_and_empty_are_admitted() {
    let path = std::env::temp_dir().join(format!("kaban-default-with-{}.yaml", std::process::id()));
    for value in ["gliner", "llm", "script", "unknown", " "] {
        let yaml = format!("ingest:\n  default_with: {value:?}\n");
        std::fs::write(&path, yaml).unwrap();
        let error = load(&path).unwrap_err();
        assert_eq!(
            error,
            ConfigError::NotAvailable {
                path: "ingest.default_with"
            }
        );
        assert_eq!(error.to_string(), "not_available_in_this_release");
        assert!(!error.to_string().contains(value.trim()) || value.trim().is_empty());
    }
    for yaml in ["{}", "ingest:\n  default_with: \"\"\n"] {
        std::fs::write(&path, yaml).unwrap();
        assert!(load(&path).is_ok());
    }
    std::fs::remove_file(path).unwrap();
}

/// Spec 05 OWN-M: the retired trigram keys are ordinary unknown keys, whatever their value.
#[test]
fn the_retired_trigram_keys_are_unknown_keys() {
    let path =
        std::env::temp_dir().join(format!("kaban-index-trigram-{}.yaml", std::process::id()));
    for yaml in [
        "index:\n  trigram: true\n",
        "index:\n  trigram: false\n",
        "budget:\n  trigram_rss_mb: 2\n",
    ] {
        std::fs::write(&path, yaml).unwrap();
        let error = load(&path).unwrap_err();
        assert!(
            matches!(error, ConfigError::Parse { .. }),
            "{yaml}: {error:?}"
        );
    }
    std::fs::remove_file(path).unwrap();
}

#[test]
fn embedding_model_is_reserved_but_absent_null_and_empty_are_admitted() {
    let path = std::env::temp_dir().join(format!("kaban-embed-model-{}.yaml", std::process::id()));
    for yaml in [
        "embed:\n  model:\n    name: test-model\n",
        "embed:\n  model:\n    name: AKIAQWERTYUIOPASDFGH\n",
        "embed:\n  model:\n    arbitrary: value\n",
    ] {
        std::fs::write(&path, yaml).unwrap();
        let error = load(&path).unwrap_err();
        assert_eq!(
            error,
            ConfigError::NotAvailable {
                path: "embed.model"
            }
        );
        assert_eq!(error.to_string(), "not_available_in_this_release");
    }
    for yaml in ["{}", "embed:\n  model: null\n", "embed:\n  model: {}\n"] {
        std::fs::write(&path, yaml).unwrap();
        assert!(load(&path).is_ok());
    }
    std::fs::remove_file(path).unwrap();
}

#[test]
fn maps_are_replaced_whole_and_optional_keys_set_show_in_the_overrides() {
    let mut cfg = load(&fixture("maps_replace.yaml")).unwrap();
    assert_eq!(cfg.recall.column_weights.len(), 1);
    assert_eq!(
        cfg.embed.model.as_ref().unwrap(),
        &std::collections::BTreeMap::new()
    );
    cfg.embed.model = Some(std::collections::BTreeMap::from([
        ("name".to_owned(), "potion-base-4M".to_owned()),
        ("dims".to_owned(), "256".to_owned()),
    ]));
    assert_eq!(cfg.ingest.default_with.as_deref(), Some(""));
    // Override rendering remains useful for constructed configs; admission separately
    // rejects enrollment in this release.
    cfg.ingest.default_with = Some("gliner".to_owned());
    assert_eq!(
        cfg.reconcile.priors.as_deref(),
        Some(std::path::Path::new("/tmp/priors.yaml"))
    );
    assert_eq!(
        cfg.effective_overrides(),
        vec![
            ("embed.model.dims".to_owned(), "256".to_owned()),
            ("embed.model.name".to_owned(), "potion-base-4M".to_owned()),
            ("ingest.default_with".to_owned(), "gliner".to_owned()),
            (
                "recall.column_weights.ident_terms".to_owned(),
                "unset".to_owned()
            ),
            (
                "recall.column_weights.summary".to_owned(),
                "unset".to_owned()
            ),
            ("recall.column_weights.tags".to_owned(), "unset".to_owned()),
            ("recall.column_weights.text".to_owned(), "2.0".to_owned()),
            ("reconcile.priors".to_owned(), "/tmp/priors.yaml".to_owned()),
        ]
    );
}

// --- v4 CG4.2: the `index:` keys of stage1-spec §10 / stage1-plan CG4.2 (OWN-29, OWN-54) -------

#[test]
fn index_keys_load_strictly_with_the_plan_defaults_and_show_as_overrides() {
    use kaban_core::config::{Index, IndexQuota, Isolation};
    // the register defaults of the plan's YAML block
    let d = Index::default();
    assert_eq!(d.isolation, Isolation::Preferred);
    assert_eq!(d.container_image, None);
    assert_eq!(d.container_cli, None);
    assert_eq!(d.deadline_secs, 600);
    assert_eq!(
        d.quota,
        IndexQuota {
            file_bytes: 1_073_741_824,
            run_dir_bytes: 4_294_967_296,
            tmp_bytes: 536_870_912,
        }
    );
    // the former "unknown key" fixture now loads (the CG4.2 flip the E1.1 test announced)
    let cfg = load(&fixture("index_isolation.yaml")).unwrap();
    assert_eq!(cfg.index.isolation, Isolation::Preferred);
    // the `doctor_lists_effective_overrides` case: exactly the two changed keys
    let cfg = load(&fixture("index_overrides.yaml")).unwrap();
    assert_eq!(cfg.index.deadline_secs, 30);
    assert_eq!(cfg.index.quota.tmp_bytes, 1);
    assert_eq!(cfg.index.isolation, Isolation::Required);
    assert_eq!(
        cfg.effective_overrides(),
        vec![
            ("index.deadline_secs".to_owned(), "30".to_owned()),
            ("index.isolation".to_owned(), "required".to_owned()),
            ("index.quota.tmp_bytes".to_owned(), "1".to_owned()),
        ]
    );
    // an unknown key under `index` names the path, never the value; a value outside the
    // closed set fails at `index.isolation`
    for (file, path) in [
        ("index_isolaton.yaml", "index.isolaton"),
        ("index_isolation_sometimes.yaml", "index.isolation"),
        ("index_quota_unknown.yaml", "index.quota.files_bytes"),
    ] {
        let err = load(&fixture(file)).unwrap_err();
        assert_eq!(
            err,
            ConfigError::Parse {
                path: path.to_owned()
            },
            "{file}"
        );
        assert!(!err.to_string().contains("sometimes"), "{file}");
    }
    // the closed set, as doctor lists it
    assert_eq!(Isolation::ALL, ["preferred", "required", "off"]);
    // a deadline or quota of 0 is out of range (an opt-in budget, OWN-54)
    assert_eq!(
        load(&fixture("index_deadline_zero.yaml")).unwrap_err(),
        ConfigError::Range {
            path: "index.deadline_secs".to_owned()
        }
    );
}
