//! kaban binary: MCP stdio server (default) and CLI (core-spec F7): the command line
//! ([`Cli`]), the stderr tracing subscriber and the service open helpers live here; the
//! dispatch is [`cli::run`] (M4-25). stdout is the MCP protocol channel, or the CLI's one JSON
//! document; logs go to stderr only.

mod bench;
mod cli;
mod codegraph;
mod lease_keeper;
mod mcp;
mod scrub;

use anyhow::Context;
use clap::{Parser, Subcommand};
use kaban_core::WorkspaceId;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use tracing_subscriber::Layer;

#[derive(Parser)]
#[command(
    name = "kaban",
    version,
    about = "kaban: local, secret-safe memory for coding agents (SQLite + Tantivy + YAML denylist)"
)]
struct Cli {
    /// SQLite database path (created if missing)
    #[arg(long, env = "KABAN_DB", default_value = "kaban.db")]
    db: PathBuf,
    /// Global (cross-repository) database; not available in this build (M6)
    #[arg(long, env = "KABAN_GLOBAL_DB")]
    global_db: Option<PathBuf>,
    /// YAML denylist policy file (builtin rules only if omitted)
    #[arg(long, env = "KABAN_POLICY")]
    policy: Option<PathBuf>,
    /// Workspace this server is bound to (core-spec S6). Default: the name of the enclosing git
    /// repository's root directory, or `default` outside a repository.
    #[arg(long, env = "KABAN_WORKSPACE")]
    workspace: Option<String>,
    /// The writer identity of this process (spec 05 SS-26), stamped on every record and change
    /// row it writes, for the CLI and `serve` alike; swarms set one per agent. Never inferred
    /// from an agent id, a session, a PID or the transport.
    #[arg(long, env = "KABAN_ACTOR_ID", default_value = kaban_core::ActorId::LOCAL, value_parser = parse_actor_id)]
    actor_id: kaban_core::ActorId,
    /// Compact single-line JSON on stdout (default: pretty-printed; both are one document)
    #[arg(long, global = true)]
    json: bool,
    /// Read the request as one JSON object from stdin (at most 1 MiB) instead of arguments
    #[arg(long, global = true)]
    stdin_json: bool,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Report counters, policy and the default layer matrix (same response as MCP stats)
    Stats(cli::stats::Flags),
    /// Recompute derived indexes from this database; canonical records are never changed
    Reindex {
        #[command(subcommand)]
        command: cli::reindex::Command,
    },
    /// Serve MCP over stdio (default)
    Serve {
        /// Loopback HTTP transport; not available in this build (M7)
        #[arg(long)]
        http: bool,
    },
    /// Populate a database with N synthetic notes and measure latencies
    Bench {
        /// Number of notes to insert
        #[arg(long, default_value_t = 100_000)]
        n: usize,
        /// Random edges per note
        #[arg(long, default_value_t = 3)]
        edges_per_node: usize,
        /// Synthetic document chunks ingested after the notes (0 = none; needs the `ingest`
        /// feature)
        #[arg(long, default_value_t = 0)]
        chunks: usize,
        /// Schema version the corpus is built at (default: the latest); below the latest the
        /// run reads the corpus, migrates it with a plain open (timed) and reads it again
        #[arg(long, default_value_t = kaban_store_sqlite::LATEST_SCHEMA_VERSION)]
        schema_version: u32,
        /// The code-graph bench (`bench cg gen|run`, stage1-plan §18); needs a build with the
        /// `bench` feature, exit 4 otherwise
        #[command(subcommand)]
        cg: Option<BenchCg>,
    },
    /// Validate a policy file and scan stdin, printing the verdict (no persistence)
    Scan,
    /// Re-judge every node stored under another policy version with the loaded policy
    /// (reject hits are tombstoned, redact hits masked in place); `serve` runs the same job in
    /// the background at startup
    Rescan,
    /// Store a memory (the `remember` tool; actor agent). Exit 0 ok, 2 rejected by policy, 4
    /// invalid request, 5 store error
    Remember(cli::remember::Flags),
    /// Search memories (the `recall` tool). Exit 0 ok, 2 rejected by policy, 4 invalid
    /// request, 5 store error
    Recall(cli::recall::Flags),
    /// Tombstone, invalidate, archive or purge a memory (the `forget` tool; actor agent). Exit
    /// 0 ok, 2 rejected by policy, 3 not found, 4 invalid request, 5 store error
    Forget(cli::forget::Flags),
    /// Check the database as it is on disk (never created, migrated or tightened): file
    /// modes, integrity, text index, schema version, outbox lag, conflicts, audit key file,
    /// audit chain and the unresolved conflict groups. Exit 0 when every check passes, 5
    /// otherwise or when the database cannot be opened
    Doctor {
        /// Explicitly optimize full-text index segments once (never runs automatically)
        #[arg(long)]
        optimize: bool,
        /// Also run VACUUM to reclaim free pages (reported as its own check)
        #[arg(long)]
        vacuum: bool,
        /// Also print, per configured prior, the Beta posterior mean of the accept/forget
        /// counters beside it (reported as its own check; never applied)
        #[arg(long)]
        priors: bool,
    },
    /// Host hook handler (core-spec F8): reads the host's event envelope as one JSON object
    /// from stdin, prints one JSON document and always exits 0; never creates or migrates the
    /// database
    Hook {
        /// The kaban hook event
        #[arg(value_enum)]
        event: cli::hook::EventArg,
        /// Which host sent the envelope
        #[arg(long, value_enum, default_value_t = cli::hook::HostArg::ClaudeCode)]
        host: cli::hook::HostArg,
    },
    /// Load or list ontology files (engine-spec EN-F2): the shipped `core` and `adr`
    /// ontologies plus `.kaban/ontology/*.yaml`, loaded at startup in sorted order
    Ontology {
        #[command(subcommand)]
        cmd: cli::ontology::OntologyCmd,
    },
    /// Ingest local documents (Markdown, text) into Document, Section and Chunk nodes of the
    /// bound workspace (docs-ingest-spec D10): deterministic, incremental, denylist-gated
    #[cfg(feature = "ingest")]
    Ingest(cli::ingest::Flags),
}

