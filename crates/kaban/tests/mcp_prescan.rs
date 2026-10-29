//! Raw MCP arguments are scanned as a JSON value before typed deserialization, so a serde
//! error can never echo a secret, and tracing never logs argument values (core-spec S1;
//! core-plan C1.5; m2 Me7). Handlers run on the sanitized argument tree and every outgoing
//! content and error string passes the response gate (review R1-3). Black-box over stdio with
//! `RUST_LOG=trace`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::Scratch;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

const CANARY: &str = "Sup3rS3cretValue!";

struct Server {
    child: Child,
    out: BufReader<std::process::ChildStdout>,
    next_id: u64,
}

impl Server {
    fn start(s: &Scratch) -> Self {
        Self::start_with_policy(s, None)
    }
    /// Start the server on the scratch database, optionally with a policy file.
    fn start_with_policy(s: &Scratch, policy: Option<&std::path::Path>) -> Self {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_kaban"));
        cmd.env("KABAN_HOME", s.0.join("home"))
            .env("RUST_LOG", "trace")
            .args(["--db", s.0.join("kaban.db").to_str().unwrap()]);
        if let Some(policy) = policy {
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
        srv.notify("notifications/initialized");
        srv
    }
    fn send(&mut self, msg: &Value) {
        let stdin = self.child.stdin.as_mut().unwrap();
        stdin.write_all(msg.to_string().as_bytes()).unwrap();
        stdin.write_all(b"\n").unwrap();
        stdin.flush().unwrap();
    }
    fn notify(&mut self, method: &str) {
        self.send(&json!({"jsonrpc": "2.0", "method": method}));
    }
    fn request(&mut self, method: &str, params: &Value) -> Value {
        self.next_id += 1;
        self.send(
            &json!({"jsonrpc": "2.0", "id": self.next_id, "method": method, "params": params}),
        );
        let mut line = String::new();
        self.out.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    }
    fn call(&mut self, tool: &str, args: &Value) -> Value {
        self.request("tools/call", &json!({"name": tool, "arguments": args}))
    }
    /// Close stdin, wait, and return everything the server wrote to stderr.
    fn finish(mut self) -> String {
        drop(self.child.stdin.take());
        let out = self.child.wait_with_output().unwrap();
        String::from_utf8_lossy(&out.stderr).into_owned()
    }
}

fn tool_error_text(resp: &Value) -> String {
    if let Some(err) = resp.get("error") {
        return err.to_string();
    }
    let res = &resp["result"];
    assert_eq!(res["isError"], json!(true), "expected a tool error: {resp}");
    res["content"][0]["text"].as_str().unwrap().to_string()
}

#[test]
fn malformed_typed_args_with_a_secret_are_rejected_with_rule_ids_only() {
    let s = Scratch::new("kaban-prescan", "malformed");
    let mut srv = Server::start(&s);
    // `kind` has the wrong type, so typed deserialization would fail; the raw scan runs first
    let resp = srv.call(
        "remember",
        &json!({"kind": 5, "text": format!("deploy password = {CANARY} for staging")}),
    );
    let text = tool_error_text(&resp);
    assert!(
        text.contains("generic-credential-assignment"),
        "rule id expected: {text}"
    );
    assert!(
        !text.contains(CANARY) && !text.contains("Sup3r"),
        "canary echoed: {text}"
    );
    // a nested secret in an unknown field is caught too (the typed struct would ignore it)
    let resp = srv.call(
        "remember",
        &json!({"kind": "Fact", "text": "harmless", "extra": {"db": {"password": CANARY}}}),
    );
    let text = tool_error_text(&resp);
    assert!(
        text.contains("REJECTED") && !text.contains(CANARY),
        "{text}"
    );
    // clean but malformed arguments still produce a type error, without the values
    let resp = srv.call("forget", &json!({"id": "x", "mode": "n0t-a-m0de-Zq8v"}));
    let text = tool_error_text(&resp);
    assert!(
        !text.contains("Zq8v"),
        "serde error echoed an argument value: {text}"
    );
    // and valid calls keep working after the rejections
    let resp = srv.call(
        "remember",
        &json!({"kind": "Fact", "text": "harmless note"}),
    );
    assert_ne!(resp["result"]["isError"], json!(true), "{resp}");
    let stderr = srv.finish();
    assert!(
        !stderr.contains(CANARY) && !stderr.contains("Sup3r"),
        "canary in trace output"
    );
    assert!(
        !stderr.contains("harmless note"),
        "argument value in trace output"
    );
    assert!(
        stderr.contains("tool=remember") || stderr.contains("tool: remember"),
        "call_tool span with the tool name expected in trace output"
    );
}

/// The shipped example policy redacts this term (`old-password`); it is not a secret.
const REDACT_CANARY: &str = "hunter2";

fn example_policy() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/policy.yaml")
}

