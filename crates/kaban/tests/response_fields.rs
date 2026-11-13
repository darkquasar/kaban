//! REC-08: the response schemas of `remember`, `forget`, `recall`, `stats` and `doctor` drop their
//! governance, audit and projector-lag fields and are otherwise unchanged. The baseline is the
//! c15cd88 catalog (`fixtures/response_fields_c15cd88.json`: every `outputSchema` property path
//! and every `doctor` check name); the allowed removals are listed here.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::{Scratch, Server};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::process::Command;
use std::time::Duration;

const DEADLINE: Duration = Duration::from_secs(10);

/// The `outputSchema` property paths each tool may lose (REC-08): governance and audit fields.
const REMOVED_PATHS: [(&str, &str); 4] = [
    ("forget", "/governance"),
    ("forget", "/outbox_seq"),
    ("recall", "#Card/governance"),
    ("remember", "#Card/governance"),
];

/// The `stats` counters that go: projector lag and the conflict count.
const REMOVED_STATS: [&str; 3] = ["changes_lag", "outbox_lag", "conflicts"];

/// The `doctor` checks that go: governance, audit and projector lag.
const REMOVED_CHECKS: [&str; 11] = [
    "conflicts",
    "unresolved",
    "priors",
    "discovered",
    "unstamped",
    "key_file",
    "audit",
    "changes_lag",
    "outbox_lag",
    "consumer_lag",
    "reconcile_cursor_lag",
];

fn baseline() -> Value {
    serde_json::from_str(include_str!("fixtures/response_fields_c15cd88.json")).unwrap()
}

/// Every property path of a JSON schema: `/a/b` under the root, `#Def/a` under `$defs`.
fn paths(schema: &Value, prefix: &str, out: &mut BTreeSet<String>) {
    let Some(obj) = schema.as_object() else {
        return;
    };
    if let Some(props) = obj.get("properties").and_then(Value::as_object) {
        for (k, v) in props {
            let p = format!("{prefix}/{k}");
            out.insert(p.clone());
            paths(v, &p, out);
        }
    }
    for k in ["items", "additionalProperties"] {
        if let Some(v) = obj.get(k).filter(|v| v.is_object()) {
            paths(v, &format!("{prefix}/{k}"), out);
        }
    }
    for k in ["anyOf", "oneOf", "allOf"] {
        for v in obj.get(k).and_then(Value::as_array).into_iter().flatten() {
            paths(v, prefix, out);
        }
    }
    if let Some(defs) = obj.get("$defs").and_then(Value::as_object) {
        for (k, v) in defs {
            paths(v, &format!("#{k}"), out);
        }
    }
}

fn call(srv: &mut Server, tool: &str, args: &Value) -> Value {
    let id = srv.request("tools/call", &json!({"name": tool, "arguments": args}));
    let result = srv.response(id, DEADLINE).unwrap()["result"].clone();
    assert_ne!(result["isError"], json!(true), "{tool}: {result}");
    result["structuredContent"].clone()
}

#[test]
fn response_schemas_lose_only_the_governance_audit_and_lag_fields() {
    let base = baseline();
    let s = Scratch::new("kaban-response-fields", "schemas");
    let mut srv = Server::start(&s);
    let id = srv.request("tools/list", &json!({}));
    let tools = srv.response(id, DEADLINE).unwrap()["result"]["tools"].clone();
    for (tool, expected) in base["schemas"].as_object().unwrap() {
        let live = tools
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == json!(tool))
            .unwrap_or_else(|| panic!("{tool} missing from tools/list"));
        let mut now = BTreeSet::new();
        paths(&live["outputSchema"], "", &mut now);
        let before: BTreeSet<String> = expected
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p.as_str().unwrap().to_owned())
            .collect();
        let allowed: BTreeSet<String> = REMOVED_PATHS
            .iter()
            .filter(|(t, _)| t == tool)
            .map(|(_, p)| (*p).to_owned())
            .collect();
        let missing: BTreeSet<String> = before.difference(&now).cloned().collect();
        let added: BTreeSet<String> = now.difference(&before).cloned().collect();
        assert_eq!(missing, allowed, "{tool}: removed paths");
        assert!(added.is_empty(), "{tool}: added paths {added:?}");
    }

    // the wire: a written card, a forget and the stats counters carry none of the fields
    let put = call(
        &mut srv,
        "remember",
        &json!({"kind": "Fact", "text": "response fields: a body"}),
    );
    let id = put["id"].as_str().unwrap().to_owned();
    let recalled = call(&mut srv, "recall", &json!({"query": "response fields"}));
    let results = recalled["results"].as_array().unwrap();
    assert!(!results.is_empty(), "{recalled}");
    for card in results {
        assert!(card.get("governance").is_none(), "{card}");
    }
    let gone = call(&mut srv, "forget", &json!({"id": id}));
    assert!(gone.get("governance").is_none(), "{gone}");
    assert!(gone.get("outbox_seq").is_none(), "{gone}");
    let stats = call(&mut srv, "stats", &json!({}));
    for field in REMOVED_STATS {
        assert!(stats.get(field).is_none(), "stats.{field}: {stats}");
    }
    srv.finish();
}

#[test]
fn doctor_drops_only_the_governance_audit_and_lag_checks() {
    let base = baseline();
    let before: BTreeSet<&str> = base["doctor"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    let s = Scratch::new("kaban-response-fields", "doctor");
    // a served project: the database and its key exist
    Server::start(&s).finish();
    let out = Command::new(env!("CARGO_BIN_EXE_kaban"))
        .env("KABAN_HOME", s.home())
        .args(["--json", "--db", s.db().to_str().unwrap(), "doctor"])
        .output()
        .unwrap();
    let text = std::str::from_utf8(&out.stdout).unwrap();
    let report: Value = serde_json::from_str(text).unwrap();
    let names: BTreeSet<&str> = report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    let unknown: Vec<&&str> = names.difference(&before).collect();
    assert!(
        unknown.is_empty(),
        "checks not in the baseline: {unknown:?}"
    );
    let kept: Vec<&str> = REMOVED_CHECKS
        .into_iter()
        .filter(|c| names.contains(c))
        .collect();
    assert!(kept.is_empty(), "removed checks still reported: {kept:?}");
}
