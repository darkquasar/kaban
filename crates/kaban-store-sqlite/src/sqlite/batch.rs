//! The `apply_batch` transaction (`apply_batch_impl`): node ops in order, the batch's claims,
//! the structural edges and the edge diff (`DeletesUnnamed`), the end-of-batch reconciliation
//! pass with its deferred lifecycle effects, and the pre-transaction `check_layer_edges`
//! refusal (EN-F9). Split out of `sqlite.rs` (OWN-79).

use crate::anchors;
use crate::audit_table;
use crate::candidates;
use crate::reconcile::{self, persist, project};
use crate::rows::bad_enum;
use crate::sql_error::SqlResultExt;
use kaban_core::Result;
use kaban_core::{
    BatchReport, EdgeDiff, Lifecycle, NodeId, Op, OutcomeKind, Owner, StoreError, WriteBatch,
    WriteCtx, now_iso,
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
        let mut after = 0_i64;
        let mut audited = false;
        loop {
            let tx = conn
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .sql()?;
            let ids = tx.prepare("SELECT a.id FROM assertion a WHERE a.workspace_id=?1 AND a.layer=?2 AND a.method='ingest_submit' AND a.claim_class='edge' AND a.subject_kind='edge' AND json_extract(a.value,'$.owner')='kaban-parse' AND a.id>?3 AND a.rank<>'deprecated' AND EXISTS(SELECT 1 FROM node n WHERE n.workspace_id=?1 AND n.kind='Document' AND json_extract(n.props,'$.owner')='kaban-ingest' AND json_extract(n.props,'$.source_id')=a.source_id) ORDER BY a.id LIMIT ?4").sql()?.query_map(params![ctx.workspace_id.as_str(),layer.as_str(),after,limit],|r|r.get::<_,i64>(0)).sql()?.collect::<rusqlite::Result<Vec<_>>>().sql()?;
            if ids.is_empty() {
                break;
            }
            let mut groups = BTreeSet::new();
            let mut excluded = BTreeSet::new();
            for id in ids {
                after = id;
                if let Some(claim) = persist::load_claim(&tx, id)? {
                    tx.execute("UPDATE assertion SET rank='deprecated',rank_reason='producer-set:reindex' WHERE id=?",[id]).sql()?;
                    persist::set_status(
                        &tx,
                        id,
                        kaban_core::AssertionStatus::Rejected,
                        persist::now_unix(),
                    )?;
                    groups.insert(kaban_core::group_key(&claim));
                    excluded.insert(id);
                    report.rows += 1;
                }
            }
            reconcile::decide_groups_without(
                &tx,
                &self.reconcile,
                &groups,
                &excluded,
                persist::now_unix(),
                None,
            )?;
            if !audited {
                audit_table::append(&tx, self.signer.as_ref(), &ctx.audit)?;
                audited = true;
            }
            tx.commit().sql()?;
            report.transactions += 1;
        }
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
            if !audited {
                audit_table::append(&tx, self.signer.as_ref(), &ctx.audit)?;
                audited = true;
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
            if !audited {
                audit_table::append(&tx, self.signer.as_ref(), &ctx.audit)?;
                audited = true;
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
        let now = persist::now_unix();
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
                let mut groups = BTreeSet::new();
                let mut excluded = BTreeSet::new();
                for id in population.claims {
                    let Some(claim) = persist::load_claim(&tx, id)? else {
                        continue;
                    };
                    let kaban_core::Subject::Edge { src, dst, rel } = &claim.subject else {
                        continue;
                    };
                    let src_id: String = tx
                        .query_row("SELECT id FROM node WHERE rid=?", [src], |r| r.get(0))
                        .sql()?;
                    let dst_id: String = tx
                        .query_row("SELECT id FROM node WHERE rid=?", [dst], |r| r.get(0))
                        .sql()?;
                    // A human-deprecated claim keeps its exact rank/reason/provenance.
                    if named.contains(&(src_id, dst_id, rel.clone()))
                        || claim.rank == kaban_core::Rank::Deprecated
                        || crate::pruning_protection::claim(&tx, ctx.workspace_id.as_str(), id)?
                    {
                        continue;
                    }
                    tx.execute("UPDATE assertion SET rank='deprecated',rank_reason='producer-set:finish' WHERE id=?",[id]).sql()?;
                    persist::set_status(&tx, id, kaban_core::AssertionStatus::Rejected, now)?;
                    groups.insert(kaban_core::group_key(&claim));
                    excluded.insert(id);
                    outcome.retired_assertions += 1;
                }
                reconcile::decide_groups_without(
                    &tx,
                    &self.reconcile,
                    &groups,
                    &excluded,
                    now,
                    None,
                )?;
                for (src, dst, rel, src_id, dst_id) in population.edges {
                    if !named.contains(&(src_id, dst_id, rel.clone()))
                        && !crate::pruning_protection::edge(
                            &tx,
                            ctx.workspace_id.as_str(),
                            src,
                            dst,
                            &rel,
                        )?
                    {
                        // Claim-backed rows have already been reprojected. Delete only an
                        // owned structural remainder; never a human/foreign replacement.
                        tx.execute("DELETE FROM edge WHERE src=?1 AND dst=?2 AND rel=?3 AND layer IS NULL AND json_extract(props,'$.owner')='kaban-parse'",params![src,dst,rel]).sql()?;
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
        audit_table::append(&tx, self.signer.as_ref(), &state.audit)?;
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
        Self::check_layer_edges(&request.batch)?;
        Self::check_about_owner(&request.batch)?;
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
        if current.expires_at <= persist::now_unix() {
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
        let before: i64 = tx
            .query_row("SELECT COALESCE(max(seq),0) FROM changes", [], |r| r.get(0))
            .sql()?;
        let restored = crate::jobs::restore_reindexed(&tx, &current, &request.batch.assertions)?;
        let mut report = self.apply_batch_body(&tx, request.batch, ctx)?;
        if restored {
            report.outbox_first_seq = Some(
                u64::try_from(before + 1)
                    .map_err(|_| StoreError::InvalidName("job".into(), "invalid_sequence"))?,
            );
            let after: i64 = tx
                .query_row("SELECT COALESCE(max(seq),0) FROM changes", [], |r| r.get(0))
                .sql()?;
            report.outbox_last_seq = Some(
                u64::try_from(after)
                    .map_err(|_| StoreError::InvalidName("job".into(), "invalid_sequence"))?,
            );
        }
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
        Self::check_layer_edges(&batch)?;
        Self::check_about_owner(&batch)?;
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
        crate::pruning_protection::reconcile_retained_children(tx, &mut batch, ctx)?;
        audit_table::append(tx, self.signer.as_ref(), &ctx.audit)?;
        let before: i64 = tx
            .query_row("SELECT COALESCE(max(seq), 0) FROM changes", [], |r| {
                r.get(0)
            })
            .sql()?;
        let mut report = BatchReport::default();
        let workspace_id = batch.workspace_id.clone();
        let ws = workspace_id.as_str();
        let tenant = Tenant::of(ctx, ws);
        let engine = reconcile::engine_tables(tx)?;
        let now_unix = persist::now_unix();
        // the conflict groups this batch touches, decided at its end (EN-E9, EN-F12), and
        // the claim rows a tombstone in it retired (decided without them, RE4-1)
        let mut touched: BTreeSet<String> = BTreeSet::new();
        let mut retired: BTreeSet<i64> = BTreeSet::new();
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
                    if ctx.removal_intent == kaban_core::RemovalIntent::Automatic
                        && crate::pruning_protection::node(tx, ws, rid)?
                    {
                        continue;
                    }
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
                    if engine {
                        let (groups, ids) = Self::hide_engine_rows(tx, rid)?;
                        touched.extend(groups);
                        retired.extend(ids);
                    }
                    candidates::recount_stats(tx, ws, &kind)?;
                    report.record(id, &kind, OutcomeKind::Tombstoned);
                }
                Op::Reactivate { id } => {
                    if let Some(rid) = Self::reactivate_node(tx, &tenant, id, ctx, &mut report)?
                        && engine
                    {
                        // the pointer and the claims of a reactivated source come back: a
                        // claim the tombstone retired (`forgotten`) votes again once its
                        // groups are re-decided below; a policy-retired or human-deprecated
                        // one stays as it is (RE4-1)
                        anchors::refresh(tx, rid, tenant.governed)?;
                        persist::restore_forgotten_claims_of_node(tx, rid)?;
                        touched.extend(persist::groups_of_node(tx, rid)?);
                    }
                }
                Op::UpsertEdge { .. } => {}
            }
        }
        // the batch's claims (EN-F8): resolved inside the workspace (EN-S3), inserted once,
        // never updated; their groups are decided after the structural edges and the diff
        let mut kept: Vec<(i64, i64, String)> = Vec::new();
        let prepared = super::artifacts::prepare_sets(tx, &mut batch)?;
        let mut claim_keys = BTreeSet::new();
        if !batch.assertions.is_empty() {
            let claimed = reconcile::insert_batch_claims(
                tx,
                &self.reconcile,
                ws,
                tenant.governed,
                batch.assertions,
                now_unix,
                &mut report,
            )?;
            touched.extend(claimed.touched);
            kept.extend(claimed.edges);
            claim_keys.extend(claimed.keys);
        }
        super::artifacts::retire_sets(
            tx,
            &batch.workspace_id,
            &prepared,
            &claim_keys,
            &mut touched,
            now_unix,
        )?;
        super::artifacts::publish(tx, &batch.artifacts)?;
        // then the structural edges of the batch, tagged with its owner (EN-E4): an edge whose
        // stored row is unchanged writes nothing (N5); every edge the batch names (an op or an
        // `edge` claim) is kept by the diff below. A claim-backed triple is the projection's
        // to write (RE4-5, EN-F9): an op matching an `edge` claim of this batch, or a triple
        // some stored claim names, skips the structural write and its lifecycle effect runs
        // only once the pass has materialised a winner; a bare op on a protected triple (a
        // human winner, an accepted-record edge) is refused as a layer op without its claim
        // would be (D6). Producer writes on claim-free, unprotected triples stay (D7).
        let claimed_in_batch: BTreeSet<(i64, i64, String)> = kept.iter().cloned().collect();
        let mut deferred: Vec<(i64, i64, &str, &NodeId, &NodeId, LinkEffect)> = Vec::new();
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
                let in_batch = claimed_in_batch.contains(&(srid, drid, rel.clone()));
                if !in_batch && reconcile::triple_protected(tx, &self.reconcile, srid, drid, rel)? {
                    return Err(StoreError::EdgeRequiresAssertion);
                }
                if in_batch || reconcile::triple_claimed(tx, srid, drid, rel)? {
                    if let Some(effect) = LinkEffect::of(tenant.governed, rel) {
                        deferred.push((srid, drid, rel, src, dst, effect));
                    }
                    kept.push((srid, drid, rel.clone()));
                    continue;
                }
                let props = project::owned_props(props.as_ref(), owner);
                let written = project::write_edge(
                    tx,
                    &project::EdgeWrite {
                        src: srid,
                        dst: drid,
                        rel,
                        confidence: None,
                        props: &props,
                        layer: None,
                        method: None,
                        score: None,
                        source_id: None,
                        offset: None,
                        len: None,
                    },
                )?;
                if written.is_some()
                    && let Some(effect) = LinkEffect::of(tenant.governed, rel)
                {
                    Self::mark_target(
                        tx,
                        &tenant,
                        dst.as_str(),
                        effect,
                        rel,
                        src.as_str(),
                        &now_iso(),
                    )?;
                }
                kept.push((srid, drid, rel.clone()));
            }
        }
        // MD-23 / EN-E4: outgoing edges of each upserted node that the batch did not name are
        // deleted (the edge delete trigger writes their changes rows); incoming edges are
        // untouched. A layer never deletes an edge it did not create: `Complete { producer }`
        // and `Owned { owner }` delete only the rows whose `props.owner` is the batch's named
        // producer (OWN-83 (4); E2.5e, RW2-3: a human's row is never a batch's to delete
        // whatever its `layer`, so `producer: human` deletes nothing, and an untagged legacy
        // row is no producer's); `None` nothing. A claim-backed triple's row is the
        // projection's (RE4-5): the reconciler's owner tag is no producer's, so no rule
        // reaches it.
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
                if !unnamed
                    || !batch.edge_diff.deletes(&props)
                    || (engine && reconcile::triple_claimed(tx, src, dst, &rel)?)
                    || crate::pruning_protection::edge(tx, ws, src, dst, &rel)?
                {
                    continue;
                }
                tx.execute(
                    "DELETE FROM edge WHERE src=? AND dst=? AND rel=?",
                    params![src, dst, rel],
                )
                .sql()?;
            }
        }
        // the end-of-batch deterministic pass (EN-E9): every touched group, over all of its
        // stored supporters, in ascending `group_key` order; the verdicts are projected here
        if engine {
            // the rows the deferred lifecycle effects' triples hold before the pass: an
            // effect runs only for a row the pass wrote (new or changed), as the structural
            // path runs it for a written row only
            let row_of = |tx: &Connection,
                          src: i64,
                          dst: i64,
                          rel: &str|
             -> Result<Option<String>> {
                tx.query_row(
                    "SELECT id || '|' || props || '|' || COALESCE(layer, '') || '|' || COALESCE(confidence, '')
                     FROM edge WHERE src = ?1 AND dst = ?2 AND rel = ?3",
                    params![src, dst, rel],
                    |r| r.get(0),
                )
                .optional()
                .sql()
            };
            let mut rows_before = Vec::with_capacity(deferred.len());
            for (srid, drid, rel, ..) in &deferred {
                rows_before.push(row_of(tx, *srid, *drid, rel)?);
            }
            reconcile::decide_groups_without(
                tx,
                &self.reconcile,
                &touched,
                &retired,
                now_unix,
                Some(&mut report),
            )?;
            for ((srid, drid, rel, src, dst, effect), before) in
                deferred.into_iter().zip(rows_before)
            {
                let after = row_of(tx, srid, drid, rel)?;
                if after.is_some() && after != before {
                    Self::mark_target(
                        tx,
                        &tenant,
                        dst.as_str(),
                        effect,
                        rel,
                        src.as_str(),
                        &now_iso(),
                    )?;
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
    /// EN-F9 negative, judged before the transaction opens: a producer edge op that names a
    /// layer in its props is a layer's claim and must carry its `edge` assertion in the same
    /// batch; the upsert is never reached from a bare op.
    fn check_layer_edges(batch: &WriteBatch) -> Result<()> {
        for op in &batch.ops {
            let Op::UpsertEdge {
                src,
                dst,
                rel,
                props: Some(props),
            } = op
            else {
                continue;
            };
            if props.get("layer").is_none() {
                continue;
            }
            let claimed = batch.assertions.iter().any(|a| {
                a.claim_class == kaban_core::ClaimClass::Edge
                    && matches!(&a.subject, kaban_core::BatchSubject::Edge { src: s, rel: r, dst: d }
                        if s == src && r == rel && d == dst)
            });
            if !claimed {
                return Err(StoreError::EdgeRequiresAssertion);
            }
        }
        Ok(())
    }
    /// EN-F20 negative (OD-AI-5 default; E2.6b), judged before the transaction opens: `ABOUT`
    /// is a judgement and no deterministic layer writes it. A producer batch (`EdgeDiff::Owned`
    /// under any owner but [`Owner::Human`]) may say it only as an `edge` claim, which the
    /// reconciliation pass projects (EN-F12); a bare `ABOUT` op of such a batch is refused with
    /// the EN-F9 class, so the producer learns what to send. An op that names the triple of an
    /// `ABOUT` claim in the same batch is the diff's name-keeper and passes (the structural
    /// write skips it). Human batches (`link`, `remember(links)`, the M5 import, every
    /// `Complete` or `None` diff) are not judged here.
    fn check_about_owner(batch: &WriteBatch) -> Result<()> {
        if batch.edge_diff.owner() == Owner::Human {
            return Ok(());
        }
        for op in &batch.ops {
            let Op::UpsertEdge { src, dst, rel, .. } = op else {
                continue;
            };
            if rel != ABOUT_REL {
                continue;
            }
            let claimed = batch.assertions.iter().any(|a| {
                a.claim_class == kaban_core::ClaimClass::Edge
                    && matches!(&a.subject, kaban_core::BatchSubject::Edge { src: s, rel: r, dst: d }
                        if s == src && r == rel && d == dst)
            });
            if !claimed {
                return Err(StoreError::EdgeRequiresAssertion);
            }
        }
        Ok(())
    }
}

/// The judgement rel of EN-F20 (`record -> Entity|CodeRef`): human or claim-projected only.
const ABOUT_REL: &str = "ABOUT";

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