/// `kaban bench cg ...` (stage1-plan §18, CG7.3; feature `bench`), `kaban bench recall` and
/// `kaban bench edges`.
#[derive(Subcommand)]
enum BenchCg {
    /// Judge public-service recall and retain paired warm lexical/fuzzy timing evidence
    Recall(bench::recall_runtime::RecallArgs),
    /// Judge actual gazetteer MENTIONS edges using a retained stable source map
    Edges(bench::recall_runtime::EdgesArgs),
    /// The code-graph bench: a synthetic snapshot and the API-level ops over it
    Cg {
        #[command(subcommand)]
        cmd: BenchCgCmd,
    },
}

#[derive(Subcommand)]
enum BenchCgCmd {
    /// Generate the synthetic snapshot `<out>/<commit40>.cg.db` (0600; the commit derives from
    /// the seed) and print its stats as one JSON object
    Gen {
        /// Symbols (rounded down to whole 20-symbol files)
        #[arg(long, default_value_t = 100_000)]
        symbols: usize,
        /// The deterministic seed, hexadecimal (`0x` optional)
        #[arg(long, default_value = "0x9e3779b97f4a7c15")]
        seed: String,
        /// Output directory (created 0700 when missing)
        out: PathBuf,
    },
    /// Time the ops over one snapshot (3 warm-up + 20 measured calls per op by default) and
    /// print the `bench_schema: 3` result document
    Run {
        /// The snapshot file `<commit40>.cg.db`
        snapshot: PathBuf,
        /// The ops to run, comma-separated (default: every op): `open_ro`, `tool_implementers`,
        /// `tool_members`, `tool_references`, `tool_definition`, `tool_tests_for`,
        /// `tool_neighbours_d2`, `rss_open`, `code_context_split`, `denylist_fp_rate`
        #[arg(long, value_delimiter = ',')]
        ops: Vec<String>,
        /// Untimed calls per op before the measured ones
        #[arg(long, default_value_t = 3)]
        warmup: usize,
        /// Measured calls per op
        #[arg(long, default_value_t = 20)]
        runs: usize,
        /// Also write the result document to this file
        #[arg(long)]
        out: Option<PathBuf>,
    },
}

