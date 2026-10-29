//! `kaban ingest <paths...> [--dry-run] [--include-code] [--max-file-bytes N] [--workers N]`
//! Plain ingestion uses `index_docs`; `--mode structured` and `ingest job brief` publish
//! external-extractor briefs through `ingest_brief`. `--link` selects the engine gazetteer.
//! The verb runs through the tool seam
//! ([`crate::cli::seam::run_tool`]) under the MCP tool name `index_docs`, so the raw request
//! (paths are caller data) is scanned before the typed parse and the one JSON report passes
//! the response gate. [`index_docs`] is the service the MCP adapter shares.
//!
//! Exit codes (core-spec §6, F7): 0 ok; 2 a reject-class hit in the request or a withheld
//! response; 4 an invalid request (a path outside the workspace root, a root that is a
//! symlink, a workspace other than the bound one, cancellation or worker exhaustion); 5 a
//! store error. Per-document failures never change the exit code: they are counted in the
//! report and recorded on their Document nodes (D8).
//!
//! The workspace root is the git repository enclosing the current directory, else the
//! current directory (the same containment root as `kaban ontology load`, S7).

use crate::cli::output::Format;
use crate::cli::seam::run_tool;
use kaban_api::ingest_jobs::{
    IngestBrief, IngestBriefRequest, IngestClaimRequest, IngestClaimResponse, IngestFinishReport,
    IngestFinishRequest, IngestNextRequest, IngestNextResponse, IngestSubmitReport,
    IngestSubmitRequest,
};
use kaban_api::{ApiError, IndexDocsRequest, IndexDocsResponse, KabanApi};
use kaban_ingest::{
    ChunkConfig, DocumentPreparer, IngestConfig, Ingestor, WalkConfig, record_last_run,
    run_blocking,
};
use serde_json::{Value, json};
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// The command's arguments as parsed by clap (caller data, not judged yet).
#[derive(Debug, clap::Args)]
#[command(subcommand_negates_reqs = true)]
#[allow(clippy::struct_excessive_bools)] // independent command flags
pub(crate) struct Flags {
    /// Script extractor protocol operations
    #[command(subcommand)]
    command: Option<IngestCommand>,
    /// Plain document ingestion or structured extractor briefs
    #[arg(long, default_value = "plain")]
    mode: String,
    /// External instruction dialect; gazetteer aliases none plus explicit linking
    #[arg(long = "with", default_value = "none")]
    dialect: String,
    /// Advisory source-type override
    #[arg(long = "type")]
    source_type: Option<String>,
    /// Keep symbol sources even above the gazetteer memory cap
    #[arg(long)]
    symbols: bool,
    /// Link matching chunk mentions (default: engine config, normally none)
    #[arg(long)]
    // the gated service validates the closed vocabulary without echoing caller data
    link: Option<String>,
    /// Files or directories to ingest (inside the workspace root)
    #[arg(required = true, num_args = 1..)]
    paths: Vec<PathBuf>,
    /// Workspace to write into; must be the bound one (`--workspace`/`KABAN_WORKSPACE`)
    #[arg(long)]
    workspace: Option<String>,
    /// Count and gate without writing
    #[arg(long)]
    dry_run: bool,
    /// Also ingest source-code files as text documents (off by default)
    #[arg(long)]
    include_code: bool,
    /// Skip files larger than this many bytes (default 2 MiB)
    #[arg(long)]
    max_file_bytes: Option<u64>,
    /// Extraction workers (default: available parallelism)
    #[arg(long)]
    workers: Option<usize>,
    /// Also re-ingest documents that `forget --document` marked forgotten (naming the file
    /// itself always does)
    #[arg(long)]
    include_forgotten: bool,
}

#[derive(Debug, clap::Subcommand)]
enum IngestCommand {
    /// Operate on a persistent structured ingestion job
    Job {
        #[command(subcommand)]
        action: JobAction,
    },
}

