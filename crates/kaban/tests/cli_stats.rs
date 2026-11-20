//! Shared stats surfaces and effective budget goldens.
#![allow(clippy::unwrap_used)]

use kaban_api::{BudgetSource, RunFlags, budgets};
use kaban_core::config::EngineConfig;
use kaban_core::{Actor, ActorId, MemoryStore, NewNode, Producer, WorkspaceId, WriteCtx};
use kaban_store_sqlite::{ProducerRunOutcome, SqliteStore};
use serde_json::{Value, json};
use std::fmt::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

mod common;

fn stats_cli(scratch: &common::Scratch, args: &[&str]) -> std::process::Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_kaban"))
        .current_dir(&scratch.0)
        .env("KABAN_HOME", scratch.home())
        .args(["--json", "--db", scratch.db().to_str().unwrap(), "stats"])
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let timed_out = loop {
        if child.try_wait().unwrap().is_some() {
            break false;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            break true;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let output = child.wait_with_output().unwrap();
    assert!(!timed_out, "stats timed out: {output:?}");
    output
}

#[test]
fn tool_resource_and_cli_share_the_producers_and_preserve_old_fields() {
    let scratch = common::Scratch::new("kaban-stats", "parity");
    std::fs::create_dir_all(scratch.home()).unwrap();
    {
        let store = SqliteStore::open(&scratch.db(), &scratch.home().join("keys")).unwrap();
        store
            .record_producer_run(Producer::Gazetteer, 42, ProducerRunOutcome::Failed)
            .unwrap();
    }
    let mut server = common::Server::start_at(&scratch, &scratch.0);
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
    assert_eq!(tool["producers"], resource["producers"]);
    assert_eq!(tool["producers"], cli["producers"]);
    for field in [
        "nodes",
        "edges",
        "by_kind",
        "by_rel",
        "engine",
        "schema_version",
        "policy",
        "stores",
        "indexes",
    ] {
        assert!(tool.get(field).is_some(), "missing {field}: {tool}");
        assert_eq!(tool[field], resource[field], "{field}");
        assert_eq!(tool[field], cli[field], "{field}");
    }
    for value in [&tool, &resource, &cli] {
        assert!(value.get("index_container").is_none(), "{value}");
        assert!(value["db_bytes"].as_u64().is_some_and(|bytes| bytes > 0));
        // seven rows (PC-16): authored, hook_events, document_ingest, gazetteer, extractor,
        // scip_import, git_history; no `layers` matrix
        assert!(value.get("layers").is_none(), "{value}");
        assert_eq!(value["producers"][0]["on"], true);
        assert_eq!(value["producers"][3]["last_run"], 42);
        assert_eq!(value["producers"][3]["last_outcome"], "failed");
        assert_eq!(value["producers"][4]["on"], false);
        assert_eq!(
            value["producers"][4]["reason_off"],
            "explicit_selection_only"
        );
        assert_eq!(value["producers"][6]["on"], false);
        assert_eq!(
            value["producers"][6]["reason_off"],
            "not_available_in_this_release"
        );
    }
    insta::assert_snapshot!(
        "producers_default",
        serde_json::to_string_pretty(&tool["producers"]).unwrap()
    );
}

/// Count canonical records even when rebuilding their index would exceed the budget.
fn inspect_degraded_project(missing_container: bool) {
    let name = if missing_container {
        "missing"
    } else {
        "stale"
    };
    let scratch = common::Scratch::new("kaban-stats", name);
    let store = SqliteStore::open(&scratch.db(), &scratch.home().join("keys")).unwrap();
    store
        .remember(
            NewNode {
                kind: "Fact".to_owned(),
                text: "quokka".to_owned(),
                ..Default::default()
            },
            &[],
            &WriteCtx::new(WorkspaceId::default(), Actor::Agent),
        )
        .unwrap();
    drop(store);
    let conn = rusqlite::Connection::open(scratch.db()).unwrap();
    SqliteStore::bind_writer(&conn, &ActorId::default()).unwrap();
    conn.execute(
        "UPDATE node SET props=json_object('padding', ?1)",
        ["x".repeat(8 << 20)],
    )
    .unwrap();
    let container = scratch.0.join("kaban.kbx");
    // Service startup would attempt to load this new ontology. An inspection of an
    // existing project must still be able to describe the committed records.
    std::fs::create_dir(scratch.0.join(".git")).unwrap();
    let ontologies = scratch.0.join(".kaban/ontology");
    std::fs::create_dir_all(&ontologies).unwrap();
    std::fs::write(ontologies.join("invalid.yaml"), "ontology: [").unwrap();
    if missing_container {
        std::fs::remove_file(&container).unwrap();
    } else {
        conn.execute(
            "UPDATE index_meta SET value='old' WHERE key='ident_terms_version'",
            [],
        )
        .unwrap();
    }
    std::fs::write(
        scratch.home().join("config.yaml"),
        "catalog:\n  maintenance_memory_mib: 16\n",
    )
    .unwrap();
    let output = stats_cli(&scratch, &["--workspace-id", "*"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    let stats: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(stats["nodes"], 1, "{stats}");
    assert_eq!(stats["by_kind"]["Fact"], 1, "{stats}");
    assert_eq!(stats["edges"], 0, "{stats}");
    assert_eq!(container.exists(), !missing_container);
    if missing_container {
        assert_eq!(stats["index_container"], "missing", "{stats}");
    } else {
        let marker: String = conn
            .query_row(
                "SELECT value FROM index_meta WHERE key='ident_terms_version'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(marker, "old");
    }
    for text in [stderr.as_ref(), &stats.to_string()] {
        assert!(
            !text.contains("reindex") && !text.contains("repair"),
            "{text}"
        );
    }
}

#[test]
fn stats_counts_with_a_missing_container_and_leaves_it_missing() {
    inspect_degraded_project(true);
}

#[test]
fn stats_counts_with_a_stale_projection_and_leaves_it_stale() {
    inspect_degraded_project(false);
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

/// The `embed` section is gone (PC-16, no compatibility): any spelling of it is refused
/// naming the key, never echoing a value, before a database exists.
#[test]
fn the_removed_embed_section_is_exit_four_naming_the_key_without_echo_or_database_creation() {
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
    assert!(stderr.contains("config key invalid: embed"), "{stderr}");
    assert!(!stderr.contains("AKIAQWERTYUIOPASDFGH"));
    assert!(!scratch.db().exists());
    for yaml in ["embed:\n  model: null\n", "embed:\n  model: {}\n"] {
        std::fs::write(&config, yaml).unwrap();
        let output = run();
        assert_eq!(output.status.code(), Some(4), "{output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("config key invalid: embed"),
            "{output:?}"
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
