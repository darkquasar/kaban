//! The ingestion run (docs-ingest-plan.md DI0.3; core-spec F16, N4; PR-8; dispositions D-3,
//! D-13, D-20, D-24, D-25): a bounded worker pool over a sorted walk, cooperative per-document
//! deadlines, a single committer and parent-token cancellation.
//!
//! ```text
//!   Ingestor { workers: Semaphore(workers), outstanding_timed_out, run_guard }   (RD-5)
//!   run(cfg, cancel)   [one at a time per (store, workspace) of the process: `service`]
//!   +----------------------------------------------------------------------------------+
//!   | walk (spawn_blocking) -> sorted items        window: Semaphore(queue_depth) (RD-4) |
//!   | for item:                                                     mpsc(queue_depth)   |
//!   |   walker skip        -> skipped += 1 (no seq)                                     |
//!   |   gate_path hit      -> unchanged? skipped (no seq)                               |
//!   |                         else window slot, seq++, blocked Prepared --------> tx    |
//!   |   gated candidate    -> window slot, worker permit (exhaustion rechecked, RD-8),  |
//!   |                         seq++ ==> spawn_blocking(prepare)                 |       |
//!   |                         select!{ done | deadline | cancel } -------------> tx    |
//!   |                         deadline: child.cancel(), failed:"timeout", permit stays  |
//!   |                         with the blocking task; all permits timed out => stop     |
//!   |                                                                            v       |
//!   |                       committer: reorder by the dense seq (RD-3), one apply_batch |
//!   |                       per document (N4); the window slot drops on commit          |
//!   +----------------------------------------------------------------------------------+
//! ```

use crate::cancel::{ExtractCtx, ExtractError};
use crate::commit::{self, Dispatched, Prepared, QueueGauge, WindowSlot};
use crate::hash::{
    GatedPath, Identity, SkipKey, blocked_path_document, config_hash, should_skip_blocked,
};
use crate::ids::{PIPELINE_VERSION, document_id};
use crate::model::{Format, IngestError, IngestReport, SourceId};
use crate::service::{RunLock, run_lock};
use crate::walk::{Candidate, Skip, WalkConfig, walk};
use kaban_api::KabanApi;
use kaban_core::{GraphStore, NodeId, WorkspaceId};
use kaban_policy::{AuditSigner, Policy};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::sync::{Notify, Semaphore, mpsc};
use tokio::task::JoinSet;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

/// Pool and committer configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)] // independent run switches, not a state machine
pub struct IngestConfig {
    /// Force symbol sources without the gazetteer memory cap for this run.
    pub symbols: bool,
    /// Explicit linking override; absent uses the engine's `[link] default`.
    pub link: Option<String>,
    /// Blocking extraction workers (default: `available_parallelism`).
    pub workers: usize,
    /// Reorder window and channel bound (default `2 * workers`): at most this many documents
    /// are dispatched but not yet committed at any time (RD-4), whatever their completion order.
    pub queue_depth: usize,
    /// Per-document extraction deadline (default 10 s).
    pub doc_timeout: Duration,
    /// Count and gate without writing.
    pub dry_run: bool,
    /// Keep `SUPERSEDES` history (D3; DI3.4; default off).
    pub history: bool,
}

impl Default for IngestConfig {
    fn default() -> Self {
        let workers = std::thread::available_parallelism().map_or(1, usize::from);
        Self {
            symbols: false,
            link: None,
            workers,
            queue_depth: 2 * workers,
            doc_timeout: Duration::from_secs(10),
            dry_run: false,
            history: false,
        }
    }
}

