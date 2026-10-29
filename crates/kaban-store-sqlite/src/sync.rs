//! The `.kaban/<ontology>/<dir>/<id>.md` serialization side of the store (core-spec F13, F9b,
//! F5, S5, S6; core-plan C5.1 and C5.3 as amended by `reviews/m5-prep.md` M5-7, M5-8, M5-15,
//! M5-18 and owner decisions OWN-18, OWN-23, OWN-25; engine-plan E1.6): the dirty set, the
//! serialized-rev stamp, the import of a record or a tombstone, the equal-rev conflict rows
//! and the `post-commit` stamp. The `SyncStore` role trait is implemented here, over the same
//! connection, transactions and audit chain as the other roles. No migration: `node.rev`,
//! `node.serialized_rev` and `conflicts` exist since 0002.
//!
//! The serialized kinds are not a literal here (EN-F4): the caller passes the stored names of
//! the kinds with `governed.serialize != none` from the loaded ontologies and they are bound
//! as `?` placeholders, one per name (the E1.4 `briefing` pattern), never interpolated.
//! Governance of an imported row comes from the write context's governed set (E1.4).

use crate::audit_table;
use crate::events;
use crate::rows::{NODE_COLS, bad_enum, row_node};
use crate::sql_error::SqlResultExt;
use crate::sqlite::{SqliteStore, content_hash};
use kaban_core::{
    ConflictRow, Governance, ImportedPurge, ImportedRecord, InvalidTransition, Lifecycle, NewEvent,
    Node, NodeId, Result, SignerKind, StoreError, SyncStore, UnstampedCounts, UnstampedReason,
    WriteCtx, lifecycle_transition, now_iso,
};

/// The `events.kind` of an OWN-65 reason row (engine-spec §3.1 "Sources that are not
/// repositories"); its `reason` is [`UnstampedReason::as_str`].
const UNSTAMPED_EVENT: &str = "unstamped";
use rusqlite::{Connection, OptionalExtension, ToSql, params};

/// `?, ?, ...` for `kinds.len()` bound kind names (the registry bounds the count).
fn kind_placeholders(kinds: &[&str]) -> String {
    vec!["?"; kinds.len()].join(", ")
}

/// The bound arguments of the two kind-filtered reads: the workspace, every kind name, the
/// limit, in placeholder order.
fn kind_args<'a>(
    workspace_id: &'a &'a str,
    kinds: &'a [&'a str],
    limit: &'a i64,
) -> Vec<&'a dyn ToSql> {
    let mut args: Vec<&dyn ToSql> = Vec::with_capacity(kinds.len() + 2);
    args.push(workspace_id);
    args.extend(kinds.iter().map(|k| k as &dyn ToSql));
    args.push(limit);
    args
}

/// Columns of a [`ConflictRow`], aliased `c`, in [`row_conflict`] order.
const CONFLICT_COLS: &str =
    "c.node_id, c.file_rev, c.db_rev, c.file_hash, c.db_hash, c.detected_at";

fn row_conflict(r: &rusqlite::Row<'_>) -> rusqlite::Result<ConflictRow> {
    Ok(ConflictRow {
        node_id: r.get(0)?,
        file_rev: r.get(1)?,
        db_rev: r.get(2)?,
        file_hash: r.get(3)?,
        db_hash: r.get(4)?,
        detected_at: r.get(5)?,
    })
}

