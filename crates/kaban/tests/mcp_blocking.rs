//! Store work reached from the MCP adapter runs off the Tokio workers (core-spec E2, N4;
//! core-plan C0.5 / review R0-1): a write that waits on SQLite's `busy_timeout` must not stall
//! protocol handling. Black-box over stdio with `TOKIO_WORKER_THREADS=1`, so a handler that
//! blocks the single worker is observable as an unanswered `tools/list`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::{Scratch, Server};
use rusqlite::Connection;
use serde_json::{Value, json};
use std::time::{Duration, Instant};

#[test]
fn a_write_blocked_on_the_database_lock_does_not_stall_other_requests() {
    let s = Scratch::new("kaban-blocking", "busy");
    let mut srv = Server::start(&s);

    // Another process holds the write lock, so the server's `remember` waits in busy_timeout.
    let locker = Connection::open(s.db()).unwrap();
    locker.execute_batch("BEGIN IMMEDIATE").unwrap();

    let write = srv.request(
        "tools/call",
        &json!({"name": "remember", "arguments": {"kind": "Fact", "text": "the lock holder test note"}}),
    );
    // Give the write time to reach the lock wait before probing. Sending both at once would
    // let the runtime schedule the probe first and hide a blocked worker.
    std::thread::sleep(Duration::from_millis(500));
    let probe_sent = Instant::now();
    let probe = srv.request("tools/list", &json!({}));
    let listed = srv.response(probe, Duration::from_secs(1));
    let waited = probe_sent.elapsed();
    assert!(
        listed.is_some(),
        "tools/list was not answered within 1 s while a write waited on the database lock (waited {waited:?})"
    );
    let listed = listed.unwrap();
    assert!(
        listed["result"]["tools"]
            .as_array()
            .is_some_and(|t| !t.is_empty()),
        "{listed}"
    );

    // Release the lock: the blocked write completes normally.
    locker.execute_batch("ROLLBACK").unwrap();
    let written = srv
        .response(write, Duration::from_secs(10))
        .expect("remember answered after the lock was released");
    assert_ne!(written["result"]["isError"], json!(true), "{written}");
    let body: Value =
        serde_json::from_str(written["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(body["status"], json!("created"), "{body}");
    let _ = srv.finish();
}
