//! Shared stats surfaces and effective budget goldens.
#![allow(clippy::unwrap_used)]

use kaban_api::{BudgetSource, RunFlags, budgets};
use kaban_core::Layer;
use kaban_core::config::EngineConfig;
use kaban_store_sqlite::{LayerRunOutcome, SqliteStore};
use serde_json::{Value, json};
use std::fmt::Write;
use std::process::Command;
use std::time::Duration;

mod common;

fn stats_cli(scratch: &common::Scratch, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_kaban"))
        .env("KABAN_HOME", scratch.home())
        .args(["--json", "--db", scratch.db().to_str().unwrap(), "stats"])
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn tool_resource_and_cli_share_the_matrix_and_preserve_old_fields() {
    let scratch = common::Scratch::new("kaban-stats", "parity");
    std::fs::create_dir_all(scratch.home()).unwrap();
    {
        let store = SqliteStore::open(&scratch.db(), &scratch.home().join("keys")).unwrap();
        store
            .record_layer_run(Layer::Gazetteer, 42, LayerRunOutcome::Failed)
            .unwrap();
    }
    let mut server = common::Server::start(&scratch);
    let id = server.request("tools/call", &json!({"name":"stats","arguments":{}}));
    let result = server.response(id, Duration::from_secs(10)).unwrap();
    assert_ne!(result["result"]["isError"], true, "{result}");
    let tool = result["result"]["structuredContent"].clone();
    let id = server.request("resources/read", &json!({"uri":"kaban://stats"}));
    let result = server.response(id, Duration::from_secs(10)).unwrap();
    let resource: Value =
        serde_json::from_str(result["result"]["contents"][0]["text"].as_str().unwrap()).unwrap();
    let output = stats_cli(&scratch, &[]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).lines().count(), 1);
    let cli: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(tool["layers"], resource["layers"]);
    assert_eq!(tool["layers"], cli["layers"]);
    for field in [
        "nodes",
        "edges",
        "by_kind",
        "by_rel",
        "changes_lag",
        "outbox_lag",
        "engine",
        "schema_version",
        "conflicts",
        "policy",
        "stores",
    ] {
        assert!(tool.get(field).is_some(), "missing {field}: {tool}");
        assert_eq!(tool[field], resource[field], "{field}");
        assert_eq!(tool[field], cli[field], "{field}");
    }
    for value in [&tool, &resource, &cli] {
        assert!(value["db_bytes"].as_u64().is_some_and(|bytes| bytes > 0));
        // four rows (spec 05 OWN-M: no retired trigram row): fts, gazetteer, embed, extractors
        assert_eq!(value["layers"][0]["on"], true);
        assert_eq!(value["layers"][1]["last_run"], 42);
        assert_eq!(value["layers"][1]["last_outcome"], "failed");
        assert_eq!(value["layers"][2]["on"], false);
        assert_eq!(
            value["layers"][2]["reason_off"],
            "not_available_in_this_release"
        );
        assert_eq!(value["layers"][3]["on"], false);
    }
    insta::assert_snapshot!(
        "matrix_embed_off",
        serde_json::to_string_pretty(&tool["layers"]).unwrap()
    );
}

#[test]
fn stats_invalid_config_is_exit_four_without_database_creation() {
    let scratch = common::Scratch::new("kaban-stats", "invalid-config");
    std::fs::create_dir_all(scratch.home()).unwrap();
    for yaml in [
        "unknown: value\n",
        "gazetteer:\n  max_automaton_mb: 0\n",
        "ingest:\n  default_with: gliner\n",
        "embed:\n  model:\n    name: external\n",
    ] {
        std::fs::write(scratch.home().join("config.yaml"), yaml).unwrap();
        let output = stats_cli(&scratch, &[]);
        assert_eq!(output.status.code(), Some(4), "{output:?}");
        assert_eq!(output.stdout, [] as [u8; 0]);
        assert!(!scratch.db().exists());
    }
}

