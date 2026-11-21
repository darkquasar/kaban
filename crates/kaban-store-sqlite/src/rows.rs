//! Row decoders: the [`NODE_COLS`] projection and the `rusqlite::Row` to [`Node`], [`TypeDef`] and
//! [`Edge`] conversions shared by the store's selects. The body (`text`) is decoded here
//! (`body_codec::read_column`, spec 05 SS-17): plain or encoded, the caller sees the original.

use kaban_core::{Edge, Lifecycle, Node, StoreScope, TypeDef};
use serde_json::Value;

/// Columns read into a [`Node`], in [`row_node`] order. `status` and `source_session` are frozen
/// legacy columns (MD-9) and are neither read nor written.
pub(crate) const NODE_COLS: &str = "id,kind,text,summary,tags,confidence,importance,lifecycle,store_scope,path_scope,rev,dup_key,created_at,updated_at,valid_from,valid_to,session_id,agent_id,workspace_id,repo,commit_sha,props,content_hash";

/// A persisted JSON column that no longer parses (RB-5): a read error with the
/// `serde_json::Error` as its source, never silently a default value, because the caller must
/// not act on data the database does not hold. The message names the column index and the
/// parser's position only; the stored bytes are never echoed (core-spec S1).
fn bad_json(idx: usize, e: serde_json::Error) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(idx, rusqlite::types::Type::Text, Box::new(e))
}

/// Decode the JSON string list (`tags`, `path_scope`, endpoint kinds) persisted in column `idx`.
pub(crate) fn decode_json_strings(idx: usize, raw: &str) -> rusqlite::Result<Vec<String>> {
    serde_json::from_str(raw).map_err(|e| bad_json(idx, e))
}

/// Decode the JSON document (props, props schema) persisted in column `idx`.
pub(crate) fn decode_json_value(idx: usize, raw: &str) -> rusqlite::Result<Value> {
    serde_json::from_str(raw).map_err(|e| bad_json(idx, e))
}

/// A column value outside the enum's vocabulary (the CHECK constraint makes this unreachable).
pub(crate) fn bad_enum(idx: usize, value: &str) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        idx,
        rusqlite::types::Type::Text,
        format!("unexpected state value '{value}'").into(),
    )
}

pub(crate) fn row_node(r: &rusqlite::Row<'_>) -> rusqlite::Result<Node> {
    row_node_at(r, 0)
}

/// [`row_node`] over [`NODE_COLS`] starting at column `at` (for selects that prefix them).
pub(crate) fn row_node_at(r: &rusqlite::Row<'_>, at: usize) -> rusqlite::Result<Node> {
    let tags: String = r.get(at + 4)?;
    let lifecycle: String = r.get(at + 7)?;
    let store_scope: String = r.get(at + 8)?;
    let path_scope: String = r.get(at + 9)?;
    let props: String = r.get(at + 21)?;
    Ok(Node {
        id: r.get(at)?,
        kind: r.get(at + 1)?,
        text: crate::body_codec::read_column(r, at + 2)?,
        summary: r.get(at + 3)?,
        tags: decode_json_strings(at + 4, &tags)?,
        confidence: r.get(at + 5)?,
        importance: r.get(at + 6)?,
        lifecycle: Lifecycle::parse(&lifecycle).ok_or_else(|| bad_enum(at + 7, &lifecycle))?,
        store_scope: StoreScope::parse(&store_scope)
            .ok_or_else(|| bad_enum(at + 8, &store_scope))?,
        path_scope: decode_json_strings(at + 9, &path_scope)?,
        rev: r.get(at + 10)?,
        dup_key: r.get(at + 11)?,
        created_at: r.get(at + 12)?,
        updated_at: r.get(at + 13)?,
        valid_from: r.get(at + 14)?,
        valid_to: r.get(at + 15)?,
        session_id: r.get(at + 16)?,
        agent_id: r.get(at + 17)?,
        workspace_id: r.get(at + 18)?,
        repo: r.get(at + 19)?,
        commit_sha: r.get(at + 20)?,
        props: decode_json_value(at + 21, &props)?,
        content_hash: r.get(at + 22)?,
    })
}
/// Decode a `schema_type` row in the [`crate::ontology::TypeTable::columns`] projection: on
/// the 0004 shape the ninth column is `ns` and `name` is bare, so the stored name is rebuilt
/// by the core rule; on the old shape the ninth column is `NULL` and `name` is already stored.
pub(crate) fn row_typedef(r: &rusqlite::Row<'_>) -> rusqlite::Result<TypeDef> {
    let f: String = r.get(3)?;
    let t: String = r.get(4)?;
    let ps: Option<String> = r.get(6)?;
    let ns: Option<String> = r.get(8)?;
    let name: String = r.get(1)?;
    let name = match &ns {
        Some(ns) => {
            let ns = kaban_core::Ns::parse(ns).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    8,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })?;
            kaban_core::stored_name(&kaban_core::Qualified { ns, name })
        }
        None => name,
    };
    Ok(TypeDef {
        kind: r.get(0)?,
        name,
        ns,
        description: r.get(2)?,
        from_kinds: decode_json_strings(3, &f)?,
        to_kinds: decode_json_strings(4, &t)?,
        directed: r.get::<_, i64>(5)? != 0,
        props_schema: ps.map(|s| decode_json_value(6, &s)).transpose()?,
        builtin: r.get::<_, i64>(7)? != 0,
        declaration: kaban_core::TypeDeclaration {
            per_commit: r.get(17)?,
            rules: json_col(r, 9)?,
            rank: json_col(r, 10)?,
            column_weights: json_col(r, 11)?,
            extract: json_col(r, 12)?,
            display: json_col(r, 13)?,
            columns: list_col(r, 14, |s| serde_json::from_str(s))?,
            rollups: list_col(r, 15, |s| serde_json::from_str(s))?,
            fts_fields: list_col(r, 16, |s| serde_json::from_str(s))?,
        },
    })
}