#[derive(Debug, clap::Subcommand)]
enum JobAction {
    /// Nonblocking manual claim of a pending document
    Claim {
        #[arg(long = "with")]
        dialect: String,
        #[arg(long = "type")]
        source_type: Option<String>,
        #[arg(long)]
        path_prefix: Option<String>,
        #[arg(long)]
        worker: Option<String>,
    },
    /// Reconcile all successful submits and retain the canonical final report
    Finish {
        job: String,
        #[arg(long)]
        fence: Option<u64>,
    },
    /// Validate and atomically publish external extractor JSON from bounded stdin
    Submit {
        job: String,
        #[arg(long)]
        fence: Option<u64>,
    },
    /// Place one document and open or repeat its structured extraction brief
    Brief {
        path: String,
        #[arg(long, default_value = "structured")]
        mode: String,
        #[arg(long = "with", default_value = "none")]
        dialect: String,
        #[arg(long, default_value = "none")]
        link: String,
        #[arg(long)]
        symbols: bool,
        #[arg(long = "type")]
        source_type: Option<String>,
        #[arg(long, default_value_t = 0)]
        plan_page: u32,
        #[arg(long)]
        dry_run: bool,
    },
    /// Reopen and deliver one chunk slice or a byte-bounded whole-chunk range
    Next {
        job: String,
        #[arg(long)]
        fence: Option<u64>,
        #[arg(long, conflicts_with = "chunks", required_unless_present = "chunks")]
        chunk: Option<u32>,
        #[arg(long, requires = "chunk")]
        slice: Option<u32>,
        #[arg(long, conflicts_with = "chunk", required_unless_present = "chunk")]
        chunks: Option<String>,
    },
}

impl Flags {
    /// Only structured submit accepts the root bounded JSON input flag.
    pub(crate) fn is_submit(&self) -> bool {
        matches!(
            self.command,
            Some(IngestCommand::Job {
                action: JobAction::Submit { .. }
            })
        )
    }
    /// The raw `index_docs` request (the paths are strings the seam judges as content).
    fn into_raw(self) -> Value {
        json!({
            "symbols": self.symbols,
            "link": self.link,
            "paths": self.paths.iter().map(|p| p.to_string_lossy()).collect::<Vec<_>>(),
            "workspace": self.workspace,
            "dry_run": self.dry_run,
            "include_code": self.include_code,
            "max_file_bytes": self.max_file_bytes,
            "workers": self.workers,
            "include_forgotten": self.include_forgotten,
        })
    }
}

/// The workspace root of this process: the enclosing git repository, else the current
/// directory (S7 containment, like `ontology load`).
fn workspace_root() -> Result<PathBuf, ApiError> {
    crate::ontology_root()
        .or_else(|| std::env::current_dir().ok())
        .ok_or_else(|| ApiError::Invalid("the current directory is not accessible".to_owned()))
}

/// Ingest `req.paths` into the bound workspace: one `Ingestor` run through
/// [`kaban_ingest::run_blocking`] (the caller is a plain thread or a blocking-pool thread; the
/// private runtime is shut down in the background, so a parser abandoned at its deadline never
/// holds this call, RD3-3), then the `last_run` stamp.
///
/// # Errors
/// `Invalid` for a workspace other than the bound one, an empty path list, a configuration
/// the walker refuses (a root outside the workspace root or through a symlink), cancellation
/// or worker exhaustion; store errors keep their class.
pub(crate) fn index_docs(
    api: &KabanApi,
    req: &IndexDocsRequest,
) -> Result<IndexDocsResponse, ApiError> {
    if let Some(ws) = &req.workspace
        && ws != api.workspace().as_str()
    {
        return Err(ApiError::Invalid(
            "workspace must be the bound workspace (--workspace or KABAN_WORKSPACE)".to_owned(),
        ));
    }
    if req.paths.is_empty() {
        return Err(ApiError::Invalid(
            "ingest needs at least one path".to_owned(),
        ));
    }
    let root = workspace_root()?;
    let mut walk = WalkConfig::new(root.clone());
    walk.roots = req
        .paths
        .iter()
        .map(|p| {
            let p = PathBuf::from(p);
            if p.is_absolute() { p } else { root.join(p) }
        })
        .collect();
    walk.include_code = req.include_code;
    walk.include_forgotten = req.include_forgotten;
    if let Some(cap) = req.max_file_bytes {
        walk.max_file_bytes = cap;
    }
    let mut cfg = IngestConfig {
        symbols: req.symbols,
        link: req.link.clone(),
        dry_run: req.dry_run,
        ..IngestConfig::default()
    };
    if let Some(w) = req.workers {
        cfg.workers = w.max(1);
        cfg.queue_depth = 2 * cfg.workers;
    }
    let store = Arc::clone(api.store());
    let signer = Arc::clone(api.signer());
    let preparer = Arc::new(DocumentPreparer::new(
        api,
        Arc::clone(&store),
        Arc::clone(&signer),
        ChunkConfig::default(),
        cfg.clone(),
    ));
    let ingestor = Ingestor::new(
        Arc::new(api.clone()),
        Arc::clone(&store),
        signer,
        preparer,
        cfg,
    );
    let report =
        run_blocking(&ingestor, &walk, CancellationToken::new()).map_err(ApiError::from)?;
    if let Some(telemetry) = &report.gazetteer {
        if !telemetry.dropped.is_empty() {
            let _ = writeln!(
                std::io::stderr(),
                "link: symbols_dropped_budget peak={} retained={}",
                telemetry.build_peak_delta,
                telemetry.memory_usage
            );
        }
        if telemetry.outcome == "skipped_budget" {
            let _ = writeln!(
                std::io::stderr(),
                "link: skipped_budget peak={} retained={}",
                telemetry.build_peak_delta,
                telemetry.memory_usage
            );
        }
    }
    if !req.dry_run {
        record_last_run(store.as_ref(), api.workspace()).map_err(ApiError::from)?;
    }
    let mut response = IndexDocsResponse::from(report);
    response.workspace = api.workspace().to_string();
    response.dry_run = req.dry_run;
    // one barrier per ingest run (SS-10), after the run's transactions and locks: the receipt
    // holds at the log head the run left (a dry run or a run that skipped everything waits
    // for the state it saw like any other write)
    response.receipt = Some(api.await_searchable(api.committed_receipt(None)));
    Ok(response)
}

