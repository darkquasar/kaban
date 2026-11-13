//! The full `tools/list` catalog as the wire sees it (core-spec §6; core-plan C3.7; insta):
//! every tool with its description, `inputSchema`, `outputSchema` and annotations. The snapshot
//! is the review surface for the MCP contract: a schema or annotation change shows up as a diff
//! here, and the fixed assertions pin the §6 annotation rules independently of the snapshot.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::{Scratch, Server};
use serde_json::{Value, json};
use std::process::Command;
use std::time::Duration;

/// Tools that only read (`readOnlyHint: true`).
#[cfg(all(feature = "ingest", feature = "code-graph"))]
const READS: [&str; 12] = [
    "recall",
    "traverse",
    "schema_describe",
    "stats",
    "index_status",
    "code_implementers",
    "code_members",
    "code_definition",
    "code_references",
    "code_tests_for",
    "code_neighbours",
    "code_context",
];

/// The one tool that can destroy data (`forget`, through its `purge` mode).
#[cfg(all(feature = "ingest", feature = "code-graph"))]
const DESTRUCTIVE: [&str; 2] = ["forget", "ingest_finish"];

/// The tools that exist today (engine-spec EN-F4, E1.5): their catalog entries are
/// byte-identical under the engine; later tools (EN-F50) are appended after them and never
/// change these. Sorted, like the wire.
const TODAY_TOOLS: [&str; 8] = [
    "forget",
    "link",
    "recall",
    "remember",
    "schema_describe",
    "schema_extend",
    "stats",
    "traverse",
];

fn tools_list() -> Vec<Value> {
    let s = Scratch::new("kaban-snapshots", "tools-list");
    let mut srv = Server::start(&s);
    let id = srv.request("tools/list", &json!({}));
    let resp = srv.response(id, Duration::from_secs(10)).unwrap();
    srv.finish();
    resp["result"]["tools"].as_array().unwrap().clone()
}

// the golden is the default catalog (every feature on); the feature-off build keeps the
// today-subset golden below (CG7.3)
#[cfg(all(feature = "ingest", feature = "code-graph"))]
#[test]
fn tools_list_declares_an_output_schema_and_the_section_6_annotations_on_every_tool() {
    let tools = tools_list();
    // the catalog is sorted by name on the wire (N5), so the snapshot is stable across runs
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    let mut sorted = names.clone();
    sorted.sort_unstable();
    assert_eq!(names, sorted, "tools/list is not sorted by name");
    for t in &tools {
        let name = t["name"].as_str().unwrap();
        assert!(
            t["outputSchema"].is_object(),
            "{name}: outputSchema missing: {t}"
        );
        // MCP 2025-06-18 and 2025-11-25 restrict `outputSchema` to a root `type: "object"`; a
        // schema whose root is only a `oneOf` (an enum response) is rejected by a strict client
        // (M3 review R3-8)
        assert_eq!(
            t["outputSchema"]["type"],
            json!("object"),
            "{name}: outputSchema root type: {}",
            t["outputSchema"]
        );
        let ann = &t["annotations"];
        assert_eq!(
            ann["readOnlyHint"],
            json!(READS.contains(&name)),
            "{name}: readOnlyHint: {ann}"
        );
        assert_eq!(
            ann["destructiveHint"],
            json!(DESTRUCTIVE.contains(&name)),
            "{name}: destructiveHint: {ann}"
        );
        assert_eq!(ann["openWorldHint"], json!(false), "{name}: {ann}");
    }
    insta::assert_json_snapshot!("tools_list", tools);
}

/// The today-subset golden (EN-F4): the catalog entries of [`TODAY_TOOLS`], each present,
/// pinned on their own so that appending a tool later (EN-F50) repins `tools_list` and leaves
/// this snapshot untouched.
#[test]
fn tools_list_today_subset_is_byte_identical_under_the_engine() {
    let tools = tools_list();
    let today: Vec<Value> = tools
        .into_iter()
        .filter(|t| TODAY_TOOLS.contains(&t["name"].as_str().unwrap()))
        .collect();
    let names: Vec<&str> = today.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(names, TODAY_TOOLS, "every tool of today is listed once");
    insta::assert_json_snapshot!("tools_list_today", today);
}

/// `ask` is absent in 3.0.0, not stubbed (engine-spec EN-F50, OWN-68 SC-02): no catalog entry,
/// no registry schema and no CLI verb carry the name; `kaban ask` is an unknown subcommand.
#[test]
fn ask_is_absent_from_the_catalog_the_registry_and_the_cli() {
    let tools = tools_list();
    assert!(
        !tools.iter().any(|t| t["name"].as_str().unwrap() == "ask"),
        "tools/list advertises `ask`"
    );
    assert!(
        !kaban_api::control::tool_schemas()
            .iter()
            .any(|t| t.tool == "ask")
    );
    let help = Command::new(env!("CARGO_BIN_EXE_kaban"))
        .arg("--help")
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&help.stdout);
    assert!(help.status.success(), "{text}");
    assert!(
        !text
            .lines()
            .any(|l| l.split_whitespace().next() == Some("ask")),
        "`kaban --help` lists an `ask` verb:\n{text}"
    );
    // the one verb that does exist beside it still parses (the negative is not a broken CLI)
    assert!(
        text.lines().any(|l| l.trim_start().starts_with("explain")),
        "{text}"
    );
    let ask = Command::new(env!("CARGO_BIN_EXE_kaban"))
        .arg("ask")
        .arg("question")
        .output()
        .unwrap();
    assert!(!ask.status.success());
    let err = String::from_utf8_lossy(&ask.stderr);
    // the CLI's own fixed text for an unknown verb (`main.rs`), never clap's echo of the name
    assert!(err.contains("subcommand is not recognized"), "{err}");
}