impl Cmd {
    /// Whether the subcommand reads its request from `--stdin-json` (M4-4); for every other
    /// subcommand the flag is refused rather than ignored.
    fn reads_body(&self) -> bool {
        #[cfg(feature = "ingest")]
        if let Self::Ingest(flags) = self {
            return flags.is_submit();
        }
        matches!(
            self,
            Self::Remember(_) | Self::Recall(_) | Self::Forget(_) | Self::Hook { .. }
        )
    }
}

/// The git repository root enclosing `start` (found by walking up for a `.git` entry, no `git`
/// subprocess), if any.
fn repo_root(start: &std::path::Path) -> Option<PathBuf> {
    start
        .ancestors()
        .find(|dir| dir.join(".git").exists())
        .map(std::path::Path::to_path_buf)
}

/// Repository identity (M3-23): the root directory's name, the same rule as the default
/// workspace.
fn repo_name(root: &std::path::Path) -> Option<String> {
    root.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
}

/// `--actor-id`: the [`kaban_core::ActorId`] rule; a refusal is clap's fixed value-validation
/// text (exit 4), never the value.
fn parse_actor_id(s: &str) -> Result<kaban_core::ActorId, kaban_core::InvalidActorId> {
    kaban_core::ActorId::parse(s)
}

/// The workspace bound when `--workspace`/`KABAN_WORKSPACE` is absent: the directory name of
/// the git repository root enclosing `start`, else [`WorkspaceId::DEFAULT`].
fn default_workspace(start: &std::path::Path) -> WorkspaceId {
    repo_root(start)
        .and_then(|root| repo_name(&root))
        .map_or_else(WorkspaceId::default, WorkspaceId::new)
}

/// `$KABAN_HOME` or `~/.kaban`: holds `keys/<db-id>.key` (and later tokens, global store).
fn kaban_home() -> anyhow::Result<PathBuf> {
    if let Some(h) = std::env::var_os("KABAN_HOME") {
        return Ok(PathBuf::from(h));
    }
    std::env::var_os("HOME")
        .map(|h| PathBuf::from(h).join(".kaban"))
        .context("neither KABAN_HOME nor HOME is set; cannot locate the audit key directory")
}

/// `<KABAN_HOME>/config.yaml`: the one engine configuration file (engine-spec §15.1; SO-11).
fn config_path() -> anyhow::Result<PathBuf> {
    Ok(kaban_home()?.join("config.yaml"))
}

/// The S7 containment root of ontology loads (engine-plan E1.3): the git repository
/// enclosing the current directory, else `None` (the registry then uses the current
/// directory).
fn ontology_root() -> Option<PathBuf> {
    std::env::current_dir().ok().and_then(|cwd| repo_root(&cwd))
}

/// Dependency log targets capped at WARN whatever `RUST_LOG` says, because they log raw
/// request or path content below that level: rmcp 3.5.1 logs full requests (including tool
/// arguments) at DEBUG/TRACE and notifications at INFO in `rmcp::service`, which would put note
/// text and rejected secrets into the log (core-spec S1; m2 Me7); `ignore` 0.4.33 logs
/// `ignoring <raw path>` / `whitelisting <raw path>` at DEBUG for every file an ignore file
/// matches, before kaban's path gate sees the name, and `globset`/`walkdir` are its matcher and
/// traversal (docs-ingest DI0 review RD-1); `tantivy` 0.26 logs the index directory's lock
/// and segment file paths at DEBUG on every open and commit (spec 05 OWN-M: every open has
/// one), which would put the database's path into every fixed-line error output.
const CAPPED_LOG_TARGETS: [&str; 5] = ["rmcp", "ignore", "globset", "walkdir", "tantivy"];

/// Install the stderr subscriber: `RUST_LOG` selects levels as usual, except for the
/// [`CAPPED_LOG_TARGETS`]. `init` also installs the `log` bridge, so dependencies that log
/// through `log` are filtered by the same cap.
fn init_tracing() {
    use tracing_subscriber::util::SubscriberInitExt;
    stderr_subscriber(
        tracing_subscriber::EnvFilter::from_default_env(),
        std::io::stderr,
        std::io::IsTerminal::is_terminal(&std::io::stderr()),
    )
    .init();
}

