//! The document-ingestion MCP surface (docs-ingest-spec D7, D10; docs-ingest-plan.md DI1.6):
//! `index_docs` and `index_status` in `tools/list` with their hints, one `index_docs` run over
//! the committed Markdown fixtures of `kaban-ingest` (the server's workspace root is this
//! repository), the `index_status` golden, the `kaban://documents` resource with its listing
//! and pagination, and one recall chunk card golden.

#![cfg(feature = "ingest")] // the surface under test exists with the feature only (CG7.3)
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::{Scratch, Server};
use serde_json::{Value, json};
use std::time::Duration;

/// The committed fixtures, relative to the repository root (the server's workspace root).
const FIXTURES: &str = "crates/kaban-ingest/tests/fixtures/md";

// The transferred qualification source need not be a Git checkout. Bind the
// fixture root explicitly instead of relying on nextest's crate-directory cwd.
fn server(scratch: &Scratch) -> Server {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    Server::start_at(scratch, &root)
}

fn call_raw(srv: &mut Server, tool: &str, args: &Value) -> Value {
    let id = srv.request("tools/call", &json!({"name": tool, "arguments": args}));
    srv.response(id, Duration::from_secs(60)).unwrap()["result"].clone()
}

fn call(srv: &mut Server, tool: &str, args: &Value) -> Value {
    let result = call_raw(srv, tool, args);
    assert_ne!(result["isError"], json!(true), "{tool} failed: {result}");
    result["structuredContent"].clone()
}

fn read_resource(srv: &mut Server, uri: &str) -> Value {
    let id = srv.request("resources/read", &json!({"uri": uri}));
    let resp = srv.response(id, Duration::from_secs(60)).unwrap();
    let text = resp["result"]["contents"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("{resp}"));
    serde_json::from_str(text).unwrap()
}

#[test]
fn index_docs_and_index_status_are_listed_with_their_hints() {
    // Arrange
    let s = Scratch::new("kaban-mcp-docs", "list");
    let mut srv = server(&s);

    // Act
    let id = srv.request("tools/list", &json!({}));
    let tools = srv.response(id, Duration::from_secs(10)).unwrap()["result"]["tools"].clone();
    let id = srv.request("resources/list", &json!({}));
    let resources =
        srv.response(id, Duration::from_secs(10)).unwrap()["result"]["resources"].clone();
    srv.finish();

    // Assert
    let by_name = |n: &str| {
        tools
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == n)
            .cloned()
            .unwrap_or_else(|| panic!("{n} missing: {tools}"))
    };
    let docs = by_name("index_docs");
    assert_eq!(docs["annotations"]["destructiveHint"], false);
    assert_eq!(docs["annotations"]["readOnlyHint"], false);
    let status = by_name("index_status");
    assert_eq!(status["annotations"]["readOnlyHint"], true);
    let uris: Vec<&str> = resources
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["uri"].as_str().unwrap())
        .collect();
    assert!(uris.contains(&"kaban://documents"), "{uris:?}");
}

#[test]
fn index_docs_over_the_fixtures_then_status_resource_and_a_chunk_card() {
    // Arrange
    let s = Scratch::new("kaban-mcp-docs", "run");
    let mut srv = server(&s);

    // Act
    let report = call(&mut srv, "index_docs", &json!({"paths": [FIXTURES]}));
    let again = call(&mut srv, "index_docs", &json!({"paths": [FIXTURES]}));
    let mut status = call(&mut srv, "index_status", &json!({}));
    let resource = read_resource(&mut srv, "kaban://documents");
    let page = read_resource(&mut srv, "kaban://documents?offset=1&limit=2");
    let recall = call(
        &mut srv,
        "recall",
        &json!({"query": "tagged project sub-topic", "kinds": ["Chunk"], "hops": 0, "limit": 1}),
    );
    let other = call_raw(
        &mut srv,
        "index_docs",
        &json!({"paths": [FIXTURES], "workspace": "other"}),
    );
    srv.finish();

    // Assert: the run, the idempotent re-run
    assert_eq!(report["documents"], 4, "{report}");
    assert!(report["outbox_rows"].as_u64().unwrap() > 0);
    assert_eq!(again["documents"], 0, "{again}");
    assert_eq!(again["skipped"], 4);
    assert_eq!(again["outbox_rows"], 0);
    // DI1.8b: the wire report carries the DI1.8 counts as their own fields
    assert_eq!(report["skipped_forgotten"], 0, "{report}");
    assert_eq!(report["sections_tombstoned"], 0, "{report}");
    assert_eq!(again["skipped_forgotten"], 0, "{again}");
    // the status golden (the stamp and lag vary); DI2.5: the Markdown fixtures flag no
    // `table_unknown` chunk, so the zero total is omitted from the wire
    assert!(status["last_run"].is_string(), "{status}");
    assert!(status.get("table_unknown_chunks").is_none(), "{status}");
    status["last_run"] = json!("[last_run]");
    status["outbox_lag"] = json!("[outbox_lag]");
    // This suite also qualifies the full download profile; assert the actual optional
    // extractor before comparing the feature-independent status shape.
    let extractors = status["extractors"].as_array_mut().unwrap();
    let pdf = extractors
        .iter()
        .position(|value| value == "pdf_oxide@0.3.78");
    assert_eq!(
        pdf.is_some(),
        kaban_ingest::extractor_for(kaban_ingest::Format::Pdf).is_some()
    );
    if let Some(index) = pdf {
        extractors.remove(index);
    }
    insta::assert_json_snapshot!("index_status_fixtures", status);
    // the resource: status plus the listing (the store's id order), paginated
    assert_eq!(
        resource["status"]["documents_by_status"]["ok"], 4,
        "{resource}"
    );
    let paths: Vec<&str> = resource["documents"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["path"].as_str().unwrap())
        .collect();
    assert!(
        paths.contains(&"crates/kaban-ingest/tests/fixtures/md/links.md"),
        "{paths:?}"
    );
    assert_eq!(resource["more"], false);
    assert_eq!(page["offset"], 1);
    assert_eq!(page["limit"], 2);
    assert_eq!(page["documents"].as_array().unwrap().len(), 2);
    assert_eq!(page["documents"][0]["path"], paths[1]);
    assert_eq!(page["more"], true);
    // one chunk card golden (ids, times and scores vary)
    let mut card = recall["results"][0].clone();
    assert_eq!(card["kind"], "Chunk", "{recall}");
    for key in ["id", "created_at", "score"] {
        card[key] = json!(format!("[{key}]"));
    }
    card["workspace_id"] = json!("[workspace]");
    insta::assert_json_snapshot!("recall_chunk_card", card);
    // a workspace other than the bound one is refused (the process binds the workspace)
    assert_eq!(other["isError"], true, "{other}");
}

