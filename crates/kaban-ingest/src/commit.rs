//! The single committer (docs-ingest-plan.md DI0.3; core-spec F16, N4, N5): drains the bounded
//! channel of [`Prepared`] documents, reorders them by `seq` and applies one
//! `GraphStore::apply_batch` transaction per document in ascending path order, on a blocking
//! thread. A gap in the sequence (a document whose worker was cancelled) ends the drain, so the
//! committed set is always a gap-free prefix of the sorted walk (D-24).

use crate::graph::{ExistingDocument, ingest_batch, reconcile_failed_batch};
use crate::ids::PIPELINE_VERSION;
use crate::model::{DocStatus, IngestError, SourceId};
use kaban_core::{
    Actor, AuditEntry, BatchReport, GraphStore, NewNode, NodeId, Op, OutcomeKind, WorkspaceId,
    WriteBatch, WriteCtx,
};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::{OwnedSemaphorePermit, mpsc};

/// Output of a worker: everything the committer needs, with ids already assigned (PR-7).
/// `G` is the assembled graph (`PreparedGraph` from DI1.5); DI0 uses the `()` default (D-22).
#[derive(Debug, Clone)]
pub struct Prepared<G = ()> {
    /// Commit sequence number (dense over the items sent to the committer, in sorted walk
    /// order, RD-3); the committer applies batches in this order.
    pub seq: u64,
    /// Source identity; `None` for a document whose path was rejected (no `SourceId` exists).
    pub source: Option<SourceId>,
    /// The Document node id (`document_id(source)`).
    pub document_id: NodeId,
    /// Outcome recorded on the Document node.
    pub status: DocStatus,
    /// The assembled graph, `None` for blocked, failed and skipped documents.
    pub graph: Option<G>,
    /// The ops to apply in one transaction. Empty ops mean "unchanged, skip": the committer
    /// writes nothing and counts the document as skipped.
    pub batch: WriteBatch,
    /// Audit rows to append in the same transaction.
    pub audit: Vec<AuditEntry>,
    /// Why the preparer wrote nothing, when it did not (`batch.ops` is then empty).
    pub skip: Option<SkipReason>,
}

/// Why a prepared document carries no ops (DI1.8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// The skip key matched the stored Document (D5).
    Unchanged,
    /// `forget --document` marked it and the run did not name the file or opt in (RD2-5).
    Forgotten,
}

impl Prepared {
    /// A `Document{status: failed, error}` write for a document whose extraction failed or
    /// timed out (docs-ingest-spec D8). No `content_hash`, so the next run retries it. The
    /// error string is one of this crate's own messages, never extractor output (DI1.4 gates
    /// extractor messages before they reach here).
    pub fn failed(
        seq: u64,
        workspace: &WorkspaceId,
        source: Option<&SourceId>,
        document_id: &NodeId,
        error: &str,
    ) -> Self {
        let node = NewNode {
            kind: "Document".into(),
            text: String::new(),
            props: Some(serde_json::json!({
                "source_id": source.map(SourceId::as_str),
                "status": DocStatus::Failed,
                "error": error,
                "pipeline_version": PIPELINE_VERSION,
                "owner": kaban_core::INGEST_OWNER,
            })),
            ..NewNode::default()
        };
        Self {
            seq,
            source: source.cloned(),
            document_id: document_id.clone(),
            status: DocStatus::Failed,
            graph: None,
            batch: ingest_batch(
                workspace,
                vec![Op::UpsertNode {
                    id: document_id.clone(),
                    node,
                }],
            ),
            audit: Vec::new(),
            skip: None,
        }
    }
}