/// The subscriber [`init_tracing`] installs, with its writer and level filter injectable so a
/// test can capture what would reach stderr: `filter` selects levels, and events from a
/// [`CAPPED_LOG_TARGETS`] target above WARN are dropped before the writer sees them.
fn stderr_subscriber<W>(
    filter: tracing_subscriber::EnvFilter,
    writer: W,
    ansi: bool,
) -> impl tracing::Subscriber + Send + Sync + 'static
where
    W: for<'w> tracing_subscriber::fmt::MakeWriter<'w> + Send + Sync + 'static,
{
    use tracing_subscriber::layer::SubscriberExt;
    let dependency_cap = tracing_subscriber::filter::filter_fn(|meta| {
        !(CAPPED_LOG_TARGETS
            .iter()
            .any(|t| meta.target().starts_with(t))
            && *meta.level() > tracing::Level::WARN)
    });
    tracing_subscriber::registry().with(filter).with(
        tracing_subscriber::fmt::layer()
            .with_writer(writer)
            .with_ansi(ansi)
            .with_filter(dependency_cap),
    )
}

/// Why the service could not be assembled: a refused configuration (exit 4), a store open
/// failure, or an environment failure. Each variant is rendered by [`OpenError::report`] with
/// its outer message only (review RM-1).
enum OpenError {
    /// `--workspace` is not a name, or the configuration does not load.
    Config(anyhow::Error),
    /// The database or its key could not be opened.
    Store(kaban_core::StoreError),
    /// Environment failure (`KABAN_HOME`/`HOME` unset, no current directory).
    Other(anyhow::Error),
    /// The startup ontology load hit a reject-class denylist rule (EN-S8, exit 2; rule ids
    /// only).
    Rejected(kaban_api::ApiError),
}

impl OpenError {
    /// Print the failure to stderr: the safe outer message of the typed error (a
    /// `StoreError`'s own `Display`, never its driver source), no chained rendering.
    fn report(&self, db: &std::path::Path) {
        match self {
            Self::Config(e) | Self::Other(e) => cli::output::print_failure(e),
            Self::Rejected(e) => cli::output::print_failure(e),
            Self::Store(e) => {
                let msg = format!("opening database {}: {e}", db.display());
                cli::output::print_failure(&msg);
            }
        }
    }
}

/// Assemble the service for `serve` and `rescan`: the code-graph provider (its key read, never
/// created; a loose key is a config error before the store is opened), the store with its key
/// and the bound workspace. `serving` says this process is the server (SS-21): the mode-B
/// projection loop runs beside the write barrier's drain; a one-shot command drives its own
/// projection through the drain and exits.
fn open_api(
    cli: Cli,
    policy: kaban_policy::Policy,
    serving: bool,
) -> Result<kaban_api::KabanApi, OpenError> {
    let home = kaban_home().map_err(OpenError::Other)?;
    // the code side of the join (stage1-plan CG5.2, G1-F8, G1-S7): the only place a provider
    // is built; hooks (`assemble_api` callers) never get one
    let cwd = std::env::current_dir().map_err(|e| OpenError::Other(e.into()))?;
    let codegraph = codegraph::wire(
        &home,
        repo_root(&cwd).as_deref(),
        &policy,
        &kaban_api::GitRunner::default(),
    )
    .map_err(|e| OpenError::Config(e.into()))?;
    let keys_dir = home.join("keys");
    // the one engine configuration, loaded before the store so its `[er]` section reaches the
    // store's startup seam (E2.10 boundary review BR-7): an invalid section is a config error
    // (exit 4) naming the loader's category and the field path, never a value
    let engine = load_engine()?;
    let er = er_priors(&engine.er)?;
    // the store owns its text index (spec 05 OWN-M): opened, admitted or rebuilt by the open,
    // published by the store; a serving process runs the store's projector thread (SS-21; the
    // caller says which, since `cli.cmd` was taken by the dispatch: fix11), every other
    // command publishes its own commits inline and exits
    let store = kaban_store_sqlite::SqliteStore::open_as(&cli.db, &keys_dir, &cli.actor_id)
        .map_err(OpenError::Store)?
        .with_er_priors(er)
        .with_recall_config(engine.recall.clone())
        .map_err(OpenError::Store)?;
    let store = Arc::new(store.serving(serving).map_err(OpenError::Store)?);
    // the one registry of this process (engine-plan E1.5): the service reads every mixin
    // decision from it, and `kaban ontology load` loads through it
    let registry = kaban_api::OntologyRegistry::new(ontology_root()).map_err(open_error)?;
    let api = assemble_api_with(cli, policy, store, engine)?.with_registry(Arc::new(registry));
    let api = match codegraph {
        Some(wiring) => api.with_snapshot_provider(wiring.dir, wiring.provider),
        None => api,
    };
    // the startup ontology load (engine-spec EN-F2): `.kaban/ontology/*.yaml` of the enclosing
    // repository in sorted path order; hooks (`assemble_api` callers) skip it, as they never
    // create or migrate anything either
    load_ontologies(&api)?;
    Ok(api)
}