/// What a worker receives for one gated candidate.
#[derive(Debug, Clone)]
pub struct PrepareInput {
    /// Commit sequence number: dense over the items sent to the committer, in sorted walk order
    /// (RD-3); the `Prepared` must carry it back unchanged.
    pub seq: u64,
    /// Source identity (from the gated path).
    pub source: SourceId,
    /// `document_id(source)`.
    pub document_id: NodeId,
    /// The gated relative path.
    pub path: GatedPath,
    /// Absolute path to read.
    pub abs: PathBuf,
    /// File size.
    pub size: u64,
    /// Modification time (Unix seconds).
    pub mtime: i64,
    /// Format from the extension.
    pub format: Format,
    /// The bound workspace (D-25).
    pub workspace: WorkspaceId,
    /// The walked candidate: the only way to read the bytes is `candidate.open_checked()`
    /// (RD-2; DI1.5).
    pub candidate: Candidate,
    /// `config_hash` (DI0.2) of this run: a component of the D5 skip key the preparer writes
    /// on the Document and compares on the next run.
    pub config_hash: String,
    /// `document_id` of every gated candidate of this run, so a link to a document that sorts
    /// before this one needs no placeholder: the committer has applied it already (DI1.5).
    pub run_documents: Arc<BTreeSet<NodeId>>,
    /// Reactivate the document even when `forget --document` marked it forgotten (DI1.8): the
    /// run's `--include-forgotten`, or this file named as a root of the walk.
    pub include_forgotten: bool,
}

/// The per-document pipeline a worker runs on a blocking thread: read, keyed-hash, gate,
/// extract, chunk, assemble (DI1 supplies it; DI0's tests use a fake). It must call
/// `ctx.check()` between stages and return `Prepared` with every op for one transaction.
pub trait Preparer: Send + Sync {
    /// Content-free telemetry collected during run preparation.
    fn gazetteer_telemetry(&self) -> Option<kaban_api::GazetteerTelemetry> {
        None
    }
    /// Refresh run-local derived state before workers start; returns its safe skip-key identity.
    ///
    /// # Errors
    /// Store or configuration errors preparing the run.
    fn begin_run(&self, _documents: &BTreeSet<NodeId>) -> Result<String, IngestError> {
        Ok(String::new())
    }
    /// Prepare one document.
    ///
    /// # Errors
    /// `ExtractError` stops this document only; it is recorded `failed` (D8).
    fn prepare(&self, input: PrepareInput, ctx: &ExtractCtx) -> Result<Prepared, ExtractError>;
}

/// Pool instrumentation (asserted by the DI0.3 tests, reported by `index_status` later).
#[derive(Debug, Default)]
pub struct PoolMetrics {
    /// Highest channel occupancy observed by the committer (<= `queue_depth`).
    pub max_queued: usize,
    /// Highest number of prepared documents not yet received by the committer, including the
    /// ones their senders still hold.
    pub max_in_flight: usize,
    /// Highest number of documents dispatched but not yet committed (processing, queued and
    /// pending in the committer's reorder buffer together): the F16 memory bound, `<=
    /// queue_depth` (RD-4).
    pub max_uncommitted: usize,
    /// Highest number of blocking extraction tasks alive at once.
    pub max_live_blocking: usize,
    /// Blocking extraction tasks that returned (including late, timed-out ones).
    pub blocking_completed: usize,
}

/// Run-local instrumentation (RD-5): fresh for every run; a timed-out task that returns after
/// its run ended still reports into its own run's counters.
#[derive(Debug, Default)]
struct Counters {
    live_blocking: AtomicUsize,
    max_live_blocking: AtomicUsize,
    blocking_completed: AtomicUsize,
    /// Documents recorded `failed:"timeout"` in this run.
    timed_out_total: AtomicUsize,
    /// Timed-out tasks of this run still holding a permit (the run-local view of
    /// `Ingestor::outstanding_timed_out`): `IngestReport::timed_out_outstanding`.
    outstanding: AtomicUsize,
}

/// The two run-local gauges of the latest run, kept so `metrics()` can read them.
#[derive(Debug, Default)]
struct RunState {
    counters: Arc<Counters>,
    gauge: Arc<QueueGauge>,
}

