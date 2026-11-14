//! The `apply_batch` transaction (`apply_batch_impl`): node ops in order, the edges under the
//! first-writer rule, the edge diff (`DeletesUnnamed`) and the deferred lifecycle effects, and
//! the structured-job submit and finish transactions. Split out of `sqlite.rs` (OWN-79).

use crate::anchors;
use crate::edge_write::{self, EdgeWrite, now_unix};
use crate::rows::bad_enum;
use crate::sql_error::SqlResultExt;
use kaban_core::Result;
use kaban_core::{
    BatchReport, EdgeDiff, Lifecycle, Op, OutcomeKind, StoreError, WriteBatch, WriteCtx, now_iso,
};
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::BTreeSet;

use super::{LinkEffect, SqliteStore};

impl SqliteStore {
    pub(super) fn job_finish_impl(
        &self,
        request: &kaban_core::JobFinishRequest,
        ctx: &WriteCtx,
        report: &mut dyn FnMut(
            &kaban_core::IngestJob,
            &kaban_core::JobFinishOutcome,
        ) -> Result<kaban_core::JobFinishStateUpdate>,
    ) -> Result<String> {
        let mut conn = self.write();
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .sql()?;
        let current = crate::jobs::get(&tx, &request.job, &ctx.workspace_id)?
            .ok_or_else(|| StoreError::NotFound("job_revoked".into()))?;
        crate::jobs::holder(&tx, &current, request.contract.fence(), false)?;
        let now = now_unix();
        if current.expires_at <= now || current.status == kaban_core::JobStatus::Expired {
            return Err(StoreError::InvalidName("job".into(), "expired"));
        }
        if !matches!(
            current.status,
            kaban_core::JobStatus::Open | kaban_core::JobStatus::Finished
        ) || kaban_core::JobSubmitContract::from_finish_job(&current)?
            .with_fence(request.contract.fence())
            != request.contract
        {
            return Err(StoreError::InvalidName("job".into(), "stale"));
        }
        crate::jobs::check_source(
            &tx,
            &current,
            &request.document,
            &request.source_rev,
            &request.chunks,
        )?;
        let mut outcome = kaban_core::JobFinishOutcome::default();
        let mut plan: serde_json::Value = serde_json::from_str(&current.plan_json)?;
        if current.status == kaban_core::JobStatus::Open {
            let population = crate::jobs::finish_population(&tx, &current, request)?;
            outcome.skipped_cap = population.skipped_cap;
            if !outcome.skipped_cap {
                let before: i64 = tx
                    .query_row("SELECT COALESCE(max(seq),0) FROM changes", [], |r| r.get(0))
                    .sql()?;
                let named: BTreeSet<_> = current.named_edges.iter().cloned().collect();
                for (src, dst, rel, src_id, dst_id) in population.edges {
                    if !named.contains(&(src_id, dst_id, rel.clone())) {
                        // Delete only an owned remainder; never a human/foreign replacement.
                        tx.execute("DELETE FROM edge WHERE src=?1 AND dst=?2 AND rel=?3 AND layer LIKE 'extractor:%'",params![src,dst,rel]).sql()?;
                        let remains: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM edge WHERE src=?1 AND dst=?2 AND rel=?3)",params![src,dst,rel],|r|r.get(0)).sql()?;
                        if !remains {
                            outcome.deleted_edges += 1;
                        }
                    }
                }
                let after: i64 = tx
                    .query_row("SELECT COALESCE(max(seq),0) FROM changes", [], |r| r.get(0))
                    .sql()?;
                if after > before {
                    outcome.first_seq = Some(u64::try_from(before + 1).map_err(|_| {
                        StoreError::InvalidName("changes".into(), "invalid_sequence")
                    })?);
                    outcome.last_seq = Some(u64::try_from(after).map_err(|_| {
                        StoreError::InvalidName("changes".into(), "invalid_sequence")
                    })?);
                }
            }
        }
        // the run's terminal sequence (SS-10): the head this transaction sees, the receipt's
        // position whether or not the finish wrote change rows
        outcome.terminal_seq = crate::changelog::head(&tx)?;
        ctx.record_terminal_seq(outcome.terminal_seq);
        let state = report(&current, &outcome)?;
        if current.status == kaban_core::JobStatus::Finished {
            if plan["finished_report_json"].as_str() != Some(state.report_json.as_str()) {
                return Err(StoreError::InvalidName("job".into(), "report_changed"));
            }
        } else {
            let marker = crate::jobs::done_artifact(&tx, &current, request)?;
            super::artifacts::publish(&tx, &[marker])?;
            crate::jobs::release(&tx, &current)?;
            let _: serde_json::Value = serde_json::from_str(&state.report_json)?;
            plan["finished_report_json"] = serde_json::Value::String(state.report_json.clone());
            tx.execute("UPDATE ingest_jobs SET plan=?1,status='finished',finished_at=?2,expires_at=?3 WHERE job_id=?4 AND workspace_id=?5 AND status='open'",params![plan.to_string(),now,now.saturating_add(i64::from(request.retention_days)*86400),current.job.as_str(),current.workspace.as_str()]).sql()?;
        }
        tx.commit().sql()?;
        Ok(state.report_json)
    }

    pub(super) fn job_submit_impl(
        &self,
        request: kaban_core::JobSubmitRequest,
        ctx: &WriteCtx,
        accumulate: &mut dyn FnMut(
            &kaban_core::IngestJob,
            &BatchReport,
        ) -> Result<kaban_core::JobSubmitStateUpdate>,
    ) -> Result<kaban_core::JobSubmitReceipt> {
        if request.batch.workspace_id != ctx.workspace_id {
            return Err(StoreError::WorkspaceMismatch {
                ctx: ctx.workspace_id.to_string(),
                batch: request.batch.workspace_id.to_string(),
            });
        }
        if request.batch.edge_diff != EdgeDiff::None
            || request
                .batch
                .ops
                .iter()
                .any(|op| matches!(op, Op::Tombstone { .. } | Op::Reactivate { .. }))
        {
            return Err(StoreError::InvalidName("submit".into(), "invalid_batch"));
        }
        let mut conn = self.write();
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .sql()?;
        let current = crate::jobs::get(&tx, &request.job, &ctx.workspace_id)?
            .ok_or_else(|| StoreError::NotFound("job_revoked".into()))?;
        crate::jobs::holder(&tx, &current, request.contract.fence(), true)?;
        if current.status != kaban_core::JobStatus::Open {
            return Err(StoreError::InvalidName("job".into(), "stale"));
        }
        if current.expires_at <= now_unix() {
            return Err(StoreError::InvalidName("job".into(), "expired"));
        }
        if kaban_core::JobSubmitContract::from_job(&current)?.with_fence(request.contract.fence())
            != request.contract
        {
            return Err(StoreError::InvalidName("job".into(), "stale"));
        }
        crate::jobs::check_source(
            &tx,
            &current,
            &request.document,
            &request.source_rev,
            &request.chunks,
        )?;
        let redactions = request.redactions;
        let report = self.apply_batch_body(&tx, request.batch)?;
        if report.stale_source != 0 {
            // Drop the uncommitted transaction, including any body bookkeeping.
            return Err(StoreError::InvalidName("job".into(), "stale"));
        }
        crate::jobs::check_source(
            &tx,
            &current,
            &request.document,
            &request.source_rev,
            &request.chunks,
        )?;
        let state = accumulate(&current, &report)?;
        let updated = crate::jobs::record_submit(&tx, current, &report, state, redactions)?;
        tx.commit().sql()?;
        Ok(kaban_core::JobSubmitReceipt {
            batch: report,
            job: updated,
        })
    }

    /// The body of [`kaban_core::IngestStore::apply_batch`].
    pub(super) fn apply_batch_impl(
        &self,
        batch: WriteBatch,
        ctx: &WriteCtx,
    ) -> Result<BatchReport> {
        // every row the batch touches must belong to one tenant
        // (R2-1): refused before the transaction opens
        if ctx.workspace_id != batch.workspace_id {
            return Err(StoreError::WorkspaceMismatch {
                ctx: ctx.workspace_id.to_string(),
                batch: batch.workspace_id.to_string(),
            });
        }
        let mut conn = self.write();
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .sql()?;
        let report = self.apply_batch_body(&tx, batch)?;
        if report.stale_source == 0 {
            crate::changelog::record_terminal(&tx, ctx)?;
            tx.commit().sql()?;
        }
        Ok(report)
    }

    /// Shared transaction body: callers retain lock/transaction ownership.
    pub(super) fn apply_batch_body(
        &self,
        tx: &Connection,
        mut batch: WriteBatch,
    ) -> Result<BatchReport> {
        let batch_producer = batch.edge_diff.producer();
        let stale_source = super::artifacts::stale_sources(tx, &batch)?;
        if stale_source != 0 {
            return Ok(BatchReport {
                stale_source,
                ..BatchReport::default()
            });
        }
        let before: i64 = tx
            .query_row("SELECT COALESCE(max(seq), 0) FROM changes", [], |r| {
                r.get(0)
            })
            .sql()?;
        let mut report = BatchReport::default();
        let workspace_id = batch.workspace_id.clone();
        let ws = workspace_id.as_str();
        let tenant = self.tenant(ws);
        let scoring = edge_write::scoring_columns(tx)?;
        // node ops first, in order; the rids of upserted nodes drive the edge diff (MD-23)
        let mut upserted: Vec<i64> = Vec::new();
        for op in &batch.ops {
            match op {
                Op::UpsertNode { id, node } => {
                    upserted.push(Self::upsert_node(tx, &tenant, id, node, &mut report)?);
                }
                // the `ForgetMode::Tombstone` path: lifecycle only, no event, edges kept;
                // already tombstoned or purged is a no-op (WD-3), unknown is `NotFound`; a
                // node of another workspace is `NotFound` too (R2-1, F17: no existence leak)
                Op::Tombstone { id, reason: _ } => {
                    let st: Option<(i64, String, String)> = tx
                        .query_row(
                            "SELECT rid, lifecycle, kind FROM node WHERE id=? AND workspace_id=?",
                            params![id.as_str(), ws],
                            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                        )
                        .optional()
                        .sql()?;
                    let (rid, lifecycle, kind) =
                        st.ok_or_else(|| StoreError::NotFound(id.to_string()))?;
                    crate::jobs::revoke_document(tx, id.as_str(), ws)?;
                    let lifecycle = Lifecycle::parse(&lifecycle)
                        .ok_or_else(|| bad_enum(1, &lifecycle))
                        .sql()?;
                    if matches!(lifecycle, Lifecycle::Tombstoned | Lifecycle::Purged) {
                        report.record(id, &kind, OutcomeKind::AlreadyHidden);
                        continue;
                    }
                    kaban_core::lifecycle_transition(lifecycle, Lifecycle::Tombstoned)?;
                    tx.execute(
                        "UPDATE node SET lifecycle='tombstoned', valid_to=?1, updated_at=?1, rev=rev+1 WHERE rid=?2",
                        params![now_iso(), rid],
                    ).sql()?;
                    if scoring {
                        anchors::remove(tx, rid)?;
                    }
                    report.record(id, &kind, OutcomeKind::Tombstoned);
                }
                Op::Reactivate { id } => {
                    if let Some(rid) = Self::reactivate_node(tx, &tenant, id, &mut report)?
                        && scoring
                    {
                        // the pointer of a reactivated source comes back
                        anchors::refresh(tx, rid)?;
                    }
                }
                Op::UpsertEdge { .. } => {}
            }
        }
        let prepared = super::artifacts::prepare_sets(tx, &mut batch)?;
        super::artifacts::publish(tx, &batch.artifacts)?;
        // then the edges of the batch, each under one producer: the edge's own
        // `props.producer` (the trusted batch boundary), else the batch's. The first writer of
        // a `(src, dst, rel)` triple keeps the row: another producer's upsert of it writes
        // nothing and is counted (owner decision 2026-11-14). An edge whose stored row is
        // unchanged writes nothing (N5); every edge the batch names is kept by the diff below.
        // A producer edge's `method`, `score` and span become the scoring columns.
        let mut kept: Vec<(i64, i64, String)> = Vec::new();
        for op in &batch.ops {
            if let Op::UpsertEdge {
                src,
                dst,
                rel,
                props,
            } = op
            {
                // both endpoints must belong to the batch's workspace (R2-1) before any
                // edge write or `SUPERSEDES`/`INVALIDATES` side effect on the target
                let (srid, skind) = Self::rid_kind_in_workspace(tx, src.as_str(), ws)?;
                let (drid, dkind) = Self::rid_kind_in_workspace(tx, dst.as_str(), ws)?;
                if srid == drid {
                    return Err(StoreError::SelfLink(src.to_string()));
                }
                Self::edge_rule(tx, rel, &skind, &dkind)?;
                let mut props = props.clone().unwrap_or_else(|| serde_json::json!({}));
                let scored = Scored::take(&mut props, batch_producer);
                let written = edge_write::write_edge(
                    tx,
                    &EdgeWrite {
                        src: srid,
                        dst: drid,
                        rel,
                        // a producer edge's confidence is its own score (no prior applies)
                        confidence: scored.score,
                        props: &props,
                        producer: scored.producer,
                        method: scored.method,
                        score: scored.score,
                        source_id: scored.source_id.as_deref(),
                        offset: scored.offset,
                        len: scored.len,
                    },
                )?;
                if written == edge_write::Written::KeptByFirstWriter {
                    report.edges_kept_by_first_writer += 1;
                } else if written.wrote()
                    && let Some(effect) = LinkEffect::of(&tenant.rules, rel)
                {
                    Self::mark_target(tx, dst.as_str(), effect, &now_iso())?;
                }
                kept.push((srid, drid, rel.clone()));
            }
        }
        super::artifacts::record_sets(tx, &batch.workspace_id, &prepared, &kept)?;
        // MD-23 / EN-E4: outgoing edges of each upserted node that the batch did not name are
        // deleted (the edge delete trigger writes their changes rows); incoming edges are
        // untouched. Producer isolation: `Complete` and `Owned` delete only the unnamed rows
        // whose stored producer is the batch's, so a rerun never deletes another producer's
        // row; `None` deletes nothing. A store below 0004 stores no producer and deletes
        // nothing. Gazetteer mentions are their set's lineage: `record_sets` retires them.
        if let Some(deleting) = batch.edge_diff.deleting().filter(|_| scoring) {
            upserted.sort_unstable();
            upserted.dedup();
            for rid in upserted {
                let outgoing: Vec<(i64, i64, String)> = {
                    let mut st = tx
                        .prepare_cached("SELECT src, dst, rel FROM edge WHERE src=?1 AND layer=?2")
                        .sql()?;
                    let rows = st
                        .query_map(params![rid, deleting.as_str()], |r| {
                            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                        })
                        .sql()?;
                    rows.collect::<rusqlite::Result<_>>().sql()?
                };
                for (src, dst, rel) in outgoing {
                    if kept.contains(&(src, dst, rel.clone())) {
                        continue;
                    }
                    tx.execute(
                        "DELETE FROM edge WHERE src=? AND dst=? AND rel=?",
                        params![src, dst, rel],
                    )
                    .sql()?;
                }
            }
        }
        let (first, last): (Option<i64>, Option<i64>) = tx
            .query_row(
                "SELECT min(seq), max(seq) FROM changes WHERE seq > ?",
                [before],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .sql()?;
        report.outbox_first_seq = first.and_then(|v| u64::try_from(v).ok());
        report.outbox_last_seq = last.and_then(|v| u64::try_from(v).ok());
        Ok(report)
    }
}

