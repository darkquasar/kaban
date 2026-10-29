//! `recall` v3 on the wire (core-spec F3, N5; core-plan C3.5): cards carry `provenance` and
//! `why` explanations, and the same query twice returns byte-identical JSON (insta pins the
//! shape with ids, timestamps and the repository name normalised).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::{Scratch, Server};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::time::Duration;

const T: Duration = Duration::from_secs(10);

fn call_text(srv: &mut Server, tool: &str, args: &Value) -> String {
    let id = srv.request("tools/call", &json!({"name": tool, "arguments": args}));
    let resp = srv.response(id, T).unwrap();
    assert_ne!(resp["result"]["isError"], json!(true), "{resp}");
    resp["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .to_owned()
}

/// Replace node ids by `<id-N>` in first-seen order, timestamps by `<ts>` and the repository
/// name (the enclosing checkout's directory) by `<repo>`, so the snapshot is stable.
fn normalise(v: &mut Value, ids: &mut BTreeMap<String, String>) {
    match v {
        Value::Object(m) => {
            for (k, val) in m.iter_mut() {
                match (k.as_str(), &mut *val) {
                    ("id", Value::String(s)) => {
                        let n = ids.len();
                        let label = ids
                            .entry(s.clone())
                            .or_insert_with(|| format!("<id-{n}>"))
                            .clone();
                        *s = label;
                    }
                    ("created_at", Value::String(s)) => *s = "<ts>".into(),
                    ("repo", Value::String(s)) => *s = "<repo>".into(),
                    _ => normalise(val, ids),
                }
            }
        }
        Value::Array(a) => a.iter_mut().for_each(|x| normalise(x, ids)),
        _ => {}
    }
}

#[test]
fn recall_cards_carry_provenance_and_the_same_query_twice_is_byte_identical() {
    let s = Scratch::new("kaban-recall-det", "n5");
    let mut srv = Server::start_with(&s, &["--workspace", "det"], T).0;
    let failure = call_text(
        &mut srv,
        "remember",
        &json!({"kind": "Failure", "text": "quokka build failed: libssl not found",
                "tags": ["build", "ssl"], "agent_id": "claude", "session_id": "s-1"}),
    );
    let failure_id = serde_json::from_str::<Value>(&failure).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    call_text(
        &mut srv,
        "remember",
        &json!({"kind": "Fact", "text": "install libssl-dev before the quokka build",
                "summary": "libssl-dev fixes the quokka build", "agent_id": "codex",
                "links": [{"rel": "FIXED_BY", "from": failure_id}]}),
    );
    call_text(
        &mut srv,
        "remember",
        &json!({"kind": "Idea", "text": "cache the quokka toolchain between builds"}),
    );

    let args = json!({"query": "quokka build", "hops": 1, "limit": 10});
    let first = call_text(&mut srv, "recall", &args);
    let second = call_text(&mut srv, "recall", &args);
    assert_eq!(first, second, "N5: identical query, identical bytes");

    let mut v: Value = serde_json::from_str(&first).unwrap();
    let results = v["results"].as_array().unwrap();
    assert_eq!(results.len(), 3, "{v}");
    for card in results {
        let prov = &card["provenance"];
        assert_eq!(prov["store"], json!("project"), "{card}");
        assert!(prov["repo"].is_string(), "{card}");
        assert!(
            card["why"].as_array().is_some_and(|w| !w.is_empty()),
            "{card}"
        );
    }
    let by_agent = |agent: &str| {
        results
            .iter()
            .find(|c| c["provenance"]["agent_id"] == json!(agent))
            .cloned()
    };
    assert_eq!(
        by_agent("claude").unwrap()["provenance"]["session_id"],
        json!("s-1")
    );
    assert!(by_agent("codex").is_some(), "{v}");

    let mut ids = BTreeMap::new();
    normalise(&mut v, &mut ids);
    insta::assert_json_snapshot!("recall_v3_cards", v);
    srv.finish();
}
