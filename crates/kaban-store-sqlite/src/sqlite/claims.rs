//! Claim and node lifecycle: policy rescans (`rescan_impl`, `rescan_assertions_impl` and
//! `ASSERTION_POLICY_HASH`), deprecation and replay (`deprecate_assertion_impl`,
//! `redecide_assertions`), retirement and restoration (`forget_impl`, `hide_engine_rows`,
//! `reactivate_node`) and the purge (`purge_in_place`, `purge_hygiene`). Includes the
//! node-policy maintenance of `rescan`, not only assertion rows. Split out of `sqlite.rs`
//! (OWN-79).

use crate::anchors;
use crate::audit_table;
use crate::candidates;
use crate::events;
use crate::reconcile::{self, persist};
use crate::rows::{NODE_COLS, bad_enum, row_node_at};
use crate::sql_error::SqlResultExt;
use kaban_core::Result;
use kaban_core::{
    AuditEntry, BatchReport, ForgetMode, ForgetOutcome, Governance, IndexMeta, Lifecycle,
    MaintenanceOutcome, NewEvent, Node, NodeId, OutcomeKind, RescanAction, RescanReport,
    StoreCapabilities, StoreError, WriteCtx, now_iso,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;
use std::collections::BTreeSet;

use super::nodes::redacted_content_hash;
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

/// `index_meta` key of the policy version every stored `assertion.value` was last judged
/// under (EN-S9; `IngestStore::rescan_assertions`), the claim-side twin of
/// `IndexMeta::POLICY_HASH`.
const ASSERTION_POLICY_HASH: &str = "assertion_policy_hash";

impl SqliteStore {
    /// Re-decide the conflict groups of these claim rows (`assertion.id`) in one transaction
    /// (engine-spec EN-F8a, the replay seam; engine-plan E2.2 item 7): a claim whose
    /// `assertion_status` row is missing gets it back as `pending` under its recomputed group
    /// key first, every touched group is then decided over all of its stored supporters and
    /// projected as `apply_batch` does (edges re-materialised or removed), and the verdicts
    /// are counted. A row id that names no claim (a purged one) is skipped. Deterministic and
    /// idempotent: a second call over the same ids changes no row and writes no `ChangeLog`
    /// row. The `ReplaySink` of the test suite rebuilds the status table through this entry;
    /// `SinkId::RECONCILE` stays reserved for the deferred asynchronous loop (OWN-68 SC-12).
    ///
    /// # Errors
    /// Backend errors; the transaction is rolled back.
    pub fn redecide_assertions(&self, ids: &[i64]) -> Result<BatchReport> {
        let mut conn = self.write();
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .sql()?;
        let now = persist::now_unix();
        let mut groups: BTreeSet<String> = BTreeSet::new();
        for id in ids {
            let Some(claim) = persist::load_claim(&tx, *id)? else {
                continue;
            };
            let key = kaban_core::group_key(&claim);
            reconcile::recover_group(&tx, &self.reconcile, &claim, now)?;
            groups.insert(key);
        }
        let mut report = BatchReport::default();
        reconcile::decide_groups(&tx, &self.reconcile, &groups, now, Some(&mut report))?;
        tx.commit().sql()?;
        Ok(report)
    }

    /// The purge of one row inside the caller's transaction (core-spec F5, S5; MD-24): the id
    /// row stays as a `purged` ghost so the id never reappears, while text, index entry (the
    /// `node_au` trigger drops it), dedup identity and edges go. `rev` becomes `rev + 1`, or
    /// `rev_floor` when that is higher (a tombstone imported at a higher file rev, M5-8);
    /// `serialized_rev` follows `rev_floor` when one is given (the tombstone is the file). The
    /// node update trigger skips purged rows; the one explicit `op='purge'` row and one
    /// `delete` row per removed edge are the `ChangeLog` record. Returns the row's new `rev`.
    pub(crate) fn purge_in_place(
        &self,
        tx: &Connection,
        rid: i64,
        id: &str,
        workspace_id: &str,
        now: &str,
        rev_floor: Option<i64>,
    ) -> Result<i64> {
        crate::jobs::revoke_document(tx, id, workspace_id)?;
        crate::pruning_protection::purge(tx, workspace_id, rid, id)?;
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
        // EN-F8a: the engine rows a cascade of the node row would take (claims naming the
        // node, their statuses, review, identity and derived-artifact rows) and the node's
        // anchors (EN-F10) go explicitly, then every group that lost a member is re-decided
        // in this transaction
        if reconcile::engine_tables(tx)? {
            let groups: BTreeSet<String> = persist::purge_claims_of_node(tx, rid)?
                .into_iter()
                .collect();
            anchors::remove(tx, rid)?;
            reconcile::decide_groups(tx, &self.reconcile, &groups, persist::now_unix(), None)?;
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
        ctx: &WriteCtx,
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
        let ev = NewEvent {
            reason: "source reconciliation".into(),
            signer_kind: ctx.actor.clone().into(),
            ..Default::default()
        };
        events::append(conn, id.as_str(), tenant.ws, "reactivated", &ev)?;
        report.record(id, &kind, OutcomeKind::Reactivated);
        Ok(Some(rid))
    }
    /// A tombstone deletes no claim (EN-F8a): every status of the node's claims becomes
    /// `rejected` (one `status` `ChangeLog` row each), the claims are retired (`forgotten`,
    /// RE4-1) and the node's anchor rows go (EN-F10: a hidden pointer never joins a
    /// snapshot). Returns the groups that lost those claims and the claims' row ids: the
    /// caller re-decides the groups without them in the same transaction.
    pub(super) fn hide_engine_rows(
        conn: &Connection,
        rid: i64,
    ) -> Result<(BTreeSet<String>, BTreeSet<i64>)> {
        let groups = persist::groups_of_node(conn, rid)?.into_iter().collect();
        let ids = persist::claim_ids_of_node(conn, rid)?.into_iter().collect();
        persist::reject_claims_of_node(conn, rid, persist::now_unix())?;
        anchors::remove(conn, rid)?;
        Ok((groups, ids))
    }
    /// The body of [`kaban_core::MemoryStore::forget`].
    pub(super) fn forget_impl(
        &self,
        id: &NodeId,
        mode: ForgetMode,
        ctx: &WriteCtx,
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
        audit_table::append(&tx, self.signer.as_ref(), &ctx.audit)?;
        let now = now_iso();
        let engine = reconcile::engine_tables(&tx)?;
        // the groups a tombstone's retired claims leave are re-decided without them (RE4-1)
        let hide = |tx: &Connection| -> Result<()> {
            let (groups, ids) = Self::hide_engine_rows(tx, rid)?;
            reconcile::decide_groups_without(
                tx,
                &self.reconcile,
                &groups,
                &ids,
                persist::now_unix(),
                None,
            )
        };
        match mode {
            // lifecycle only: governance untouched, no event (D1)
            ForgetMode::Tombstone => {
                tx.execute(
                    "UPDATE node SET lifecycle='tombstoned', valid_to=?1, updated_at=?1, rev=rev+1 WHERE rid=?2",
                    params![now, rid],
                ).sql()?;
                if engine {
                    hide(&tx)?;
                }
            }
            // tombstone + governance invalidated (governed kinds, strict table) + `invalidated`
            // event; non-governed kinds get the tombstone and the event too (OD-4)
            ForgetMode::Invalidate => {
                let ev = NewEvent {
                    reason: "forget:invalidate".into(),
                    signer_kind: ctx.actor.clone().into(),
                    ..Default::default()
                };
                let governed = ctx.governed.contains(&st.kind);
                if governed {
                    ctx.governed.governance_transition(
                        &st.kind,
                        st.governance,
                        Governance::Invalidated,
                    )?;
                }
                tx.execute(
                    "UPDATE node SET lifecycle='tombstoned',
                         governance = CASE WHEN ?1 THEN 'invalidated' ELSE governance END,
                         valid_to=?2, updated_at=?2, rev=rev+1
                     WHERE rid=?3",
                    params![governed, now, rid],
                )
                .sql()?;
                events::append(&tx, id, &st.workspace_id, "invalidated", &ev)?;
                if engine {
                    hide(&tx)?;
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
                self.purge_in_place(&tx, rid, id, &st.workspace_id, &now, None)?;
            }
        }
        // a retirement moves the kind's counters (EN-F6; E1.9a RE3-7)
        candidates::recount_stats(&tx, &st.workspace_id, &st.kind)?;
        let (lifecycle, governance): (String, String) = tx
            .query_row(
                "SELECT lifecycle, governance FROM node WHERE rid=?",
                [rid],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
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
            governance: Governance::parse(&governance)
                .ok_or_else(|| bad_enum(1, &governance))
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
    /// The body of [`kaban_core::IngestStore::rescan`].
    pub(super) fn rescan_impl(
        &self,
        policy_version: &str,
        batch: usize,
        f: &mut dyn FnMut(&Node) -> RescanAction,
    ) -> Result<RescanReport> {
        let mut conn = self.write();
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .sql()?;
        // the batch is read under the write lock, so a node judged here cannot be rewritten by
        // another writer before its stamp lands; purged ghosts carry no content to judge
        let nodes: Vec<(i64, Node)> = {
            let mut st = tx.prepare_cached(&format!(
                "SELECT rid, {NODE_COLS} FROM node WHERE policy_version <> ?1 AND lifecycle <> 'purged' ORDER BY rid LIMIT ?2"
            )).sql()?;
            let limit = i64::try_from(batch).unwrap_or(i64::MAX);
            let rows = st
                .query_map(params![policy_version, limit], |r| {
                    Ok((r.get(0)?, row_node_at(r, 1)?))
                })
                .sql()?;
            rows.collect::<rusqlite::Result<_>>().sql()?
        };
        let mut report = RescanReport::default();
        if nodes.is_empty() {
            // nothing stale under the write lock: the database-wide marker (MD-19) is recorded
            // in this same transaction (R2-3), so no stale row can slip between the check and
            // the marker; an equal marker is left untouched (no write, no changes row)
            tx.execute(
                "INSERT INTO index_meta(key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value WHERE value <> excluded.value",
                params![IndexMeta::POLICY_HASH, policy_version],
            ).sql()?;
        }
        let now = now_iso();
        for (rid, node) in &nodes {
            report.scanned += 1;
            match f(node) {
                RescanAction::Keep => {
                    tx.execute(
                        "UPDATE node SET policy_version=?1 WHERE rid=?2",
                        params![policy_version, rid],
                    )
                    .sql()?;
                    report.kept += 1;
                }
                RescanAction::Redact { strings, audit } => {
                    // every rewritable string column is replaced (R2-2) and the content
                    // identity with it: the pre-redaction hash would keep a guess oracle on
                    // the masked secret, and a hash of the masked text alone would let two
                    // notes masked alike collide on `node_hash_active`, so the new hash is
                    // domain-tagged and mixes in the node id
                    let hash = redacted_content_hash(&node.id, &strings);
                    let bodies = crate::body_codec::Writer::for_connection(&tx)?;
                    tx.execute(
                        "UPDATE node SET text=?1, summary=?2, tags=?3, props=?4, path_scope=?5, session_id=?6, agent_id=?7, repo=?8, commit_sha=?9, accepted_commit=?10, dup_key=?11, content_hash=?12, policy_version=?13, updated_at=?14, rev=rev+1 WHERE rid=?15",
                        params![
                            bodies.stored(&strings.text),
                            strings.summary,
                            serde_json::to_string(&strings.tags)?,
                            serde_json::to_string(&strings.props)?,
                            serde_json::to_string(&strings.path_scope)?,
                            strings.session_id,
                            strings.agent_id,
                            strings.repo,
                            strings.commit_sha,
                            strings.accepted_commit,
                            strings.dup_key,
                            hash,
                            policy_version,
                            now,
                            rid
                        ],
                    ).sql()?;
                    // EN-F10 (E2.7b): the rewritten `path_scope` (and a `CodeRef`'s masked
                    // `props`) move the record's anchor keys in this transaction, as every
                    // other node write's do, or the `governing` prefilter keeps a stale key
                    crate::anchors::rekey(&tx, *rid)?;
                    audit_table::append(&tx, self.signer.as_ref(), &audit)?;
                    report.redacted += 1;
                }
                RescanAction::Reject { audit } => {
                    crate::jobs::revoke_document(&tx, node.id.as_str(), &node.workspace_id)?;
                    // the `ForgetMode::Tombstone` path (lifecycle only, governance untouched,
                    // edges kept) plus the event the plan asks for; a node that is already
                    // tombstoned is stamped only
                    if node.lifecycle == Lifecycle::Tombstoned {
                        tx.execute(
                            "UPDATE node SET policy_version=?1 WHERE rid=?2",
                            params![policy_version, rid],
                        )
                        .sql()?;
                    } else {
                        kaban_core::lifecycle_transition(node.lifecycle, Lifecycle::Tombstoned)?;
                        tx.execute(
                            "UPDATE node SET lifecycle='tombstoned', valid_to=?1, updated_at=?1, rev=rev+1, policy_version=?2 WHERE rid=?3",
                            params![now, policy_version, rid],
                        ).sql()?;
                        let ev = NewEvent {
                            reason: "policy-reject".into(),
                            signer_kind: kaban_core::SignerKind::System,
                            ..Default::default()
                        };
                        events::append(&tx, &node.id, &node.workspace_id, "tombstoned", &ev)?;
                        candidates::recount_stats(&tx, &node.workspace_id, &node.kind)?;
                    }
                    audit_table::append(&tx, self.signer.as_ref(), &audit)?;
                    report.rejected += 1;
                }
            }
            super::artifacts::invalidate_stale(&tx, *rid)?;
        }
        tx.commit().sql()?;
        Ok(report)
    }
    /// The body of [`kaban_core::IngestStore::rescan_assertions`].
    pub(super) fn rescan_assertions_impl(
        &self,
        policy_version: &str,
        f: &mut dyn FnMut(&kaban_core::Assertion, &mut Vec<AuditEntry>) -> Option<Value>,
    ) -> Result<RescanReport> {
        let mut conn = self.write();
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .sql()?;
        let mut report = RescanReport::default();
        if !reconcile::engine_tables(&tx)? {
            return Ok(report);
        }
        // claim rows carry no per-row stamp, so the pass is keyed by two DB-scoped rows read
        // under the write lock (RE4-3): the policy version the last pass ran under and the
        // high-water `assertion.id` it reached. An equal version judges the rows above the
        // high-water mark only (a claim another process inserted since: under `BEGIN
        // IMMEDIATE` an insert and a pass serialise, so its id is above the mark); none
        // above it means nothing to judge and no write. Another version judges every row.
        let meta = |key: &str| -> Result<Option<String>> {
            tx.query_row("SELECT value FROM index_meta WHERE key = ?1", [key], |r| {
                r.get(0)
            })
            .optional()
            .sql()
        };
        let fresh = meta(ASSERTION_POLICY_HASH)?.as_deref() == Some(policy_version);
        let high_water: i64 = meta(persist::ASSERTION_POLICY_HW)?
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let max_id: i64 = tx
            .query_row("SELECT COALESCE(max(id), 0) FROM assertion", [], |r| {
                r.get(0)
            })
            .sql()?;
        if fresh && max_id <= high_water {
            return Ok(report);
        }
        let floor = if fresh { high_water } else { 0 };
        let ids: Vec<i64> = {
            let mut st = tx
                .prepare_cached(
                    "SELECT id FROM assertion WHERE value IS NOT NULL AND id > ?1 ORDER BY id",
                )
                .sql()?;
            let rows = st.query_map([floor], |r| r.get(0)).sql()?;
            rows.collect::<rusqlite::Result<_>>().sql()?
        };
        let now = persist::now_unix();
        let mut audit: Vec<AuditEntry> = Vec::new();
        let mut groups: BTreeSet<String> = BTreeSet::new();
        let mut masked: BTreeSet<i64> = BTreeSet::new();
        for id in ids {
            let Some(claim) = persist::load_claim(&tx, id)? else {
                continue;
            };
            if claim.value.as_value().is_null() {
                continue;
            }
            report.scanned += 1;
            let pushed = audit.len();
            match f(&claim, &mut audit) {
                None => report.kept += 1,
                Some(value) => {
                    // the value UPDATE the immutable claim table sees (EN-S9 over RE-9): the
                    // denied string must leave storage; every other column is kept. The row
                    // is retired with it (RE4-1): `deprecated` under `policy:<rule ids>` (the
                    // rules of the entries the judge pushed for this claim, else the policy
                    // version), so no later decide or replay lets it vote again
                    let value = kaban_core::CanonicalJson::new(value);
                    let rules: BTreeSet<&str> = audit[pushed..]
                        .iter()
                        .flat_map(|e| e.rules.iter().map(String::as_str))
                        .collect();
                    let reason = if rules.is_empty() {
                        format!("{}{policy_version}", persist::POLICY_REASON_PREFIX)
                    } else {
                        format!(
                            "{}{}",
                            persist::POLICY_REASON_PREFIX,
                            rules.into_iter().collect::<Vec<_>>().join(",")
                        )
                    };
                    tx.execute(
                        "UPDATE assertion SET value = ?1, rank = 'deprecated', rank_reason = ?3 WHERE id = ?2",
                        params![value.as_str(), id, reason],
                    )
                    .sql()?;
                    persist::set_status(&tx, id, kaban_core::AssertionStatus::Rejected, now)?;
                    groups.insert(kaban_core::group_key(&claim));
                    masked.insert(id);
                    report.rejected += 1;
                }
            }
        }
        audit_table::append(&tx, self.signer.as_ref(), &audit)?;
        reconcile::decide_groups_without(&tx, &self.reconcile, &groups, &masked, now, None)?;
        for (key, value) in [
            (ASSERTION_POLICY_HASH, policy_version.to_owned()),
            (persist::ASSERTION_POLICY_HW, max_id.to_string()),
        ] {
            tx.execute(
                "INSERT INTO index_meta(key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![key, value],
            )
            .sql()?;
        }
        tx.commit().sql()?;
        Ok(report)
    }
    /// The body of [`kaban_core::IngestStore::deprecate_assertion`].
    pub(super) fn deprecate_assertion_impl(
        &self,
        key: &str,
        reason: &str,
        ctx: &WriteCtx,
    ) -> Result<kaban_core::AssertionStatus> {
        let mut conn = self.write();
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .sql()?;
        let ws = ctx.workspace_id.as_str();
        // the one rank write the claim table sees (EN-F12b): rank and reason are outside the
        // identity, so the row keeps its key, its status row and its ChangeLog history. The
        // claim is bound to the caller's workspace (S6, RE4-4): a foreign key and an unknown
        // one are the same `NotFound`, nothing written
        let changed = tx
            .execute(
                "UPDATE assertion SET rank = 'deprecated', rank_reason = ?2 WHERE key = ?1 AND workspace_id = ?3",
                params![key, reason, ws],
            )
            .sql()?;
        if changed == 0 {
            return Err(StoreError::NotFound(key.to_owned()));
        }
        super::artifacts::clear_machine_retirement(&tx, key, ws)?;
        audit_table::append(&tx, self.signer.as_ref(), &ctx.audit)?;
        let (id, group): (i64, String) = tx
            .query_row(
                "SELECT a.id, s.group_key FROM assertion a JOIN assertion_status s ON s.assertion_id = a.id
                 WHERE a.key = ?1 AND a.workspace_id = ?2",
                params![key, ws],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .sql()?;
        let now = persist::now_unix();
        let groups: BTreeSet<String> = std::iter::once(group).collect();
        reconcile::decide_groups(&tx, &self.reconcile, &groups, now, None)?;
        let disposition: String = tx
            .query_row(
                "SELECT disposition FROM assertion_status WHERE assertion_id = ?1",
                [id],
                |r| r.get(0),
            )
            .sql()?;
        tx.commit().sql()?;
        kaban_core::AssertionStatus::parse(&disposition)
            .ok_or_else(|| bad_enum(0, &disposition))
            .sql()
    }
}