/// The producer and scoring columns of a batch edge, read from its props: `producer` (an
/// edge naming its own producer at the trusted batch boundary, else the batch's) is moved
/// out of the props into the `layer` column; `method`, `score` (fixed point, `/ 10000`),
/// `source_id`, `offset` and `len` stay in the props as written.
struct Scored {
    producer: kaban_core::Producer,
    method: Option<kaban_core::Method>,
    score: Option<f64>,
    source_id: Option<String>,
    offset: Option<u32>,
    len: Option<u32>,
}

impl Scored {
    fn take(props: &mut serde_json::Value, batch: kaban_core::Producer) -> Self {
        let Some(map) = props.as_object_mut() else {
            return Self {
                producer: batch,
                method: None,
                score: None,
                source_id: None,
                offset: None,
                len: None,
            };
        };
        let named = map
            .remove("producer")
            .and_then(|v| v.as_str().and_then(kaban_core::Producer::parse));
        let int = |key: &str| {
            map.get(key)
                .and_then(serde_json::Value::as_u64)
                .and_then(|v| u32::try_from(v).ok())
        };
        Self {
            producer: named.unwrap_or(batch),
            method: map
                .get("method")
                .and_then(serde_json::Value::as_str)
                .and_then(kaban_core::Method::parse),
            score: int("score").map(|raw| f64::from(raw) / 10_000.0),
            source_id: map
                .get("source_id")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
            offset: int("offset"),
            len: int("len"),
        }
    }
}
