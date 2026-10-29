//! CLI/MCP same-fixture parity, root schemas, primitive booleans and structured content.
#![cfg(feature = "ingest")]
#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;
use common::{Scratch, Server};
use serde_json::{Value, json};
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Duration;

/// `report` with the receipt's per-database identities removed.
fn without_identities(report: &Value) -> Value {
    let mut v = report.clone();
    if let Some(receipt) = v.get_mut("receipt").and_then(Value::as_object_mut) {
        receipt.remove("instance_id");
        receipt.remove("pack_lineage_id");
    }
    v
}

fn call(server: &mut Server, tool: &str, args: &Value) -> Value {
    let id = server.request("tools/call", &json!({"name":tool,"arguments":args}));
    let response = server.response(id, Duration::from_secs(10)).unwrap();
    assert_ne!(response["result"]["isError"], true, "{response}");
    let text: Value =
        serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(text, response["result"]["structuredContent"]);
    text
}
fn cli(s: &Scratch, root: &std::path::Path, args: &[&str], body: Option<&Value>) -> Value {
    let mut child = Command::new(env!("CARGO_BIN_EXE_kaban"))
        .env("KABAN_HOME", s.home())
        .env("RUST_LOG", "warn")
        .current_dir(root)
        .args(["--db", s.db().to_str().unwrap(), "--json"])
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    if let Some(body) = body {
        child
            .stdin
            .take()
            .unwrap()
            .write_all(body.to_string().as_bytes())
            .unwrap();
    }
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{out:?}");
    serde_json::from_slice(&out.stdout).unwrap()
}
fn rows(s: &Scratch) -> Vec<(String, String, String, String)> {
    let conn = rusqlite::Connection::open(s.db()).unwrap();
    kaban_store_sqlite::register_body_reader(&conn).unwrap();
    conn.prepare("SELECT id,kind,kaban_body(text),props FROM node WHERE kind='Fact' ORDER BY id")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

#[test]
fn cli_and_mcp_place_identical_rows_and_canonical_finish_reports() {
    let cli_home = Scratch::new("kaban-e310-parity", "cli");
    let mcp_home = Scratch::new("kaban-e310-parity", "mcp");
    let root = cli_home.0.join("repo");
    std::fs::create_dir_all(root.join(".git")).unwrap();
    std::fs::write(root.join("a.md"), "# Source\n\nHarmless body.\n").unwrap();
    let prepared = cli(&cli_home, &root, &["ingest", "job", "brief", "a.md"], None);
    let job = prepared["job"].as_str().unwrap();
    let body = json!({"job":job,"records":[{"local_id":"r","kind":"Fact","text":"Source-supported record"}]});
    let submitted = cli(
        &cli_home,
        &root,
        &["ingest", "job", "submit", job, "--stdin-json"],
        Some(&body),
    );
    assert_eq!(submitted["created"], 1);
    let report = cli(&cli_home, &root, &["ingest", "job", "finish", job], None);
    let mut server = Server::start_at(&mcp_home, &root);
    let mcp_prepared = call(&mut server, "ingest_brief", &json!({"path":"a.md"}));
    assert_eq!(mcp_prepared["job"], prepared["job"]);
    let mcp_submitted = call(&mut server, "ingest_submit", &body);
    assert_eq!(mcp_submitted, submitted);
    let mcp_report = call(&mut server, "ingest_finish", &json!({"job":job}));
    // two databases: the canonical report agrees byte for byte except the receipt's own
    // identities (SS-10: each instance signs its receipt with its `db_id` and lineage)
    assert_eq!(without_identities(&mcp_report), without_identities(&report));
    assert_eq!(mcp_report["receipt"]["outcome"], "committed");
    assert_ne!(
        mcp_report["receipt"]["instance_id"],
        report["receipt"]["instance_id"]
    );
    assert_eq!(
        call(&mut server, "ingest_finish", &json!({"job":job})),
        mcp_report
    );
    assert_eq!(rows(&mcp_home), rows(&cli_home));
    let id = server.request("tools/list", &json!({}));
    let catalog = server.response(id, Duration::from_secs(10)).unwrap();
    let tools = catalog["result"]["tools"].as_array().unwrap();
    let finish = tools.iter().find(|t| t["name"] == "ingest_finish").unwrap();
    assert_eq!(finish["outputSchema"]["type"], "object");
    assert_eq!(finish["annotations"]["readOnlyHint"], false);
    let next = tools.iter().find(|t| t["name"] == "ingest_next").unwrap();
    assert_eq!(next["annotations"]["readOnlyHint"], false);
    let invalid = server.request(
        "tools/call",
        &json!({"name":"ingest_finish","arguments":{"job":job,"unknown":"AKIAQWERTYUIOPASDFGH"}}),
    );
    let refused = server.response(invalid, Duration::from_secs(10)).unwrap();
    assert_eq!(refused["result"]["isError"], true);
    assert!(!refused.to_string().contains("AKIAQWERTYUIOPASDFGH"));
    server.finish();
}
