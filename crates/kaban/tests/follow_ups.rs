//! `follow_ups` on the wire (core-spec F19; core-plan C3.9; M3 review R3-10): a `recall` whose
//! result carries a note with two `CodeRef` pointers suggests two `serena.find_symbol` calls
//! built from the pointers (with or without the `CodeRef` cards themselves in the result), and
//! the response gate passes them (every follow-up string is content). insta pins the shape.

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

/// Node ids become `<id-N>` in first-seen order, timestamps `<ts>`, the repository `<repo>`.
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
fn a_recall_with_two_coderef_cards_suggests_two_follow_ups() {
    let s = Scratch::new("kaban-follow-ups", "two");
    let mut srv = Server::start_with(&s, &["--workspace", "fu"], T).0;
    let coderef = |path: &str| json!({"repo": "kaban", "commit": "8d10f88", "tool": "manual", "relative_path": path});
    call_text(
        &mut srv,
        "remember",
        &json!({"kind": "Fact", "text": "quokka opens and closes the store",
                "refs": [coderef("src/store/open.rs"), coderef("src/store/close.rs")]}),
    );
    let text = call_text(&mut srv, "recall", &json!({"query": "quokka", "hops": 1}));
    let mut v: Value = serde_json::from_str(&text).unwrap();

    let follow_ups = v["follow_ups"].as_array().unwrap();
    assert_eq!(follow_ups.len(), 2, "{text}");
    for f in follow_ups {
        assert_eq!(f["tool"], json!("serena.find_symbol"));
        assert!(f["args"]["name_path"].is_null(), "no symbol was named: {f}");
    }
    let paths: Vec<&str> = follow_ups
        .iter()
        .map(|f| f["args"]["relative_path"].as_str().unwrap())
        .collect();
    assert_eq!(paths, vec!["src/store/open.rs", "src/store/close.rs"]);

    // the pointers belong to the note (R3-10): a hop-0 recall returns the note alone and
    // suggests the same two calls
    let hop0 = call_text(&mut srv, "recall", &json!({"query": "quokka", "hops": 0}));
    let hop0: Value = serde_json::from_str(&hop0).unwrap();
    assert_eq!(hop0["count"], json!(1), "{hop0}");
    assert_eq!(hop0["follow_ups"], v["follow_ups"], "{hop0}");

    normalise(&mut v, &mut BTreeMap::new());
    insta::assert_json_snapshot!("recall_two_coderefs", v);
    srv.finish();
}
