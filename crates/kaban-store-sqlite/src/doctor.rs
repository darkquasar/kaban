//! The maintenance checks behind `kaban doctor` (core-spec §7; core-plan C4.2 as amended by
//! M4-19): the engine's structural check and `VACUUM`.
//!
//! A busy database and a corrupt file or index are [`MaintenanceOutcome`]s the report names;
//! every other driver failure is a [`StoreError::Backend`]. No engine message reaches a
//! renderer except the `PRAGMA integrity_check` findings, which name tables, indexes and row
//! numbers and never stored content (and which the adapter still gates before printing).

use crate::sql_error::backend_error;
use kaban_core::{MaintenanceOutcome, Result};
use rusqlite::{Connection, ErrorCode};

/// The fixed finding reported when the engine refuses a file or index as corrupt instead of
/// listing findings (`SQLITE_CORRUPT`, `SQLITE_CORRUPT_VTAB`, `SQLITE_NOTADB`).
const CORRUPT: &str = "the engine reports the file or index as corrupt";

/// Most findings kept from `PRAGMA integrity_check` (the engine stops at 100 itself).
const MAX_FINDINGS: usize = 20;

/// Map a driver failure of a maintenance statement: busy and corrupt are outcomes, the rest
/// stays a backend error.
pub(crate) fn outcome_of(e: rusqlite::Error) -> Result<MaintenanceOutcome> {
    match e.sqlite_error_code() {
        Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked) => Ok(MaintenanceOutcome::Busy),
        Some(ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase) => {
            Ok(MaintenanceOutcome::Failed(vec![CORRUPT.to_owned()]))
        }
        _ => Err(backend_error(e)),
    }
}

/// `PRAGMA integrity_check`: `Ok` when the single row is `ok`, otherwise the findings.
pub(crate) fn integrity_check(conn: &Connection) -> Result<MaintenanceOutcome> {
    let rows = conn.prepare("PRAGMA integrity_check").and_then(|mut st| {
        st.query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<String>>>()
    });
    match rows {
        Ok(rows) if rows.len() == 1 && rows[0] == "ok" => Ok(MaintenanceOutcome::Ok),
        Ok(mut rows) => {
            rows.truncate(MAX_FINDINGS);
            Ok(MaintenanceOutcome::Failed(rows))
        }
        Err(e) => outcome_of(e),
    }
}

/// `VACUUM` (autocommit: it cannot run inside a transaction). `Busy` when the lock is held.
pub(crate) fn vacuum(conn: &Connection) -> Result<MaintenanceOutcome> {
    match conn.execute_batch("VACUUM") {
        Ok(()) => Ok(MaintenanceOutcome::Ok),
        Err(e) => outcome_of(e),
    }
}

/// Explicit FTS5 optimization, never part of a canonical write or diagnostic read: the purge
/// hygiene, so it also completes a purge's cleanup that a reader or a lock held back (SS-03).
#[allow(clippy::unnecessary_wraps)] // the `IngestStore::optimize_fts` shape
pub(crate) fn optimize_fts(conn: &Connection) -> Result<MaintenanceOutcome> {
    Ok(crate::SqliteStore::purge_hygiene(conn))
}

/// Offline counts only; malformed source encodings are stale rather than decoded as truth.
pub(crate) fn artifact_diagnostics(
    conn: &Connection,
    workspace: &kaban_core::WorkspaceId,
    authority: kaban_core::CurrentProducerAuthority,
) -> Result<kaban_core::ArtifactDiagnostics> {
    use crate::sql_error::SqlResultExt;
    use rusqlite::params;
    let stale_artifacts=conn.query_row("SELECT count(*) FROM derived_artifact d LEFT JOIN node n ON n.rid=d.source_rid WHERE d.workspace_id=?1 AND (n.rid IS NULL OR n.workspace_id<>d.workspace_id OR n.lifecycle<>'active' OR d.policy_version<>n.policy_version OR CASE WHEN json_valid(d.source_rev) THEN json_extract(d.source_rev,'$.content_hash') IS NOT n.content_hash OR json_extract(d.source_rev,'$.rev') IS NOT n.rev ELSE 1 END)",[workspace.as_str()],|row|row.get::<_,i64>(0)).sql()?;
    let retired_producers=conn.query_row("SELECT count(*) FROM derived_artifact WHERE workspace_id=?1 AND layer='gazetteer' AND producer_id<>?2",params![workspace.as_str(),authority.producer().producer_id()],|row|row.get::<_,i64>(0)).sql()?;
    let cursors_behind=conn.query_row("SELECT count(*) FROM projections WHERE name LIKE 'reconcile:%' AND last_seq<(SELECT COALESCE(max(seq),0) FROM changes)",[],|row|row.get::<_,i64>(0)).sql()?;
    Ok(kaban_core::ArtifactDiagnostics {
        stale_artifacts: u64::try_from(stale_artifacts)
            .map_err(|_| backend_error(rusqlite::Error::InvalidQuery))?,
        retired_producers: u64::try_from(retired_producers)
            .map_err(|_| backend_error(rusqlite::Error::InvalidQuery))?,
        cursors_behind: u64::try_from(cursors_behind)
            .map_err(|_| backend_error(rusqlite::Error::InvalidQuery))?,
    })
}
