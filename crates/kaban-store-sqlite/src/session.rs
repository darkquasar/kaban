//! The hook session scratch `session_edits` (migration 0003), the briefing query, the
//! pending-failure lookup and the Failure props patch (core-plan C4.3/C4.4 as amended by
//! `reviews/m4-prep-dispositions.md` M4-13 to M4-17). Plain SQL helpers over a connection or a
//! transaction; `SqliteStore`'s `GovernanceStore` impl delegates here. The briefing's kinds are
//! the caller's (EN-F4, engine-plan E1.6: the stored names the registry hands over, bound as
//! placeholders); the pending-failure lookup and the props patch are about the hooks' adapter
//! kind [`FAILURE_KIND`].

use crate::rows::{NODE_COLS, decode_json_value, row_node};
use crate::sql_error::{SqlResultExt, backend_error};
use kaban_core::{
    FailurePatch, Node, PendingFailuresQuery, PendingScope, Result, SessionEdit, SessionEditKind,
    StoreError,
};
use rusqlite::types::ToSql;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;

/// The coding-agent adapter kind of a failed command (declared in the shipped `adr.yaml`,
/// engine-spec EN-F4 "retained as the adapter kinds"): the kind the pending-failure lookup and
/// the props patch are about. Its props (`pending_fix`, `evidence_runs`, `FIXED_BY`) are this
/// kind's contract, not a mixin field, so the name stays a constant until the ontology declares
/// adapter roles.
pub(crate) const FAILURE_KIND: &str = "Failure";

/// Insert one row.
pub(crate) fn record(conn: &Connection, edit: &SessionEdit) -> Result<()> {
    conn.execute(
        "INSERT INTO session_edits(session_id, kind, path, ts) VALUES (?, ?, ?, ?)",
        params![edit.session_id, edit.kind.as_str(), edit.path, edit.ts],
    )
    .sql()?;
    Ok(())
}

/// The `kind` rows of a session from `after_ts` on, oldest first, at most `limit`.
pub(crate) fn list(
    conn: &Connection,
    session_id: &str,
    kind: SessionEditKind,
    after_ts: i64,
    limit: usize,
) -> Result<Vec<SessionEdit>> {
    let mut st = conn
        .prepare_cached(
            "SELECT session_id, kind, path, ts FROM session_edits
             WHERE session_id = ? AND kind = ? AND ts >= ? ORDER BY ts, rowid LIMIT ?",
        )
        .sql()?;
    let rows = st
        .query_map(
            params![
                session_id,
                kind.as_str(),
                after_ts,
                i64::try_from(limit).unwrap_or(i64::MAX)
            ],
            |r| {
                let kind: String = r.get(1)?;
                Ok(SessionEdit {
                    session_id: r.get(0)?,
                    kind: SessionEditKind::parse(&kind).ok_or_else(|| {
                        rusqlite::Error::FromSqlConversionFailure(
                            1,
                            rusqlite::types::Type::Text,
                            format!("unexpected session_edits.kind '{kind}'").into(),
                        )
                    })?,
                    path: r.get(2)?,
                    ts: r.get(3)?,
                })
            },
        )
        .sql()?
        .collect::<rusqlite::Result<Vec<_>>>()
        .sql()?;
    Ok(rows)
}

/// Delete at most `limit` rows older than `older_than_ts`; returns how many.
pub(crate) fn prune(conn: &Connection, older_than_ts: i64, limit: usize) -> Result<u64> {
    let n = conn
        .execute(
            "DELETE FROM session_edits WHERE rowid IN
               (SELECT rowid FROM session_edits WHERE ts < ? ORDER BY ts LIMIT ?)",
            params![older_than_ts, i64::try_from(limit).unwrap_or(i64::MAX)],
        )
        .sql()?;
    Ok(u64::try_from(n).unwrap_or(0))
}

/// Active, accepted records of the kinds in `kinds` (the caller's stored names from the loaded
/// ontologies, engine-plan E1.6; the shipped `adr.yaml`: Decisions and Conventions) of a
/// workspace by importance, newest first, then id (N5), at most `limit` (M4-17). The names
/// are bound as `?` placeholders, one each, never interpolated.
pub(crate) fn briefing(
    conn: &Connection,
    kinds: &[&str],
    workspace_id: &str,
    limit: usize,
) -> Result<Vec<Node>> {
    if limit == 0 || kinds.is_empty() {
        return Ok(Vec::new());
    }
    let placeholders = vec!["?"; kinds.len()].join(", ");
    let mut st = conn
        .prepare_cached(&format!(
            "SELECT {NODE_COLS} FROM node
             WHERE lifecycle = 'active' AND governance = 'accepted'
               AND kind IN ({placeholders}) AND workspace_id = ?
             ORDER BY COALESCE(importance, 0) DESC, created_at DESC, id LIMIT ?"
        ))
        .sql()?;
    let limit = i64::try_from(limit).unwrap_or(i64::MAX);
    let mut args: Vec<&dyn ToSql> = kinds.iter().map(|k| k as &dyn ToSql).collect();
    args.push(&workspace_id);
    args.push(&limit);
    let rows = st
        .query_map(args.as_slice(), row_node)
        .sql()?
        .collect::<rusqlite::Result<Vec<_>>>()
        .sql()?;
    Ok(rows)
}