/// The ingestion service: one policy instance shared with the API (D-3), the store, the audit
/// signer and the per-document pipeline. The worker limit lives here (RD-5): the semaphore and
/// the count of timed-out tasks still holding a permit span runs, so a detached non-cooperative
/// task keeps its capacity reserved until it returns, whichever run started it.
pub struct Ingestor {
    api: Arc<KabanApi>,
    store: Arc<dyn GraphStore>,
    policy: Arc<RwLock<Policy>>,
    signer: Arc<dyn AuditSigner>,
    preparer: Arc<dyn Preparer>,
    cfg: IngestConfig,
    /// `workers` permits for the whole service (RD-5, PR-8).
    workers: Arc<Semaphore>,
    /// Timed-out blocking tasks that still hold a worker permit, across runs.
    outstanding_timed_out: Arc<AtomicUsize>,
    /// Serializes runs: a second concurrent `run` waits for the first to return (RD-5). Shared
    /// by every `Ingestor` of this process over the same store handle and workspace (DI2.0 (a);
    /// [`crate::service`]), so two adapter calls cannot ingest one workspace at the same time.
    run_guard: RunLock,
    /// Instrumentation of the latest run.
    last_run: std::sync::Mutex<RunState>,
}

impl std::fmt::Debug for Ingestor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ingestor")
            .field("cfg", &self.cfg)
            .field("workspace", self.api.workspace())
            .finish_non_exhaustive()
    }
}

/// Everything a dispatched document shares with its supervisor task.
struct Shared {
    tx: mpsc::Sender<Dispatched>,
    /// The reorder window (RD-4): `queue_depth` permits over dispatched, uncommitted documents.
    window: Arc<Semaphore>,
    gauge: Arc<QueueGauge>,
    counters: Arc<Counters>,
    preparer: Arc<dyn Preparer>,
    exhausted: Arc<Notify>,
    /// Timed-out tasks still holding a permit (service-wide, RD-5).
    outstanding_timed_out: Arc<AtomicUsize>,
    workers: usize,
    doc_timeout: Duration,
}

impl Ingestor {
    /// Assemble the service; the policy is `api.policy_handle()` (D-3) and the workspace
    /// `api.workspace()` (D-25).
    pub fn new(
        api: Arc<KabanApi>,
        store: Arc<dyn GraphStore>,
        signer: Arc<dyn AuditSigner>,
        preparer: Arc<dyn Preparer>,
        cfg: IngestConfig,
    ) -> Self {
        let policy = api.policy_handle();
        let workers = Arc::new(Semaphore::new(cfg.workers.max(1)));
        let run_guard = run_lock(&store, api.workspace());
        Self {
            api,
            store,
            policy,
            signer,
            preparer,
            cfg,
            workers,
            outstanding_timed_out: Arc::default(),
            run_guard,
            last_run: std::sync::Mutex::default(),
        }
    }

    /// Instrumentation of the latest run (run-local, RD-5): zero before the first run; a task of
    /// that run returning late still counts into it.
    pub fn metrics(&self) -> PoolMetrics {
        let (counters, gauge) = {
            let state = self
                .last_run
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (Arc::clone(&state.counters), Arc::clone(&state.gauge))
        };
        PoolMetrics {
            max_queued: gauge.max_queued.load(Ordering::SeqCst),
            max_in_flight: gauge.max_in_flight.load(Ordering::SeqCst),
            max_uncommitted: gauge.max_uncommitted.load(Ordering::SeqCst),
            max_live_blocking: counters.max_live_blocking.load(Ordering::SeqCst),
            blocking_completed: counters.blocking_completed.load(Ordering::SeqCst),
        }
    }

