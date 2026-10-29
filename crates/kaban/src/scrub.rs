//! The fixed text of a typed-argument failure, shared by the MCP adapter and the CLI seam (one
//! module, M4-3; RW2-9, RW3-1). serde's message quotes the offending value (`invalid type:
//! string "..."`, a backticked name after `unknown variant`) with no escaping a scrubber could
//! trust: an escaped
//! quote inside a value closes a quoted segment early, a backtick inside an unknown field name
//! does the same. So the message is never scrubbed; it is classified, and the answer is built
//! from the parts serde takes from the request type (the class, the expected shape, the names
//! of the declared fields) and never from the input. Anything unclassified is one fixed line.
//! The gate (`KabanApi::gate_response_text`) still judges the text afterwards.

/// rmcp reports typed-argument failures as an `isError` result whose text starts with this.
const ARG_ERROR_PREFIX: &str = "failed to deserialize parameters:";

/// The answer to a serde message no class below recognises (a custom `Deserialize` message
/// may quote the input in a shape of its own, so nothing of it is kept).
const UNCLASSIFIED: &str = "invalid request: a typed argument did not parse";

/// serde's `, expected ` separator: the expected shape follows the quoted value, so the LAST
/// occurrence is the one serde wrote, whatever the value contains.
const EXPECTED_SEP: &str = ", expected ";

/// What a refused call carries beside its error text (spec 05 SS-10): nothing for a read, the
/// typed `rejected` receipt for a write. Both transports decide this per tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OnRefusal {
    /// A read: the error text alone.
    ErrorOnly,
    /// A write: the error text and the `rejected` receipt (identities, no position).
    RejectedReceipt,
}

/// The `{"receipt": ...}` document of a write `tool` refused before anything committed, gated
/// under the tool's response scope like every result (its leaves are control there): `None`
/// when the gate withholds it (fail closed, like the result it stands in for).
pub(crate) fn rejected_receipt_document(api: &kaban_api::KabanApi, tool: &str) -> Option<String> {
    let document = serde_json::json!({ "receipt": api.rejected_receipt() });
    api.gate_response(tool, &document)
        .ok()
        .map(|clean| clean.to_string())
}

/// The fixed text of a typed-argument failure for `serde_msg` (`serde_json::Error`'s
/// `Display`): the class and the type-derived parts survive, no caller byte does.
///
/// - `missing field` with its backticked name: kept whole (the name is the struct's, from
///   the derive).
/// - `invalid type`, `invalid value`, `invalid length`: the class and the expected shape (the
///   visitor's `expecting`, type-derived; taken after the last [`EXPECTED_SEP`], which follows
///   the value).
/// - `unknown field`, `unknown variant`: the class and the declared names (`expected one of
///   ...`, type-derived), the input name dropped; a type with no fields or variants gives the
///   class alone.
/// - `duplicate field`: the class alone.
/// - anything else: [`UNCLASSIFIED`].
pub(crate) fn typed_argument_failure(serde_msg: &str) -> String {
    let msg = strip_position(serde_msg.trim());
    if let Some(rest) = msg.strip_prefix("missing field `") {
        return format!("missing field `{rest}");
    }
    for class in ["invalid type", "invalid value", "invalid length"] {
        if msg.starts_with(class) {
            return with_expected(class, msg);
        }
    }
    for (class, none) in [
        ("unknown field", ", there are no fields"),
        ("unknown variant", ", there are no variants"),
    ] {
        if msg.starts_with(class) {
            if msg.ends_with(none) {
                return class.to_owned();
            }
            return with_expected(class, msg);
        }
    }
    if msg.starts_with("duplicate field") {
        return "duplicate field".to_owned();
    }
    UNCLASSIFIED.to_owned()
}

/// `class, expected <shape>` when serde wrote an expected part, else the class alone.
fn with_expected(class: &str, msg: &str) -> String {
    match msg.rsplit_once(EXPECTED_SEP) {
        Some((_, shape)) if !shape.is_empty() => format!("{class}{EXPECTED_SEP}{shape}"),
        _ => class.to_owned(),
    }
}