/// Run `kaban ingest` through the seam and print the one JSON report.
pub(crate) fn run(api: &KabanApi, mut flags: Flags, format: Format) -> ExitCode {
    if flags.is_submit() {
        crate::cli::output::print_error("error: ingest job submit requires --stdin-json");
        return ExitCode::from(crate::cli::output::EXIT_INVALID);
    }
    if let Some(IngestCommand::Job { action }) = flags.command.take() {
        if let JobAction::Claim {
            dialect,
            source_type,
            path_prefix,
            worker,
        } = action
        {
            return run_tool::<IngestClaimRequest, IngestClaimResponse, _>(
                api,
                "ingest_claim",
                json!({"with":dialect,"type":source_type,"path_prefix":path_prefix,"worker":worker}),
                &[],
                format,
                |api, request| ingest_claim(api, &request),
            );
        }
        if let JobAction::Finish { job, fence } = action {
            return crate::cli::seam::run_write_tool::<IngestFinishRequest, IngestFinishReport, _>(
                api,
                "ingest_finish",
                json!({"job":job,"fence":fence}),
                &[],
                format,
                |api, request| ingest_finish(api, &request),
            );
        }
        if let JobAction::Brief {
            path,
            mode,
            dialect,
            link,
            symbols,
            source_type,
            plan_page,
            dry_run,
        } = action
        {
            return run_brief(
                api,
                json!({"path":path,"mode":mode,"with":if dialect == "gazetteer" { "none" } else { &dialect },"link":if dialect == "gazetteer" { "gazetteer" } else { &link },"symbols":symbols,"type":source_type,"plan_page":plan_page,"dry_run":dry_run}),
                format,
            );
        }
        let JobAction::Next {
            job,
            fence,
            chunk,
            slice,
            chunks,
        } = action
        else {
            unreachable!("brief action handled above")
        };
        return run_tool::<IngestNextRequest, IngestNextResponse, _>(
            api,
            "ingest_next",
            json!({"job": job, "fence":fence, "n": chunk, "slice": slice, "chunks": chunks}),
            &[],
            format,
            |api, request| ingest_next(api, &request),
        );
    }
    if flags.mode == "structured" {
        return run_structured(api, &flags, format);
    }
    if flags.mode != "plain" || flags.dialect != "none" || flags.source_type.is_some() {
        return crate::cli::seam::run_tool::<IngestBriefRequest, IngestBrief, _>(
            api,
            "ingest_brief",
            json!({"path":"","mode":flags.mode,"with":flags.dialect}),
            &[],
            format,
            |_, _| Err(ApiError::Invalid("invalid_ingest_mode".into())),
        );
    }
    run_tool::<IndexDocsRequest, IndexDocsResponse, _>(
        api,
        "index_docs",
        flags.into_raw(),
        &[],
        format,
        |api, request: IndexDocsRequest| index_docs(api, &request),
    )
}