impl SyncStore for SqliteStore {
    fn dirty_serializable(
        &self,
        workspace_id: &str,
        kinds: &[&str],
        limit: usize,
    ) -> Result<Vec<Node>> {
        if limit == 0 || kinds.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.lock();
        let placeholders = kind_placeholders(kinds);
        // a live row is dirty until its current rev is on disk; a purged row only once it had
        // a file (M5-7) and until its tombstone is written at the purge's rev
        let mut st = conn
            .prepare_cached(&format!(
                "SELECT {NODE_COLS} FROM node
                 WHERE workspace_id = ? AND store_scope = 'project' AND kind IN ({placeholders})
                   AND ((lifecycle <> 'purged' AND (serialized_rev IS NULL OR serialized_rev <> rev))
                        OR (lifecycle = 'purged' AND serialized_rev IS NOT NULL AND serialized_rev <> rev))
                 ORDER BY id LIMIT ?"
            ))
            .sql()?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let rows = st
            .query_map(kind_args(&workspace_id, kinds, &limit).as_slice(), row_node)
            .sql()?
            .collect::<rusqlite::Result<Vec<_>>>()
            .sql()?;
        Ok(rows)
    }
    fn serialized_records(
        &self,
        workspace_id: &str,
        kinds: &[&str],
        limit: usize,
    ) -> Result<Vec<Node>> {
        if limit == 0 || kinds.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.lock();
        let placeholders = kind_placeholders(kinds);
        let mut st = conn
            .prepare_cached(&format!(
                "SELECT {NODE_COLS} FROM node
                 WHERE workspace_id = ? AND store_scope = 'project' AND kind IN ({placeholders})
                   AND serialized_rev IS NOT NULL AND lifecycle IN ('active', 'archived')
                 ORDER BY id LIMIT ?"
            ))
            .sql()?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let rows = st
            .query_map(kind_args(&workspace_id, kinds, &limit).as_slice(), row_node)
            .sql()?
            .collect::<rusqlite::Result<Vec<_>>>()
            .sql()?;
        Ok(rows)
    }
    fn mark_serialized(&self, id: &NodeId, rev: i64) -> Result<()> {
        // a canonical column moves: the change is logged and published like any write
        let conn = self.write();
        let n = conn
            .execute(
                "UPDATE node SET serialized_rev = ?1 WHERE id = ?2",
                params![rev, id.as_str()],
            )
            .sql()?;
        if n == 0 {
            return Err(StoreError::NotFound(id.to_string()));
        }
        Ok(())
    }
    fn import_record(&self, record: &ImportedRecord, ctx: &WriteCtx) -> Result<Node> {
        let ws = ctx.workspace_id.as_str();
        let id = record.id.as_str();
        let mut conn = self.write();
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .sql()?;
        if !SqliteStore::kind_exists(&tx, &record.kind)? {
            return Err(StoreError::UnknownKind(record.kind.clone()));
        }
        let stored = stored_state(&tx, id)?;
        // a purged id never comes back (F5, M5-8) and another tenant's row is invisible (S6):
        // both are `NotFound`, no existence leak
        if let Some(st) = &stored {
            if st.lifecycle == Lifecycle::Purged || st.workspace_id != ws {
                return Err(StoreError::NotFound(id.to_owned()));
            }
            if st.kind != record.kind {
                return Err(StoreError::KindMismatch {
                    id: id.to_owned(),
                    stored: st.kind.clone(),
                    requested: record.kind.clone(),
                });
            }
            // the spec's tables decide every state move (§4, M5-31), before any write; the
            // governed kinds are the context's loaded set (E1.4; E1.7a RE2-6), as for the
            // insert path and `transition`
            if st.governance != record.governance {
                ctx.governed.governance_transition(
                    &record.kind,
                    st.governance,
                    record.governance,
                )?;
                if st.lifecycle != Lifecycle::Active {
                    return Err(InvalidTransition::NotActive(st.lifecycle).into());
                }
            }
            if st.lifecycle != record.lifecycle {
                lifecycle_transition(st.lifecycle, record.lifecycle)?;
            }
        }
        audit_table::append(&tx, self.signer().as_ref(), &ctx.audit)?;
        if matches!(record.lifecycle, Lifecycle::Tombstoned | Lifecycle::Purged) {
            crate::jobs::revoke_document(&tx, id, ws)?;
        }
        let tags = serde_json::to_string(&record.tags)?;
        let path_scope = serde_json::to_string(&record.path_scope)?;
        let props = serde_json::to_string(&record.props)?;
        let hash = content_hash(ws, &record.kind, &record.text);
        let bodies = crate::body_codec::Writer::for_connection(&tx)?;
        let governance_changed = if let Some(st) = &stored {
            tx.execute(
                    "UPDATE node SET text=?1, summary=?2, tags=?3, confidence=?4, importance=?5,
                         lifecycle=?6, governance=?7, path_scope=?8, rev=?9, serialized_rev=?9,
                         dup_key=?10, created_at=?11, updated_at=?12, valid_from=?13, valid_to=?14,
                         session_id=?15, agent_id=?16, repo=?17, commit_sha=?18, accepted_commit=?19,
                         props=?20, content_hash=?21, policy_version=?22
                     WHERE rid=?23",
                    params![
                        bodies.stored(&record.text), record.summary, tags, record.confidence, record.importance,
                        record.lifecycle.as_str(), record.governance.as_str(), path_scope, record.rev,
                        record.dup_key, record.created_at, record.updated_at, record.valid_from,
                        record.valid_to, record.session_id, record.agent_id, record.repo,
                        record.commit_sha, record.accepted_commit, props, hash, ctx.policy_version,
                        st.rid
                    ],
                )
                .sql()?;
            st.governance != record.governance
        } else {
            tx.execute(
                    "INSERT INTO node(id,kind,text,summary,tags,confidence,importance,lifecycle,governance,store_scope,path_scope,rev,serialized_rev,dup_key,created_at,updated_at,valid_from,valid_to,session_id,agent_id,workspace_id,repo,commit_sha,accepted_commit,props,content_hash,policy_version)
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,'project',?10,?11,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24,?25)",
                    params![
                        id, record.kind, bodies.stored(&record.text), record.summary, tags, record.confidence,
                        record.importance, record.lifecycle.as_str(), record.governance.as_str(),
                        path_scope, record.rev, record.dup_key, record.created_at, record.updated_at,
                        record.valid_from, record.valid_to, record.session_id, record.agent_id, ws,
                        record.repo, record.commit_sha, record.accepted_commit, props, hash,
                        ctx.policy_version
                    ],
                )
                .sql()?;
            // a fresh row records the state the file arrived in when it is past the start
            // state (the acceptance proof, OD-15); governed per the context's loaded set (E1.4)
            ctx.governed.contains(&record.kind)
                && !matches!(record.governance, Governance::Proposed | Governance::None)
        };
        // EN-F10: the imported record's anchor rows follow its stored props and scope, byte for
        // byte as a remembered one's (CG-JOIN v1 §5 `anchor_imported_coderef_rows`)
        if crate::reconcile::engine_tables(&tx)? {
            let rid: i64 = tx
                .query_row("SELECT rid FROM node WHERE id=?", [id], |r| r.get(0))
                .sql()?;
            crate::anchors::refresh(&tx, rid, &ctx.governed)?;
            crate::sqlite::artifacts::invalidate_stale(&tx, rid)?;
        }
        if governance_changed {
            let default_event = NewEvent {
                reason: "import".into(),
                signer_kind: SignerKind::System,
                ..Default::default()
            };
            let ev = record.event.as_ref().unwrap_or(&default_event);
            events::append(&tx, id, ws, record.governance.as_str(), ev)?;
        }
        let node = tx
            .query_row(
                &format!("SELECT {NODE_COLS} FROM node WHERE id=?"),
                [id],
                row_node,
            )
            .sql()?;
        crate::changelog::record_terminal(&tx, ctx)?;
        tx.commit().sql()?;
        Ok(node)
    }
    fn import_purged(
        &self,
        id: &NodeId,
        kind: &str,
        rev: i64,
        ctx: &WriteCtx,
    ) -> Result<ImportedPurge> {
        let ws = ctx.workspace_id.as_str();
        let id = id.as_str();
        let mut conn = self.write();
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .sql()?;
        let stored = stored_state(&tx, id)?;
        if let Some(st) = &stored {
            if st.workspace_id != ws {
                return Err(StoreError::NotFound(id.to_owned()));
            }
            if st.kind != kind {
                return Err(StoreError::KindMismatch {
                    id: id.to_owned(),
                    stored: st.kind.clone(),
                    requested: kind.to_owned(),
                });
            }
        }
        audit_table::append(&tx, self.signer().as_ref(), &ctx.audit)?;
        let now = now_iso();
        // the rid of a live row purged in place: its post-commit cleanup runs below
        let (new_rev, purged) = match &stored {
            // already purged: the tombstone only moves the rev up (the update trigger skips
            // purged rows, so no `ChangeLog` noise)
            Some(st) if st.lifecycle == Lifecycle::Purged => {
                tx.execute(
                    "UPDATE node SET rev=?1, serialized_rev=?1 WHERE rid=?2 AND rev < ?1",
                    params![rev, st.rid],
                )
                .sql()?;
                let current: i64 = tx
                    .query_row("SELECT rev FROM node WHERE rid=?", [st.rid], |r| r.get(0))
                    .sql()?;
                (current, None)
            }
            // live: the tombstone always wins (M5-8), the purge path is `forget`'s
            Some(st) => (
                self.purge_in_place(&tx, st.rid, id, ws, &now, Some(rev))?,
                Some(st.rid),
            ),
            // unknown: a ghost so the id never reappears (F5)
            None => {
                if !SqliteStore::kind_exists(&tx, kind)? {
                    return Err(StoreError::UnknownKind(kind.to_owned()));
                }
                let governance = if ctx.governed.contains(kind) {
                    Governance::Proposed
                } else {
                    Governance::None
                };
                tx.execute(
                    "INSERT INTO node(id,kind,text,summary,tags,confidence,importance,lifecycle,governance,store_scope,path_scope,rev,serialized_rev,dup_key,created_at,updated_at,valid_to,workspace_id,props,content_hash,policy_version)
                     VALUES (?1,?2,'',NULL,'[]',NULL,NULL,'purged',?3,'project','[]',?4,?4,NULL,?5,?5,?5,?6,'{}','',?7)",
                    params![id, kind, governance.as_str(), rev, now, ws, ctx.policy_version],
                )
                .sql()?;
                (rev, None)
            }
        };
        crate::changelog::record_terminal(&tx, ctx)?;
        tx.commit().sql()?;
        // committed: the same post-commit cleanup as `forget(Purge)` (SS-03, SS-12: the index
        // scope included), after the write's guard published the purge; an incomplete cleanup
        // is not this import's failure but its report
        drop(conn);
        let cleanup = purged.and_then(|rid| self.purge_cleanup(rid).cleanup);
        Ok(ImportedPurge {
            rev: new_rev,
            cleanup,
        })
    }
    fn conflict_upsert(&self, row: &ConflictRow) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO conflicts(node_id, file_rev, db_rev, file_hash, db_hash, detected_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(node_id) DO UPDATE SET file_rev=excluded.file_rev, db_rev=excluded.db_rev,
                 file_hash=excluded.file_hash, db_hash=excluded.db_hash, detected_at=excluded.detected_at",
            params![
                row.node_id,
                row.file_rev,
                row.db_rev,
                row.file_hash,
                row.db_hash,
                row.detected_at
            ],
        )
        .sql()?;
        Ok(())
    }
    fn conflict_clear(&self, id: &NodeId) -> Result<bool> {
        let conn = self.lock();
        let n = conn
            .execute("DELETE FROM conflicts WHERE node_id = ?", [id.as_str()])
            .sql()?;
        Ok(n > 0)
    }
    fn conflicts(&self, workspace_id: &str) -> Result<Vec<ConflictRow>> {
        let conn = self.lock();
        // the table is DB-scoped (F17); the node row says which tenant a conflict belongs to
        let mut st = conn
            .prepare_cached(&format!(
                "SELECT {CONFLICT_COLS} FROM conflicts c
                 JOIN node n ON n.id = c.node_id
                 WHERE n.workspace_id = ? ORDER BY c.node_id"
            ))
            .sql()?;
        let rows = st
            .query_map([workspace_id], row_conflict)
            .sql()?
            .collect::<rusqlite::Result<Vec<_>>>()
            .sql()?;
        Ok(rows)
    }
    fn conflicts_for(&self, ids: &[NodeId]) -> Result<Vec<ConflictRow>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.lock();
        // the ids as a positional values table, so the rows come back in card order (the
        // `linked_refs` pattern, R3-10)
        let values = (1..=ids.len())
            .map(|i| format!("(?{i},{})", i - 1))
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "WITH wanted(id, pos) AS (VALUES {values})
             SELECT {CONFLICT_COLS} FROM wanted w JOIN conflicts c ON c.node_id = w.id
             ORDER BY w.pos"
        );
        let mut st = conn.prepare(&sql).sql()?;
        let rows = st
            .query_map(
                rusqlite::params_from_iter(ids.iter().map(NodeId::as_str)),
                row_conflict,
            )
            .sql()?
            .collect::<rusqlite::Result<Vec<_>>>()
            .sql()?;
        Ok(rows)
    }
    fn stamp_commit(
        &self,
        workspace_id: &str,
        sha: &str,
        repo: Option<&str>,
        kinds: &[&str],
    ) -> Result<u64> {
        // OWN-65: the policy is the caller's (the registry's stamping set); nothing to stamp
        // without a kind, and the kind names are bound, never interpolated (E1.6 pattern)
        if kinds.is_empty() {
            return Ok(0);
        }
        let mut conn = self.write();
        // a write path: BEGIN IMMEDIATE like every other (N4)
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .sql()?;
        let sql = format!(
            "UPDATE node SET commit_sha=?, rev=rev+1, updated_at=?
             WHERE workspace_id=? AND commit_sha IS NULL AND lifecycle <> 'purged'
               AND (repo IS NULL OR repo = ?) AND kind IN ({})",
            kind_placeholders(kinds)
        );
        let now = now_iso();
        let mut args: Vec<&dyn ToSql> = Vec::with_capacity(kinds.len() + 4);
        args.push(&sha);
        args.push(&now);
        args.push(&workspace_id);
        args.push(&repo);
        args.extend(kinds.iter().map(|k| k as &dyn ToSql));
        let n = tx.execute(&sql, rusqlite::params_from_iter(args)).sql()?;
        tx.commit().sql()?;
        Ok(u64::try_from(n).unwrap_or(0))
    }
    fn record_unstamped(
        &self,
        node: &NodeId,
        workspace_id: &str,
        reason: UnstampedReason,
    ) -> Result<u64> {
        let mut conn = self.write();
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .sql()?;
        let ev = NewEvent {
            reason: reason.as_str().to_owned(),
            path_scope: Vec::new(),
            signer: None,
            signer_kind: SignerKind::System,
            host: None,
            session_id: None,
            commit_sha: None,
        };
        events::append(&tx, node.as_str(), workspace_id, UNSTAMPED_EVENT, &ev)?;
        let terminal = crate::changelog::head(&tx)?;
        tx.commit().sql()?;
        Ok(terminal)
    }
    fn unstamped_current_count(
        &self,
        workspace_id: &str,
        repo: Option<&str>,
        kinds: &[&str],
    ) -> Result<u64> {
        if kinds.is_empty() {
            return Ok(0);
        }
        let conn = self.lock();
        let sql = format!(
            "SELECT count(*) FROM node WHERE workspace_id=? AND commit_sha IS NULL
             AND lifecycle <> 'purged' AND (repo IS NULL OR repo = ?) AND kind IN ({})",
            kind_placeholders(kinds)
        );
        let mut args: Vec<&dyn ToSql> = Vec::with_capacity(kinds.len() + 2);
        args.push(&workspace_id);
        args.push(&repo);
        args.extend(kinds.iter().map(|k| k as &dyn ToSql));
        let n: i64 = conn
            .query_row(&sql, rusqlite::params_from_iter(args), |r| r.get(0))
            .sql()?;
        Ok(u64::try_from(n).unwrap_or(0))
    }
    fn unstamped_counts(&self, workspace_id: &str) -> Result<UnstampedCounts> {
        let conn = self.lock();
        let mut st = conn
            .prepare_cached(
                "SELECT reason, count(*) FROM events
                 WHERE kind = ?1 AND workspace_id = ?2 GROUP BY reason",
            )
            .sql()?;
        let rows = st
            .query_map(params![UNSTAMPED_EVENT, workspace_id], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
            })
            .sql()?
            .collect::<rusqlite::Result<Vec<_>>>()
            .sql()?;
        let mut counts = UnstampedCounts::default();
        for (reason, n) in rows {
            let n = u64::try_from(n).unwrap_or(0);
            match reason.as_str() {
                "no_repository" => counts.no_repository = n,
                "no_history" => counts.no_history = n,
                _ => {} // a reason this binary does not know: not counted, never an error
            }
        }
        Ok(counts)
    }
}

/// What the store holds under an id, whatever its workspace or lifecycle (the caller decides
/// what is visible).
struct Stored {
    rid: i64,
    kind: String,
    lifecycle: Lifecycle,
    governance: Governance,
    workspace_id: String,
}

fn stored_state(conn: &Connection, id: &str) -> Result<Option<Stored>> {
    let row: Option<(i64, String, String, String, String)> = conn
        .query_row(
            "SELECT rid, kind, lifecycle, governance, workspace_id FROM node WHERE id=?",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()
        .sql()?;
    row.map(|(rid, kind, lifecycle, governance, workspace_id)| {
        Ok(Stored {
            rid,
            kind,
            lifecycle: Lifecycle::parse(&lifecycle)
                .ok_or_else(|| bad_enum(2, &lifecycle))
                .sql()?,
            governance: Governance::parse(&governance)
                .ok_or_else(|| bad_enum(3, &governance))
                .sql()?,
            workspace_id,
        })
    })
    .transpose()
}
