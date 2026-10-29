//! The `tools/list` input schema of `remember` exposes the core-spec §6 `CodeRef` wire shape:
//! `refs[].location` is a nested object with `line` and `column` (core-plan C3.1; insta).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::{Scratch, Server};
use serde_json::{Value, json};
use std::time::Duration;

/// Resolve an optional (`anyOf [T, null]`) or direct `$ref` against the schema's `$defs`.
fn resolve<'a>(root: &'a Value, node: &'a Value) -> &'a Value {
    let node = match node.get("anyOf").and_then(Value::as_array) {
        Some(branches) => branches
            .iter()
            .find(|b| b.get("type") != Some(&json!("null")))
            .unwrap(),
        None => node,
    };
    match node.get("$ref").and_then(Value::as_str) {
        Some(r) => &root["$defs"][r.trim_start_matches("#/$defs/")],
        None => node,
    }
}

#[test]
fn remember_input_schema_shows_refs_location_as_a_nested_object() {
    let s = Scratch::new("kaban-schema", "remember");
    let mut srv = Server::start(&s);
    let id = srv.request("tools/list", &json!({}));
    let resp = srv.response(id, Duration::from_secs(10)).unwrap();
    let tools = resp["result"]["tools"].as_array().unwrap();
    let remember = tools
        .iter()
        .find(|t| t["name"] == json!("remember"))
        .unwrap();
    let schema = &remember["inputSchema"];
    let refs = &schema["properties"]["refs"];
    assert_eq!(refs["type"], json!("array"));
    let item = resolve(schema, &refs["items"]);
    let location = resolve(schema, &item["properties"]["location"]);
    insta::assert_json_snapshot!("remember_refs_location", location);
    // the stored-form derived `line` is not on the wire: a flat `line` key is rejected
    assert!(item["properties"].get("line").is_none(), "{item}");
    assert_eq!(item["additionalProperties"], json!(false));
    srv.finish();
}

/// Stage1-spec G1-F9, §4.4 (stage1-plan CG5.1): the ref item carries `symbol`, `snapshot_commit`
/// and `observed_at`, each an optional string; the three are pinned on their own beside the
/// `location` pin.
#[test]
fn remember_input_schema_shows_the_symbol_fields_on_the_ref_item() {
    let s = Scratch::new("kaban-schema", "remember-symbol");
    let mut srv = Server::start(&s);
    let id = srv.request("tools/list", &json!({}));
    let resp = srv.response(id, Duration::from_secs(10)).unwrap();
    let tools = resp["result"]["tools"].as_array().unwrap();
    let remember = tools
        .iter()
        .find(|t| t["name"] == json!("remember"))
        .unwrap();
    let schema = &remember["inputSchema"];
    let item = resolve(schema, &schema["properties"]["refs"]["items"]);
    let props = &item["properties"];
    let fields = json!({
        "symbol": props["symbol"],
        "snapshot_commit": props["snapshot_commit"],
        "observed_at": props["observed_at"],
    });
    for name in ["symbol", "snapshot_commit", "observed_at"] {
        assert_eq!(fields[name]["type"], json!(["string", "null"]), "{name}");
        assert!(
            !item["required"].as_array().unwrap().contains(&json!(name)),
            "{name} is optional"
        );
    }
    insta::assert_json_snapshot!("remember_refs_symbol_fields", fields);
    srv.finish();
}

/// MT-5 (`rust-bp-review-dispositions.md` RB-4; core-plan C3.6): `traverse.direction` is a closed
/// enum on the wire, `out | in | both`, optional (the server defaults to `both`).
#[test]
fn traverse_input_schema_closes_direction_to_out_in_both() {
    let s = Scratch::new("kaban-schema", "traverse");
    let mut srv = Server::start(&s);
    let id = srv.request("tools/list", &json!({}));
    let resp = srv.response(id, Duration::from_secs(10)).unwrap();
    let tools = resp["result"]["tools"].as_array().unwrap();
    let traverse = tools
        .iter()
        .find(|t| t["name"] == json!("traverse"))
        .unwrap();
    let schema = &traverse["inputSchema"];
    let direction = resolve(schema, &schema["properties"]["direction"]);
    // schemars spells a documented enum as `oneOf` branches with `const` values
    let values: Vec<&Value> = direction["oneOf"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| &b["const"])
        .collect();
    assert_eq!(values, vec![&json!("out"), &json!("in"), &json!("both")]);
    insta::assert_json_snapshot!("traverse_direction", direction);
    srv.finish();
}