fn run_brief(api: &KabanApi, raw: Value, format: Format) -> ExitCode {
    run_tool::<IngestBriefRequest, IngestBrief, _>(
        api,
        "ingest_brief",
        raw,
        &[],
        format,
        |api, request| ingest_brief(api, &request),
    )
}

/// Forward already-bounded stdin through the shared canonical submit seam.
pub(crate) fn run_submit(api: &KabanApi, flags: Flags, mut raw: Value, format: Format) -> ExitCode {
    let Some(IngestCommand::Job {
        action: JobAction::Submit { job, fence },
    }) = flags.command
    else {
        crate::cli::output::print_error("error: invalid_ingest_action");
        return ExitCode::from(crate::cli::output::EXIT_INVALID);
    };
    if let Some(object) = raw.as_object_mut() {
        object.entry("job").or_insert_with(|| json!(job));
        if let Some(fence) = fence {
            object.entry("fence").or_insert_with(|| json!(fence));
        }
    }
    run_tool::<IngestSubmitRequest, IngestSubmitReport, _>(
        api,
        "ingest_submit",
        raw,
        &[],
        format,
        |api, request| {
            if request.job != job {
                return Err(ApiError::Invalid("job_mismatch".into()));
            }
            if fence.is_some() && request.fence != fence {
                return Err(ApiError::Invalid("lease_lost".into()));
            }
            ingest_submit(api, &request)
        },
    )
}

/// Shared CLI/MCP structured-submit service; no extractor or model execution.
pub(crate) fn ingest_submit(
    api: &KabanApi,
    request: &IngestSubmitRequest,
) -> Result<IngestSubmitReport, ApiError> {
    let root = workspace_root()?;
    let raw =
        serde_json::to_value(request).map_err(|_| ApiError::Invalid("invalid_body".into()))?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| ApiError::Invalid("ingest_runtime_unavailable".into()))?;
    let result = runtime.block_on(kaban_ingest::jobs::submit::submit(api, &root, raw));
    runtime.shutdown_background();
    serde_json::from_value(result.map_err(job_error)?)
        .map_err(|_| ApiError::Invalid("invalid_report".into()))
}

/// Shared canonical finish service for the CLI and MCP transports.
pub(crate) fn ingest_claim(
    api: &KabanApi,
    request: &IngestClaimRequest,
) -> Result<IngestClaimResponse, ApiError> {
    let root = workspace_root()?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| ApiError::Invalid("ingest_runtime_unavailable".into()))?;
    let result = runtime.block_on(kaban_ingest::jobs::claim::claim(api, &root, request));
    runtime.shutdown_background();
    result.map_err(job_error)
}

pub(crate) fn ingest_finish(
    api: &KabanApi,
    request: &IngestFinishRequest,
) -> Result<IngestFinishReport, ApiError> {
    let root = workspace_root()?;
    let job = kaban_core::JobId::parse(&request.job)
        .map_err(|_| ApiError::Invalid("invalid_job_handle".into()))?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| ApiError::Invalid("ingest_runtime_unavailable".into()))?;
    let result = runtime.block_on(kaban_ingest::jobs::finish::finish_fenced(
        api,
        &root,
        &job,
        request.fence,
    ));
    runtime.shutdown_background();
    serde_json::from_str(&result.map_err(job_error)?)
        .map_err(|_| ApiError::Invalid("invalid_report".into()))
}