    fn policy(&self) -> std::sync::RwLockReadGuard<'_, Policy> {
        // held briefly and never across an await (`async-no-lock-await`)
        self.policy
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Ingest every candidate under `cfg.roots` into the bound workspace.
    ///
    /// Runs over one `(store, workspace)` of this process go one at a time: a run that finds
    /// the shared lock held waits for the holder to return (observing its own `cancel` while
    /// it waits) and then walks, so it sees the holder's commits (DI2.0 (a)).
    ///
    /// Cancellation of `cancel` stops dispatch, cancels every document's child token, lets the
    /// committer finish the documents already prepared up to the first gap and returns
    /// `Ok(IngestReport { cancelled: true, .. })`; a token cancelled before the walk starts,
    /// or while waiting for the lock, is `Err(Cancelled)` (D-20). When every worker permit is held by a timed-out, non-cooperative
    /// extractor, dispatch stops and the run returns `WorkersExhausted` with the partial report
    /// (D-13).
    ///
    /// # Errors
    /// `Cancelled`, `WorkersExhausted`, `Config`, store errors from the committer.
    pub async fn run(
        &self,
        cfg: &WalkConfig,
        cancel: CancellationToken,
    ) -> Result<IngestReport, IngestError> {
        if cancel.is_cancelled() {
            return Err(IngestError::Cancelled);
        }
        // one run at a time per (store, workspace) of the process (RD-5, DI2.0 (a)); this async
        // mutex is the serialization itself, held for the whole run on purpose (not a std lock
        // across an await), and the wait for it is cancellable
        let _run_guard = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(IngestError::Cancelled),
            guard = self.run_guard.lock() => guard,
        };
        if cancel.is_cancelled() {
            return Err(IngestError::Cancelled);
        }
        let started = std::time::Instant::now();
        let counters: Arc<Counters> = Arc::default();
        let gauge: Arc<QueueGauge> = Arc::default();
        *self
            .last_run
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = RunState {
            counters: Arc::clone(&counters),
            gauge: Arc::clone(&gauge),
        };
        let ws = self.api.workspace().clone();
        let (policy_version, config_digest) = {
            let p = self.policy();
            (p.effective_hash().to_owned(), config_hash(cfg, &p))
        };
        let walk_cfg = cfg.clone();
        let items =
            tokio::task::spawn_blocking(move || walk(&walk_cfg).map(Iterator::collect::<Vec<_>>))
                .await
                .map_err(|_| IngestError::Internal("the walk task did not complete"))??;

        let run_documents: Arc<BTreeSet<NodeId>> = {
            let policy = self.policy();
            Arc::new(
                items
                    .iter()
                    .filter_map(|item| item.as_ref().ok())
                    .filter_map(|c| c.gate(&policy).ok())
                    .map(|gated| document_id(&SourceId::new(&ws, &gated)))
                    .collect(),
            )
        };
        let run_builder = Arc::clone(&self.preparer);
        let documents = Arc::clone(&run_documents);
        let linking_digest = tokio::task::spawn_blocking(move || run_builder.begin_run(&documents))
            .await
            .map_err(|_| IngestError::Internal("gazetteer preparation did not complete"))??;
        let config_digest = if linking_digest.is_empty() {
            config_digest
        } else {
            blake3::hash(format!("{config_digest}:{linking_digest}").as_bytes())
                .to_hex()
                .to_string()
        };
        let mut report = IngestReport {
            gazetteer: self.preparer.gazetteer_telemetry(),
            ..IngestReport::default()
        };
        let workers = self.cfg.workers.max(1);
        let (tx, rx) = mpsc::channel::<Dispatched>(self.cfg.queue_depth.max(1));
        // the committer takes the batch role only (MT-9): `dyn GraphStore` upcasts to it
        let committer = tokio::spawn(commit::run(
            Arc::clone(&self.store),
            rx,
            Arc::clone(&gauge),
            ws.clone(),
            policy_version.clone(),
            self.cfg.dry_run,
        ));
        let semaphore = Arc::clone(&self.workers);
        let shared = Arc::new(Shared {
            tx,
            window: Arc::new(Semaphore::new(self.cfg.queue_depth.max(1))),
            gauge: Arc::clone(&gauge),
            counters: Arc::clone(&counters),
            preparer: Arc::clone(&self.preparer),
            exhausted: Arc::new(Notify::new()),
            outstanding_timed_out: Arc::clone(&self.outstanding_timed_out),
            workers,
            doc_timeout: self.cfg.doc_timeout,
        });
        let mut supervisors: JoinSet<()> = JoinSet::new();
        let mut exhausted = false;
        // the commit sequence is dense (RD-3): a number is taken only by an item that is sent to
        // the committer, so a walker skip or an unchanged blocked document leaves no gap
        let mut next_seq: u64 = 0;

