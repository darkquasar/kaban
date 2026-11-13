//! The `apply_batch` transaction (`apply_batch_impl`): node ops in order, the batch's claims,
//! the structural edges and the edge diff (`DeletesUnnamed`), the end-of-batch reconciliation
//! pass with its deferred lifecycle effects, and the pre-transaction `check_layer_edges`
//! refusal (EN-F9). Split out of `sqlite.rs` (OWN-79).

use crate::anchors;
use crate::edge_write::{self, EdgeWrite, now_unix};
use crate::rows::bad_enum;
use crate::sql_error::SqlResultExt;
use kaban_core::Result;
use kaban_core::{
    BatchReport, EdgeDiff, Lifecycle, Op, OutcomeKind, Owner, StoreError, WriteBatch, WriteCtx,
    now_iso,
};
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::BTreeSet;

use super::{LinkEffect, SqliteStore, Tenant};

impl SqliteStore {
    pub(super) fn jobs_reindex_impl(
        &self,
        layer: kaban_core::Layer,
        limit: u32,
        ctx: &WriteCtx,
    ) -> Result<kaban_core::RetireReport> {
        if !matches!(
            layer,
            kaban_core::Layer::Llm | kaban_core::Layer::Gliner | kaban_core::Layer::Script
        ) || limit == 0
        {
            return Err(StoreError::EdgeRequiresAssertion);
        }
        let dialect = if layer == kaban_core::Layer::Script {
            "none"
        } else {
            layer.as_str()
        };
        let mut conn = self.write();
        let mut report = kaban_core::RetireReport::default();
        loop {
            let tx = conn
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .sql()?;
            let keys = tx.prepare("SELECT artifact_key FROM derived_artifact a JOIN node n ON n.rid=a.source_rid WHERE a.workspace_id=?1 AND a.layer=?2 AND n.workspace_id=?1 AND n.kind='Document' AND json_extract(n.props,'$.owner')='kaban-ingest' ORDER BY artifact_key LIMIT ?3").sql()?.query_map(params![ctx.workspace_id.as_str(),layer.as_str(),limit],|r|r.get::<_,String>(0)).sql()?.collect::<rusqlite::Result<Vec<_>>>().sql()?;
            if keys.is_empty() {
                break;
            }
            for key in &keys {
                tx.execute("DELETE FROM derived_artifact WHERE artifact_key=?", [key])
                    .sql()?;
            }
            tx.commit().sql()?;
            report.rows += keys.len() as u64;
            report.transactions += 1;
        }
        loop {
            let tx = conn
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .sql()?;
            let jobs = tx.prepare("SELECT job_id FROM ingest_jobs WHERE workspace_id=?1 AND \"with\"=?2 ORDER BY job_id LIMIT ?3").sql()?.query_map(params![ctx.workspace_id.as_str(),dialect,limit],|r|r.get::<_,String>(0)).sql()?.collect::<rusqlite::Result<Vec<_>>>().sql()?;
            if jobs.is_empty() {
                break;
            }
            for id in &jobs {
                // Only completed attempts may be removed; unmarked exhausted counters survive reset.
                tx.execute("DELETE FROM leases WHERE name IN (SELECT json_extract(plan,'$.lease.name') FROM ingest_jobs WHERE job_id=?1 AND workspace_id=?2 AND status='finished' UNION ALL SELECT 'attempt:'||json_extract(plan,'$.lease.name') FROM ingest_jobs WHERE job_id=?1 AND workspace_id=?2 AND status='finished')",params![id,ctx.workspace_id.as_str()]).sql()?;
                tx.execute("UPDATE leases SET expires_at=unixepoch()-1 WHERE name IN (SELECT json_extract(plan,'$.lease.name') FROM ingest_jobs WHERE job_id=?1 AND workspace_id=?2)",params![id,ctx.workspace_id.as_str()]).sql()?;
                tx.execute(
                    "DELETE FROM ingest_jobs WHERE job_id=?1 AND workspace_id=?2",
                    params![id, ctx.workspace_id.as_str()],
                )
                .sql()?;
            }
            tx.commit().sql()?;
            report.rows += jobs.len() as u64;
            report.transactions += 1;
        }
        Ok(report)
    }

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
            outcome.orphans = population.orphans;
            if !outcome.skipped_cap {
                let before: i64 = tx
                    .query_row("SELECT COALESCE(max(seq),0) FROM changes", [], |r| r.get(0))
                    .sql()?;
                let named: BTreeSet<_> = current.named_edges.iter().cloned().collect();
                for (src, dst, rel, src_id, dst_id) in population.edges {
                    if !named.contains(&(src_id, dst_id, rel.clone())) {
                        // Delete only an owned remainder; never a human/foreign replacement.
                        tx.execute("DELETE FROM edge WHERE src=?1 AND dst=?2 AND rel=?3 AND json_extract(props,'$.owner')='kaban-parse'",params![src,dst,rel]).sql()?;
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
            let marker = crate::jobs::done_artifact(&tx, &current, request, ctx)?;
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
        let report = self.apply_batch_body(&tx, request.batch, ctx)?;
        if report.stale_source != 0 {
            // Drop the uncommitted transaction, including any audit/body bookkeeping.
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
        let updated = crate::jobs::record_submit(&tx, current, &report, state, ctx)?;
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
        // the audit rows (D-17) and every row the batch touches must belong to one tenant
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
        let report = self.apply_batch_body(&tx, batch, ctx)?;
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
        ctx: &WriteCtx,
    ) -> Result<BatchReport> {
        let owner = batch.edge_diff.owner();
        let stale_source = super::artifacts::stale_sources(
            tx,
            &batch,
            ctx.policy_version.as_str(),
            &ctx.governed,
        )?;
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
        let tenant = Tenant::of(ctx, ws);
        let scoring = edge_write::scoring_columns(tx)?;
        // node ops first, in order; the rids of upserted nodes drive the edge diff (MD-23)
        let mut upserted: Vec<i64> = Vec::new();
        for op in &batch.ops {
            match op {
                Op::UpsertNode { id, node } => {
                    if let Some(rid) = Self::upsert_node(
                        tx,
                        &tenant,
                        id,
                        node,
                        ctx.policy_version.as_str(),
                        &mut report,
                    )? {
                        upserted.push(rid);
                    }
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
                        anchors::refresh(tx, rid, tenant.governed)?;
                    }
                }
                Op::UpsertEdge { .. } => {}
            }
        }
        let prepared = super::artifacts::prepare_sets(tx, &mut batch)?;
        super::artifacts::publish(tx, &batch.artifacts)?;
        // then the edges of the batch, tagged with its owner (EN-E4): an edge whose stored row
        // is unchanged writes nothing (N5); every edge the batch names is kept by the diff
        // below. A producer edge names its layer in `props.layer` (moved to the `layer`
        // column) with its `method`, `score` and span, which become the scoring columns.
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
                Self::edge_rule(tx, tenant.governed, rel, &skind, &dkind)?;
                let mut props = edge_write::owned_props(props.as_ref(), owner);
                // a human's edge is never a producer's to overwrite (the write-side twin of
                // the diff rule below, OWN-83 (4)): the triple stays the human's, named
                if props["owner"] != Owner::Human.as_str()
                    && edge_write::stored_owner(tx, srid, drid, rel)?.as_deref()
                        == Some(Owner::Human.as_str())
                {
                    kept.push((srid, drid, rel.clone()));
                    continue;
                }
                let scored = if scoring {
                    Scored::take(&mut props)
                } else {
                    Scored::default()
                };
                let written = edge_write::write_edge(
                    tx,
                    &EdgeWrite {
                        src: srid,
                        dst: drid,
                        rel,
                        confidence: None,
                        props: &props,
                        layer: scored.layer,
                        method: scored.method,
                        score: scored.score,
                        source_id: scored.source_id.as_deref(),
                        offset: scored.offset,
                        len: scored.len,
                    },
                )?;
                if written.is_some()
                    && let Some(effect) = LinkEffect::of(tenant.governed, rel)
                {
                    Self::mark_target(tx, dst.as_str(), effect, &now_iso())?;
                }
                kept.push((srid, drid, rel.clone()));
            }
        }
        super::artifacts::record_sets(tx, &batch.workspace_id, &prepared, &kept)?;
        // MD-23 / EN-E4: outgoing edges of each upserted node that the batch did not name are
        // deleted (the edge delete trigger writes their changes rows); incoming edges are
        // untouched. A layer never deletes an edge it did not create: `Complete { producer }`
        // and `Owned { owner }` delete only the rows whose `props.owner` is the batch's named
        // producer (OWN-83 (4); E2.5e, RW2-3: a human's row is never a batch's to delete
        // whatever its `layer`, so `producer: human` deletes nothing, and an untagged legacy
        // row is no producer's); `None` nothing.
        upserted.sort_unstable();
        upserted.dedup();
        for rid in upserted {
            let outgoing: Vec<(i64, i64, String, String)> = {
                let mut st = tx
                    .prepare_cached("SELECT src, dst, rel, props FROM edge WHERE src=?")
                    .sql()?;
                let rows = st
                    .query_map([rid], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
                    .sql()?;
                rows.collect::<rusqlite::Result<_>>().sql()?
            };
            for (src, dst, rel, props) in outgoing {
                let unnamed = !kept.contains(&(src, dst, rel.clone()));
                if !unnamed || !batch.edge_diff.deletes(&props) {
                    continue;
                }
                tx.execute(
                    "DELETE FROM edge WHERE src=? AND dst=? AND rel=?",
                    params![src, dst, rel],
                )
                .sql()?;
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

/// Whether the diff rule deletes an unnamed outgoing edge with these stored `props` (EN-E4: a
/// layer never deletes an edge it did not create). Only a positively identified producer has
/// rows to delete, and only the rows tagged with it; a human is never that producer.
trait DeletesUnnamed {
    fn deletes(&self, props_json: &str) -> bool;
}

impl DeletesUnnamed for EdgeDiff {
    fn deletes(&self, props_json: &str) -> bool {
        match self {
            Self::None => false,
            // OWN-83 (4), after E2.5e (the wave-2 review's RW2-3): a human's row is never a
            // batch's to delete, whatever its `layer` (a pre-0004 human `link` carries `owner
            // = human` and a NULL `layer`, and the migration backfills nothing), so the human
            // producer is refused here, by the predicate; an untagged legacy row (no `owner`,
            // no `layer`) matches no producer's tag. Both spellings delete by the same tag:
            // `Complete` states the producer's complete set, `Owned` its own rows.
            Self::Complete { producer: owner } | Self::Owned { owner } => {
                *owner != Owner::Human
                    && stored_owner(props_json).is_some_and(|o| o == owner.as_str())
            }
        }
    }
}

/// The `props.owner` of a stored edge row, if it carries one.
fn stored_owner(props_json: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(props_json)
        .ok()
        .and_then(|v| v.get("owner").and_then(|o| o.as_str().map(str::to_owned)))
}

/// The scoring columns of a producer edge, read from its props (`layer` is moved out of the
/// props into its column; `method`, `score` (fixed point, `/ 10000`), `source_id`, `offset` and
/// `len` stay in the props as written). An edge without a known `layer` has none.
#[derive(Default)]
struct Scored {
    layer: Option<kaban_core::Layer>,
    method: Option<kaban_core::Method>,
    score: Option<f64>,
    source_id: Option<String>,
    offset: Option<u32>,
    len: Option<u32>,
}

impl Scored {
    fn take(props: &mut serde_json::Value) -> Self {
        let Some(map) = props.as_object_mut() else {
            return Self::default();
        };
        let Some(layer) = map
            .get("layer")
            .and_then(serde_json::Value::as_str)
            .and_then(kaban_core::Layer::parse)
        else {
            return Self::default();
        };
        map.remove("layer");
        let int = |key: &str| {
            map.get(key)
                .and_then(serde_json::Value::as_u64)
                .and_then(|v| u32::try_from(v).ok())
        };
        Self {
            layer: Some(layer),
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
