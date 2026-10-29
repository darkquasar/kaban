//! `remember` v3 helpers (core-spec F1, §4, §6; core-plan C3.1): the wire to stored `CodeRef`
//! conversion, the Serena ingestion adapter and the title-and-scope near-duplicate key. Field
//! names follow Serena's output (settled by research E4); no Serena code is reused. Kind props
//! are validated by [`crate::ontology::validate_kind_props`] against the kind's `props_schema`
//! (engine-spec EN-F3, E1.5); the typed-struct path survives only in the agreement test below.

use crate::control::SNAPSHOT_COMMIT;
use crate::error::ApiError;
use crate::types::CodeRefWire;
use kaban_codegraph::{CodeGraphReader, HeadInput, NodeKind, NodeRow, Resolved};
use kaban_core::CodeRef;
use serde_json::{Map, Value};
use std::collections::BTreeMap;

/// Tool name stamped on refs produced by [`coderefs_from_serena`].
pub(crate) const SERENA_TOOL: &str = "serena";

/// Serena result keys that are never stored and are dropped before anything else.
const DROPPED_KEYS: &[&str] = &["body", "content_around_reference", "children"];

impl TryFrom<CodeRefWire> for CodeRef {
    type Error = ApiError;

    /// Normalise backslashes to `/`, derive `line` (`location.line`, else
    /// `body_location.start_line`) and drop `info` (it was scanned by the caller).
    fn try_from(w: CodeRefWire) -> Result<Self, ApiError> {
        let relative_path = w.relative_path.replace('\\', "/");
        if relative_path.trim().is_empty() {
            return Err(ApiError::Invalid("refs.relative_path".into()));
        }
        let line = w
            .location
            .map(|l| l.line)
            .or_else(|| w.body_location.map(|b| b.start_line));
        Ok(Self {
            repo: w.repo,
            commit: w.commit,
            tool: w.tool,
            relative_path,
            name_path: w.name_path,
            kind: w.kind,
            body_location: w.body_location,
            location: w.location,
            reference_line: w.reference_line,
            symbol: w.symbol,
            snapshot_commit: w.snapshot_commit,
            observed_at: w.observed_at,
            line,
        })
    }
}

/// The fixed `symbol` rejection text (G1-F9 `symbol_invalid`): never the value. Also the
/// refusal of a derived key the live policy would redact (RU3-1: a structural key is never
/// rewritten).
pub(crate) fn symbol_invalid() -> ApiError {
    ApiError::Invalid("refs[].symbol: symbol_invalid".into())
}

/// The fixed `snapshot_commit` rejection text (SO-2): never the value.
fn snapshot_commit_invalid() -> ApiError {
    ApiError::Invalid("refs[].snapshot_commit".into())
}

/// The key-form rule of a `CodeRef.symbol`, the one check every path into the field passes
/// (G1-F9, G1-S4; RW1-3): the size bound is applied before the parser sees the string, then
/// `kaban_codegraph::parse`. A `CodeRef.symbol` is a normalised global key (stage1-spec §4.2;
/// RU3-6, OWN-81 (3)): the version segment must be `.` and no descriptor may be `local`, so a
/// versioned form or a document-local symbol is refused here while the code tools' `ref`
/// selector keeps accepting them.
///
/// # Errors
/// `Invalid("refs[].symbol: symbol_invalid")`, the value never echoed.
fn global_key(s: &str) -> Result<kaban_codegraph::symbol::Symbol, ApiError> {
    if s.len() > kaban_codegraph::symbol::MAX_SYMBOL_BYTES {
        return Err(symbol_invalid());
    }
    let sym = kaban_codegraph::parse(s).map_err(|_| symbol_invalid())?;
    let local = sym
        .descriptors
        .iter()
        .any(|d| matches!(d, kaban_codegraph::symbol::Descriptor::Local(_)));
    if sym.version != "." || local {
        return Err(symbol_invalid());
    }
    Ok(sym)
}