        'dispatch: for item in items {
            if cancel.is_cancelled() {
                report.cancelled = true;
                break;
            }
            let candidate = match item {
                Err(_skip) => {
                    report.skipped += 1;
                    continue;
                }
                Ok(c) => c,
            };
            let gated = {
                let policy = self.policy();
                candidate.gate(&policy)
            };
            let gated = match gated {
                Ok(g) => g,
                Err(Skip::PathRejected { rule_ids }) => {
                    let keyed = candidate.keyed_path_id(self.signer.as_ref(), &ws);
                    let key = SkipKey {
                        identity: Identity::Keyed(keyed.clone()),
                        pipeline_version: PIPELINE_VERSION,
                        extractor_version: String::new(),
                        config_hash: config_digest.clone(),
                        position_fingerprint: None,
                    };
                    let store = Arc::clone(&self.store);
                    let lookup_key = key.clone();
                    let lookup_keyed = keyed.clone();
                    let skip = tokio::task::spawn_blocking(move || {
                        should_skip_blocked(store.as_ref(), &lookup_keyed, &lookup_key)
                    })
                    .await
                    .map_err(|_| IngestError::Internal("the skip lookup did not complete"))??;
                    // the rule ids of a committed blocked Document are folded by the
                    // committer from its props (DI1.8, RD2-8); a skipped one is counted here
                    if skip {
                        for r in &rule_ids {
                            *report.rule_ids.entry(r.clone()).or_default() += 1;
                        }
                        report.skipped += 1;
                        continue;
                    }
                    let Some(slot) = acquire_window(&shared, &cancel).await else {
                        report.cancelled = true;
                        break;
                    };
                    let blocked =
                        blocked_path_document(&ws, &keyed, &rule_ids, &key, &policy_version);
                    let seq = next_seq;
                    next_seq += 1;
                    let prepared = Prepared {
                        seq,
                        source: None,
                        document_id: blocked.id,
                        status: crate::model::DocStatus::Blocked,
                        graph: None,
                        batch: blocked.batch,
                        audit: vec![blocked.audit],
                        skip: None,
                    };
                    if send(&shared, prepared, slot).await.is_err() {
                        break; // the committer is gone: its error surfaces below
                    }
                    continue;
                }
                Err(_other) => {
                    report.skipped += 1;
                    continue;
                }
            };