/// The startup load of the repository's ontology files: a denylist rejection is exit 2, an
/// invalid file or a containment failure a config error (exit 4), a store failure exit 5.
fn load_ontologies(api: &kaban_api::KabanApi) -> Result<(), OpenError> {
    let loaded = api.registry().load_startup(api).map_err(open_error)?;
    for report in loaded.iter().filter(|r| r.changed) {
        tracing::info!(ns = %report.ns, version = report.version, "ontology loaded");
    }
    Ok(())
}

/// Map a service error of the startup path to an exit class.
fn open_error(e: kaban_api::ApiError) -> OpenError {
    match e {
        kaban_api::ApiError::Rejected { .. } => OpenError::Rejected(e),
        kaban_api::ApiError::Store(store) => OpenError::Store(store),
        other => OpenError::Config(anyhow::anyhow!("ontology: {other}")),
    }
}

/// [`open_api`] after the store is open: the migration log line and the bound workspace
/// (core-spec S6; the same rule for the MCP server and the hooks, M4-32).
fn assemble_api(
    cli: Cli,
    policy: kaban_policy::Policy,
    store: kaban_store_sqlite::SqliteStore,
) -> Result<kaban_api::KabanApi, OpenError> {
    let engine = load_engine()?;
    assemble_api_with(cli, policy, Arc::new(store), engine)
}

/// The one engine configuration, loaded once per process (SO-11): a `ConfigError` names the
/// field path, never a value, and is a config error (exit 4).
fn load_engine() -> Result<kaban_core::config::EngineConfig, OpenError> {
    config_path()
        .and_then(|path| kaban_core::config::load(&path).map_err(anyhow::Error::from))
        .map_err(OpenError::Config)
}

/// Freeze the loaded `[er]` section for the store's startup seam (BR-7): the loader's error
/// category (`er_unknown_field`, `er_probability_out_of_range`, `er_threshold_invalid`) and
/// its field path under `er.` are the config error's text (exit 4); no value is printed.
fn er_priors(er: &kaban_core::config::Er) -> Result<kaban_core::ErPriors, OpenError> {
    kaban_core::ErPriors::from_config(er).map_err(|e| {
        let path = match &e {
            // RW4-5: an unknown key never crosses the error boundary; the table names the hole
            kaban_core::ErConfigError::UnknownField { table } => table.unknown_path().to_owned(),
            kaban_core::ErConfigError::OutOfRange { path }
            | kaban_core::ErConfigError::Threshold { path } => path.clone(),
        };
        OpenError::Config(anyhow::anyhow!("config key invalid: er.{path} ({e})"))
    })
}

/// [`assemble_api`] over an already loaded engine configuration.
fn assemble_api_with(
    cli: Cli,
    policy: kaban_policy::Policy,
    store: Arc<kaban_store_sqlite::SqliteStore>,
    engine: kaban_core::config::EngineConfig,
) -> Result<kaban_api::KabanApi, OpenError> {
    let migration = store.migration();
    if migration.from != migration.to {
        tracing::info!(to = migration.to, "schema created on a fresh database");
    }
    let signer = store.signer();
    let cwd = std::env::current_dir().map_err(|e| OpenError::Other(e.into()))?;
    let workspace = match cli.workspace {
        Some(ws) if ws == WorkspaceId::ALL || ws.is_empty() => {
            return Err(OpenError::Config(anyhow::anyhow!(
                "--workspace must name one workspace (\"*\" is a read selector)"
            )));
        }
        Some(ws) => WorkspaceId::new(ws),
        None => default_workspace(&cwd),
    };
    // repository identity, computed once without a subprocess (M3-23)
    let repo_root = repo_root(&cwd);
    let repo = repo_root.as_deref().and_then(repo_name);
    Ok(kaban_api::KabanApi::with_config(
        store,
        policy,
        signer,
        None,
        kaban_api::ApiConfig {
            workspace,
            repo_root,
            repo,
            record_signer_identity: false,
            engine: Arc::new(engine),
        },
    ))
}