/// What the committer did (folded into the `IngestReport` by the ingestor).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct CommitStats {
    pub(crate) documents: u32,
    pub(crate) partial: u32,
    pub(crate) failed: u32,
    pub(crate) blocked: u32,
    pub(crate) skipped: u32,
    /// Forgotten documents a directory or workspace walk left alone (DI1.8, RD2-5).
    pub(crate) skipped_forgotten: u32,
    pub(crate) outbox_rows: u64,
    /// Chunk rows the store inserted or updated (typed commit outcomes, DI1.8 RD2-8; a dry run
    /// projects from the submitted ops instead).
    pub(crate) chunks_upserted: u32,
    /// Chunk rows whose content hash matched (nothing written).
    pub(crate) chunks_unchanged: u32,
    /// Chunk rows the store moved to `tombstoned`.
    pub(crate) chunks_tombstoned: u32,
    /// Section rows the store moved to `tombstoned`.
    pub(crate) sections_tombstoned: u32,
    /// Chunks stored as keyed stubs, from the Document props of applied batches (D6).
    pub(crate) blocked_chunks: u32,
    /// Denylist rule id -> hits, folded from the Document props of applied batches (D6).
    pub(crate) rule_ids: BTreeMap<String, u32>,
    /// `seq` of the first document that never arrived (the drain stopped there), if any.
    pub(crate) gap_at: Option<u64>,
}

