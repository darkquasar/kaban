//! The `ChangeLog` (core-spec F11; EN-E10b): reading `changes`, fenced cursor advance in
//! `projections` (MD-13) and bounded pruning. Rows are written by the triggers of migration 0002 and by the
//! explicit purge row (MD-24); this module never writes `changes` itself.

use crate::sql_error::SqlResultExt;
use kaban_core::{Change, ChangeEntity, ChangeOp, Result, StoreError};
use rusqlite::{Connection, params};
use std::collections::BTreeMap;

const CHANGE_COLS: &str = "seq,entity,op,id,workspace_id,ts,paths";

/// Rows with `seq > after`, oldest first, at most `limit`.
pub(crate) fn read(conn: &Connection, after: u64, limit: usize) -> Result<Vec<Change>> {
    let after = i64::try_from(after).unwrap_or(i64::MAX);
    let limit = i64::try_from(limit).unwrap_or(i64::MAX);
    let mut st = conn
        .prepare_cached(&format!(
            "SELECT {CHANGE_COLS} FROM changes WHERE seq > ? ORDER BY seq LIMIT ?"
        ))
        .sql()?;
    let rows = st
        .query_map(params![after, limit], row_change)
        .sql()?
        .collect::<rusqlite::Result<Vec<_>>>()
        .sql()?;
    Ok(rows)
}

fn bad_enum(idx: usize, value: &str) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        idx,
        rusqlite::types::Type::Text,
        format!("unexpected changes value '{value}'").into(),
    )
}

fn row_change(r: &rusqlite::Row<'_>) -> rusqlite::Result<Change> {
    let seq: i64 = r.get(0)?;
    let entity: String = r.get(1)?;
    let op: String = r.get(2)?;
    Ok(Change {
        seq: u64::try_from(seq).unwrap_or(0),
        entity: ChangeEntity::parse(&entity).ok_or_else(|| bad_enum(1, &entity))?,
        op: ChangeOp::parse(&op).ok_or_else(|| bad_enum(2, &op))?,
        id: r.get(3)?,
        workspace_id: r.get(4)?,
        ts: r.get(5)?,
        paths: r.get(6)?,
    })
}

/// Upsert `consumer`'s cursor to `max(current, upto)` when a lease named `consumer` holds
/// `fence` (MD-13). Zero affected rows means the fence is stale or absent: `LeaseLost`.
pub(crate) fn ack(conn: &Connection, consumer: &str, upto: u64, fence: u64) -> Result<()> {
    let upto_i = i64::try_from(upto).unwrap_or(i64::MAX);
    let fence_i = i64::try_from(fence).unwrap_or(i64::MAX);
    let changed = conn
        .execute(
            "INSERT INTO projections(name, last_seq)
         SELECT ?1, ?2 WHERE EXISTS(SELECT 1 FROM leases WHERE name = ?1 AND fence = ?3)
         ON CONFLICT(name) DO UPDATE SET last_seq = max(last_seq, excluded.last_seq)",
            params![consumer, upto_i, fence_i],
        )
        .sql()?;
    if changed == 0 {
        return Err(StoreError::LeaseLost {
            name: consumer.to_owned(),
            fence,
        });
    }
    Ok(())
}

/// Register `consumer`'s cursor at the newest `seq` (0 on an empty log) unless it already has
/// one, when a lease named `consumer` holds `fence`; returns the cursor (SS-06). Registered
/// before the consumer's bootstrap, the cursor is the floor [`prune`] keeps rows above.
pub(crate) fn register(conn: &Connection, consumer: &str, fence: u64) -> Result<u64> {
    let fence_i = i64::try_from(fence).unwrap_or(i64::MAX);
    let held: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM leases WHERE name = ?1 AND fence = ?2)",
            params![consumer, fence_i],
            |r| r.get(0),
        )
        .sql()?;
    if !held {
        return Err(StoreError::LeaseLost {
            name: consumer.to_owned(),
            fence,
        });
    }
    conn.execute(
        "INSERT INTO projections(name, last_seq)
         VALUES (?1, (SELECT COALESCE(max(seq), 0) FROM changes))
         ON CONFLICT(name) DO NOTHING",
        [consumer],
    )
    .sql()?;
    let cursor: i64 = conn
        .query_row(
            "SELECT last_seq FROM projections WHERE name = ?1",
            [consumer],
            |r| r.get(0),
        )
        .sql()?;
    Ok(u64::try_from(cursor).unwrap_or(0))
}

/// The newest row's `seq`, 0 for an empty log.
pub(crate) fn head(conn: &Connection) -> Result<u64> {
    let head: i64 = conn
        .query_row("SELECT COALESCE(max(seq), 0) FROM changes", [], |r| {
            r.get(0)
        })
        .sql()?;
    Ok(u64::try_from(head).unwrap_or(0))
}

/// Record the call's terminal sequence in `ctx` (spec 05 SS-10): the head as `tx` sees it,
/// read right before its commit. Under `BEGIN IMMEDIATE` no other writer commits in between,
/// so this is the call's last change row, or the head the call saw when it wrote none.
pub(crate) fn record_terminal(tx: &Connection, ctx: &kaban_core::WriteCtx) -> Result<()> {
    ctx.record_terminal_seq(head(tx)?);
    Ok(())
}

/// `max(seq) - min(last_seq)` over the registered projections; 0 without consumers or rows.
pub(crate) fn lag(conn: &Connection) -> Result<i64> {
    let lag: i64 = conn.query_row(
        "SELECT CASE WHEN (SELECT count(*) FROM projections) = 0 THEN 0
                     ELSE max(0, (SELECT COALESCE(max(seq), 0) FROM changes) - (SELECT min(last_seq) FROM projections))
                END",
        [],
        |r| r.get(0),
    ).sql()?;
    Ok(lag)
}

/// `max(seq) - last_seq` of every registered projection, by name (spec 05 SS-21: `doctor`'s
/// per-consumer retention lag; [`lag`] is its maximum).
pub(crate) fn lag_by_consumer(conn: &Connection) -> Result<BTreeMap<String, u64>> {
    let mut st = conn
        .prepare_cached(
            "SELECT name, max(0, (SELECT COALESCE(max(seq), 0) FROM changes) - last_seq)
             FROM projections",
        )
        .sql()?;
    let rows = st
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                u64::try_from(r.get::<_, i64>(1)?).unwrap_or(0),
            ))
        })
        .sql()?
        .collect::<rusqlite::Result<BTreeMap<_, _>>>()
        .sql()?;
    Ok(rows)
}

/// Delete rows with `seq < floor - tail`, where `floor` is the slowest cursor (or the newest
/// row's `seq` when no projection is registered), never the newest row itself: `changes.seq`
/// has no AUTOINCREMENT (F17), so an emptied table would restart at 1, below every cursor.
pub(crate) fn prune(conn: &Connection, tail: u64) -> Result<u64> {
    let tail = i64::try_from(tail).unwrap_or(i64::MAX);
    let deleted = conn.execute(
        "DELETE FROM changes
         WHERE seq < COALESCE((SELECT min(last_seq) FROM projections), (SELECT max(seq) FROM changes)) - ?1
           AND seq < (SELECT max(seq) FROM changes)",
        [tail],
    ).sql()?;
    Ok(u64::try_from(deleted).unwrap_or(0))
}