/// Drop `serde_json`'s trailing ` at line N column M` (a position, not a byte of the input,
/// but not part of the fixed text either).
fn strip_position(msg: &str) -> &str {
    let Some((head, tail)) = msg.rsplit_once(" at line ") else {
        return msg;
    };
    let Some((line, column)) = tail.split_once(" column ") else {
        return msg;
    };
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if digits(line) && digits(column) {
        head
    } else {
        msg
    }
}

/// A typed-argument failure of the MCP router (`failed to deserialize parameters: <serde
/// message>`) becomes the prefix plus [`typed_argument_failure`] of the message; any other
/// error text is passed on for the gate. The name is the adapter's call site (`mcp/mod.rs`,
/// `scrub_response`), kept while the text is no longer scrubbed but rebuilt.
pub(crate) fn scrub_error_text(text: &str) -> String {
    text.strip_prefix(ARG_ERROR_PREFIX).map_or_else(
        || text.to_owned(),
        |rest| format!("{ARG_ERROR_PREFIX} {}", typed_argument_failure(rest)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The marker of every input; a failure message names the case, never the output.
    const MARKER: &str = "ZXQV9";

    fn fixed(case: &str, input_msg: &str, expect: &str) {
        let out = typed_argument_failure(input_msg);
        assert!(
            !out.contains(MARKER),
            "{case}: the input survived into the fixed text"
        );
        assert!(
            out == expect,
            "{case}: the fixed text is not the expected one"
        );
    }

    #[test]
    fn quoted_values_of_every_shape_are_dropped_and_the_type_parts_survive() {
        // serde renders a string value with `{:?}`: an escaped quote inside the value
        fixed(
            "escaped quote",
            r#"invalid type: string "yes\"ZXQV9\", expected a string", expected a boolean"#,
            "invalid type, expected a boolean",
        );
        fixed(
            "mixed delimiters",
            "invalid type: string \"`a\\\" \u{2026}ZXQV9` \\\"b`\", expected a boolean",
            "invalid type, expected a boolean",
        );
        fixed(
            "integer",
            "invalid value: integer `300ZXQV9`, expected u8",
            "invalid value, expected u8",
        );
        fixed(
            "length",
            "invalid length 7, expected a tuple of size 2 at line 1 column 9",
            "invalid length, expected a tuple of size 2",
        );
        // an unknown variant carrying a backtick, a quote and the separator itself
        fixed(
            "variant with delimiters",
            "unknown variant `side`ways\", expected ZXQV9`, expected one of `out`, `in`",
            "unknown variant, expected one of `out`, `in`",
        );
        fixed(
            "unknown field",
            "unknown field `bogusZXQV9`, expected `query` or `limit`",
            "unknown field, expected `query` or `limit`",
        );
        fixed(
            "unknown field of a fieldless type",
            "unknown field `ZXQV9, expected q`, there are no fields",
            "unknown field",
        );
        fixed("duplicate", "duplicate field `ZXQV9`", "duplicate field");
        // the one name kept is the request type's own
        fixed(
            "missing",
            "missing field `start_ids`",
            "missing field `start_ids`",
        );
        fixed(
            "custom message",
            "a custom deserializer said: ZXQV9 is not a thing",
            UNCLASSIFIED,
        );
    }

    #[test]
    fn the_router_prefix_is_kept_and_other_error_texts_pass_through_for_the_gate() {
        let typed =
            format!("{ARG_ERROR_PREFIX} invalid type: string \"yes-{MARKER}\", expected a boolean");
        let out = scrub_error_text(&typed);
        assert!(!out.contains(MARKER), "the input survived the router text");
        assert!(
            out == format!("{ARG_ERROR_PREFIX} invalid type, expected a boolean"),
            "the router text is the prefix plus the fixed failure"
        );
        let other = "not found: \"kept\"";
        assert_eq!(scrub_error_text(other), other);
    }
}