/// DI2.0 (a): two `index_docs` calls over one workspace started concurrently through the MCP
/// server serialise on the process-wide ingest lock: one of them ingests the fixtures, the other
/// waits and then skips them all (it sees the first one's commits), and the final graph is the
/// graph one call builds: the same status counts and the same chunk ids as a single-call server,
/// nothing duplicated, nothing lost, every call stamping `last_run` after its own run.
#[test]
fn two_concurrent_index_docs_calls_over_one_workspace_serialise() {
    // Arrange: a server for the pair and a reference server for the single call
    let s = Scratch::new("kaban-mcp-docs", "concurrent");
    let mut srv = server(&s);
    let r = Scratch::new("kaban-mcp-docs", "concurrent-ref");
    let mut reference = server(&r);
    // the listing (path, status, chunks per document) stands for the graph: ids are derived
    let listing = |srv: &mut Server| -> Value {
        let mut body = read_resource(srv, "kaban://documents?limit=500");
        assert!(body["status"]["last_run"].is_string(), "{body}");
        body["status"]["last_run"] = json!("[last_run]");
        body["status"]["outbox_lag"] = json!("[outbox_lag]");
        body
    };

    // Act: both requests on the wire before either response is read
    let first = srv.request(
        "tools/call",
        &json!({"name": "index_docs", "arguments": {"paths": [FIXTURES]}}),
    );
    let second = srv.request(
        "tools/call",
        &json!({"name": "index_docs", "arguments": {"paths": [FIXTURES]}}),
    );
    let a = srv.response(first, Duration::from_secs(60)).unwrap()["result"].clone();
    let b = srv.response(second, Duration::from_secs(60)).unwrap()["result"].clone();
    let after_pair = listing(&mut srv);
    srv.finish();
    let single = call(&mut reference, "index_docs", &json!({"paths": [FIXTURES]}));
    let after_single = listing(&mut reference);
    reference.finish();

    // Assert: both succeeded; exactly one of them did the work, the other skipped everything
    assert_ne!(a["isError"], json!(true), "{a}");
    assert_ne!(b["isError"], json!(true), "{b}");
    let (a, b) = (&a["structuredContent"], &b["structuredContent"]);
    let documents = a["documents"].as_u64().unwrap() + b["documents"].as_u64().unwrap();
    let skipped = a["skipped"].as_u64().unwrap() + b["skipped"].as_u64().unwrap();
    assert_eq!(single["documents"], 4, "{single}");
    assert_eq!(
        (documents, skipped),
        (4, 4),
        "the pair must behave like one run followed by a no-op re-run: {a} / {b}"
    );
    assert!(
        a["outbox_rows"].as_u64().unwrap() == 0 || b["outbox_rows"].as_u64().unwrap() == 0,
        "the waiting call must write nothing: {a} / {b}"
    );
    // the final graph is the single-call graph: status counts and the per-document listing
    // (the stamp and the lag vary per process; the stamp exists because each call set it)
    assert_eq!(
        after_pair, after_single,
        "the pair must leave the single-call graph"
    );
    assert_eq!(after_pair["status"]["chunks"], 15, "{after_pair}");
    assert_eq!(
        after_pair["documents"].as_array().unwrap().len(),
        8,
        "{after_pair}"
    );
}
