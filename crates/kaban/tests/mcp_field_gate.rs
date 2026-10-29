//! Every string-bearing argument field of every tool is gated by its typed handler, and every
//! outgoing string by the adapter's response gate (core-spec S1; review R1-3, coordinator
//! decision: option A). Table-driven black-box over stdio with `RUST_LOG=trace`: a redact-class
//! canary is sent in one field at a time and must appear neither in the JSON-RPC response, nor
//! in stderr, nor in the db/-wal/-shm/audit-export bytes.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::Scratch;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

/// Field-gate view of the scratch home: the audit export target and everything persisted.
trait ScratchExt {
    fn export(&self) -> PathBuf;
    /// Every byte SQLite and the export may have written (main file, WAL, SHM, JSONL).
    fn persisted_bytes(&self) -> Vec<u8>;
}
impl ScratchExt for Scratch {
    fn export(&self) -> PathBuf {
        self.0.join("audit-export.jsonl")
    }
    fn persisted_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for f in std::fs::read_dir(&self.0).unwrap() {
            let p = f.unwrap().path();
            if p.is_file() {
                out.extend(std::fs::read(&p).unwrap());
            }
        }
        out
    }
}

/// The shipped example policy redacts this term (`old-password`); it is not a secret.
const CANARY: &str = "hunter2";

struct Server {
    child: Child,
    out: BufReader<std::process::ChildStdout>,
    next_id: u64,
}

impl Server {
    /// Start on the scratch database with the shipped example policy (`with_policy`) or with
    /// the builtin rules only.
    fn start(s: &Scratch, with_policy: bool) -> Self {
        let policy = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/policy.yaml");
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_kaban"));
        cmd.env("KABAN_HOME", s.0.join("home"))
            .env("RUST_LOG", "trace")
            .arg("--db")
            .arg(s.db())
            .arg("--audit-export")
            .arg(s.export());
        if with_policy {
            cmd.arg("--policy").arg(policy);
        }
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let out = BufReader::new(child.stdout.take().unwrap());
        let mut srv = Self {
            child,
            out,
            next_id: 0,
        };
        let init = srv.request(
            "initialize",
            &json!({"protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": {"name": "t", "version": "0"}}),
        );
        assert_eq!(init["result"]["serverInfo"]["name"], json!("kaban"));
        srv.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        srv
    }
    fn send(&mut self, msg: &Value) {
        let stdin = self.child.stdin.as_mut().unwrap();
        stdin.write_all(msg.to_string().as_bytes()).unwrap();
        stdin.write_all(b"\n").unwrap();
        stdin.flush().unwrap();
    }
    fn request(&mut self, method: &str, params: &Value) -> Value {
        self.next_id += 1;
        self.send(
            &json!({"jsonrpc": "2.0", "id": self.next_id, "method": method, "params": params}),
        );
        let mut line = String::new();
        self.out.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("{e}: {line:?}"))
    }
    fn call(&mut self, tool: &str, args: &Value) -> Value {
        self.request("tools/call", &json!({"name": tool, "arguments": args}))
    }
    /// A successful call's parsed JSON content.
    fn call_ok(&mut self, tool: &str, args: &Value) -> Value {
        let resp = self.call(tool, args);
        assert_ne!(resp["result"]["isError"], json!(true), "{resp}");
        serde_json::from_str(resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
    }
    /// Close stdin, wait, and return everything the server wrote to stderr.
    fn finish(mut self) -> String {
        drop(self.child.stdin.take());
        let out = self.child.wait_with_output().unwrap();
        String::from_utf8_lossy(&out.stderr).into_owned()
    }
}

fn contains_ci(haystack: &str, needle: &str) -> bool {
    haystack.to_lowercase().contains(&needle.to_lowercase())
}

fn bytes_contain_ci(haystack: &[u8], needle: &str) -> bool {
    let lower: Vec<u8> = haystack.iter().map(u8::to_ascii_lowercase).collect();
    let needle = needle.to_lowercase();
    lower.windows(needle.len()).any(|w| w == needle.as_bytes())
}

