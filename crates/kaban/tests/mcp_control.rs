//! Black-box tests of the field-class seam at the MCP adapter (core-spec S1, S4 as amended by
//! OWN-10; core-plan C3.0a, review RC-1, RC-3, RC-4): caller strings outside the typed
//! arguments (tool names, resource URIs, serde error text) never reach tracing or the wire;
//! registered ids are validated, not scanned, end to end; the live tool catalog equals the
//! registry's schema list.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::{Scratch, Server};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::Duration;

const DEADLINE: Duration = Duration::from_secs(10);

/// Not a denylist hit: shows structurally that caller data outside the typed arguments is
/// never echoed, whatever the policy.
const MARKER: &str = "Zq8vLeakMarkerQx";

/// The shipped example policy redacts this term (`old-password`).
const REDACT_CANARY: &str = "hunter2";

fn example_policy() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/policy.yaml")
}

fn contains_ci(haystack: &str, needle: &str) -> bool {
    haystack.to_lowercase().contains(&needle.to_lowercase())
}

fn call(srv: &mut Server, tool: &str, args: &Value) -> Value {
    let id = srv.request("tools/call", &json!({"name": tool, "arguments": args}));
    srv.response(id, DEADLINE).expect("tools/call answered")
}

fn result_json(resp: &Value) -> Value {
    assert_ne!(resp["result"]["isError"], json!(true), "{resp}");
    serde_json::from_str(resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
}

#[cfg(feature = "ingest")]
#[test]
fn ingest_submit_shared_service_places_repeats_and_refuses_hostile_fields() {
    let s = Scratch::new("kaban-mcp-submit", "public");
    let root = s.0.join("repo");
    std::fs::create_dir_all(root.join(".git")).unwrap();
    std::fs::write(root.join("a.md"), "# Source\n\nHarmless source.\n").unwrap();
    let mut srv = Server::start_at(&s, &root);
    let prepared = result_json(&call(&mut srv, "ingest_brief", &json!({"path":"a.md"})));
    let body = json!({"job":prepared["job"],"records":[{"local_id":"r","kind":"Fact","text":"Source-supported fact"}]});
    let first = result_json(&call(&mut srv, "ingest_submit", &body));
    assert_eq!(first["created"], 1);
    let repeat = result_json(&call(&mut srv, "ingest_submit", &body));
    assert_eq!(repeat["unchanged"], 1);
    let mut bad = body;
    bad["records"][0]["governance"] = json!("accepted");
    assert_eq!(
        call(&mut srv, "ingest_submit", &bad)["result"]["isError"],
        true
    );
    let rejected = call(
        &mut srv,
        "ingest_submit",
        &json!({"job":prepared["job"],"records":[],"unknown":"AKIAQWERTYUIOPASDFGH"}),
    );
    assert_eq!(rejected["result"]["isError"], true);
    assert!(!rejected.to_string().contains("AKIAQWERTYUIOPASDFGH"));
    srv.finish();
}

/// RC-3: an unknown tool name, an unknown resource URI and a serde argument error carry caller
/// strings that never pass the typed handlers; none of them reaches the wire (result text,
/// JSON-RPC error message or `data`) or stderr, and the span records only a recognized tool
/// name.
#[test]
fn caller_strings_outside_typed_arguments_never_reach_the_wire_or_stderr() {
    let s = Scratch::new("kaban-control", "canaries");
    let mut srv = Server::start_traced(&s, &["--policy", example_policy().to_str().unwrap()]);
    let tainted = format!("{REDACT_CANARY}-{MARKER}");
    let mut responses = Vec::new();
    // unknown tool name (with arguments, so the pre-parse path runs too)
    responses.push(call(&mut srv, &tainted, &json!({"text": "x"})));
    // unknown resource URI
    let id = srv.request(
        "resources/read",
        &json!({"uri": format!("kaban://{tainted}")}),
    );
    responses.push(srv.response(id, DEADLINE).unwrap());
    // typed-argument error quoting the value (unknown enum variant)
    let node = result_json(&call(
        &mut srv,
        "remember",
        &json!({"kind": "Fact", "text": "plain note"}),
    ));
    responses.push(call(
        &mut srv,
        "forget",
        &json!({"id": node["id"], "mode": tainted}),
    ));
    // a denylist rejection logs inside the span, so the recorded tool name becomes visible
    let resp = call(
        &mut srv,
        "remember",
        &json!({"kind": "Fact", "text": "deploy password = Sup3rS3cretValue! now"}),
    );
    assert_eq!(resp["result"]["isError"], json!(true), "{resp}");
    for resp in &responses {
        let text = resp.to_string();
        assert!(!text.contains(MARKER), "caller string echoed: {resp}");
        assert!(
            !contains_ci(&text, REDACT_CANARY),
            "redact-class value echoed: {resp}"
        );
        if let Some(data) = resp["error"].get("data") {
            assert!(data.is_null(), "error data carries caller data: {resp}");
        }
    }
    // the first two are failures of some kind, the third a tool error
    assert!(
        responses[0].get("error").is_some() || responses[0]["result"]["isError"] == json!(true)
    );
    assert!(responses[1].get("error").is_some());
    assert_eq!(
        responses[2]["result"]["isError"],
        json!(true),
        "{}",
        responses[2]
    );
    let (_, stderr) = srv.finish_with_stderr();
    assert!(!stderr.contains(MARKER), "caller string in trace output");
    assert!(
        !contains_ci(&stderr, REDACT_CANARY),
        "redact-class value in trace output"
    );
    assert!(
        stderr.contains("tool=remember"),
        "recognized tool name expected in the span: {stderr}"
    );
    assert!(
        stderr.contains("tool=unknown"),
        "fixed label expected for an unrecognized tool name: {stderr}"
    );
}

/// A test policy whose only user rule rejects anything shaped like a UUID.
const UUID_REJECT: &str = "rules:\n  - id: uuid-reject\n    regex: '\\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\\b'\n    action: reject\n";

/// RC-1 end to end: under a policy that rejects UUIDs, calls that only name kaban's own ids
/// succeed through the adapter (`remember` inline links, `link`, `traverse` with its edges,
/// `forget`), while a UUID in content is still rejected.
#[test]
fn registered_ids_are_validated_not_scanned_end_to_end_under_a_uuid_reject_policy() {
    let s = Scratch::new("kaban-control", "uuid-reject");
    let policy = s.0.join("uuid-reject.yaml");
    std::fs::write(&policy, UUID_REJECT).unwrap();
    let mut srv = Server::start_with(&s, &["--policy", policy.to_str().unwrap()], DEADLINE).0;
    let a = result_json(&call(
        &mut srv,
        "remember",
        &json!({"kind": "Fact", "text": "node a"}),
    ))["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let b = result_json(&call(
        &mut srv,
        "remember",
        &json!({"kind": "Fact", "text": "node b", "links": [{"rel": "RELATES_TO", "to": a}]}),
    ));
    assert_eq!(b["links_created"], json!(1), "{b}");
    let b = b["id"].as_str().unwrap().to_owned();
    let edge = result_json(&call(
        &mut srv,
        "link",
        &json!({"from": a, "rel": "ABOUT", "to": b}),
    ));
    let t = result_json(&call(&mut srv, "traverse", &json!({"start_ids": [a]})));
    assert!(
        t["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n["id"] == json!(b)),
        "{t}"
    );
    assert!(
        t["edges"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["id"] == edge["id"]),
        "{t}"
    );
    let gone = result_json(&call(&mut srv, "forget", &json!({"id": a})));
    assert_eq!(gone["id"], json!(a));
    let resp = call(
        &mut srv,
        "remember",
        &json!({"kind": "Fact", "text": format!("see {b}")}),
    );
    assert_eq!(resp["result"]["isError"], json!(true), "{resp}");
    assert!(
        resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("uuid-reject"),
        "{resp}"
    );
    srv.finish();
}

/// RC-4: the live `tools/list` names equal the names `control::tool_schemas()` covers, exactly;
/// a tool added to one list without the other fails here. The schema list is declared whatever
/// the build: the resource-only bodies (`RESOURCE_ONLY`, CG7.3) are subtracted, and the
/// `index_*` / `code_*` tools only count in a build with their feature.
#[test]
fn live_tool_catalog_equals_the_registry_schema_list() {
    let s = Scratch::new("kaban-control", "catalog");
    let mut srv = Server::start(&s);
    let id = srv.request("tools/list", &json!({}));
    let resp = srv.response(id, DEADLINE).unwrap();
    let live: BTreeSet<String> = resp["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_owned())
        .collect();
    let declared: BTreeSet<String> = kaban_api::control::tool_schemas()
        .iter()
        .map(|t| t.tool)
        .filter(|t| !kaban_api::control::RESOURCE_ONLY.contains(t))
        .filter(|t| {
            !kaban_api::control::RESERVED.contains(t) && !kaban_api::control::CLI_ONLY.contains(t)
        })
        .filter(|t| {
            cfg!(feature = "ingest") || !(t.starts_with("index_") || t.starts_with("ingest_"))
        })
        .filter(|t| cfg!(feature = "code-graph") || !t.starts_with("code_"))
        .map(str::to_owned)
        .collect();
    assert_eq!(live, declared);
    srv.finish();
}
