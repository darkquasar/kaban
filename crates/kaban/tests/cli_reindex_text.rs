//! Black-box text index maintenance command (`kaban reindex text`) and argument rejection.
#![allow(clippy::unwrap_used, clippy::expect_used)] // test assertions
mod common;
use common::Scratch;
use kaban_core::{Actor, IngestStore, MemoryStore, NewNode, RecallQuery, WorkspaceId, WriteCtx};
use kaban_store_sqlite::SqliteStore;
use std::process::Command;

#[test]
fn reindex_text_backfills_and_repeated_pass_preserves_content() {
    let scratch = Scratch::new("e33-cli", "reindex");
    let store = SqliteStore::open(&scratch.db(), &scratch.home().join("keys")).unwrap();
    store
        .remember(
            NewNode {
                kind: "Fact".into(),
                text: "LegacyReader".into(),
                ..Default::default()
            },
            &[],
            &WriteCtx::new(WorkspaceId::default(), Actor::Agent),
        )
        .unwrap();
    drop(store);
    // `reindex text` rebuilds the text index from the rows, twice with the same content
    for _ in 0..2 {
        let output = Command::new(env!("CARGO_BIN_EXE_kaban"))
            .env("KABAN_HOME", scratch.home())
            .args([
                "--json",
                "--db",
                scratch.db().to_str().unwrap(),
                "reindex",
                "text",
            ])
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        let output: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(output["rows"], 1);
    }
    let store = SqliteStore::open(&scratch.db(), &scratch.home().join("keys")).unwrap();
    let hits = store
        .recall(&RecallQuery {
            query: "legacy".into(),
            hops: 0,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].node.text, "LegacyReader");
}

#[test]
fn reindex_rejects_unknown_arm_and_flag() {
    for args in [["reindex", "unknown"], ["reindex", "--unknown"]] {
        let output = Command::new(env!("CARGO_BIN_EXE_kaban"))
            .args(args)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(4));
        assert_eq!(output.stdout, [] as [u8; 0]);
    }
}

fn check_startup_rebuild_budget(missing_container: bool) {
    let scratch = Scratch::new(
        "text-rebuild",
        if missing_container {
            "container"
        } else {
            "projection"
        },
    );
    let keys = scratch.home().join("keys");
    let store = SqliteStore::open(&scratch.db(), &keys).unwrap();
    store
        .remember(
            NewNode {
                kind: "Fact".into(),
                text: "quokka".into(),
                ..Default::default()
            },
            &[],
            &WriteCtx::new(WorkspaceId::default(), Actor::Agent),
        )
        .unwrap();
    store.reindex_text().unwrap();
    drop(store);
    let conn = rusqlite::Connection::open(scratch.db()).unwrap();
    SqliteStore::bind_writer(&conn, &kaban_core::ActorId::default()).unwrap();
    // Rebuilding charges twice the canonical input size. This row alone exceeds 16 MiB.
    conn.execute(
        "UPDATE node SET text='wombat', props=json_object('padding', ?1)",
        ["x".repeat(8 << 20)],
    )
    .unwrap();
    let marker = || {
        conn.query_row(
            "SELECT value FROM index_meta WHERE key='ident_terms_version'",
            [],
            |r| r.get::<_, String>(0),
        )
        .unwrap()
    };
    let current = marker();
    if missing_container {
        std::fs::remove_file(scratch.0.join("kaban.kbx")).unwrap();
    } else {
        conn.execute(
            "UPDATE index_meta SET value='old' WHERE key='ident_terms_version'",
            [],
        )
        .unwrap();
    }
    let run = |args: &[&str], mib| {
        std::fs::write(
            scratch.home().join("config.yaml"),
            format!("catalog:\n  maintenance_memory_mib: {mib}\n"),
        )
        .unwrap();
        let stderr = scratch.0.join("startup.stderr");
        let mut cli = Command::new(env!("CARGO_BIN_EXE_kaban"))
            .current_dir(&scratch.0)
            .env("KABAN_HOME", scratch.home())
            .args(["--json", "--db", scratch.db().to_str().unwrap()])
            .args(args)
            .stdout(std::process::Stdio::null())
            .stderr(std::fs::File::create(&stderr).unwrap())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let status = loop {
            if let Some(status) = cli.try_wait().unwrap() {
                break status;
            }
            if std::time::Instant::now() >= deadline {
                let _ = cli.kill();
                break cli.wait().unwrap();
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        (status, std::fs::read_to_string(stderr).unwrap())
    };
    let (refused, error) = run(&["stats"], 16);
    assert_eq!(
        refused.code(),
        Some(5),
        "startup must apply the configured rebuild budget"
    );
    assert!(error.contains("budget exceeded: Memory"), "{error}");
    if !missing_container {
        assert_eq!(
            marker(),
            "pending",
            "a refused rebuild must remain retryable"
        );
    }
    let (repaired, error) = run(&["reindex", "text"], 128);
    assert!(repaired.success(), "{error}");
    assert_eq!(marker(), current);
    let store = SqliteStore::open(&scratch.db(), &keys).unwrap();
    for (term, expected) in [("wombat", 1), ("quokka", 0)] {
        let hits = store
            .recall(&RecallQuery {
                query: term.into(),
                hops: 0,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(hits.len(), expected, "{term}");
    }
}

#[test]
fn stale_text_uses_the_configured_startup_budget() {
    check_startup_rebuild_budget(false);
}

#[test]
fn missing_container_uses_the_configured_startup_budget() {
    check_startup_rebuild_budget(true);
}