/// One row per tool and string-bearing argument field; `{C}` marks where the canary goes.
fn table(fact: &str, other: &str) -> Vec<(&'static str, &'static str, Value)> {
    vec![
        ("remember", "kind", json!({"kind": "{C}", "text": "a note"})),
        (
            "remember",
            "text",
            json!({"kind": "Fact", "text": "a note with {C} inside"}),
        ),
        (
            "remember",
            "summary",
            json!({"kind": "Fact", "text": "summary carrier", "summary": "{C}"}),
        ),
        (
            "remember",
            "tags",
            json!({"kind": "Fact", "text": "tag carrier", "tags": ["{C}"]}),
        ),
        (
            "remember",
            "workspace_id",
            json!({"kind": "Fact", "text": "workspace carrier", "workspace_id": "{C}"}),
        ),
        (
            "remember",
            "source_session",
            json!({"kind": "Fact", "text": "session carrier", "source_session": "{C}"}),
        ),
        (
            "remember",
            "agent_id",
            json!({"kind": "Fact", "text": "agent carrier", "agent_id": "{C}"}),
        ),
        (
            "remember",
            "props leaf",
            json!({"kind": "Fact", "text": "props carrier", "props": {"note": "{C}"}}),
        ),
        (
            "remember",
            "props nested leaf",
            json!({"kind": "Fact", "text": "nested props carrier", "props": {"a": {"b": ["{C}"]}}}),
        ),
        (
            "remember",
            "props key",
            json!({"kind": "Fact", "text": "props key carrier", "props": {"{C}": "v"}}),
        ),
        (
            "remember",
            "links.rel",
            json!({"kind": "Fact", "text": "link rel carrier", "links": [{"rel": "{C}", "to": fact}]}),
        ),
        (
            "remember",
            "links.to",
            json!({"kind": "Fact", "text": "link to carrier", "links": [{"rel": "RELATES_TO", "to": "{C}"}]}),
        ),
        (
            "remember",
            "links.from",
            json!({"kind": "Fact", "text": "link from carrier", "links": [{"rel": "RELATES_TO", "from": "{C}"}]}),
        ),
        (
            "link",
            "from",
            json!({"from": "{C}", "to": fact, "rel": "RELATES_TO"}),
        ),
        (
            "link",
            "to",
            json!({"from": fact, "to": "{C}", "rel": "RELATES_TO"}),
        ),
        (
            "link",
            "rel",
            json!({"from": fact, "to": other, "rel": "{C}"}),
        ),
        (
            "link",
            "props leaf",
            json!({"from": fact, "to": other, "rel": "RELATES_TO", "props": {"note": "{C}"}}),
        ),
        ("recall", "query", json!({"query": "{C}"})),
        (
            "recall",
            "kinds",
            json!({"query": "note", "kinds": ["{C}"]}),
        ),
        ("recall", "tags", json!({"query": "note", "tags": ["{C}"]})),
        (
            "recall",
            "workspace_id",
            json!({"query": "note", "workspace_id": "{C}"}),
        ),
        ("traverse", "start_ids", json!({"start_ids": ["{C}"]})),
        (
            "traverse",
            "rels",
            json!({"start_ids": [fact], "rels": ["{C}"]}),
        ),
        (
            "traverse",
            "direction",
            json!({"start_ids": [fact], "direction": "{C}"}),
        ),
        (
            "traverse",
            "node_kinds",
            json!({"start_ids": [fact], "node_kinds": ["{C}"]}),
        ),
        (
            "traverse",
            "workspace_id",
            json!({"start_ids": [fact], "workspace_id": "{C}"}),
        ),
        ("forget", "id", json!({"id": "{C}"})),
        ("forget", "mode", json!({"id": fact, "mode": "{C}"})),
        (
            "schema_extend",
            "kind",
            json!({"kind": "{C}", "name": "Thing", "description": "d"}),
        ),
        (
            "schema_extend",
            "name",
            json!({"kind": "label", "name": "{C}", "description": "d"}),
        ),
        (
            "schema_extend",
            "description",
            json!({"kind": "label", "name": "Thing", "description": "{C}"}),
        ),
        (
            "schema_extend",
            "from_kinds",
            json!({"kind": "rel", "name": "POINTS_AT", "description": "d", "from_kinds": ["{C}"]}),
        ),
        (
            "schema_extend",
            "to_kinds",
            json!({"kind": "rel", "name": "POINTS_AT", "description": "d", "to_kinds": ["{C}"]}),
        ),
        (
            "schema_extend",
            "props_schema leaf",
            json!({"kind": "label", "name": "Thing", "description": "d", "props_schema": {"title": "{C}"}}),
        ),
        ("stats", "workspace_id", json!({"workspace_id": "{C}"})),
        ("accept", "id", json!({"id": "{C}"})),
        (
            "accept",
            "reason",
            json!({"id": fact, "reason": "{C}", "path_scope": ["src/**"]}),
        ),
        (
            "accept",
            "path_scope",
            json!({"id": fact, "reason": "r", "path_scope": ["{C}"]}),
        ),
        (
            "accept",
            "signer",
            json!({"id": fact, "reason": "r", "path_scope": ["src/**"], "signer": "{C}"}),
        ),
    ]
}