/// Pending Failures (M4-14): active Failures of the workspace with no outgoing `FIXED_BY` edge
/// and `props.pending_fix` not `false`. `Session` scope keeps those named by a `failure` row of
/// the session; `Workspace` scope returns every pending Failure, the session's first. Then
/// newest first, then id.
pub(crate) fn pending_failures(conn: &Connection, q: &PendingFailuresQuery) -> Result<Vec<Node>> {
    if q.limit == 0 {
        return Ok(Vec::new());
    }
    let in_session = "EXISTS (SELECT 1 FROM session_edits se
                              WHERE se.session_id = ?2 AND se.kind = 'failure' AND se.path = n.id)";
    let scope = match q.scope {
        PendingScope::Session => format!("AND {in_session}"),
        PendingScope::Workspace => String::new(),
    };
    let sql = format!(
        "SELECT {} FROM node n
         WHERE n.lifecycle = 'active' AND n.kind = ?4 AND n.workspace_id = ?1
           AND COALESCE(json_extract(n.props, '$.pending_fix'), 1) <> 0
           AND NOT EXISTS (SELECT 1 FROM edge e WHERE e.src = n.rid AND e.rel = 'FIXED_BY')
           {scope}
         ORDER BY CASE WHEN {in_session} THEN 0 ELSE 1 END, n.created_at DESC, n.id LIMIT ?3",
        NODE_COLS
            .split(',')
            .map(|c| format!("n.{c}"))
            .collect::<Vec<_>>()
            .join(",")
    );
    let mut st = conn.prepare_cached(&sql).sql()?;
    let rows = st
        .query_map(
            params![
                q.workspace_id,
                q.session_id,
                i64::try_from(q.limit).unwrap_or(i64::MAX),
                FAILURE_KIND
            ],
            row_node,
        )
        .sql()?
        .collect::<rusqlite::Result<Vec<_>>>()
        .sql()?;
    Ok(rows)
}

/// Apply `patch` to the props of the active Failure `id` of `workspace_id` inside the caller's
/// transaction (M4-15): the row's `props` JSON is read, the closed set of fields is changed,
/// `rev` is bumped and `updated_at` set; the node triggers write the changes row. Returns the
/// updated node.
pub(crate) fn update_failure_props(
    tx: &Connection,
    id: &str,
    workspace_id: &str,
    patch: &FailurePatch,
    now: &str,
) -> Result<Node> {
    let row: Option<(i64, String, String)> = tx
        .query_row(
            "SELECT rid, kind, props FROM node
             WHERE id = ? AND workspace_id = ? AND lifecycle = 'active'",
            params![id, workspace_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()
        .sql()?;
    let (rid, kind, props) = row.ok_or_else(|| StoreError::NotFound(id.to_owned()))?;
    if kind != FAILURE_KIND {
        return Err(StoreError::KindMismatch {
            id: id.to_owned(),
            stored: kind,
            requested: FAILURE_KIND.into(),
        });
    }
    // a props column that does not hold a JSON object is a corrupted row (RB-5): refuse to
    // guess; the message names the column, never the stored bytes
    let mut props = decode_json_value(2, &props).sql()?;
    let Value::Object(map) = &mut props else {
        return Err(backend_error(rusqlite::Error::FromSqlConversionFailure(
            2,
            rusqlite::types::Type::Text,
            "node.props is not a JSON object".into(),
        )));
    };
    if let Some(fix) = &patch.fix {
        map.insert("fix".into(), Value::String(fix.clone()));
    }
    if let Some(pending) = patch.pending_fix {
        map.insert("pending_fix".into(), Value::Bool(pending));
    }
    if let Some(run) = &patch.evidence_run {
        let runs = map
            .entry("evidence_runs")
            .or_insert_with(|| Value::Array(Vec::new()));
        match runs {
            Value::Array(items) => items.push(serde_json::to_value(run)?),
            other => *other = Value::Array(vec![serde_json::to_value(run)?]),
        }
    }
    let props = serde_json::to_string(&props)?;
    tx.execute(
        "UPDATE node SET props = ?1, updated_at = ?2, rev = rev + 1 WHERE rid = ?3",
        params![props, now, rid],
    )
    .sql()?;
    let node = tx
        .query_row(
            &format!("SELECT {NODE_COLS} FROM node WHERE rid = ?"),
            [rid],
            row_node,
        )
        .sql()?;
    Ok(node)
}
