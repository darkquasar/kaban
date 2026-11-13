//! The global `--actor-id` option (spec 05 SS-26): one writer identity per process, `local`
//! when unset, set by the flag or `KABAN_ACTOR_ID` for the CLI and for `kaban serve` (MCP)
//! alike, and stamped on the records and change rows the process writes. The audit chain still
//! verifies.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::Scratch;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Output, Stdio};

const ENV: &str = "KABAN_ACTOR_ID";

/// `kaban --db <scratch> [global..] <args..>` with a private home and no inherited actor.
fn kaban(s: &Scratch, global: &[&str], env: Option<&str>) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_kaban"));
    cmd.env("KABAN_HOME", s.home())
        .env_remove(ENV)
        .args(["--db", s.db().to_str().unwrap()])
        .args(global)
        .stdin(Stdio::null());
    if let Some(value) = env {
        cmd.env(ENV, value);
    }
    cmd
}

/// `remember Fact <text>` on the CLI; the new node's id.
fn cli_remember(s: &Scratch, global: &[&str], env: Option<&str>, text: &str) -> String {
    let out = kaban(s, global, env)
        .args(["--json", "remember", "Fact", text])
        .output()
        .unwrap();
    created_id(&out)
}

fn created_id(out: &Output) -> String {
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let doc: Value = serde_json::from_slice(&out.stdout).unwrap();
    doc["id"].as_str().unwrap().to_owned()
}

/// `remember` over MCP through `kaban [global..] serve`; the new node's id.
fn serve_remember(s: &Scratch, global: &[&str], env: Option<&str>, text: &str) -> String {
    let mut child = kaban(s, global, env)
        .env("TOKIO_WORKER_THREADS", "1")
        .arg("serve")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let mut send = |msg: &Value| {
        stdin.write_all(format!("{msg}\n").as_bytes()).unwrap();
        stdin.flush().unwrap();
    };
    let mut answer = |id: u64| -> Value {
        loop {
            let line = lines.next().expect("the server answers").unwrap();
            let msg: Value = serde_json::from_str(&line).unwrap();
            if msg["id"] == json!(id) && msg.get("method").is_none() {
                return msg;
            }
        }
    };
    send(
        &json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
        "protocolVersion": "2025-11-25", "capabilities": {},
        "clientInfo": {"name": "t", "version": "0"}}}),
    );
    answer(1);
    send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    send(
        &json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {
        "name": "remember", "arguments": {"kind": "Fact", "text": text}}}),
    );
    let resp = answer(2);
    drop(send);
    drop(stdin);
    assert!(child.wait().unwrap().success());
    assert_ne!(resp["result"]["isError"], json!(true), "{resp}");
    let doc: Value =
        serde_json::from_str(resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    doc["id"].as_str().unwrap().to_owned()
}

/// The actor stamped on node `id` and on every change row naming it.
fn stamped(s: &Scratch, id: &str) -> (String, Vec<String>) {
    let conn = rusqlite::Connection::open(s.db()).unwrap();
    let node: String = conn
        .query_row("SELECT actor_id FROM node WHERE id = ?1", [id], |r| {
            r.get(0)
        })
        .unwrap();
    let mut st = conn
        .prepare("SELECT actor_id FROM changes WHERE id = ?1 ORDER BY seq")
        .unwrap();
    let changes = st
        .query_map([id], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    (node, changes)
}

fn by(actor: &str) -> (String, Vec<String>) {
    (actor.to_owned(), vec![actor.to_owned()])
}

#[test]
fn the_cli_stamps_local_by_default_and_the_flag_or_env_otherwise() {
    let s = Scratch::new("kaban-actor", "cli");
    let unset = cli_remember(&s, &[], None, "nobody configured an actor");
    let flag = cli_remember(
        &s,
        &["--actor-id", "agent-a"],
        None,
        "the flag names agent a",
    );
    let env = cli_remember(&s, &[], Some("agent-b"), "the env names agent b");
    let both = cli_remember(
        &s,
        &["--actor-id", "agent-c"],
        Some("agent-b"),
        "the flag wins over the env",
    );
    assert_eq!(stamped(&s, &unset), by("local"));
    assert_eq!(stamped(&s, &flag), by("agent-a"));
    assert_eq!(stamped(&s, &env), by("agent-b"));
    assert_eq!(stamped(&s, &both), by("agent-c"));
    let store = kaban_store_sqlite::SqliteStore::open_existing_key(
        &s.db(),
        &s.home().join("keys"),
        &kaban_core::ActorId::default(),
    )
    .unwrap();
    let verify = kaban_core::AuditStore::audit_verify(&store).unwrap();
    assert!(verify.ok, "the audit chain still verifies: {verify:?}");
}

#[test]
fn serve_stamps_local_by_default_and_the_flag_or_env_otherwise() {
    let s = Scratch::new("kaban-actor", "serve");
    let unset = serve_remember(&s, &[], None, "an mcp write with no actor");
    let flag = serve_remember(
        &s,
        &["--actor-id", "swarm-1"],
        None,
        "an mcp write by swarm 1",
    );
    let env = serve_remember(&s, &[], Some("swarm-2"), "an mcp write by swarm 2");
    assert_eq!(stamped(&s, &unset), by("local"));
    assert_eq!(stamped(&s, &flag), by("swarm-1"));
    assert_eq!(stamped(&s, &env), by("swarm-2"));
}

#[test]
fn an_invalid_actor_id_is_a_usage_error_that_echoes_nothing() {
    let s = Scratch::new("kaban-actor", "invalid");
    for bad in ["", "two words", "x".repeat(129).as_str()] {
        let out = kaban(&s, &["--actor-id", bad], None)
            .args(["remember", "Fact", "never stored"])
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(4), "{bad:?}");
        if !bad.is_empty() {
            assert!(!String::from_utf8_lossy(&out.stderr).contains(bad));
        }
    }
    assert!(!s.db().exists(), "a refused option opens nothing");
}
