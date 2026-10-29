//! RW2-9 (wave-2 astra review; E2.7b): a typed-argument failure of the MCP router never echoes
//! the caller's bytes. serde quotes an offending value in backticks (`unknown variant`) or in
//! double quotes (`invalid type: string "..."`); both forms are scrubbed before the response
//! gate and the shape of the error (`expected a boolean`) survives. The assertion
//! `traverse_v3.rs` withheld at E2.7 lands here.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::{Scratch, Server};
use serde_json::{Value, json};
use std::time::Duration;

const T: Duration = Duration::from_secs(10);

fn call(srv: &mut Server, tool: &str, args: &Value) -> Value {
    let id = srv.request("tools/call", &json!({"name": tool, "arguments": args}));
    srv.response(id, T).unwrap()
}

fn ok_json(resp: &Value) -> Value {
    assert_ne!(resp["result"]["isError"], json!(true), "{resp}");
    serde_json::from_str(resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
}

/// A string where a typed field is expected: the call fails, the response carries no byte of
/// the string, and the error keeps its shape (`expected ...`). One boolean field and one
/// integer field, the two typed shapes `traverse` has.
#[test]
fn a_type_mismatch_on_a_typed_argument_is_refused_without_echo() {
    let s = Scratch::new("kaban-mcp-scrub", "type-mismatch");
    let mut srv = Server::start(&s);
    let a = ok_json(&call(
        &mut srv,
        "remember",
        &json!({"kind": "Fact", "text": "wombat alpha"}),
    ))["id"]
        .as_str()
        .unwrap()
        .to_owned();

    for (field, marker, shape) in [
        ("follow_anchors", "yes-please-ZXQV9", "expected a boolean"),
        ("max_depth", "very-deep-QWJ7X", "expected"),
    ] {
        let mut args = json!({"start_ids": [a]});
        args[field] = json!(marker);
        let resp = call(&mut srv, "traverse", &args);
        let failed = resp.get("error").is_some() || resp["result"]["isError"] == json!(true);
        assert!(
            failed,
            "{field}: a mistyped value must not traverse: {resp}"
        );
        let whole = resp.to_string();
        // the property: no caller byte comes back (RW2-9)
        assert!(
            !whole.contains(marker),
            "{field}: the caller's value is echoed in the response"
        );
        // positive: the typed shape of the error survives the scrub
        assert!(
            whole.contains(shape),
            "{field}: the error lost its shape: {resp}"
        );
    }
}

/// RW3-1 (wave-3 astra review): the failure text is fixed whatever the quoting of the value. A
/// value carrying an escaped double quote (serde renders the value with `{:?}`, so `"` becomes
/// `\"`), one mixing backticks, double quotes and an ellipsis, and an unknown variant whose
/// bytes carry a backtick and a quote: none of their markers comes back, the shape survives.
/// The failure diagnostics print no byte of the response (the response is the suspect).
#[test]
fn escaped_quotes_and_mixed_delimiters_in_a_mistyped_value_are_not_echoed() {
    let s = Scratch::new("kaban-mcp-scrub", "escaped-quotes");
    let mut srv = Server::start(&s);
    let a = ok_json(&call(
        &mut srv,
        "remember",
        &json!({"kind": "Fact", "text": "wombat bravo"}),
    ))["id"]
        .as_str()
        .unwrap()
        .to_owned();

    let cases: [(&str, String, &str, &str); 4] = [
        (
            "follow_anchors",
            "yes\"ZXQV9-tail\" please".to_owned(),
            "ZXQV9",
            "expected a boolean",
        ),
        (
            "follow_anchors",
            format!("`mixed\" {}QWJ7X` \"and` more", '\u{2026}'),
            "QWJ7X",
            "expected a boolean",
        ),
        (
            "max_depth",
            "deep\\\"K4M2P\\\" quoted".to_owned(),
            "K4M2P",
            "expected",
        ),
        (
            "direction",
            "side`ways\"-R8TY3".to_owned(),
            "R8TY3",
            "unknown variant",
        ),
    ];
    for (case, (field, value, marker, shape)) in cases.into_iter().enumerate() {
        let mut args = json!({"start_ids": [a]});
        args[field] = json!(value);
        let resp = call(&mut srv, "traverse", &args);
        let failed = resp.get("error").is_some() || resp["result"]["isError"] == json!(true);
        assert!(
            failed,
            "case {case} ({field}): a mistyped value must not traverse"
        );
        let whole = resp.to_string();
        // the property (RW3-1): no caller byte comes back, whatever the quoting
        assert!(
            !whole.contains(marker),
            "case {case} ({field}): the caller's value is echoed in the response"
        );
        // positive: the shape of the error survives as fixed text
        assert!(
            whole.contains(shape),
            "case {case} ({field}): the error lost its shape ({shape} absent)"
        );
    }
}
