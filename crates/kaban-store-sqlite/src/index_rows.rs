//! The canonical rows as the text index is built from them (spec 05 SS-08, spec 11 T7): the
//! `summary` column is the `summary_projection` SQL, the identifier inputs are the ones
//! `ident::refresh` selects, both read from the committed `index_meta` maps on every call, so
//! an ontology redeployment is seen by the next read, never by a cached map.

use crate::catalog::phase::Reader;
use crate::ident::{FIELDS, FTS_FIELDS, Fields, committed_fields, summary_projection_over};
use crate::sql_error::SqlResultExt;
use kaban_core::Result;
use kaban_core::query_budget::{BudgetLease, GrowError, QueryBudget};
use kaban_core::text_index::IndexRow;
use rusqlite::Connection;
use rusqlite::types::ValueRef;
use serde_json::Value;
use std::convert::Infallible;

/// The active rows among `rids`, in `rid` order, read through a commit's staging reader (spec
/// 11 T7: the text class stages the rows a commit changed; every statement is prepared fresh
/// in the phase).
///
/// # Errors
/// SQLite's; a stored value that does not decode.
pub(crate) fn staged_rows(
    reader: &Reader<'_>,
    rids: &[i64],
    budget: &QueryBudget,
) -> Result<(Vec<IndexRow>, BudgetLease)> {
    let mut meta = reader.prepare("SELECT value FROM index_meta WHERE key = ?1")?;
    let mut map = |key: &str| -> Result<Option<Fields>> {
        meta.query_row([key], |r| r.get::<_, String>(0))?
            .map(|v| serde_json::from_str(&v))
            .transpose()
            .map_err(Into::into)
    };
    let committed = map(FIELDS)?.zip(map(FTS_FIELDS)?);
    let sql = select(
        committed.as_ref(),
        " AND n.rid IN (SELECT value FROM json_each(?1))",
    );
    let ident_fields = committed.map(|(ident, _)| ident).unwrap_or_default();
    let selected = serde_json::to_string(rids)?;
    // the collected rows: at most one per rid, in a vector that grows by doubling
    let rows = rids
        .len()
        .saturating_mul(2 * size_of::<IndexRow>())
        .try_into()
        .unwrap_or(u64::MAX);
    let mut lease = budget.reserve(rows)?;
    let mut refused = None;
    let decoded = reader.prepare(&sql)?.query_map([selected], |r| {
        // each row is charged before it is decoded: its values and what is derived from them
        let refusal = match lease.grow_with(row_charge(r)?, || Ok::<_, Infallible>(())) {
            Ok(()) => return decode(r, &ident_fields),
            Err(GrowError::Budget(refusal)) => refusal,
            Err(GrowError::Alloc(never)) => match never {},
        };
        refused = Some(refusal);
        Err(rusqlite::Error::InvalidQuery)
    });
    match (refused, decoded) {
        (Some(refusal), _) => Err(refusal.into()),
        (None, decoded) => Ok((decoded?, lease)),
    }
}

/// Every active row in `rid` order, read on `conn` (spec 11 T7: the text class's rebuild, in
/// its commit's writer phase), each charged on `budget` before it is decoded, with its place
/// in the growing vector.
///
/// # Errors
/// SQLite's; a stored value that does not decode; the budget's refusal.
pub(crate) fn active_rows(
    conn: &Connection,
    budget: &QueryBudget,
) -> Result<(Vec<IndexRow>, BudgetLease)> {
    let committed = committed_fields(conn)?;
    let sql = select(committed.as_ref(), "");
    let ident_fields = committed.map(|(ident, _)| ident).unwrap_or_default();
    let slot = u64::try_from(2 * size_of::<IndexRow>()).unwrap_or(u64::MAX);
    let mut lease = budget.reserve(0)?;
    let mut refused = None;
    let mut stmt = conn.prepare(&sql).sql()?;
    let decoded = stmt
        .query_map([], |r| {
            let charge = row_charge(r)?.saturating_add(slot);
            let refusal = match lease.grow_with(charge, || Ok::<_, Infallible>(())) {
                Ok(()) => return decode(r, &ident_fields),
                Err(GrowError::Budget(refusal)) => refusal,
                Err(GrowError::Alloc(never)) => match never {},
            };
            refused = Some(refusal);
            Err(rusqlite::Error::InvalidQuery)
        })
        .sql()?
        .collect::<rusqlite::Result<Vec<_>>>();
    match (refused, decoded) {
        (Some(refusal), _) => Err(refusal.into()),
        (None, decoded) => Ok((decoded.sql()?, lease)),
    }
}

