//! Spec 05 SS-10: the write receipt is one typed value whatever the transport. The same
//! `remember` request through the MCP server and through the CLI yields the same receipt
//! object (same keys, identities and outcome; the CLI's position is at or after the server's,
//! since a duplicate `remember` writes nothing new); `committed` exits 0; a refused write keeps
//! its reason-specific status and carries the same `rejected` receipt on both transports (the
//! CLI prints it on stderr beside the error, stdout staying empty; the MCP reply carries it as
//! a second text block beside the error text).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::{Scratch, Server};
use serde_json::{Value, json};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

const DEADLINE: Duration = Duration::from_secs(10);
const WORKSPACE: &str = "default";
const UNKNOWN_ID: &str = "01a17a03-5cbc-7000-8000-000000000000";

fn kaban(s: &Scratch) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_kaban"));
    cmd.env("KABAN_HOME", s.home())
        .env("RUST_LOG", "warn")
        .args(["--db", s.db().to_str().unwrap(), "--workspace", WORKSPACE])
        .stdin(Stdio::null());
    cmd
}

fn run(s: &Scratch, args: &[&str]) -> Output {
    kaban(s).args(args).output().unwrap()
}

/// stdout of a run as its one JSON document (the caller checks the status).
fn document(out: &Output) -> Value {
    let text = String::from_utf8(out.stdout.clone()).unwrap();
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("one JSON document: {e}: {text}"))
}

fn call(srv: &mut Server, tool: &str, args: &Value) -> Value {
    let id = srv.request("tools/call", &json!({"name": tool, "arguments": args}));
    srv.response(id, DEADLINE).unwrap()["result"].clone()
}

#[test]
fn the_same_remember_yields_the_same_receipt_over_mcp_and_the_cli() {
    let s = Scratch::new("kaban-receipt", "transports");
    let text = "receipts are transport neutral";
    let mut srv = Server::start_with(&s, &["--workspace", WORKSPACE], DEADLINE).0;
    let over_mcp = call(&mut srv, "remember", &json!({"kind": "Fact", "text": text}));
    assert_ne!(over_mcp["isError"], json!(true), "{over_mcp}");
    let structured = over_mcp["structuredContent"].clone();
    assert_eq!(
        serde_json::from_str::<Value>(over_mcp["content"][0]["text"].as_str().unwrap()).unwrap(),
        structured,
        "text and structured forms agree"
    );
    srv.finish();

    let out = run(&s, &["remember", "Fact", text]);
    assert_eq!(out.status.code(), Some(0), "committed exits 0");
    let over_cli = document(&out);
    assert_eq!(over_cli["status"], "existing", "{over_cli}");
    assert_eq!(over_cli["id"], structured["id"]);

    let (mcp_receipt, cli_receipt) = (&structured["receipt"], &over_cli["receipt"]);
    let keys = |r: &Value| -> Vec<String> { r.as_object().unwrap().keys().cloned().collect() };
    assert_eq!(
        keys(mcp_receipt),
        keys(cli_receipt),
        "{structured}\n{over_cli}"
    );
    assert_eq!(mcp_receipt["outcome"], json!("committed"));
    assert_eq!(cli_receipt["outcome"], json!("committed"));
    assert_eq!(mcp_receipt["instance_id"], cli_receipt["instance_id"]);
    assert_eq!(
        mcp_receipt["pack_lineage_id"],
        cli_receipt["pack_lineage_id"]
    );
    let (a, b) = (
        mcp_receipt["seq"].as_u64().unwrap(),
        cli_receipt["seq"].as_u64().unwrap(),
    );
    assert!(a > 0 && b >= a, "positions: mcp {a}, cli {b}");
}

/// The `{"receipt": ...}` document a transport attaches to a refused write.
fn receipt_document(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or_else(|e| panic!("a receipt document: {e}: {text}"))
}

#[test]
fn a_refused_write_keeps_its_status_and_carries_the_same_rejected_receipt_on_both_transports() {
    let s = Scratch::new("kaban-receipt", "refused");
    // the database exists before either transport refuses the write
    let seeded = run(&s, &["remember", "Fact", "a seeded note"]);
    assert_eq!(seeded.status.code(), Some(0), "{seeded:?}");

    let out = run(&s, &["forget", UNKNOWN_ID]);
    assert_eq!(out.status.code(), Some(3), "not found keeps its status");
    assert!(
        out.stdout.is_empty(),
        "stdout stays empty on error: {out:?}"
    );
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        stderr.starts_with("error: "),
        "the error line first: {stderr}"
    );
    let line = stderr
        .lines()
        .find_map(|l| l.strip_prefix("receipt: "))
        .unwrap_or_else(|| panic!("a receipt line on stderr: {stderr}"));
    let cli_receipt = receipt_document(line)["receipt"].clone();

    let mut srv = Server::start_with(&s, &["--workspace", WORKSPACE], DEADLINE).0;
    let over_mcp = call(&mut srv, "forget", &json!({"id": UNKNOWN_ID}));
    srv.finish();
    assert_eq!(over_mcp["isError"], json!(true), "{over_mcp}");
    let blocks = over_mcp["content"].as_array().unwrap();
    assert_eq!(
        blocks.len(),
        2,
        "the error text and the receipt: {over_mcp}"
    );
    assert!(
        blocks[0]["text"].as_str().unwrap().contains("not found"),
        "{over_mcp}"
    );
    let mcp_receipt = receipt_document(blocks[1]["text"].as_str().unwrap())["receipt"].clone();

    for (transport, receipt) in [("cli", &cli_receipt), ("mcp", &mcp_receipt)] {
        assert_eq!(
            receipt["outcome"],
            json!("rejected"),
            "{transport}: {receipt}"
        );
        assert_eq!(
            receipt["seq"],
            Value::Null,
            "{transport}: no position: {receipt}"
        );
        assert!(
            receipt["instance_id"]
                .as_str()
                .is_some_and(|s| s.len() == 36),
            "{transport}: {receipt}"
        );
    }
    assert_eq!(
        cli_receipt, mcp_receipt,
        "one typed receipt whatever the transport"
    );
}
