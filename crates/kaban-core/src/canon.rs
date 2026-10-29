//! Canonical JSON text (engine-spec EN-F3a `enum` equality, EN-F12a rule (3); engine-plan E1.1,
//! shared with the E2.1 assertion keys): object keys sorted byte-wise, no whitespace, integers
//! written as integers, every other number as the shortest text that round-trips through `f64`,
//! strings escaped as `serde_json` escapes them. The function is total and idempotent: parsing
//! its output and canonicalising again yields the same bytes.

use serde_json::Value;

/// The canonical text of `v` (see the module doc).
#[must_use]
pub fn canonical_json(v: &Value) -> String {
    let mut out = String::new();
    write_value(v, &mut out);
    out
}

fn write_value(v: &Value, out: &mut String) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => {
            // `serde_json::Number` prints `i64`/`u64` as integers and `f64` through `ryu`
            // (the shortest round-trip text); its `Display` is exactly the rule.
            out.push_str(&n.to_string());
        }
        Value::String(s) => write_string(s, out),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_value(item, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            // sort the keys byte-wise whatever the map's own order is
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_unstable_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
            out.push('{');
            for (i, key) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_string(key, out);
                out.push(':');
                if let Some(value) = map.get(key) {
                    write_value(value, out);
                }
            }
            out.push('}');
        }
    }
}

/// A JSON string literal with `serde_json`'s escaping (the only escaping rule kaban uses).
fn write_string(s: &str, out: &mut String) {
    match serde_json::to_string(s) {
        Ok(text) => out.push_str(&text),
        // `to_string` of a `str` cannot fail; keep the function total anyway
        Err(_) => out.push_str("\"\""),
    }
}
