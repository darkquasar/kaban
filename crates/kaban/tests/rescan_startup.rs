//! The rescan job runs in the background of `kaban serve` (core-plan C2.7; MD-18): with
//! `TOKIO_WORKER_THREADS=1` the server still answers `initialize` while thousands of stale
//! nodes are being re-judged on the blocking pool, and closing the client cancels the job
//! between batches (the batches that ran stay committed). `kaban rescan` runs the same job in
//! the foreground and prints its report.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::{Scratch, Server};
use kaban_core::{Actor, IndexMeta, IngestStore, MemoryStore, NewNode, WorkspaceId, WriteCtx};
use kaban_store_sqlite::SqliteStore;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Stale nodes to seed: enough batches of 500 for the job to outlive the handshake in a debug
/// build (observed several seconds; the handshake takes well under one).
const STALE_NODES: usize = 20_000;

/// Seed `n` nodes stamped with the policy version `stale` through the store (no policy gate),
/// with the key under the scratch's `$KABAN_HOME` so the server reuses it.
fn seed_stale(s: &Scratch, n: usize) -> Arc<SqliteStore> {
    let store = Arc::new(SqliteStore::open(&s.db(), &s.home().join("keys")).unwrap());
    let ctx = WriteCtx::new("stale", WorkspaceId::default(), Actor::Agent);
    for i in 0..n {
        store
            .remember(
                NewNode {
                    kind: "Fact".into(),
                    text: format!("stale note {i} about cargo build {}", i % 97),
                    workspace_id: Some("default".into()),
                    ..Default::default()
                },
                &[],
                &ctx,
            )
            .unwrap();
    }
    store
}

fn stale_count(s: &Scratch) -> usize {
    let conn = rusqlite::Connection::open(s.db()).unwrap();
    conn.query_row(
        "SELECT count(*) FROM node WHERE policy_version = 'stale'",
        [],
        |r| r.get::<_, i64>(0),
    )
    .map(|n| usize::try_from(n).unwrap())
    .unwrap()
}

#[test]
fn initialize_is_answered_while_the_startup_rescan_runs_and_shutdown_cancels_it() {
    // Arrange
    let s = Scratch::new("kaban-rescan", "startup");
    let store = seed_stale(&s, STALE_NODES);
    assert!(
        store
            .index_meta()
            .unwrap()
            .get(IndexMeta::POLICY_HASH)
            .is_none()
    );
    drop(store);

    // Act: start the server (one worker thread); the handshake must not wait for the job
    let t_start = Instant::now();
    let (srv, init_took) = Server::start_with(&s, &[], Duration::from_secs(3));
    let stale_at_init = stale_count(&s);
    let elapsed_at_init = t_start.elapsed();

    // Assert: answered promptly while the job was still running (stale rows left)
    assert!(
        init_took < Duration::from_secs(2),
        "initialize took {init_took:?} with a rescan of {STALE_NODES} nodes pending"
    );
    assert!(
        stale_at_init > 0,
        "the rescan had already finished {STALE_NODES} nodes when initialize was answered ({elapsed_at_init:?}); the test cannot witness concurrency"
    );
    assert!(
        stale_at_init < STALE_NODES || elapsed_at_init < Duration::from_millis(1500),
        "no batch committed after {elapsed_at_init:?}: is the job running? stale={stale_at_init}"
    );

    // Act: the client leaves; the job is cancelled between batches
    let (status, shutdown_took) = srv.finish();

    // Assert: a clean exit within a batch's worth of time, progress committed, and the hash
    // only recorded if everything was judged
    assert!(status.success(), "{status:?}");
    assert!(
        shutdown_took < Duration::from_secs(10),
        "shutdown waited {shutdown_took:?} for the rescan"
    );
    let stale_after = stale_count(&s);
    assert!(stale_after < STALE_NODES, "no batch committed at all");
    assert!(
        stale_after.is_multiple_of(500),
        "whole batches only: {stale_after} stale rows left"
    );
    let store = SqliteStore::open(&s.db(), &s.home().join("keys")).unwrap();
    let hash = store.index_meta().unwrap();
    if stale_after == 0 {
        assert!(hash.get(IndexMeta::POLICY_HASH).is_some());
    } else {
        assert!(hash.get(IndexMeta::POLICY_HASH).is_none(), "{hash:?}");
    }
    #[allow(clippy::print_stderr)] // test evidence for the log
    {
        eprintln!(
            "initialize {init_took:?}; stale at initialize {stale_at_init}; shutdown {shutdown_took:?}; stale after {stale_after}"
        );
    }
}

#[test]
fn kaban_rescan_runs_the_job_in_the_foreground_and_reports_it() {
    // Arrange: one full batch and a part of one, one of them a credential the builtin rules
    // reject (the CLI runs with the builtin policy only)
    let s = Scratch::new("kaban-rescan", "cli");
    let store = seed_stale(&s, 700);
    let ctx = WriteCtx::new("stale", WorkspaceId::default(), Actor::Agent);
    store
        .remember(
            NewNode {
                kind: "Fact".into(),
                text: "AKIAQ7Z2M4N8P1R5T9W3 is the access key id".into(),
                workspace_id: Some("default".into()),
                ..Default::default()
            },
            &[],
            &ctx,
        )
        .unwrap();
    drop(store);
    let run = |args: &[&str]| {
        std::process::Command::new(env!("CARGO_BIN_EXE_kaban"))
            .env("KABAN_HOME", s.home())
            .args(["--db", s.db().to_str().unwrap()])
            .args(args)
            .output()
            .unwrap()
    };

    // Act
    let first = run(&["rescan"]);

    // Assert: exit 0, the report on stdout, every node judged, the credential hidden
    assert!(first.status.success(), "{first:?}");
    let report: serde_json::Value = serde_json::from_slice(&first.stdout).unwrap();
    assert_eq!(report["status"], serde_json::json!("completed"), "{report}");
    assert_eq!(report["scanned"], serde_json::json!(701), "{report}");
    assert_eq!(report["batches"], serde_json::json!(2), "{report}");
    assert_eq!(report["rejected"], serde_json::json!(1), "{report}");
    assert_eq!(stale_count(&s), 0);

    // a second run has nothing to do
    let second = run(&["rescan"]);
    assert!(second.status.success(), "{second:?}");
    let report: serde_json::Value = serde_json::from_slice(&second.stdout).unwrap();
    assert_eq!(
        report["status"],
        serde_json::json!("up_to_date"),
        "{report}"
    );
    assert_eq!(report["scanned"], serde_json::json!(0), "{report}");

    // a policy that does not load is a configuration error (core-spec §6: exit 4), stdout empty
    let bad = run(&["--policy", "/nonexistent/policy.yaml", "rescan"]);
    assert_eq!(bad.status.code(), Some(4), "{bad:?}");
    assert_eq!(bad.stdout.len(), 0, "{bad:?}");
    assert!(String::from_utf8_lossy(&bad.stderr).contains("error:"));
}