            // a place in the reorder window first (RD-4), then a worker permit, unless the run
            // is cancelled or every worker permit is held by a timed-out task
            let Some(slot) = acquire_window(&shared, &cancel).await else {
                report.cancelled = true;
                break;
            };
            let permit = loop {
                tokio::select! {
                    biased;
                    () = cancel.cancelled() => {
                        report.cancelled = true;
                        break 'dispatch;
                    }
                    () = shared.exhausted.notified() => {
                        // RD-8: the notification may be stale. A timed-out task can return
                        // (and give its permit back) between notifying and this wake-up, so
                        // exhaustion is declared only while every permit is still held by a
                        // timed-out task; otherwise acquisition simply continues.
                        if shared.outstanding_timed_out.load(Ordering::SeqCst) >= workers
                            && semaphore.available_permits() == 0
                        {
                            exhausted = true;
                            break 'dispatch;
                        }
                    }
                    permit = Arc::clone(&semaphore).acquire_owned() => match permit {
                        Ok(p) => break p,
                        Err(_) => break 'dispatch, // the semaphore is never closed
                    },
                }
            };
            let source = SourceId::new(&ws, &gated);
            let seq = next_seq;
            next_seq += 1;
            let include_forgotten = cfg.include_forgotten || cfg.roots.contains(&candidate.abs);
            let input = PrepareInput {
                seq,
                document_id: document_id(&source),
                source,
                path: gated,
                include_forgotten,
                abs: candidate.abs.clone(),
                size: candidate.size,
                mtime: candidate.mtime,
                format: candidate.format,
                workspace: ws.clone(),
                config_hash: config_digest.clone(),
                run_documents: Arc::clone(&run_documents),
                candidate,
            };
            supervisors.spawn(supervise(
                Arc::clone(&shared),
                permit,
                slot,
                input,
                cancel.child_token(),
            ));
        }

        // settle: every supervisor returns promptly (a timed-out blocking task is left to end
        // on its own), then the channel closes and the committer drains to the first gap
        while let Some(joined) = supervisors.join_next().await {
            if joined.is_err() {
                return Err(IngestError::Internal(
                    "a worker supervisor did not complete",
                ));
            }
        }
        let outstanding = self.outstanding_timed_out.load(Ordering::SeqCst);
        drop(shared);
        let stats = committer
            .await
            .map_err(|_| IngestError::Internal("the committer did not complete"))??;
        report.documents = stats.documents;
        report.partial = stats.partial;
        report.failed = stats.failed;
        report.blocked = stats.blocked;
        report.skipped += stats.skipped;
        report.skipped_forgotten = stats.skipped_forgotten;
        report.outbox_rows = stats.outbox_rows;
        report.chunks_upserted = stats.chunks_upserted;
        report.chunks_unchanged = stats.chunks_unchanged;
        report.chunks_tombstoned = stats.chunks_tombstoned;
        report.sections_tombstoned = stats.sections_tombstoned;
        report.blocked_chunks = stats.blocked_chunks;
        for (rule, n) in stats.rule_ids {
            *report.rule_ids.entry(rule).or_default() += n;
        }
        report.timed_out =
            u32::try_from(counters.timed_out_total.load(Ordering::SeqCst)).unwrap_or(u32::MAX);
        report.timed_out_outstanding =
            u32::try_from(counters.outstanding.load(Ordering::SeqCst)).unwrap_or(u32::MAX);
        report.elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        if cancel.is_cancelled() {
            report.cancelled = true;
        }
        if report.timed_out_outstanding > 0 {
            // the count only: no path, no document identity (S1); the adapter boundary leaves
            // these threads behind (`run_blocking`, RD3-3)
            let count = report.timed_out_outstanding;
            tracing::warn!(count, "parser threads abandoned at the document timeout");
        }
        tracing::info!(
            documents = report.documents,
            skipped = report.skipped,
            failed = report.failed,
            blocked = report.blocked,
            timed_out = report.timed_out,
            cancelled = report.cancelled,
            "ingestion run finished"
        );
        if exhausted {
            return Err(IngestError::WorkersExhausted {
                outstanding,
                partial: Box::new(report),
            });
        }
        if !self.cfg.dry_run
            && let Some(telemetry) = &report.gazetteer
        {
            let raw = serde_json::to_string(telemetry)
                .map_err(|_| IngestError::Config("gazetteer telemetry failed".into()))?;
            self.store
                .set_index_meta(&format!("gazetteer_last_run:{ws}"), &raw)?;
            let at = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |duration| duration.as_secs());
            let at = i64::try_from(at).unwrap_or(i64::MAX);
            let outcome = if telemetry.outcome == "skipped_budget" {
                kaban_api::layers::LayerOutcome::SkippedBudget
            } else {
                kaban_api::layers::LayerOutcome::Ok
            };
            let run = kaban_api::layers::LayerRun { at, outcome };
            let raw = serde_json::to_string(&run)
                .map_err(|_| IngestError::Config("gazetteer telemetry failed".into()))?;
            self.store.set_index_meta("layer_run:gazetteer", &raw)?;
        }
        Ok(report)
    }
}

/// Take a place in the reorder window (RD-4), or `None` when the run is cancelled first.
async fn acquire_window(shared: &Shared, cancel: &CancellationToken) -> Option<WindowSlot> {
    tokio::select! {
        biased;
        () = cancel.cancelled() => None,
        permit = Arc::clone(&shared.window).acquire_owned() => permit
            .ok() // the window semaphore is never closed
            .map(|p| WindowSlot::new(p, Arc::clone(&shared.gauge))),
    }
}

/// Send one prepared document and its window place to the committer, tracking the queue gauge.
async fn send(shared: &Shared, prepared: Prepared, slot: WindowSlot) -> Result<(), ()> {
    shared.gauge.prepared();
    if shared.tx.send(Dispatched { prepared, slot }).await.is_err() {
        shared.gauge.dropped();
        return Err(());
    }
    Ok(())
}