/// The grammar check of an explicit `symbol` after the content scan ([`global_key`]); the
/// stored form is `format` of the parse.
///
/// # Errors
/// `Invalid("refs[].symbol: symbol_invalid")`, the value never echoed.
pub(crate) fn check_symbol(s: &str) -> Result<String, ApiError> {
    Ok(kaban_codegraph::format(&global_key(s)?))
}

/// The same key-form rule over a key derived from an admitted snapshot ([`global_key`];
/// RW1-3): the snapshot's admission checks file and schema integrity, not key form, and
/// `symbol_unique` checks a row count, so a uniquely stored versioned or `local` key reaches
/// here and is refused. The key is checked, never rewritten (RU3-1: a structural key is never
/// rewritten), so the stored form stays byte-equal to the snapshot's row.
///
/// # Errors
/// `Invalid("refs[].symbol: symbol_invalid")`, the value never echoed.
pub(crate) fn check_derived_symbol(key: &str) -> Result<(), ApiError> {
    global_key(key).map(drop)
}

/// The 40-hex canonical form of an admitted `snapshot_commit` (SO-2; cg-join-v1 §5): 40 hex is
/// stored as given; 7 to 39 hex only when it prefixes exactly one of the resolved snapshot's
/// commit and the head's ancestors (deduplicated, already fetched: no git call here); anything
/// else, including a short value with no resolved snapshot and 41 to 64 hex, is refused. The
/// control shape is re-checked first: a value the pre-parse scan treated as content (uppercase,
/// non-hex) is refused here too.
///
/// # Errors
/// `Invalid("refs[].snapshot_commit")`, the value never echoed.
pub(crate) fn canonical_commit(
    value: &str,
    resolved: Option<&Resolved>,
    head: Option<&HeadInput>,
) -> Result<String, ApiError> {
    if !SNAPSHOT_COMMIT.accepts(value) {
        return Err(snapshot_commit_invalid());
    }
    if value.len() == FULL_COMMIT_HEX {
        return Ok(value.to_owned());
    }
    if value.len() > FULL_COMMIT_HEX {
        return Err(snapshot_commit_invalid());
    }
    let resolved = resolved.ok_or_else(snapshot_commit_invalid)?;
    let mut candidates: Vec<&str> = vec![resolved.r#ref.commit.as_str()];
    if let Some(head) = head {
        candidates.extend(head.ancestors.iter().map(String::as_str));
    }
    candidates.sort_unstable();
    candidates.dedup();
    let mut matches = candidates.into_iter().filter(|c| c.starts_with(value));
    match (matches.next(), matches.next()) {
        (Some(c), None) => Ok(c.to_owned()),
        _ => Err(snapshot_commit_invalid()),
    }
}

/// The length of a full commit id in hex.
const FULL_COMMIT_HEX: usize = 40;

/// The name a Serena `name_path` ends in: its last `/` segment minus a trailing `[n]`
/// overload suffix (`Class/method[2]` is `method`).
pub(crate) fn name_of(name_path: &str) -> &str {
    let last = name_path.rsplit('/').next().unwrap_or(name_path);
    match last.strip_suffix(']').and_then(|s| s.rsplit_once('[')) {
        Some((name, index)) if !index.is_empty() && index.bytes().all(|b| b.is_ascii_digit()) => {
            name
        }
        _ => last,
    }
}

/// The one `symbol` key of a file's rows for `name_path` (G1-F9, SD-6; RU3-7): exactly one of
/// the file's rows named like the path's last segment that is `kind = symbol`, not external
/// and carries a key is the candidate (zero or several rows, two sharing one key included, are
/// `ambiguous_symbol`); the snapshot then confirms the key is unique snapshot-wide
/// (`symbol_unique`, cg-join-v1 §6), so a key another file also defines is `ambiguous_symbol`
/// too. A key the snapshot cannot confirm (a read failure) is not derived.
///
/// # Errors
/// `Invalid("refs[].name_path: ambiguous_symbol")`.
pub(crate) fn derive_symbol(
    snapshot: &(impl CodeGraphReader + ?Sized),
    rows: &[NodeRow],
    name_path: &str,
) -> Result<String, ApiError> {
    let ambiguous = || ApiError::Invalid("refs[].name_path: ambiguous_symbol".into());
    let name = name_of(name_path);
    let mut keys = rows
        .iter()
        .filter(|r| r.kind == NodeKind::Symbol && !r.external && r.name == name)
        .filter_map(|r| r.key.as_deref());
    let (Some(key), None) = (keys.next(), keys.next()) else {
        return Err(ambiguous());
    };
    match snapshot.symbol_unique(key) {
        Ok(Some(_)) => Ok(key.to_owned()),
        Ok(None) | Err(_) => Err(ambiguous()),
    }
}

/// The RFC 3339 shape of an explicit `observed_at` (stage1-spec §4.4):
/// `YYYY-MM-DDThh:mm:ss[.frac](Z|±hh:mm)`, fields in range and the date a calendar date
/// (month length, the leap day by the Gregorian rule; RU3-9); hand-written because the
/// workspace `time` has `formatting` only.
///
/// # Errors
/// `Invalid("refs[].observed_at")`, the value never echoed.
pub(crate) fn check_observed_at(s: &str) -> Result<(), ApiError> {
    fn digits(s: &[u8], range: std::ops::RangeInclusive<u32>) -> Option<u32> {
        if s.is_empty() || !s.iter().all(u8::is_ascii_digit) {
            return None;
        }
        let v = std::str::from_utf8(s).ok()?.parse::<u32>().ok()?;
        range.contains(&v).then_some(v)
    }
    fn days_in_month(year: u32, month: u32) -> u32 {
        match month {
            1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
            4 | 6 | 9 | 11 => 30,
            _ => {
                let leap = year.is_multiple_of(4)
                    && (!year.is_multiple_of(100) || year.is_multiple_of(400));
                if leap { 29 } else { 28 }
            }
        }
    }
    let invalid = || ApiError::Invalid("refs[].observed_at".into());
    let b = s.as_bytes();
    if b.len() < 20 {
        return Err(invalid());
    }
    // the date, fixed positions, then the calendar
    let (year, month, day) = match (
        digits(&b[0..4], 0..=9999),
        digits(&b[5..7], 1..=12),
        digits(&b[8..10], 1..=31),
    ) {
        (Some(year), Some(month), Some(day)) if b[4] == b'-' && b[7] == b'-' => (year, month, day),
        _ => return Err(invalid()),
    };
    if day > days_in_month(year, month) {
        return Err(invalid());
    }
    // the time, fixed positions
    let ok = (b[10] == b'T' || b[10] == b't')
        && digits(&b[11..13], 0..=23).is_some()
        && b[13] == b':'
        && digits(&b[14..16], 0..=59).is_some()
        && b[16] == b':'
        && digits(&b[17..19], 0..=60).is_some();
    if !ok {
        return Err(invalid());
    }
    let mut rest = &b[19..];
    if rest.first() == Some(&b'.') {
        let frac = rest[1..].iter().take_while(|c| c.is_ascii_digit()).count();
        if frac == 0 {
            return Err(invalid());
        }
        rest = &rest[1 + frac..];
    }
    let offset_ok = match rest {
        [b'Z' | b'z'] => true,
        [sign, h1, h2, b':', m1, m2] if *sign == b'+' || *sign == b'-' => {
            digits(&[*h1, *h2], 0..=23).is_some() && digits(&[*m1, *m2], 0..=59).is_some()
        }
        _ => false,
    };
    offset_ok.then_some(()).ok_or_else(invalid)
}

/// Now, RFC 3339 UTC (the `observed_at` default of stage1-spec §4.4).
pub(crate) fn now_rfc3339() -> Option<String> {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .ok()
}

/// Convert a Serena result value into wire refs, flat or grouped (core-spec §6):
///
/// - an array of entries (`find_symbol`, `find_referencing_symbols` flat form): one ref per
///   object entry; bare strings (`get_symbols_overview` names) yield nothing, they carry no path;
/// - an object keyed `relative_path -> kind -> [entry]` (grouped `find_referencing_symbols`):
///   flattened with the group's `relative_path` and `kind` re-injected where the entry has none;
/// - a single entry object (has `relative_path`).
///
/// `body`, `content_around_reference` and `children` are dropped before the entry is parsed;
/// every remaining key must be a `CodeRefWire` field. `tool` is `"serena"`.
///
/// # Errors
/// `ApiError::Invalid("refs")` for a shape that is neither, or an entry with unknown keys
/// (the message never echoes a value).
pub fn coderefs_from_serena(
    v: &Value,
    repo: &str,
    commit: &str,
) -> Result<Vec<CodeRefWire>, ApiError> {
    let mut out = Vec::new();
    match v {
        Value::Array(entries) => {
            for e in entries {
                if let Value::Object(o) = e {
                    out.push(entry_to_wire(o, repo, commit, None, None)?);
                }
                // bare strings (overview names) carry no path: no ref
            }
        }
        Value::Object(o) if o.contains_key("relative_path") => {
            out.push(entry_to_wire(o, repo, commit, None, None)?);
        }
        Value::Object(groups) => {
            for (relative_path, kinds) in groups {
                let Value::Object(kinds) = kinds else {
                    return Err(ApiError::Invalid("refs".into()));
                };
                for (kind, entries) in kinds {
                    let Value::Array(entries) = entries else {
                        return Err(ApiError::Invalid("refs".into()));
                    };
                    for e in entries {
                        let Value::Object(o) = e else {
                            return Err(ApiError::Invalid("refs".into()));
                        };
                        out.push(entry_to_wire(
                            o,
                            repo,
                            commit,
                            Some(relative_path),
                            Some(kind),
                        )?);
                    }
                }
            }
        }
        _ => return Err(ApiError::Invalid("refs".into())),
    }
    Ok(out)
}

/// One Serena entry as a wire ref: dropped keys removed, group keys re-injected, `repo`,
/// `commit` and `tool` set, then parsed strictly.
fn entry_to_wire(
    entry: &Map<String, Value>,
    repo: &str,
    commit: &str,
    relative_path: Option<&str>,
    kind: Option<&str>,
) -> Result<CodeRefWire, ApiError> {
    let mut m = Map::new();
    for (k, v) in entry {
        if DROPPED_KEYS.contains(&k.as_str()) {
            continue;
        }
        m.insert(k.clone(), v.clone());
    }
    if let Some(p) = relative_path
        && !m.contains_key("relative_path")
    {
        m.insert("relative_path".into(), Value::String(p.to_owned()));
    }
    if let Some(k) = kind
        && !m.contains_key("kind")
    {
        m.insert("kind".into(), Value::String(k.to_owned()));
    }
    m.insert("repo".into(), Value::String(repo.to_owned()));
    m.insert("commit".into(), Value::String(commit.to_owned()));
    m.insert("tool".into(), Value::String(SERENA_TOOL.to_owned()));
    serde_json::from_value(Value::Object(m)).map_err(|e| invalid_field("refs", &e))
}

/// The pre-E1.5 request-path validation of kind props against their typed struct
/// (`deny_unknown_fields`): Decision (needs `rationale`), Convention and Failure; other kinds
/// free-form. Kept behind the agreement test only (engine-spec EN-F3: "the three Rust structs
/// remain as validators of the shipped `adr` ontology during the transition").
///
/// # Errors
/// `ApiError::Invalid("props.<field>")` naming the missing or unknown field, `Invalid("props")`
/// for a type mismatch.
#[cfg(test)]
fn validate_props(kind: &str, props: &Value) -> Result<(), ApiError> {
    use kaban_core::{ConventionProps, DecisionProps, FailureProps};
    let r = match kind {
        "Decision" => serde_json::from_value::<DecisionProps>(props.clone()).map(|_| ()),
        "Convention" => serde_json::from_value::<ConventionProps>(props.clone()).map(|_| ()),
        "Failure" => serde_json::from_value::<FailureProps>(props.clone()).map(|_| ()),
        _ => return Ok(()),
    };
    r.map_err(|e| invalid_field("props", &e))
}

/// `Invalid("<prefix>.<field>")` from a serde error that names a field in backticks (missing or
/// unknown field), `Invalid("<prefix>")` otherwise. Only the field name is echoed, never a value.
fn invalid_field(prefix: &str, e: &serde_json::Error) -> ApiError {
    let msg = e.to_string();
    let named = (msg.starts_with("missing field") || msg.starts_with("unknown field"))
        .then(|| msg.split('`').nth(1))
        .flatten();
    match named {
        Some(field) => ApiError::Invalid(format!("{prefix}.{field}")),
        None => ApiError::Invalid(prefix.to_owned()),
    }
}

/// Near-duplicate key of a record whose kind has `dedup: title+scope` (F1, EN-F4; the shipped
/// Decision): the separator-free normalized title (summary, else the first line of the text)
/// and the sorted `path_scope`, `\u{1}`-separated.
pub(crate) fn dup_key(summary: Option<&str>, text: &str, path_scope: &[String]) -> String {
    let title = summary
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| text.lines().next().unwrap_or(""));
    let mut scope: Vec<&str> = path_scope.iter().map(String::as_str).collect();
    scope.sort_unstable();
    let mut key = kaban_policy::norm_term(title, false);
    for s in scope {
        key.push('\u{1}');
        key.push_str(s);
    }
    key
}