fn list_col<T>(
    r: &rusqlite::Row<'_>,
    idx: usize,
    decode: impl FnOnce(&str) -> serde_json::Result<Vec<T>>,
) -> rusqlite::Result<Vec<T>> {
    match r.get::<_, Option<String>>(idx)? {
        Some(text) => decode(&text).map_err(|e| bad_json(idx, e)),
        None => Ok(Vec::new()),
    }
}
/// A nullable JSON text column (the 0004 declaration columns; `NULL` on the 0003 shape).
fn json_col(r: &rusqlite::Row<'_>, idx: usize) -> rusqlite::Result<Option<serde_json::Value>> {
    let text: Option<String> = r.get(idx)?;
    text.map(|s| decode_json_value(idx, &s)).transpose()
}
pub(crate) fn row_edge(r: &rusqlite::Row<'_>) -> rusqlite::Result<Edge> {
    let props: String = r.get(6)?;
    Ok(Edge {
        id: r.get(0)?,
        src: r.get(1)?,
        dst: r.get(2)?,
        rel: r.get(3)?,
        confidence: r.get(4)?,
        created_at: r.get(5)?,
        props: decode_json_value(6, &props)?,
    })
}

/// `s` as epoch milliseconds when it is an RFC 3339 date-time (`YYYY-MM-DDTHH:MM:SS`, an
/// optional fraction, then `Z` or `±HH:MM` with hours 0-23 and minutes 0-59); `None`
/// otherwise. Digits below a millisecond are
/// truncated.
#[expect(
    clippy::many_single_char_names,
    reason = "the date fields and the civil-date algorithm's own names"
)]
pub(crate) fn rfc3339_ms(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    let num = |r: std::ops::Range<usize>| -> Option<i64> {
        b.get(r)?.iter().try_fold(0_i64, |acc, d| {
            d.is_ascii_digit().then(|| acc * 10 + i64::from(d - b'0'))
        })
    };
    let at = |i: usize, c: &[u8]| b.get(i).is_some_and(|x| c.contains(x));
    if !(at(4, b"-") && at(7, b"-") && at(10, b"Tt ") && at(13, b":") && at(16, b":")) {
        return None;
    }
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, sec) = (num(11..13)?, num(14..16)?, num(17..19)?);
    let mut i = 19;
    let mut ms = 0;
    if at(i, b".") {
        let start = i + 1;
        i = start;
        while b.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
        let digits = (i - start).min(3);
        if digits == 0 {
            return None;
        }
        ms = num(start..start + digits)? * 10_i64.pow(u32::try_from(3 - digits).ok()?);
    }
    let offset_min = match b.get(i..)? {
        [b'Z' | b'z'] => 0,
        [sign @ (b'+' | b'-'), _, _, b':', _, _] => {
            let (oh, om) = (num(i + 1..i + 3)?, num(i + 4..i + 6)?);
            if oh > 23 || om > 59 {
                return None;
            }
            let m = oh * 60 + om;
            if *sign == b'+' { m } else { -m }
        }
        _ => return None,
    };
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let month_days = match mo {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return None,
    };
    if !(1..=month_days).contains(&d) || h > 23 || mi > 59 || sec > 60 {
        return None;
    }
    // days from the civil date (Howard Hinnant's algorithm)
    let yy = if mo <= 2 { y - 1 } else { y };
    let era = yy.div_euclid(400);
    let yoe = yy - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = days * 86_400 + h * 3_600 + mi * 60 + sec - offset_min * 60;
    Some(secs * 1_000 + ms)
}