impl CommitStats {
    /// Fold what the Document op of a batch reports (`blocked_chunks`, `rule_ids`).
    fn fold_document_props(&mut self, batch: &WriteBatch) {
        let doc_props = batch.ops.iter().find_map(|op| match op {
            Op::UpsertNode { node, .. } if node.kind == "Document" => node.props.as_ref(),
            _ => None,
        });
        let Some(props) = doc_props else { return };
        self.blocked_chunks += props
            .get("blocked_chunks")
            .and_then(serde_json::Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
            .unwrap_or(0);
        if let Some(rules) = props.get("rule_ids").and_then(serde_json::Value::as_object) {
            for (rule, n) in rules {
                let n = n.as_u64().and_then(|n| u32::try_from(n).ok()).unwrap_or(0);
                *self.rule_ids.entry(rule.clone()).or_default() += n;
            }
        }
    }

    /// Fold the store's typed outcomes of one applied batch.
    fn fold_outcomes(&mut self, report: &BatchReport) {
        for o in &report.outcomes {
            match (o.node_kind.as_str(), o.outcome) {
                ("Chunk", OutcomeKind::Inserted | OutcomeKind::Updated) => {
                    self.chunks_upserted += 1;
                }
                ("Chunk", OutcomeKind::Unchanged) => self.chunks_unchanged += 1,
                ("Chunk", OutcomeKind::Tombstoned) => self.chunks_tombstoned += 1,
                ("Section", OutcomeKind::Tombstoned) => self.sections_tombstoned += 1,
                _ => {}
            }
        }
    }

    /// A dry run has no outcomes: project from the submitted ops (every Chunk upsert counts as
    /// written, every Tombstone as a chunk, which the dry run cannot tell apart).
    fn project_ops(&mut self, batch: &WriteBatch) {
        for op in &batch.ops {
            match op {
                Op::UpsertNode { node, .. } if node.kind == "Chunk" => self.chunks_upserted += 1,
                Op::Tombstone { .. } => self.chunks_tombstoned += 1,
                Op::UpsertNode { .. } | Op::UpsertEdge { .. } | Op::Reactivate { .. } => {}
            }
        }
    }
}

/// Instrumentation shared with the dispatcher: `Prepared` items in flight (prepared, not yet
/// received) and the channel occupancy sampled by the receiver (bounded by `queue_depth`), plus
/// the dispatched-but-uncommitted count (processing, queued and pending together: the F16
/// memory bound, `<= queue_depth`, RD-4). The in-flight counter is incremented by the sender
/// before `send` and decremented by the receiver after `recv`, so it never underflows;
/// occupancy comes from `Receiver::len` on the receiving side only; the uncommitted counter is
/// incremented at dispatch and decremented when the item's [`WindowSlot`] drops.
#[derive(Debug, Default)]
pub(crate) struct QueueGauge {
    in_flight: AtomicUsize,
    pub(crate) max_in_flight: AtomicUsize,
    pub(crate) max_queued: AtomicUsize,
    uncommitted: AtomicUsize,
    pub(crate) max_uncommitted: AtomicUsize,
}

/// One place in the reorder window (RD-4): a permit of the `queue_depth` window semaphore,
/// taken at dispatch, carried with the document through preparation, the channel and the
/// committer's pending map, dropped when the document is committed or discarded. Dropping it
/// releases the window place (the permit) and the gauge, so dispatched-but-uncommitted
/// documents never exceed `queue_depth`.
#[derive(Debug)]
pub(crate) struct WindowSlot {
    _permit: OwnedSemaphorePermit,
    gauge: Arc<QueueGauge>,
}

impl WindowSlot {
    /// Wrap an acquired window permit and count it.
    pub(crate) fn new(permit: OwnedSemaphorePermit, gauge: Arc<QueueGauge>) -> Self {
        let now = gauge.uncommitted.fetch_add(1, Ordering::SeqCst) + 1;
        gauge.max_uncommitted.fetch_max(now, Ordering::SeqCst);
        Self {
            _permit: permit,
            gauge,
        }
    }
}

impl Drop for WindowSlot {
    fn drop(&mut self) {
        self.gauge.uncommitted.fetch_sub(1, Ordering::SeqCst);
    }
}

/// What travels on the channel: the prepared document and its window place.
#[derive(Debug)]
pub(crate) struct Dispatched {
    pub(crate) prepared: Prepared,
    pub(crate) slot: WindowSlot,
}

impl QueueGauge {
    /// A prepared item exists and is about to be sent.
    pub(crate) fn prepared(&self) {
        let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_in_flight.fetch_max(now, Ordering::SeqCst);
    }
    /// A send failed (the committer is gone): the item is dropped.
    pub(crate) fn dropped(&self) {
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
    /// The receiver took one item while `still_queued` remain in the channel.
    fn received(&self, still_queued: usize) {
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
        self.max_queued
            .fetch_max(still_queued + 1, Ordering::SeqCst);
    }
}

/// Drain `rx` until it closes or a sequence gap cannot be filled, committing in `seq` order.
/// Each `apply_batch` runs on `spawn_blocking` (N4). `dry_run` counts without writing.
///
/// # Errors
/// The first store error aborts the committer (and, through the closed channel, the run).
pub(crate) async fn run(
    store: Arc<dyn GraphStore>,
    mut rx: mpsc::Receiver<Dispatched>,
    gauge: Arc<QueueGauge>,
    workspace: WorkspaceId,
    policy_version: String,
    dry_run: bool,
) -> Result<CommitStats, IngestError> {
    let mut stats = CommitStats::default();
    let mut pending: BTreeMap<u64, Dispatched> = BTreeMap::new();
    let mut next: u64 = 0;
    while let Some(item) = rx.recv().await {
        gauge.received(rx.len());
        pending.insert(item.prepared.seq, item);
        while let Some(Dispatched { prepared, slot }) = pending.remove(&next) {
            commit_one(
                &store,
                prepared,
                &workspace,
                &policy_version,
                dry_run,
                &mut stats,
            )
            .await?;
            drop(slot); // committed: the window place is free (RD-4)
            next += 1;
        }
    }
    if !pending.is_empty() {
        // the channel closed with `next` missing: a cancelled worker never sent it; everything
        // after the gap is dropped so the committed set stays a gap-free prefix (D-24)
        stats.gap_at = Some(next);
        tracing::debug!(
            gap_at = next,
            dropped = pending.len(),
            "committer stopped at a gap"
        );
    }
    Ok(stats)
}

async fn commit_one(
    store: &Arc<dyn GraphStore>,
    mut item: Prepared,
    workspace: &WorkspaceId,
    policy_version: &str,
    dry_run: bool,
    stats: &mut CommitStats,
) -> Result<(), IngestError> {
    if item.batch.ops.is_empty() {
        match item.skip {
            Some(SkipReason::Forgotten) => stats.skipped_forgotten += 1,
            Some(SkipReason::Unchanged) | None => stats.skipped += 1,
        }
        return Ok(());
    }
    let document_status = item.status;
    let fold_status = |stats: &mut CommitStats| match document_status {
        DocStatus::Ok => stats.documents += 1,
        DocStatus::Partial => {
            stats.documents += 1;
            stats.partial += 1;
        }
        DocStatus::Failed => stats.failed += 1,
        DocStatus::Blocked => stats.blocked += 1,
        DocStatus::Missing => {}
    };
    if dry_run {
        fold_status(stats);
        stats.fold_document_props(&item.batch);
        stats.project_ops(&item.batch);
        return Ok(());
    }
    // Stage metadata counts, but don't report them as applied until the optimistic
    // source/generation guard has admitted this batch.
    let mut document_counts = CommitStats::default();
    document_counts.fold_document_props(&item.batch);
    let mut ctx = WriteCtx::new(policy_version, workspace.clone(), Actor::Agent);
    ctx.audit = std::mem::take(&mut item.audit);
    if item.status == DocStatus::Blocked
        || ctx.audit.iter().any(|entry| {
            matches!(
                entry.op,
                kaban_core::AuditOp::Reject | kaban_core::AuditOp::Redact
            )
        })
    {
        ctx.removal_intent = kaban_core::RemovalIntent::Policy;
    }
    let store = Arc::clone(store);
    let report = tokio::task::spawn_blocking(move || {
        // RD2-7: a failure batch reconciles against the stored child inventory here, at the
        // serialized commit boundary, so the children a failed refresh can no longer describe
        // are tombstoned and the Document keeps the lists for the next success or forget
        let mut batch = if item.status == DocStatus::Failed
            && let Some(stored) = store.get(&item.document_id, true)?
        {
            reconcile_failed_batch(item.batch, &ExistingDocument::from_node(&stored))
        } else {
            item.batch
        };
        for set in &mut batch.gazetteer_claim_sets {
            if set.source_rev.is_some() || set.expected_generation != 0 {
                continue;
            }
            let Some(observed) = store.get(&set.document, true)? else {
                continue;
            };
            let props = &observed.props;
            // An earlier document can create this exact fresh link placeholder after
            // parallel preparation. Bind its revision, never an existing source's;
            // apply_batch still checks this revision and generation transactionally.
            if observed.id == set.document.as_str()
                && observed.workspace_id == batch.workspace_id.as_str()
                && observed.kind == "Document"
                && observed.lifecycle == kaban_core::Lifecycle::Active
                && observed.rev == 1
                && props.get("owner").and_then(serde_json::Value::as_str)
                    == Some(kaban_core::INGEST_OWNER)
                && props.get("source_id").and_then(serde_json::Value::as_str)
                    == Some(set.source_id.as_str())
                && props["status"] == "missing"
                && props["version"] == 0
                && props["size"] == 0
                && props
                    .get("content_hash")
                    .is_none_or(serde_json::Value::is_null)
                && props.get("section_ids").is_none()
                && props.get("chunk_ids").is_none()
                && ["extractor", "extractor_version", "config_hash"]
                    .iter()
                    .all(|key| props[*key] == "")
            {
                set.source_rev = Some(kaban_core::SourceRev::from(&observed));
            }
        }
        store.apply_batch(batch, &ctx)
    })
    .await
    .map_err(|_| IngestError::Internal("the committer's store task did not complete"))??;
    if report.stale_source != 0 {
        stats.skipped += 1;
        return Ok(());
    }
    fold_status(stats);
    stats.blocked_chunks += document_counts.blocked_chunks;
    for (rule, count) in document_counts.rule_ids {
        *stats.rule_ids.entry(rule).or_default() += count;
    }
    stats.fold_outcomes(&report);
    if let (Some(first), Some(last)) = (report.outbox_first_seq, report.outbox_last_seq) {
        stats.outbox_rows += last - first + 1;
    }
    Ok(())
}