fn run_structured(api: &KabanApi, flags: &Flags, format: Format) -> ExitCode {
    if flags
        .workspace
        .as_deref()
        .is_some_and(|ws| ws != api.workspace().as_str())
        || flags.include_code
        || flags.include_forgotten
    {
        return run_brief(api, json!({"path":"","mode":"plain"}), format);
    }
    let Ok(root) = workspace_root() else {
        return ExitCode::from(4);
    };
    let mut walk = WalkConfig::new(root.clone());
    walk.roots = flags
        .paths
        .iter()
        .map(|path| {
            if path.is_absolute() {
                path.clone()
            } else {
                root.join(path)
            }
        })
        .collect();
    if let Some(cap) = flags.max_file_bytes {
        walk.max_file_bytes = cap;
    }
    let paths = (|| {
        let policy = api.policy_handle();
        let policy = policy
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut paths = kaban_ingest::walk(&walk)
            .map_err(|_| ())?
            .map(|candidate| {
                candidate.map_err(|_| ()).and_then(|candidate| {
                    candidate
                        .gate(&policy)
                        .map(|path| path.relative_path().to_owned())
                        .map_err(|_| ())
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        paths.sort();
        paths.dedup();
        if paths.is_empty() {
            return Err(());
        }
        Ok(paths)
    })();
    let Ok(paths) = paths else {
        return ExitCode::from(4);
    };
    for path in paths {
        let code = run_brief(
            api,
            json!({"path":path,"mode":"structured","with":if flags.dialect == "gazetteer" { "none" } else { &flags.dialect },"link":if flags.dialect == "gazetteer" { "gazetteer" } else { flags.link.as_deref().unwrap_or("none") },"symbols":flags.symbols,"type":flags.source_type,"dry_run":flags.dry_run}),
            format,
        );
        if code != ExitCode::SUCCESS {
            return code;
        }
    }
    ExitCode::SUCCESS
}

/// Shared typed CLI/MCP brief service. Extractors remain external and are never executed.
pub(crate) fn ingest_brief(
    api: &KabanApi,
    request: &IngestBriefRequest,
) -> Result<IngestBrief, ApiError> {
    let root = workspace_root()?;
    let opts = kaban_ingest::jobs::types::IngestOpts {
        mode: request.mode,
        r#with: request.r#with,
        link: request.link.clone(),
        symbols: request.symbols,
        r#type: request.r#type.clone(),
        plan_page: request.plan_page,
        dry_run: request.dry_run,
        ..Default::default()
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| ApiError::Invalid("ingest_runtime_unavailable".into()))?;
    let result = runtime.block_on(kaban_ingest::jobs::brief::brief(
        api,
        &root,
        &request.path,
        &opts,
    ));
    runtime.shutdown_background();
    result.map_err(job_error)
}

fn job_error(error: kaban_ingest::jobs::types::JobError) -> ApiError {
    match error {
        kaban_ingest::jobs::types::JobError::Api(error) => error,
        kaban_ingest::jobs::types::JobError::Store(error) => ApiError::from(error),
        kaban_ingest::jobs::types::JobError::Revoked => ApiError::NotFound("job_revoked".into()),
        other => ApiError::Invalid(other.to_string()),
    }
}

/// Shared CLI/MCP job-page service; selectors are validated before bounded preparation.
pub(crate) fn ingest_next(
    api: &KabanApi,
    request: &IngestNextRequest,
) -> Result<IngestNextResponse, ApiError> {
    use kaban_ingest::jobs::next::{PageBudget, next_fenced, next_range_fenced};
    let job = kaban_core::JobId::parse(&request.job)
        .map_err(|_| ApiError::Invalid("invalid_job_handle".into()))?;
    let range = match (&request.n, &request.chunks, &request.slice) {
        (Some(_), None, _) => None,
        (None, Some(chunks), None) => {
            let (a, b) = chunks
                .split_once("..")
                .ok_or_else(|| ApiError::Invalid("invalid_chunk_selector".into()))?;
            let parse = |value: &str| {
                value
                    .parse::<u32>()
                    .map_err(|_| ApiError::Invalid("invalid_chunk_selector".into()))
            };
            let (a, b) = (parse(a)?, parse(b)?);
            if a >= b {
                return Err(ApiError::Invalid("invalid_chunk_selector".into()));
            }
            Some((a, b))
        }
        _ => return Err(ApiError::Invalid("invalid_chunk_selector".into())),
    };
    let root = workspace_root()?;
    let budget = PageBudget {
        metadata_allowance: usize::try_from(api.engine().ingest.metadata_allowance_kib)
            .unwrap_or(usize::MAX)
            .saturating_mul(1024),
        ..PageBudget::default()
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| ApiError::Invalid("ingest_runtime_unavailable".into()))?;
    let result = runtime.block_on(async {
        if let Some((a, b)) = range {
            next_range_fenced(api, &root, &job, a, b, budget, request.fence)
                .await
                .map(IngestNextResponse::from)
        } else {
            next_fenced(
                api,
                &root,
                &job,
                request.n.unwrap_or(0),
                request.slice.unwrap_or(0),
                budget,
                request.fence,
            )
            .await
            .map(IngestNextResponse::from)
        }
    });
    runtime.shutdown_background();
    result.map_err(job_error)
}