#[test]
fn stats_selector_remains_content_scanned() {
    let scratch = common::Scratch::new("kaban-stats", "selector-gate");
    let output = stats_cli(&scratch, &["--workspace-id", "AKIAQWERTYUIOPASDFGH"]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(!String::from_utf8_lossy(&output.stdout).contains("AKIAQWERTYUIOPASDFGH"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("AKIAQWERTYUIOPASDFGH"));
}

#[test]
fn default_extractor_admission_is_exit_four_and_never_echoes_the_value() {
    let scratch = common::Scratch::new("kaban-stats", "default-with-admission");
    std::fs::create_dir_all(scratch.home()).unwrap();
    let config = scratch.home().join("config.yaml");
    for value in ["gliner", "llm", "script", "AKIAQWERTYUIOPASDFGH", " "] {
        std::fs::write(&config, format!("ingest:\n  default_with: {value:?}\n")).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_kaban"))
            .env("KABAN_HOME", scratch.home())
            .args(["--db", scratch.db().to_str().unwrap(), "doctor"])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(4), "{output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("not_available_in_this_release"), "{stderr}");
        if !value.trim().is_empty() {
            assert!(!stderr.contains(value));
        }
        assert_eq!(output.stdout, [] as [u8; 0]);
        assert!(!scratch.db().exists());
    }
}

fn budget_table(cfg: &EngineConfig, flags: RunFlags) -> String {
    let mut output = String::from("key | default | effective | source | cap_lifted\n");
    for row in budgets(cfg, &flags) {
        let source = match row.source {
            BudgetSource::Default => "default",
            BudgetSource::Config => "config",
            BudgetSource::Flag(flag) => flag,
        };
        writeln!(
            output,
            "{} | {} | {} | {} | {}",
            row.key, row.default, row.effective, source, row.cap_lifted
        )
        .unwrap();
    }
    output
}

#[test]
fn embedding_model_admission_is_exit_four_without_echo_or_database_creation() {
    let scratch = common::Scratch::new("kaban-stats", "embed-model-admission");
    std::fs::create_dir_all(scratch.home()).unwrap();
    let config = scratch.home().join("config.yaml");
    std::fs::write(
        &config,
        "embed:\n  model:\n    name: AKIAQWERTYUIOPASDFGH\n",
    )
    .unwrap();
    let run = || {
        Command::new(env!("CARGO_BIN_EXE_kaban"))
            .env("KABAN_HOME", scratch.home())
            .args(["--db", scratch.db().to_str().unwrap(), "doctor"])
            .output()
            .unwrap()
    };
    let output = run();
    assert_eq!(output.status.code(), Some(4), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("not_available_in_this_release"));
    assert!(!stderr.contains("AKIAQWERTYUIOPASDFGH"));
    assert!(!scratch.db().exists());
    for yaml in ["embed:\n  model: null\n", "embed:\n  model: {}\n"] {
        std::fs::write(&config, yaml).unwrap();
        assert_eq!(
            run().status.code(),
            Some(5),
            "accepted config reaches missing diagnostic DB"
        );
        assert!(!scratch.db().exists());
    }
}

#[test]
fn effective_budget_defaults_golden() {
    let cfg = EngineConfig::default();
    insta::assert_snapshot!("budgets_default", budget_table(&cfg, RunFlags::default()));
}

#[test]
fn effective_budget_config_golden() {
    let mut cfg = EngineConfig::default();
    cfg.gazetteer.max_automaton_mb = 200;
    insta::assert_snapshot!("budgets_config", budget_table(&cfg, RunFlags::default()));
}

#[test]
fn effective_budget_flag_golden() {
    let mut cfg = EngineConfig::default();
    cfg.gazetteer.max_automaton_mb = 200;
    insta::assert_snapshot!(
        "budgets_symbols",
        budget_table(&cfg, RunFlags { symbols: true })
    );
}
