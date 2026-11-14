//! Node lifecycle: forget (`forget_impl`), reactivation
//! (`reactivate_node`) and the purge (`purge_in_place`, `purge_hygiene`, `purge_cleanup`).
//! Split out of `sqlite.rs` (OWN-79).

use crate::anchors;
use crate::rows::bad_enum;
use crate::sql_error::SqlResultExt;
use kaban_core::Result;
use kaban_core::{
    BatchReport, ForgetMode, ForgetOutcome, Lifecycle, MaintenanceOutcome, NodeId, OutcomeKind,
    StoreCapabilities, StoreError, WriteCtx, now_iso,
};
use rusqlite::{Connection, OptionalExtension, params};

use super::{SqliteStore, Tenant};

/// What every purge's post-commit hygiene leaves behind (SS-03): the freed pages and their
/// bytes, until a VACUUM (`kaban doctor --optimize --vacuum`) rebuilds the file.
const FREE_PAGE_RESIDUE: &str = "free-page residue until VACUUM";
/// The index scope of a purge on a retired database with no index attached (SS-12, SS-16):
/// this process reaches no segment, so none is claimed released.
const UNATTACHED_INDEX_SCOPE: &str =
    "no text index attached in this process; its segments are outstanding or unknown";

/// What a canonical purge's post-commit cleanup claims (SS-03, SS-12), as `ForgetOutcome`
/// words it: physically cleaned only when every scope is, and the detail over the scopes.
pub(crate) struct PurgeCleanup {
    pub(crate) physically_cleaned: bool,
    /// Why the purge is not physically cleaned; `None` when it is.
    pub(crate) cleanup: Option<String>,
}

impl SqliteStore {
    /// The purge of one row inside the caller's transaction (core-spec F5, S5; MD-24): the id
    /// row stays as a `purged` ghost so the id never reappears, while text, index entry (the
    /// `node_au` trigger drops it), dedup identity and edges go. `rev` becomes `rev + 1`, or
    /// `rev_floor` when that is higher (a tombstone imported at a higher file rev, M5-8);
    /// `serialized_rev` follows `rev_floor` when one is given (the tombstone is the file). The
    /// node update trigger skips purged rows; the one explicit `op='purge'` row and one
    /// `delete` row per removed edge are the `ChangeLog` record. Returns the row's new `rev`.
    pub(crate) fn purge_in_place(
        tx: &Connection,
        rid: i64,
        id: &str,
        workspace_id: &str,
        now: &str,
        rev_floor: Option<i64>,
    ) -> Result<i64> {
        crate::jobs::revoke_document(tx, id, workspace_id)?;
        tx.execute(
            "UPDATE node SET lifecycle='purged', text='', summary=NULL, tags='[]', props='{}',
                 content_hash='', dup_key=NULL, valid_to=?1, updated_at=?1,
                 rev=max(rev+1, COALESCE(?3, 0)),
                 serialized_rev=CASE WHEN ?3 IS NULL THEN serialized_rev ELSE max(rev+1, ?3) END
             WHERE rid=?2",
            params![now, rid, rev_floor],
        )
        .sql()?;
        tx.execute("DELETE FROM edge WHERE src=?1 OR dst=?1", [rid])
            .sql()?;
        tx.execute(
            "INSERT INTO changes(entity, op, id, workspace_id) VALUES ('node', 'purge', ?, ?)",
            params![id, workspace_id],
        )
        .sql()?;
        // the node's anchors (EN-F10) and the derived artifacts of its source go explicitly
        if crate::edge_write::scoring_columns(tx)? {
            anchors::remove(tx, rid)?;
            tx.execute("DELETE FROM derived_artifact WHERE source_rid = ?1", [rid])
                .sql()?;
        }
        tx.query_row("SELECT rev FROM node WHERE rid=?", [rid], |r| r.get(0))
            .sql()
    }

