//! The one `edge` row writer (the `INSERT ... ON CONFLICT(src,dst,rel) DO UPDATE` statement
//! lives here and nowhere else), its owner tag and the clock the job and batch paths share.
//! Kept from the deleted reconciliation projection: every edge is written directly.

use crate::ontology::TypeTable;
use crate::sql_error::SqlResultExt;
use kaban_core::{Layer, Method, Owner, Result, new_id, now_iso};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Map, Value};

/// Whether the connection carries the 0004 scoring columns of `edge` (`layer`, `method`,
/// `score`, the span): a store opened at an older schema (fixtures, benches) has none.
pub(crate) fn scoring_columns(conn: &Connection) -> Result<bool> {
    Ok(TypeTable::of(conn)? == TypeTable::Qualified)
}

/// Seconds since the epoch, saturating.
pub(crate) fn now_unix() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs()),
    )
    .unwrap_or(i64::MAX)
}

/// One `edge` row as the store writes it: the structural edges of a producer batch carry no
/// scoring columns; a mention edge carries its span and score.
pub(crate) struct EdgeWrite<'a> {
    pub(crate) src: i64,
    pub(crate) dst: i64,
    pub(crate) rel: &'a str,
    pub(crate) confidence: Option<f64>,
    /// The stored props object, `props.owner` included (EN-E4).
    pub(crate) props: &'a Value,
    pub(crate) layer: Option<Layer>,
    pub(crate) method: Option<Method>,
    pub(crate) score: Option<f64>,
    pub(crate) source_id: Option<&'a str>,
    pub(crate) offset: Option<u32>,
    pub(crate) len: Option<u32>,
}

/// The row as stored, for the "unchanged writes nothing" rule (N5).
type Stored = (
    Option<f64>,
    String,
    Option<String>,
    Option<String>,
    Option<f64>,
    Option<String>,
    Option<u32>,
    Option<u32>,
);

/// Write the edge unless the stored row already holds these values. Returns `Some(true)` for
/// a new row, `Some(false)` for a refreshed one, `None` when nothing was written.
pub(crate) fn write_edge(conn: &Connection, e: &EdgeWrite<'_>) -> Result<Option<bool>> {
    let props = serde_json::to_string(e.props)?;
    if !scoring_columns(conn)? {
        return write_edge_0002(conn, e, &props);
    }
    let stored: Option<Stored> = conn
        .query_row(
            "SELECT confidence, props, layer, method, score, source_id, offset, len
             FROM edge WHERE src=?1 AND dst=?2 AND rel=?3",
            params![e.src, e.dst, e.rel],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    r.get(7)?,
                ))
            },
        )
        .optional()
        .sql()?;
    let layer = e.layer.map(Layer::as_str);
    let method = e.method.map(Method::as_str);
    if let Some((c, p, l, m, s, sid, off, len)) = &stored
        && *c == e.confidence
        && *p == props
        && l.as_deref() == layer
        && m.as_deref() == method
        && *s == e.score
        && sid.as_deref() == e.source_id
        && *off == e.offset
        && *len == e.len
    {
        return Ok(None);
    }
    // the edge is tenant-scoped like its source node (F17)
    conn.execute(
        "INSERT INTO edge(id,src,dst,rel,confidence,created_at,props,workspace_id,layer,method,score,source_id,offset,len)
         VALUES (?1,?2,?3,?4,?5,?6,?7,(SELECT workspace_id FROM node WHERE rid=?2),?8,?9,?10,?11,?12,?13)
         ON CONFLICT(src,dst,rel) DO UPDATE SET confidence=excluded.confidence, props=excluded.props,
             layer=excluded.layer, method=excluded.method, score=excluded.score,
             source_id=excluded.source_id, offset=excluded.offset, len=excluded.len",
        params![
            new_id(),
            e.src,
            e.dst,
            e.rel,
            e.confidence,
            now_iso(),
            props,
            layer,
            method,
            e.score,
            e.source_id,
            e.offset,
            e.len
        ],
    )
    .sql()?;
    Ok(Some(stored.is_none()))
}

/// The edge write of a store below schema 0004 (the 0003 fixtures and benches): no scoring
/// columns, no claims; the pre-E2.2 statement.
fn write_edge_0002(conn: &Connection, e: &EdgeWrite<'_>, props: &str) -> Result<Option<bool>> {
    let stored: Option<(Option<f64>, String)> = conn
        .query_row(
            "SELECT confidence, props FROM edge WHERE src=?1 AND dst=?2 AND rel=?3",
            params![e.src, e.dst, e.rel],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .sql()?;
    if let Some((c, p)) = &stored
        && *c == e.confidence
        && p == props
    {
        return Ok(None);
    }
    conn.execute(
        "INSERT INTO edge(id,src,dst,rel,confidence,created_at,props,workspace_id)
         VALUES (?1,?2,?3,?4,?5,?6,?7,(SELECT workspace_id FROM node WHERE rid=?2))
         ON CONFLICT(src,dst,rel) DO UPDATE SET confidence=excluded.confidence, props=excluded.props",
        params![new_id(), e.src, e.dst, e.rel, e.confidence, now_iso(), props],
    )
    .sql()?;
    Ok(Some(stored.is_none()))
}

/// The stored `props.owner` of the `(src, dst, rel)` edge, if the row exists and carries one.
pub(crate) fn stored_owner(
    conn: &Connection,
    src: i64,
    dst: i64,
    rel: &str,
) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT json_extract(props, '$.owner') FROM edge WHERE src=?1 AND dst=?2 AND rel=?3",
            params![src, dst, rel],
            |r| r.get::<_, Option<String>>(0),
        )
        .optional()
        .sql()?
        .flatten())
}

/// The `props.owner` tag of an edge a write stores (EN-E4). A human write (`link`, a human
/// batch) is always `human`, whatever owner its caller put in `props`: the overwrite guard
/// and the MD-23 diff trust this tag, so only the producer boundary may name another owner.
/// A producer batch keeps the producer's value when it set one, else the batch's owner.
pub(crate) fn owned_props(props: Option<&Value>, owner: Owner) -> Value {
    let mut map = match props {
        Some(Value::Object(m)) => m.clone(),
        _ => Map::new(),
    };
    let tag = Value::String(owner.as_str().to_owned());
    if owner == Owner::Human {
        map.insert("owner".to_owned(), tag);
    } else {
        map.entry("owner").or_insert(tag);
    }
    Value::Object(map)
}