fn contains_ci(haystack: &str, needle: &str) -> bool {
    haystack.to_lowercase().contains(&needle.to_lowercase())
}

/// Review R1-3: a redact-class value in any argument (`forget.id`, `recall.query`,
/// `traverse.start_ids`) never reaches a handler raw, lookup errors never echo it, and every
/// content string leaving the adapter is gated even when the stored value predates the policy.
#[test]
fn redact_class_arguments_and_stored_strings_never_leave_the_adapter_raw() {
    let s = Scratch::new("kaban-prescan", "response-gate");
    // 1) data written under the builtin policy only: a legacy kind named after the term
    let mut legacy = Server::start(&s);
    let resp = legacy.call(
        "schema_extend",
        &json!({"kind": "label", "name": "Hunter2", "description": "a legacy kind"}),
    );
    assert_ne!(resp["result"]["isError"], json!(true), "{resp}");
    let resp = legacy.call(
        "remember",
        &json!({"kind": "Hunter2", "text": "legacy note about migrating ducks"}),
    );
    assert_ne!(resp["result"]["isError"], json!(true), "{resp}");
    let legacy_id = {
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        serde_json::from_str::<Value>(text).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string()
    };
    legacy.finish();

    // 2) the same database under the shipped policy, which redacts the term
    let mut srv = Server::start_with_policy(&s, Some(&example_policy()));
    let resp = srv.call("forget", &json!({"id": REDACT_CANARY}));
    let text = tool_error_text(&resp);
    assert!(
        !contains_ci(&text, REDACT_CANARY),
        "forget echoed the raw id: {text}"
    );
    let resp = srv.call(
        "recall",
        &json!({"query": format!("{REDACT_CANARY} ducks")}),
    );
    assert!(
        !contains_ci(&resp.to_string(), REDACT_CANARY),
        "recall response carries the raw query term: {resp}"
    );
    let resp = srv.call("traverse", &json!({"start_ids": [REDACT_CANARY]}));
    assert!(
        !contains_ci(&resp.to_string(), REDACT_CANARY),
        "traverse echoed the raw start id: {resp}"
    );
    // stored strings that predate the policy: node kind, schema name, stats key
    for (tool, args) in [
        (
            "recall",
            json!({"query": "migrating ducks", "full_text": true}),
        ),
        ("traverse", json!({"start_ids": [legacy_id]})),
        ("schema_describe", json!({})),
        ("stats", json!({})),
    ] {
        let resp = srv.call(tool, &args);
        assert!(
            !contains_ci(&resp.to_string(), REDACT_CANARY),
            "{tool} returned a stored redact-class string raw: {resp}"
        );
    }
    for uri in ["kaban://schema", "kaban://stats"] {
        let resp = srv.request("resources/read", &json!({"uri": uri}));
        assert!(
            !contains_ci(&resp.to_string(), REDACT_CANARY),
            "{uri} returned a stored redact-class string raw: {resp}"
        );
    }
    // masking keeps the result well-formed: the kind is masked, its count stays a number
    let resp = srv.call("stats", &json!({}));
    let stats: Value =
        serde_json::from_str(resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    let by_kind = stats["by_kind"].as_object().unwrap();
    assert!(
        by_kind.contains_key("[REDACTED:old-password]") && by_kind.values().all(Value::is_u64),
        "{stats}"
    );
    // a clean call still works after all that
    let resp = srv.call(
        "remember",
        &json!({"kind": "Fact", "text": "harmless note"}),
    );
    assert_ne!(resp["result"]["isError"], json!(true), "{resp}");
    let stderr = srv.finish();
    assert!(
        !contains_ci(&stderr, REDACT_CANARY),
        "redact-class value in trace output"
    );
}