/// Replace the `{C}` marker in every string of `v` with the canary.
fn fill(v: &Value) -> Value {
    match v {
        Value::String(s) => Value::String(s.replace("{C}", CANARY)),
        Value::Array(a) => Value::Array(a.iter().map(fill).collect()),
        Value::Object(o) => Value::Object(
            o.iter()
                .map(|(k, x)| (k.replace("{C}", CANARY), fill(x)))
                .collect(),
        ),
        other => other.clone(),
    }
}

#[test]
fn a_redact_class_canary_in_any_argument_field_of_any_tool_never_leaves_the_server_raw() {
    let s = Scratch::new("kaban-field-gate", "table");
    // a note written before the policy knew the term still carries it raw in the index
    let mut legacy = Server::start(&s, false);
    let legacy_id = legacy.call_ok(
        "remember",
        &json!({"kind": "Fact", "text": format!("legacy note mentioning {CANARY} once")}),
    )["id"]
        .as_str()
        .unwrap()
        .to_string();
    legacy.finish();
    let mut srv = Server::start(&s, true);
    // the sanitized query is what reaches FTS, so the raw term never reaches the index; the
    // note itself is either still raw (the C2.7 startup rescan has not reached it yet: no
    // match) or already masked in place (the masked query matches the masked text, R1-3):
    // either way nothing raw comes back
    let found = srv.call_ok("recall", &json!({"query": CANARY}));
    assert!(
        !contains_ci(&found.to_string(), CANARY),
        "the raw term left the server: {found}"
    );
    let fact = srv.call_ok("remember", &json!({"kind": "Fact", "text": "anchor note"}))["id"]
        .as_str()
        .unwrap()
        .to_string();
    let other = srv.call_ok(
        "remember",
        &json!({"kind": "Fact", "text": "second anchor"}),
    )["id"]
        .as_str()
        .unwrap()
        .to_string();
    let mut failures = Vec::new();
    for (tool, field, args) in table(&fact, &other) {
        let args = fill(&args);
        assert!(
            contains_ci(&args.to_string(), CANARY),
            "{tool}.{field}: the row does not carry the canary"
        );
        let resp = srv.call(tool, &args);
        let text = resp.to_string();
        assert!(
            resp.get("result").is_some() || resp.get("error").is_some(),
            "{tool}.{field}: no JSON-RPC reply: {text}"
        );
        if contains_ci(&text, CANARY) {
            failures.push(format!("{tool}.{field}: {text}"));
        }
    }
    // the server is still healthy after the whole table
    let stats = srv.call_ok("stats", &json!({}));
    assert!(stats["nodes"].as_u64().unwrap() >= 3, "{stats}");
    // the legacy note is the one place the term legitimately persisted: purge it through the
    // server, then every byte left behind (db, -wal, -shm, export) must be free of the canary
    srv.call_ok("forget", &json!({"id": legacy_id, "mode": "purge"}));
    let stderr = srv.finish();
    assert!(
        failures.is_empty(),
        "{} field(s) echoed the canary:\n{}",
        failures.len(),
        failures.join("\n")
    );
    assert!(!contains_ci(&stderr, CANARY), "canary in trace output");
    assert!(
        stderr.contains("kaban ready"),
        "stderr expected at RUST_LOG=trace"
    );
    let bytes = s.persisted_bytes();
    assert!(bytes.len() > 1024, "database and export expected on disk");
    assert!(
        !bytes_contain_ci(&bytes, CANARY),
        "canary in db/-wal/-shm/audit-export bytes"
    );
    assert!(s.export().is_file(), "redactions were exported");
}
