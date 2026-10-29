//! The `code_*` tools as the wire sees them (stage1-spec §6, G1-F8, G1-S8; stage1-plan CG7.1):
//! listed with the read-only annotations under the `code-graph` feature, absent without it,
//! and the fixed `no_snapshot` text through a server started without a codegraph key.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::{Scratch, Server};
use serde_json::{Value, json};
use std::time::Duration;

const DEADLINE: Duration = Duration::from_secs(10);

/// The six tools of CG7.1 and CG7.2 plus CG9.1's `code_context`, sorted like the wire.
const CODE_TOOLS: [&str; 7] = [
    "code_context",
    "code_definition",
    "code_implementers",
    "code_members",
    "code_neighbours",
    "code_references",
    "code_tests_for",
];

fn tools_list(srv: &mut Server) -> Vec<Value> {
    let id = srv.request("tools/list", &json!({}));
    let resp = srv.response(id, DEADLINE).unwrap();
    resp["result"]["tools"].as_array().unwrap().clone()
}

#[cfg(feature = "code-graph")]
#[test]
fn code_tools_listed_with_read_only_annotations() {
    let s = Scratch::new("kaban-code", "listed");
    let mut srv = Server::start(&s);
    let tools = tools_list(&mut srv);
    for name in CODE_TOOLS {
        let t = tools
            .iter()
            .find(|t| t["name"] == json!(name))
            .unwrap_or_else(|| panic!("{name} missing from tools/list"));
        let ann = &t["annotations"];
        assert_eq!(ann["readOnlyHint"], json!(true), "{name}: {ann}");
        assert_eq!(ann["destructiveHint"], json!(false), "{name}: {ann}");
        assert_eq!(ann["idempotentHint"], json!(true), "{name}: {ann}");
        assert_eq!(ann["openWorldHint"], json!(false), "{name}: {ann}");
        assert_eq!(
            t["outputSchema"]["type"],
            json!("object"),
            "{name}: outputSchema root type (R3-8)"
        );
        // the selector is part of the input schema, with its one-of forms as properties
        let target = &t["inputSchema"]["properties"]["target"];
        assert!(target.is_object(), "{name}: target: {}", t["inputSchema"]);
        let desc = t["description"].as_str().unwrap();
        for form in ["symbol", "ref", "{label, name}", "{path, line}"] {
            assert!(
                desc.contains(form),
                "{name}: description names {form}: {desc}"
            );
        }
    }
    srv.finish();
}

/// A server without a codegraph key has no snapshot provider: every code tool answers the
/// fixed `no_snapshot` hint, and neither the error nor the trace names a path.
#[cfg(feature = "code-graph")]
#[test]
fn code_no_snapshot_through_mcp() {
    let s = Scratch::new("kaban-code", "no-snapshot");
    let mut srv = Server::start_traced(&s, &[]);
    let marker = "zq8v-no-such-file";
    for (name, args) in [
        (
            "code_definition",
            json!({"target": {"path": format!("src/{marker}.go")}}),
        ),
        (
            "code_members",
            json!({"target": {"label": "Struct", "name": marker}, "labels": ["Method"]}),
        ),
        (
            "code_implementers",
            json!({"target": {"ref": format!("scip:scip-go gomod example.com/x . `{marker}`/A#")}}),
        ),
        (
            "code_references",
            json!({"target": {"path": format!("src/{marker}.go")}, "include_tests": true}),
        ),
        (
            "code_tests_for",
            json!({"target": {"label": "Function", "name": marker}}),
        ),
        (
            "code_neighbours",
            json!({"target": {"path": format!("src/{marker}.go")}, "depth": 2}),
        ),
        (
            "code_context",
            json!({"target": {"path": format!("src/{marker}.go")}}),
        ),
    ] {
        let id = srv.request("tools/call", &json!({"name": name, "arguments": args}));
        let resp = srv.response(id, DEADLINE).unwrap();
        assert_eq!(resp["result"]["isError"], json!(true), "{name}: {resp}");
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert_eq!(
            text,
            "no_snapshot: no code-graph snapshot for this repository and commit; run `kaban-index index`",
            "{name}"
        );
        assert!(!text.contains(marker), "{name} echoed the target: {text}");
    }
    let (_, stderr) = srv.finish_with_stderr();
    assert!(
        !stderr.contains(marker),
        "the trace named the target:\n{stderr}"
    );
    assert!(
        !stderr.contains(&*s.0.to_string_lossy()),
        "the trace named a path:\n{stderr}"
    );
}

/// [red] CG7.3 (G1-F18): `kaban://code-index` is listed, and on a server without a snapshot
/// provider its body is the fixed `no_snapshot` sentence (content, not an error: the resource
/// exists, the snapshot does not), never a path.
#[cfg(feature = "code-graph")]
#[test]
fn code_index_no_snapshot() {
    let s = Scratch::new("kaban-code", "index-no-snapshot");
    let mut srv = Server::start_traced(&s, &[]);
    let id = srv.request("resources/list", &json!({}));
    let resp = srv.response(id, DEADLINE).unwrap();
    let uris: Vec<&str> = resp["result"]["resources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["uri"].as_str().unwrap())
        .collect();
    assert!(uris.contains(&"kaban://code-index"), "{uris:?}");
    let id = srv.request("resources/read", &json!({"uri": "kaban://code-index"}));
    let resp = srv.response(id, DEADLINE).unwrap();
    let contents = &resp["result"]["contents"][0];
    assert_eq!(contents["uri"], json!("kaban://code-index"), "{resp}");
    assert_eq!(
        contents["text"],
        json!(
            "no_snapshot: no code-graph snapshot for this repository and commit; run `kaban-index index`"
        ),
        "{resp}"
    );
    let (_, stderr) = srv.finish_with_stderr();
    assert!(
        !stderr.contains(&*s.0.to_string_lossy()),
        "the trace named a path:\n{stderr}"
    );
}

/// Without the feature the router has no code tool (plan CG7.1 negative path: the feature-off
/// build restores the previous tool list).
#[cfg(not(feature = "code-graph"))]
#[test]
fn code_tools_absent_without_feature() {
    let s = Scratch::new("kaban-code", "absent");
    let mut srv = Server::start(&s);
    let tools = tools_list(&mut srv);
    for name in CODE_TOOLS {
        assert!(
            !tools.iter().any(|t| t["name"] == json!(name)),
            "{name} listed without the code-graph feature"
        );
    }
    let id = srv.request(
        "tools/call",
        &json!({"name": "code_definition", "arguments": {"target": {"path": "x.go"}}}),
    );
    let resp = srv.response(id, DEADLINE).unwrap();
    assert_eq!(resp["error"]["message"], json!("tool not found"), "{resp}");
    srv.finish();
}