/// Parse the command line and run it (`cli::run`). A usage error prints a fixed text and exits
/// 4; help and version exit 0 (M4-2). Nothing is returned to Rust's top-level reporter, which
/// would print a whole error chain (review RM-1).
fn main() -> ExitCode {
    // first: a panic anywhere below prints one fixed line, never its payload (RD3-2); the
    // `hook` subcommand replaces it with its own line as before
    #[cfg(feature = "ingest")]
    kaban_ingest::panic_hook::install();
    init_tracing();
    match Cli::try_parse() {
        Ok(cli) => cli::run(cli),
        Err(e) => cli::parse_failure(&e),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // test code: failures are the assertion
mod tests {
    use super::default_workspace;

    /// A `tracing` writer into a shared buffer (what would reach stderr); used by the
    /// `ingest`-gated walker test only, so it is gated with it (DI2.0 (d)).
    #[cfg(feature = "ingest")]
    #[derive(Clone, Default)]
    struct LogBuf(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
    #[cfg(feature = "ingest")]
    impl std::io::Write for LogBuf {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Review RD-1 (core-spec S1, PR-2): the pinned `ignore` crate logs `ignoring <raw path>`
    /// through `log` at DEBUG for every file an ignore file excludes, before kaban's path gate
    /// ever sees the name. With the `log` bridge installed (as `init_tracing` does) and
    /// `RUST_LOG=trace`, a `.gitignore`d `notes-<CANARY>.md` must not reach the captured output,
    /// while kaban's own events still do.
    #[cfg(feature = "ingest")]
    #[test]
    fn ignored_paths_logged_by_the_walker_dependency_never_reach_stderr() {
        const CANARY: &str = "AKIAQWERTYUIOPASDFGH";
        let _ = tracing_log::LogTracer::init(); // the bridge `.init()` installs in production
        let base = std::env::temp_dir().join(format!("kaban-log-cap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(base.join(".gitignore"), "notes-*.md\n").unwrap();
        std::fs::write(base.join(format!("notes-{CANARY}.md")), "# ignored\n").unwrap();
        std::fs::write(base.join("clean.md"), "# clean\n").unwrap();

        let logs = LogBuf::default();
        let sink = logs.clone();
        let subscriber = super::stderr_subscriber(
            tracing_subscriber::EnvFilter::new("trace"),
            move || sink.clone(),
            false,
        );
        let candidates = tracing::subscriber::with_default(subscriber, || {
            kaban_ingest::walk(&kaban_ingest::WalkConfig::new(base.clone()))
                .unwrap()
                .flatten()
                .count()
        });
        assert_eq!(candidates, 1, "only clean.md is a candidate");
        let text = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
        assert!(
            text.contains("walk complete"),
            "kaban's own debug event must still be logged: {text}"
        );
        assert!(
            !text.contains(CANARY) && !text.contains("notes-"),
            "an ignored path reached the log: {text}"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn default_workspace_is_the_repo_root_name_or_default() {
        let base = std::env::temp_dir().join(format!("kaban-ws-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let repo = base.join("nightjar-service");
        let nested = repo.join("crates").join("deep");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        assert_eq!(default_workspace(&nested).as_str(), "nightjar-service");
        assert_eq!(default_workspace(&repo).as_str(), "nightjar-service");
        let plain = base.join("no-repo-here");
        std::fs::create_dir_all(&plain).unwrap();
        assert_eq!(default_workspace(&plain).as_str(), "default");
        let _ = std::fs::remove_dir_all(&base);
    }
}