/// What one row of [`select`] costs decoded, read from the values SQLite holds before any is
/// copied: every value twice (the decoded value and the identifier values and parsed tags
/// derived from it), the body at its decoded length.
fn row_charge(r: &rusqlite::Row<'_>) -> rusqlite::Result<u64> {
    let mut bytes = u64::try_from(size_of::<IndexRow>()).unwrap_or(u64::MAX);
    for idx in 0..11 {
        let len = match r.get_ref(idx)? {
            ValueRef::Blob(b) if idx == 6 => crate::body_codec::decoded_len(b).unwrap_or(0),
            ValueRef::Text(b) | ValueRef::Blob(b) => b.len() as u64,
            ValueRef::Null | ValueRef::Integer(_) | ValueRef::Real(_) => 0,
        };
        bytes = bytes.saturating_add(len.saturating_mul(2));
    }
    Ok(bytes)
}

/// The statement of the active rows under the committed maps (`None`: a store without the
/// engine deployment, schema < 4, has neither a projection nor identifier fields nor anchors,
/// like its FTS5 index), filtered by `selected`.
fn select(committed: Option<&(Fields, Fields)>, selected: &str) -> String {
    let fts_fields = committed.map(|(_, fts)| fts.clone()).unwrap_or_default();
    let projection = summary_projection_over(&fts_fields, "n", "kaban_body(n.text)");
    let anchors = if committed.is_some() {
        "(SELECT json_group_array(key) FROM (SELECT key FROM node_anchor a WHERE a.rid = n.rid ORDER BY anchor_kind, key))"
    } else {
        "'[]'"
    };
    format!(
        "SELECT n.id, n.rid, n.rev, n.kind, n.workspace_id, n.tags, n.text, {projection}, n.summary, n.props, {anchors} \
         FROM node n WHERE n.lifecycle = 'active'{selected} ORDER BY n.rid"
    )
}

/// One row of [`select`] as the index sees it.
fn decode(r: &rusqlite::Row<'_>, ident_fields: &Fields) -> rusqlite::Result<IndexRow> {
    let json = |idx: usize, e: serde_json::Error| {
        rusqlite::Error::FromSqlConversionFailure(idx, rusqlite::types::Type::Text, Box::new(e))
    };
    let kind: String = r.get(3)?;
    let tags: Vec<String> =
        serde_json::from_str(&r.get::<_, String>(5)?).map_err(|e| json(5, e))?;
    let text = crate::body_codec::read_column(r, 6)?;
    let summary: Option<String> = r.get(7)?;
    let stored_summary: Option<String> = r.get(8)?;
    let props: Value = serde_json::from_str(&r.get::<_, String>(9)?).map_err(|e| json(9, e))?;
    let anchor_keys: Vec<String> =
        serde_json::from_str(&r.get::<_, String>(10)?).map_err(|e| json(10, e))?;
    let fields = ident_fields.get(&kind).map_or(&[][..], Vec::as_slice);
    Ok(IndexRow {
        id: r.get(0)?,
        rid: r.get(1)?,
        revision: r.get(2)?,
        ident_values: ident_values(fields, &text, stored_summary.as_deref(), &tags, &props),
        kind,
        workspace_id: r.get(4)?,
        tags,
        text,
        summary: summary.unwrap_or_default(),
        stored_summary,
        anchor_keys,
    })
}

/// The kind's `ident_fields` values of one row, exactly as `ident::refresh` selects them for
/// `ident_terms`: `text`, `summary` and `tags` by name, any other field as a `props` path whose
/// string and array-of-string values count.
fn ident_values(
    fields: &[String],
    text: &str,
    summary: Option<&str>,
    tags: &[String],
    props: &Value,
) -> Vec<String> {
    fn strings(value: &Value, out: &mut Vec<String>) {
        match value {
            Value::String(s) => out.push(s.clone()),
            Value::Array(items) => items.iter().for_each(|item| strings(item, out)),
            _ => {}
        }
    }
    let mut selected = Vec::new();
    for field in fields {
        match field.as_str() {
            "text" => selected.push(text.to_owned()),
            "summary" => selected.extend(summary.map(str::to_owned)),
            "tags" => selected.extend(tags.iter().cloned()),
            path => {
                let mut current = props;
                for part in path.strip_prefix("props.").unwrap_or(path).split('.') {
                    current = current.get(part).unwrap_or(&Value::Null);
                }
                strings(current, &mut selected);
            }
        }
    }
    selected
}
