//! The append-only `events` table (core-spec §4, F17): one row per governance transition,
//! written inside the transaction that moves the record. The table's triggers refuse UPDATE
//! and DELETE; the `ChangeLog` trigger emits one `event insert` row per append (F11).

use crate::sql_error::SqlResultExt;
use kaban_core::{Event, NewEvent, Result, SignerKind};
use rusqlite::{Connection, params};

const EVENT_COLS: &str = "seq,node_id,kind,reason,path_scope,signer,signer_kind,host,session_id,ts,commit_sha,workspace_id";

/// Append one event of `kind` for `node_id` (which lives in `workspace_id`); returns its `seq`.
pub(crate) fn append(
    conn: &Connection,
    node_id: &str,
    workspace_id: &str,
    kind: &str,
    ev: &NewEvent,
) -> Result<i64> {
    let path_scope = serde_json::to_string(&ev.path_scope)?;
    conn.execute(
        "INSERT INTO events(node_id,kind,reason,path_scope,signer,signer_kind,host,session_id,ts,commit_sha,workspace_id)
         VALUES (?,?,?,?,?,?,?,?,unixepoch(),?,?)",
        params![
            node_id,
            kind,
            ev.reason,
            path_scope,
            ev.signer,
            ev.signer_kind.as_str(),
            ev.host,
            ev.session_id,
            ev.commit_sha,
            workspace_id
        ],
    ).sql()?;
    Ok(conn.last_insert_rowid())
}

/// Every event of `node_id`, oldest first.
pub(crate) fn list(conn: &Connection, node_id: &str) -> Result<Vec<Event>> {
    let mut st = conn
        .prepare_cached(&format!(
            "SELECT {EVENT_COLS} FROM events WHERE node_id = ? ORDER BY seq"
        ))
        .sql()?;
    let rows = st
        .query_map([node_id], row_event)
        .sql()?
        .collect::<rusqlite::Result<Vec<_>>>()
        .sql()?;
    Ok(rows)
}

fn row_event(r: &rusqlite::Row<'_>) -> rusqlite::Result<Event> {
    let path_scope: String = r.get(4)?;
    let signer_kind: String = r.get(6)?;
    Ok(Event {
        seq: r.get(0)?,
        node_id: r.get(1)?,
        kind: r.get(2)?,
        reason: r.get(3)?,
        path_scope: serde_json::from_str(&path_scope).unwrap_or_default(),
        signer: r.get(5)?,
        signer_kind: SignerKind::parse(&signer_kind).ok_or_else(|| {
            rusqlite::Error::FromSqlConversionFailure(
                6,
                rusqlite::types::Type::Text,
                format!("unexpected signer_kind '{signer_kind}'").into(),
            )
        })?,
        host: r.get(7)?,
        session_id: r.get(8)?,
        ts: r.get(9)?,
        commit_sha: r.get(10)?,
        workspace_id: r.get(11)?,
    })
}