/// Supervise one document: run the preparer on a blocking thread with the permit moved into
/// the closure (released only when the closure returns, PR-8), race it against the deadline
/// and the run's cancellation, and send the outcome to the committer.
async fn supervise(
    shared: Arc<Shared>,
    permit: tokio::sync::OwnedSemaphorePermit,
    slot: WindowSlot,
    input: PrepareInput,
    child: CancellationToken,
) {
    let deadline = Instant::now() + shared.doc_timeout;
    let ctx = ExtractCtx::new(child.clone(), deadline, input.source.clone());
    let seq = input.seq;
    let workspace = input.workspace.clone();
    let source = Some(input.source.clone());
    let doc_id = input.document_id.clone();
    let counters = Arc::clone(&shared.counters);
    let preparer = Arc::clone(&shared.preparer);
    let mut handle = tokio::task::spawn_blocking(move || {
        // the permit lives inside the closure until it returns, cooperative or not (PR-8);
        // on a normal return it travels back to the supervisor, which holds it through the
        // send so that waiting senders, like live tasks, never exceed `workers` (F16); the
        // gauges are kept by a drop guard, so a preparer that unwinds (RD3-2) still leaves
        // the live count and is still counted as returned
        let _live = LiveTask::enter(counters);
        let result = preparer.prepare(input, &ctx);
        (permit, result)
    });
    let (held_permit, outcome) = tokio::select! {
        biased;
        () = child.cancelled() => {
            // the run was cancelled: do not wait for the extraction, send nothing (the
            // committer stops at this gap, D-24); the blocking task ends on its own and the
            // window place is released with `slot`
            drop(slot);
            return;
        }
        () = tokio::time::sleep_until(deadline) => {
            child.cancel();
            let outstanding = shared.outstanding_timed_out.fetch_add(1, Ordering::SeqCst) + 1;
            shared.counters.timed_out_total.fetch_add(1, Ordering::SeqCst);
            shared.counters.outstanding.fetch_add(1, Ordering::SeqCst);
            if outstanding >= shared.workers {
                shared.exhausted.notify_one();
            }
            let outstanding_timed_out = Arc::clone(&shared.outstanding_timed_out);
            let run_counters = Arc::clone(&shared.counters);
            tokio::spawn(async move {
                let _ = handle.await;
                outstanding_timed_out.fetch_sub(1, Ordering::SeqCst);
                run_counters.outstanding.fetch_sub(1, Ordering::SeqCst);
            });
            tracing::debug!(seq, "document extraction timed out");
            (None, Prepared::failed(seq, &workspace, source.as_ref(), &doc_id, "timeout"))
        }
        joined = &mut handle => match joined {
            Ok((permit, Ok(done))) => (Some(permit), done),
            Ok((permit, Err(ExtractError::DeadlineExceeded))) => {
                // the parser observed the deadline before the timer branch did: the same
                // timeout, recorded the same way (the permit came back, nothing outstanding)
                shared.counters.timed_out_total.fetch_add(1, Ordering::SeqCst);
                (Some(permit), Prepared::failed(seq, &workspace, source.as_ref(), &doc_id, "timeout"))
            }
            Ok((permit, Err(e))) => {
                (Some(permit), Prepared::failed(seq, &workspace, source.as_ref(), &doc_id, &e.to_string()))
            }
            Err(_) => (None, Prepared::failed(seq, &workspace, source.as_ref(), &doc_id, "extractor panicked")),
        },
    };
    let _ = send(&shared, outcome, slot).await;
    drop(held_permit);
}

/// One blocking extraction task in the run-local gauges: live on entry, returned on drop, so
/// the accounting holds whether the preparer returns or unwinds (RD3-2).
struct LiveTask(Arc<Counters>);

impl LiveTask {
    fn enter(counters: Arc<Counters>) -> Self {
        let live = counters.live_blocking.fetch_add(1, Ordering::SeqCst) + 1;
        counters.max_live_blocking.fetch_max(live, Ordering::SeqCst);
        Self(counters)
    }
}

impl Drop for LiveTask {
    fn drop(&mut self) {
        self.0.live_blocking.fetch_sub(1, Ordering::SeqCst);
        self.0.blocking_completed.fetch_add(1, Ordering::SeqCst);
    }
}