    /// Purge hygiene after the transaction committed (core-spec S5; SS-03), also the explicit
    /// FTS optimize that retries it: fold the WAL into the main file and drop its bytes
    /// (TRUNCATE), below the FTS5 retirement (SS-16) merge the segments of `node_fts` and,
    /// when enabled, `node_fts_tri` so the deleted document's tokens leave the index (a
    /// retired database has neither: nothing to merge, T6 review t6-4), then truncate again.
    /// Every step runs and its
    /// result is kept: a checkpoint that a reader still on the WAL holds back is `Busy`, a
    /// failed step `Failed`. Never an error: the purge already committed, so its cleanup is a
    /// reported outcome. No step reclaims a freed page, and `secure_delete=FAST` zeroes b-tree
    /// cells only: a freed overflow or segment page keeps its bytes on the free list, so
    /// neither an `Ok` here, a merged index nor an empty result proves erasure (SS-03). A
    /// VACUUM does (`doctor --optimize --vacuum`); [`Self::cleanup_detail`] names the residue.
    pub(crate) fn purge_hygiene(conn: &Connection) -> MaintenanceOutcome {
        // `Ok(false)`: a checkpoint that a reader still on the WAL (or a writer) held back
        let checkpoint = || {
            conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| {
                r.get::<_, i64>(0)
            })
            .map(|busy| busy == 0)
        };
        let mut findings: Vec<String> = Vec::new();
        let mut busy = false;
        let mut keep = |step: rusqlite::Result<bool>| match step.map_err(crate::doctor::outcome_of)
        {
            Ok(true) | Err(Ok(MaintenanceOutcome::Ok)) => {}
            Ok(false) | Err(Ok(MaintenanceOutcome::Busy)) => busy = true,
            Err(Ok(MaintenanceOutcome::Failed(found))) => findings.extend(found),
            Err(Err(e)) => findings.push(e.to_string()),
        };
        keep(checkpoint());
        keep(checkpoint());
        if !findings.is_empty() {
            MaintenanceOutcome::Failed(findings)
        } else if busy {
            MaintenanceOutcome::Busy
        } else {
            MaintenanceOutcome::Ok
        }
    }
    /// The `cleanup` detail of a purge whose hygiene reported `outcome` (SS-03): the hygiene
    /// reclaims no page, so the detail always ends with the free-page residue, after `busy` or
    /// the failure findings when a step was held back or failed. A purge is physically cleaned
    /// only by a step that reclaims the pages, which none of the post-commit paths runs.
    pub(crate) fn cleanup_detail(outcome: MaintenanceOutcome) -> String {
        match outcome {
            MaintenanceOutcome::Ok => FREE_PAGE_RESIDUE.to_owned(),
            MaintenanceOutcome::Busy => format!("busy; {FREE_PAGE_RESIDUE}"),
            MaintenanceOutcome::Failed(findings) => {
                format!("{}; {FREE_PAGE_RESIDUE}", findings.join("; "))
            }
        }
    }
    /// `Op::Reactivate` (DI1.8, RD2-5): the ingestion reconciliation transition. Accepted under
    /// [`GovernedSet::reconciliation_transition`] only (an ingest-owned Document, Section or
    /// Chunk that is `tombstoned`): `active`, `valid_to` cleared, `rev` bumped, a `reactivated`
    /// event (the `ChangeLog` trigger writes the rows). An active node is a no-op counted
    /// `unchanged`; a purged id or another tenant's row is `NotFound` (F5, F17).
    pub(super) fn reactivate_node(
        conn: &Connection,
        tenant: &Tenant<'_>,
        id: &NodeId,
        report: &mut BatchReport,
    ) -> Result<Option<i64>> {
        let st: Option<(i64, String, String, String)> = conn
            .query_row(
                "SELECT rid, kind, lifecycle, props FROM node WHERE id=? AND workspace_id=?",
                params![id.as_str(), tenant.ws],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()
            .sql()?;
        let (rid, kind, lifecycle, props) =
            st.ok_or_else(|| StoreError::NotFound(id.to_string()))?;
        let lifecycle = Lifecycle::parse(&lifecycle)
            .ok_or_else(|| bad_enum(2, &lifecycle))
            .sql()?;
        match lifecycle {
            Lifecycle::Purged => return Err(StoreError::NotFound(id.to_string())),
            Lifecycle::Active => {
                report.record(id, &kind, OutcomeKind::Unchanged);
                return Ok(None);
            }
            // `Lifecycle` is `non_exhaustive`: the table refuses anything it does not know
            _ => {}
        }
        let props: serde_json::Value = serde_json::from_str(&props)?;
        let owner = props.get("owner").and_then(serde_json::Value::as_str);
        tenant
            .governed
            .reconciliation_transition(lifecycle, &kind, owner)?;
        conn.execute(
            "UPDATE node SET lifecycle='active', valid_to=NULL, updated_at=?1, rev=rev+1 WHERE rid=?2",
            params![now_iso(), rid],
        )
        .sql()?;
        report.record(id, &kind, OutcomeKind::Reactivated);
        Ok(Some(rid))
    }
    /// The body of [`kaban_core::MemoryStore::forget`].
    pub(super) fn forget_impl(
        &self,
        id: &NodeId,
        mode: ForgetMode,
        _ctx: &WriteCtx,
    ) -> Result<ForgetOutcome> {
        let id = id.as_str();
        let mut conn = self.write();
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .sql()?;
        let st = Self::state_of(&tx, id)?;
        let rid = st.rid;
        let target = match mode {
            ForgetMode::Tombstone | ForgetMode::Invalidate => Lifecycle::Tombstoned,
            ForgetMode::Archive => Lifecycle::Archived,
            ForgetMode::Purge => Lifecycle::Purged,
        };
        // the spec's lifecycle table decides first (tombstoned and purged are terminal)
        kaban_core::lifecycle_transition(st.lifecycle, target)?;
        if matches!(target, Lifecycle::Tombstoned | Lifecycle::Purged) {
            crate::jobs::revoke_document(&tx, id, &st.workspace_id)?;
        }
        let now = now_iso();
        let scoring = crate::edge_write::scoring_columns(&tx)?;
        match mode {
            // lifecycle only (D1); `invalidate` is the same tombstone since governance went
            ForgetMode::Tombstone | ForgetMode::Invalidate => {
                tx.execute(
                    "UPDATE node SET lifecycle='tombstoned', valid_to=?1, updated_at=?1, rev=rev+1 WHERE rid=?2",
                    params![now, rid],
                ).sql()?;
                if scoring {
                    anchors::remove(&tx, rid)?;
                }
            }
            ForgetMode::Archive => {
                tx.execute(
                    "UPDATE node SET lifecycle='archived', updated_at=?1, rev=rev+1 WHERE rid=?2",
                    params![now, rid],
                )
                .sql()?;
            }
            // the id row stays as a `purged` ghost so the id never reappears (F5): text, index
            // entry (the `node_au` trigger drops it), dedup identity and edges go. The node
            // update trigger skips purged rows; the one explicit `op='purge'` row and one
            // `delete` row per removed edge are the `ChangeLog` record (MD-24).
            ForgetMode::Purge => {
                Self::purge_in_place(&tx, rid, id, &st.workspace_id, &now, None)?;
            }
        }
        let lifecycle: String = tx
            .query_row("SELECT lifecycle FROM node WHERE rid=?", [rid], |r| {
                r.get(0)
            })
            .sql()?;
        let last_seq: i64 = tx
            .query_row("SELECT COALESCE(max(seq), 0) FROM changes", [], |r| {
                r.get(0)
            })
            .sql()?;
        tx.commit().sql()?;
        let outcome = ForgetOutcome {
            lifecycle: Lifecycle::parse(&lifecycle)
                .ok_or_else(|| bad_enum(0, &lifecycle))
                .sql()?,
            outbox_seq: u64::try_from(last_seq).unwrap_or(0),
            revoked: false,
            physically_cleaned: false,
            cleanup: None,
        };
        if !matches!(mode, ForgetMode::Purge) {
            return Ok(outcome);
        }
        // committed: whatever the cleanup reports, the content is revoked (SS-03). The write's
        // guard goes first, so the purge is projected (the row leaves the index) before the
        // erasure merges it away and the checkpoint truncates the WAL (SS-12, OWN-M)
        drop(conn);
        let cleanup = self.purge_cleanup(rid);
        Ok(ForgetOutcome {
            revoked: true,
            physically_cleaned: cleanup.physically_cleaned,
            cleanup: cleanup.cleanup,
            ..outcome
        })
    }
    /// The post-commit cleanup every canonical purge runs once its transaction committed
    /// (`forget(Purge)` and an imported purge share it): the database hygiene (SS-03), then,
    /// with an index of its own storage attached, its erasure under the writer lock (SS-12),
    /// the scopes reported apart. A retired database (SS-16) keeps its index outside the
    /// database; with nothing attached this process cannot reach it, so its scope is reported
    /// outstanding or unknown, never omitted and never claimed (T6 review t6-4). Never an
    /// error and never a rejection: the content is revoked whatever the cleanup reports.
    /// The database side runs under the connection lock, released before the adapter is
    /// called (OWN-M constraint 1; review t8a-5): the erasure's merge and collection never
    /// run under the store's lock.
    pub(crate) fn purge_cleanup(&self, rid: i64) -> PurgeCleanup {
        let report = {
            let conn = self.lock();
            Self::hygiene_report(Self::CAPABILITIES, Self::purge_hygiene(&conn))
        };
        let index = match self.seed_index() {
            Some(index) => index.purge(&[rid]),
            None => Ok(kaban_core::text_index::PurgeOutcome {
                cleaned: false,
                detail: Some(UNATTACHED_INDEX_SCOPE.to_owned()),
            }),
        };
        Self::scoped_purge_report(report, index)
    }
    /// The purge report over its scopes (SS-12): the database and its WAL (`report`'s own
    /// detail), the index artefacts (`index`'s outcome; a failure is not cleaned, with its
    /// class) and the temporary outputs (none are held by a purge). Physically cleaned only
    /// when every scope is.
    fn scoped_purge_report(
        report: PurgeCleanup,
        index: Result<kaban_core::text_index::PurgeOutcome>,
    ) -> PurgeCleanup {
        let (index_cleaned, index_detail) = match index {
            Ok(outcome) => (
                outcome.cleaned,
                outcome.detail.unwrap_or_else(|| "cleaned".to_owned()),
            ),
            Err(e) => (false, format!("erasure failed: {e}")),
        };
        let database = report.cleanup.unwrap_or_else(|| "cleaned".to_owned());
        PurgeCleanup {
            physically_cleaned: report.physically_cleaned && index_cleaned,
            cleanup: Some(format!(
                "database/WAL: {database}; index: {index_detail}; temporary outputs: none held"
            )),
        }
    }
    /// What the hygiene step claims (SS-03), under `capabilities` (SS-23's first consumer):
    /// the hygiene reclaims no page, so the step claims nothing and is never physically
    /// cleaned here, with [`Self::cleanup_detail`] as the residue; a const without physical
    /// cleanup says so before the residue.
    pub(super) fn hygiene_report(
        capabilities: StoreCapabilities,
        hygiene: MaintenanceOutcome,
    ) -> PurgeCleanup {
        let (physically_cleaned, cleanup) =
            capabilities.physical_cleanup_report(false, Self::cleanup_detail(hygiene));
        PurgeCleanup {
            physically_cleaned,
            cleanup,
        }
    }
}
