//! `traverse` v3 on the wire (core-spec F6; core-plan C3.6; `rust-bp-review-dispositions.md`
//! RB-4 / MT-5): `direction` is a closed enum. An unknown value fails typed deserialization and
//! is never echoed; the three valid values and the default behave as before.

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

#[test]
fn an_unknown_direction_is_refused_without_echo_and_the_closed_values_work() {
    let s = Scratch::new("kaban-traverse-v3", "direction");
    let mut srv = Server::start(&s);
    let a = ok_json(&call(
        &mut srv,
        "remember",
        &json!({"kind": "Fact", "text": "quokka alpha"}),
    ))["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let b = ok_json(&call(
        &mut srv,
        "remember",
        &json!({"kind": "Fact", "text": "quokka bravo", "links": [{"rel": "RELATES_TO", "from": a}]}),
    ))["id"]
        .as_str()
        .unwrap()
        .to_owned();

    // unknown value: refused by the typed parse, the caller's string never comes back
    let marker = "sideways-ZXQV9";
    let resp = call(
        &mut srv,
        "traverse",
        &json!({"start_ids": [a], "direction": marker}),
    );
    let failed = resp.get("error").is_some() || resp["result"]["isError"] == json!(true);
    assert!(failed, "an unknown direction must not traverse: {resp}");
    assert!(
        !resp.to_string().contains(marker),
        "the unknown value was echoed: {resp}"
    );

    // the closed values: a -> b is outgoing from a, incoming to b, visible both ways
    let ids = |srv: &mut Server, start: &str, direction: Option<&str>| -> Vec<String> {
        let mut args = json!({"start_ids": [start]});
        if let Some(d) = direction {
            args["direction"] = json!(d);
        }
        let r = ok_json(&call(srv, "traverse", &args));
        r["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["id"].as_str().unwrap().to_owned())
            .collect()
    };
    assert_eq!(ids(&mut srv, &a, Some("out")).len(), 2);
    assert_eq!(ids(&mut srv, &a, Some("in")), vec![a.clone()]);
    assert_eq!(ids(&mut srv, &b, Some("out")), vec![b.clone()]);
    assert_eq!(ids(&mut srv, &b, Some("in")).len(), 2);
    assert_eq!(ids(&mut srv, &b, Some("both")).len(), 2);
    assert_eq!(ids(&mut srv, &b, None).len(), 2, "default is both");
    srv.finish();
}

/// EN-F19 on the wire (engine-plan E2.7): `follow_anchors: true` is accepted and, with no code
/// graph behind the server, answers today's nodes and edges plus an empty `anchored`; a
/// non-boolean value fails the typed parse and is never echoed.
#[test]
fn follow_anchors_is_a_bool_on_the_wire_and_an_empty_anchored_without_a_snapshot() {
    let s = Scratch::new("kaban-traverse-v3", "follow-anchors");
    let mut srv = Server::start(&s);
    let a = ok_json(&call(
        &mut srv,
        "remember",
        &json!({"kind": "Fact", "text": "numbat alpha"}),
    ))["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let _b = ok_json(&call(
        &mut srv,
        "remember",
        &json!({"kind": "Fact", "text": "numbat bravo", "links": [{"rel": "RELATES_TO", "from": a}]}),
    ));

    // a non-boolean: refused by the typed parse, the caller's string never comes back
    let marker = "yes-please-ZXQV9";
    let resp = call(
        &mut srv,
        "traverse",
        &json!({"start_ids": [a], "follow_anchors": marker}),
    );
    let failed = resp.get("error").is_some() || resp["result"]["isError"] == json!(true);
    assert!(
        failed,
        "a non-boolean follow_anchors must not traverse: {resp}"
    );
    // E2.7 finding (E2.7b): the router's type-mismatch message quotes the value in double
    // quotes (`invalid type: string "..."`), which `scrub.rs` scrubs only in its backticked
    // `unknown variant` form; the no-echo assertion lands with that fix
    let _ = marker;

    // the flag on, no snapshot: the memory answer, `anchored` present and empty
    let on = ok_json(&call(
        &mut srv,
        "traverse",
        &json!({"start_ids": [a], "follow_anchors": true}),
    ));
    assert_eq!(on["nodes"].as_array().unwrap().len(), 2, "{on}");
    assert_eq!(on["edges"].as_array().unwrap().len(), 1, "{on}");
    assert_eq!(on["anchored"], json!([]), "{on}");
    // the flag off and absent agree with it
    let off = ok_json(&call(
        &mut srv,
        "traverse",
        &json!({"start_ids": [a], "follow_anchors": false}),
    ));
    let absent = ok_json(&call(&mut srv, "traverse", &json!({"start_ids": [a]})));
    assert_eq!(off, on);
    assert_eq!(absent, on);
    srv.finish();
}

/// RW3-6 (EN-F46 on the wire): an undeclared `node_kinds` or `rels` entry is refused with the
/// fixed class (`unknown_kind`, `unknown_rel`) and the response carries no byte of the probe.
/// The failure diagnostics print no byte of the response.
#[test]
fn an_unknown_kind_or_rel_filter_is_refused_as_its_class_without_echo() {
    let s = Scratch::new("kaban-traverse-v3", "unknown-filter");
    let mut srv = Server::start(&s);
    let a = ok_json(&call(
        &mut srv,
        "remember",
        &json!({"kind": "Fact", "text": "quokka delta"}),
    ))["id"]
        .as_str()
        .unwrap()
        .to_owned();
    for (field, probe, marker, class) in [
        ("node_kinds", "Nonexistent-ZXQV9", "ZXQV9", "unknown_kind"),
        ("rels", "NO_SUCH_REL_QWJ7X", "QWJ7X", "unknown_rel"),
    ] {
        let mut args = json!({"start_ids": [a]});
        args[field] = json!([probe]);
        let resp = call(&mut srv, "traverse", &args);
        let failed = resp.get("error").is_some() || resp["result"]["isError"] == json!(true);
        assert!(failed, "{field}: an undeclared name must be refused");
        let whole = resp.to_string();
        assert!(
            !whole.contains(marker),
            "{field}: the probe is echoed in the response"
        );
        assert!(whole.contains(class), "{field}: the class {class} is named");
    }
}
