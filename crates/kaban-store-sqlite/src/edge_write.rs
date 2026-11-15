//! The one `edge` row writer (the `INSERT ... ON CONFLICT(src,dst,rel) DO UPDATE` statement
//! lives here and nowhere else), the first-writer rule and the clock the job and batch paths
//! share. Every edge carries one producer identity, its `producer` column (PC-16).

use crate::ontology::TypeTable;
use crate::sql_error::SqlResultExt;
use kaban_core::{Method, Producer, Result, new_id, now_iso};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;

/// Whether the connection carries the 0004 scoring columns of `edge` (`producer`, `method`,
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

/// One `edge` row as the store writes it: every edge names its producer; the structural edges
/// of a producer batch carry no other scoring column, a mention edge carries its span and
/// score.
pub(crate) struct EdgeWrite<'a> {
    pub(crate) src: i64,
    pub(crate) dst: i64,
    pub(crate) rel: &'a str,
    pub(crate) confidence: Option<f64>,
    /// The stored props object.
    pub(crate) props: &'a Value,
    pub(crate) producer: Producer,
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

/// What [`write_edge`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Written {
    /// A new row.
    Inserted,
    /// The producer's own row, refreshed with new values.
    Refreshed,
    /// The producer's own row already held these values: nothing written (N5).
    Unchanged,
    /// Another producer wrote the triple first and keeps the row: nothing written (owner
    /// decision 2026-11-14; producer isolation).
    KeptByFirstWriter,
}

impl Written {
    /// Whether a row was inserted or changed.
    pub(crate) fn wrote(self) -> bool {
        matches!(self, Self::Inserted | Self::Refreshed)
    }
}

/// Write the edge under the first-writer rule: a triple another producer wrote is left as
/// it is, the producer's own row is refreshed unless it already holds these values.
pub(crate) fn write_edge(conn: &Connection, e: &EdgeWrite<'_>) -> Result<Written> {
    let props = serde_json::to_string(e.props)?;
    if !scoring_columns(conn)? {
        return write_edge_0002(conn, e, &props);
    }
    let stored: Option<Stored> = conn
        .query_row(
            "SELECT confidence, props, producer, method, score, source_id, offset, len
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
    let producer = e.producer.as_str();
    let method = e.method.map(Method::as_str);
    if let Some((c, p, l, m, s, sid, off, len)) = &stored {
        // a row without a producer (written before producers existed) has no first writer
        if l.as_deref().is_some_and(|stored| stored != producer) {
            return Ok(Written::KeptByFirstWriter);
        }
        if *c == e.confidence
            && *p == props
            && l.as_deref() == Some(producer)
            && m.as_deref() == method
            && *s == e.score
            && sid.as_deref() == e.source_id
            && *off == e.offset
            && *len == e.len
        {
            return Ok(Written::Unchanged);
        }
    }
    // the edge is tenant-scoped like its source node (F17)
    conn.execute(
        "INSERT INTO edge(id,src,dst,rel,confidence,created_at,props,workspace_id,producer,method,score,source_id,offset,len)
         VALUES (?1,?2,?3,?4,?5,?6,?7,(SELECT workspace_id FROM node WHERE rid=?2),?8,?9,?10,?11,?12,?13)
         ON CONFLICT(src,dst,rel) DO UPDATE SET confidence=excluded.confidence, props=excluded.props,
             producer=excluded.producer, method=excluded.method, score=excluded.score,
             source_id=excluded.source_id, offset=excluded.offset, len=excluded.len",
        params![
            new_id(),
            e.src,
            e.dst,
            e.rel,
            e.confidence,
            now_iso(),
            props,
            producer,
            method,
            e.score,
            e.source_id,
            e.offset,
            e.len
        ],
    )
    .sql()?;
    Ok(if stored.is_none() {
        Written::Inserted
    } else {
        Written::Refreshed
    })
}

/// The edge write of a store below schema 0004 (the 0003 fixtures and benches): no scoring
/// columns, so no producer and no first-writer rule; the pre-E2.2 statement.
fn write_edge_0002(conn: &Connection, e: &EdgeWrite<'_>, props: &str) -> Result<Written> {
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
        return Ok(Written::Unchanged);
    }
    conn.execute(
        "INSERT INTO edge(id,src,dst,rel,confidence,created_at,props,workspace_id)
         VALUES (?1,?2,?3,?4,?5,?6,?7,(SELECT workspace_id FROM node WHERE rid=?2))
         ON CONFLICT(src,dst,rel) DO UPDATE SET confidence=excluded.confidence, props=excluded.props",
        params![new_id(), e.src, e.dst, e.rel, e.confidence, now_iso(), props],
    )
    .sql()?;
    Ok(if stored.is_none() {
        Written::Inserted
    } else {
        Written::Refreshed
    })
}