/// The text of a `CodeRef` node: canonical JSON of the stored fields with sorted keys and no
/// absent fields, so the store's `(workspace, kind, text)` content hash deduplicates refs by
/// their stored fields (M3-2). `symbol` and `snapshot_commit` are part of the text: a ref with
/// and without `symbol` are two nodes (G1-F9). `observed_at` is left out: a repeated ref still
/// deduplicates to one node, whose first `observed_at` stands (stage1-spec §4.4).
pub(crate) fn coderef_text(c: &CodeRef) -> String {
    let v = serde_json::to_value(c).unwrap_or(Value::Null);
    let mut sorted: BTreeMap<String, Value> = match v {
        Value::Object(m) => m.into_iter().collect(),
        _ => BTreeMap::new(),
    };
    sorted.remove("observed_at");
    serde_json::to_string(&sorted).unwrap_or_default()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion
mod tests {
    use super::*;
    use crate::ontology::{OntologyRegistry, validate_kind_props};
    use serde_json::json;

    /// E1.5 (engine-spec EN-F3): on the request path the validator seam emits the texts the
    /// typed-struct path emitted for the shipped kinds (`props.<field>` for a missing or
    /// unknown field, `Ok` for the typed shapes, free-form kinds untouched), case by case.
    #[test]
    fn the_validator_seam_agrees_with_the_struct_path_on_the_request_path_texts() {
        // Arrange
        let registry = OntologyRegistry::new(None).unwrap();
        let cases: [(&str, Value); 9] = [
            ("Decision", json!({})),
            ("Decision", json!({"decision": "x"})),
            ("Decision", json!({"rationale": "why", "ratonale": "typo"})),
            (
                "Decision",
                json!({"rationale": "why", "evidence": [{"id": "01a0", "quote": "q"}]}),
            ),
            ("Convention", json!({"rule": "r", "rationale": "why"})),
            ("Convention", json!({"unknown": 1})),
            (
                "Failure",
                json!({"error_text": "boom", "pending_fix": true}),
            ),
            ("Failure", json!({"stak_head": "typo"})),
            ("Fact", json!({"anything": ["goes"]})),
        ];

        for (kind, props) in &cases {
            // Act
            let seam = validate_kind_props(&registry, kind, props).map_err(|e| e.to_string());
            let structs = validate_props(kind, props).map_err(|e| e.to_string());

            // Assert
            assert_eq!(seam, structs, "{kind} {props}");
        }
        assert_eq!(
            validate_kind_props(&registry, "Decision", &json!({"decision": "x"}))
                .unwrap_err()
                .to_string(),
            ApiError::Invalid("props.rationale".into()).to_string()
        );
    }
}
