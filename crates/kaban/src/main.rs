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
    /// Optional JSONL copy of audit entries for external shipping (rule ids and keyed hashes
    /// only). The chain of record is the `audit` table inside the database; see `verify-audit`.
    #[arg(long, alias = "audit", env = "KABAN_AUDIT_EXPORT")]
    audit_export: Option<PathBuf>,
    /// Workspace this server is bound to (core-spec S6). Default: the name of the enclosing git
    /// repository's root directory, or `default` outside a repository.
    #[arg(long, env = "KABAN_WORKSPACE")]
    workspace: Option<String>,
    /// The writer identity of this process (spec 05 SS-26), stamped on every record and change
    /// row it writes, for the CLI and `serve` alike; swarms set one per agent. Never inferred
    /// from an agent id, a session, a PID or the transport.
    #[arg(long, env = "KABAN_ACTOR_ID", default_value = kaban_core::ActorId::LOCAL, value_parser = parse_actor_id)]
    actor_id: kaban_core::ActorId,
    /// Read from the immutable pack at this directory instead of --db (spec 05 SS-19): the
    /// read tools search its snapshot opened read-only; nothing is migrated or written
    #[arg(long, global = true, value_name = "DIR")]
    pack: Option<PathBuf>,
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
    /// Recompute the database's audit chain (exit 5 when a row fails)
    VerifyAudit,
    /// Immutable packs (spec 05 SS-18 to SS-20): build one from --db, verify one, or fork
    /// one into a writable database
    Pack {
        #[command(subcommand)]
        cmd: cli::pack::PackCmd,
    },
    /// Re-judge every node stored under another policy version with the loaded policy
    /// (reject hits are tombstoned, redact hits masked in place); `serve` runs the same job in
    /// the background at startup
    Rescan,
    /// Store a memory (the `remember` tool; actor agent). Exit 0 ok, 2 rejected by policy, 4
    /// invalid request, 5 store error
    Remember(cli::remember::Flags),
    /// The governing records for paths and queries (the `why` tool). Exit 0 ok, 2 rejected by
    /// policy, 4 invalid request, 5 store error
    Why(cli::why::Flags),
    /// The SUPERSEDES chain and the governance events of the records one selector reaches
    /// (the `governance_history` tool). Exit 0 ok, 2 rejected by policy, 3 not found, 4
    /// invalid request, 5 store error
    GovernanceHistory(cli::governance_history::Flags),
    /// Search memories (the `recall` tool). Exit 0 ok, 2 rejected by policy, 4 invalid
    /// request, 5 store error
    Recall(cli::recall::Flags),
    /// Tombstone, invalidate, archive or purge a memory (the `forget` tool; actor agent). Exit
    /// 0 ok, 2 rejected by policy, 3 not found, 4 invalid request, 5 store error
    Forget(cli::forget::Flags),
    /// Inspect, set or clear durable automatic-pruning protection (not replacement protection).
    Protect(cli::protect::Flags),
    /// Reverse a merge (engine-spec EN-F51): delete the node's identity redirect, deprecate the
    /// identity claim under --reason and re-decide its group; both nodes stay. A human action.
    /// Exit 0 ok, 2 rejected by policy, 3 not found, 4 invalid request, 5 store error
    Unmerge(cli::unmerge::Flags),
    /// Explain the status of anything addressable (a node id, e:<edge id>, a claim key,
    /// x:Kind or <path>:<offset>): the status with its reason sentence, the conflict group's
    /// explanation where one exists, the history and the exact next actions; or deprecate a
    /// claim as a person with --deprecate <reason> (actor human-cli): it stops voting and its
    /// group is re-decided. Exit 0 ok, 2 rejected by policy, 3 not found, 4 invalid request,
    /// 5 store error
    Explain(cli::explain::Flags),
    /// List every pending item of the bound workspace with its one sentence and its first
    /// next action (unresolved, candidate, conflict, stale, proposed, discovered). Exit 0 ok,
    /// 2 rejected by policy, 4 invalid request, 5 store error
    Review(cli::review::Flags),
    /// Accept a governed record as a person (signer human-cli), or redeem the confirmation
    /// token an agent received (--confirmation-token or --token-stdin, with
    /// --confirmed-by-user; signer human-via-chat). Exit 0 ok, 2 rejected by policy, 3 not
    /// found, 4 invalid request, 5 store error
    Accept {
        /// Node id of the Decision, Convention or Failure (omit with --stdin-json)
        id: Option<String>,
        /// Why it is accepted (required unless a token is redeemed)
        #[arg(long)]
        reason: Option<String>,
        /// Path or glob the record governs; repeat for several (required unless a token is
        /// redeemed)
        #[arg(long = "path-scope")]
        path_scope: Vec<String>,
        /// Declared signer identity (free text)
        #[arg(long)]
        signer: Option<String>,
        /// The one-time token from the agent's confirmation-required response (64 hex digits).
        /// Parsed infallibly into zeroizing memory, so clap never formats the value; the seam
        /// judges it with the whole raw request (review R3-1)
        #[arg(long)]
        confirmation_token: Option<kaban_api::RawSecret>,
        /// Read the token from stdin instead (one line, at most 1 KiB), so it never appears in
        /// the process arguments (OWN-20): `... | kaban accept <id> --token-stdin
        /// --confirmed-by-user`. Not with --confirmation-token or --stdin-json
        #[arg(long, default_value_t = false, conflicts_with = "confirmation_token")]
        token_stdin: bool,
        /// You confirm the acceptance; redeems --confirmation-token or --token-stdin
        #[arg(long, default_value_t = false)]
        confirmed_by_user: bool,
    },
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
    /// Render the accepted records of the exportable kinds (the shipped ontology: Decisions
    /// and Conventions) into a managed marker block (core-spec F9a): printed, or written into
    /// a user file with --write
    Export {
        /// Block format
        #[arg(long, value_enum, default_value_t = cli::export::FormatArg::AgentsMd)]
        format: cli::export::FormatArg,
        /// Write the block into this file (created when missing; text outside the block is
        /// never touched) instead of printing it
        #[arg(long)]
        write: Option<PathBuf>,
        /// Render only the kinds of these ontologies: `<name>[,<name>...]` or `all`
        /// (default); an unloaded name is refused (exit 2)
        #[arg(long, value_name = "NAME[,NAME...]|all")]
        ontology: Option<String>,
    },
    /// Write the stale `.kaban/<ontology>/<dir>/<id>.md` records and the per-ontology digests
    /// `.kaban/<ontology>/INDEX.md` from the database and import the files another clone
    /// advanced (core-spec F13); the first run under the engine moves a pre-engine
    /// `.kaban/<dir>/` layout once (`git mv` inside a repository)
    Sync {
        /// Also `git add` the written files, so they land in the triggering commit (the
        /// pre-commit hook's mode)
        #[arg(long)]
        stage: bool,
        /// Only the layout move, the digests and the `.gitignore`: no import, no record files
        #[arg(long)]
        digest_only: bool,
        /// Generate the digests of these ontologies only: `<name>[,<name>...]` or `all`
        /// (default); the record files are always written for every ontology; an unloaded
        /// name is refused (exit 2)
        #[arg(long, value_name = "NAME[,NAME...]|all")]
        ontology: Option<String>,
        /// Repository root (default: the git repository enclosing the current directory)
        #[arg(long)]
        repo: Option<PathBuf>,
        /// Hard wall-clock bound of the whole run in seconds (the Stop hooks pass 60): at the
        /// deadline the process prints a fixed line on stderr and exits 4; partial state is
        /// safe (files land by rename, the store is transactional, the repository lock is
        /// released with the process) and the next sync continues
        #[arg(long, env = "KABAN_SYNC_DEADLINE", value_name = "SECONDS")]
        deadline: Option<u64>,
    },
    /// Import every `.kaban/*.md` file into the database (the explicit full import, core-spec
    /// F13). The absence pass tombstones records whose files git deleted, guarded by their
    /// serialized revision. Does not write repository files.
    Import {
        /// Repository root (default: the git repository enclosing the current directory)
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// Resolve an equal-rev conflict between a record's file and the database (core-spec
    /// F13); not available in this build until C5.1b lands (M5)
    Resolve {
        /// Node id of the record in conflict (or of the node whose candidate group is decided,
        /// or of the stale code reference to re-target)
        id: Option<String>,
        /// Which side wins (an equal-rev file/database conflict); exactly one of `--take`,
        /// `--winner` and `--symbol`
        #[arg(
            long,
            value_enum,
            conflicts_with_all = ["winner", "symbol"],
            required_unless_present_any = ["winner", "symbol"]
        )]
        take: Option<cli::resolve::Take>,
        /// The value a human names as the winner of the node's candidate group (engine-plan
        /// E2.5; a human claim, agents are refused); exactly one of `--take`, `--winner` and
        /// `--symbol`
        #[arg(
            long,
            conflicts_with_all = ["take", "symbol"],
            required_unless_present_any = ["take", "symbol"]
        )]
        winner: Option<String>,
        /// The normalised key a stale code reference's symbol has now (engine-plan E2.5, SO-6; a
        /// human claim, agents are refused; a versioned or `local` key is refused); exactly
        /// one of `--take`, `--winner` and `--symbol`
        #[arg(
            long,
            conflicts_with_all = ["take", "winner"],
            required_unless_present_any = ["take", "winner"]
        )]
        symbol: Option<String>,
        /// With `--winner`: the candidate group to decide, the key `kaban review` lists on the
        /// item (needed when the node has several candidate groups; the rendered action
        /// carries it)
        #[arg(long, requires = "winner")]
        group: Option<String>,
        /// Repository root (default: the git repository enclosing the current directory)
        #[arg(long)]
        repo: Option<PathBuf>,
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
    /// Promote a record to the global database; not available in this build (M6)
    Promote {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, hide = true)]
        rest: Vec<String>,
    },
    /// Ingest ADRs and decision markers as proposed Decisions (core-spec F10); not available
    /// in this build (the assisted ingestion spec is pending, OWN-26)
    IngestDecisions {
        /// Repository root (default: the git repository enclosing the current directory)
        #[arg(long)]
        repo: Option<PathBuf>,
        /// ADR directory, repository-relative; repeat for several (default: docs/adr, doc/adr,
        /// adr, docs/decisions)
        #[arg(long = "dir")]
        dirs: Vec<PathBuf>,
        /// Skip the `WHY:` / `DECISION:` / `TRADEOFF:` / `REJECTED:` source markers
        #[arg(long)]
        no_markers: bool,
    },
}

/// `kaban bench cg ...` (stage1-plan §18, CG7.3; feature `bench`) and `kaban bench why-paired`
/// (engine-spec EN-N7; engine-plan E2.7, no feature).
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
    /// The paired `why` bench (EN-N7): the same path target on a schema-3 copy of the bench
    /// database and on its migrated copy, alternating rounds in one process; both series,
    /// both p50s, the ratio and an informational pass against the threshold
    WhyPaired {
        /// The schema-3 copy (`kaban bench --schema-version 3` leaves it as `<db>.bak-3-<ts>`)
        #[arg(long)]
        before: PathBuf,
        /// The migrated copy (the bench database itself after that run)
        #[arg(long)]
        after: PathBuf,
        /// Timed rounds, one path target each
        #[arg(long, default_value_t = 30)]
        samples: usize,
        /// Untimed calls per copy before the rounds
        #[arg(long, default_value_t = 3)]
        warmup: usize,
        /// `pass` when `p50_after <= p50_before * (1 + pct / 100)`
        #[arg(long, default_value_t = 10)]
        regression_pct: u32,
        /// Also write the result document here
        #[arg(long)]
        out: Option<PathBuf>,
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
    /// The subcommand's own name, for its fixed messages (never caller data).
    const fn name(&self) -> &'static str {
        match self {
            Self::Stats(_) => "stats",
            Self::Reindex { .. } => "reindex",
            Self::Serve { .. } => "serve",
            Self::Bench { .. } => "bench",
            Self::Scan => "scan",
            Self::VerifyAudit => "verify-audit",
            Self::Pack { .. } => "pack",
            Self::Rescan => "rescan",
            Self::Remember(_) => "remember",
            Self::Why(_) => "why",
            Self::GovernanceHistory(_) => "governance-history",
            Self::Recall(_) => "recall",
            Self::Forget(_) => "forget",
            Self::Protect(_) => "protect",
            Self::Unmerge(_) => "unmerge",
            Self::Explain(_) => "explain",
            Self::Review(_) => "review",
            Self::Accept { .. } => "accept",
            Self::Doctor { .. } => "doctor",
            Self::Hook { .. } => "hook",
            Self::Export { .. } => "export",
            Self::Sync { .. } => "sync",
            Self::Import { .. } => "import",
            Self::Resolve { .. } => "resolve",
            Self::Ontology { .. } => "ontology",
            #[cfg(feature = "ingest")]
            Self::Ingest(_) => "ingest",
            Self::Promote { .. } => "promote",
            Self::IngestDecisions { .. } => "ingest-decisions",
        }
    }

    /// How the subcommand relates to `--pack` (review e-3): a reader opens the pack through
    /// [`open_api`] (a write sub-mode of `protect`, `explain --deprecate` or `ontology load`
    /// is then refused by the immutable store); `verify-audit` verifies the pack's chain
    /// ([`cli::verify_audit::run_pack`]); everything else either writes or works on `--db`
    /// alone and is refused before any database is opened, never redirected to `--db`.
    const fn pack_mode(&self) -> PackMode {
        match self {
            Self::Serve { .. }
            | Self::Scan
            | Self::Stats(_)
            | Self::Why(_)
            | Self::GovernanceHistory(_)
            | Self::Recall(_)
            | Self::Explain(_)
            | Self::Review(_)
            | Self::Export { .. }
            | Self::Ontology { .. }
            | Self::Protect(_) => PackMode::Reads,
            Self::VerifyAudit => PackMode::VerifiesChain,
            Self::Doctor { .. } | Self::Pack { .. } | Self::Bench { .. } | Self::Hook { .. } => {
                PackMode::WorksOnDb
            }
            Self::Reindex { .. }
            | Self::Rescan
            | Self::Remember(_)
            | Self::Forget(_)
            | Self::Unmerge(_)
            | Self::Accept { .. }
            | Self::Sync { .. }
            | Self::Import { .. }
            | Self::Resolve { .. }
            | Self::Promote { .. }
            | Self::IngestDecisions { .. } => PackMode::Writes,
            #[cfg(feature = "ingest")]
            Self::Ingest(_) => PackMode::Writes,
        }
    }

    /// Whether the subcommand reads its request from `--stdin-json` (M4-4); for every other
    /// subcommand the flag is refused rather than ignored.
    fn reads_body(&self) -> bool {
        #[cfg(feature = "ingest")]
        if let Self::Ingest(flags) = self {
            return flags.is_submit();
        }
        matches!(
            self,
            Self::Accept { .. }
                | Self::Remember(_)
                | Self::Why(_)
                | Self::GovernanceHistory(_)
                | Self::Recall(_)
                | Self::Forget(_)
                | Self::Protect(_)
                | Self::Unmerge(_)
                | Self::Hook { .. }
        )
    }
}

/// What a subcommand does with `--pack` ([`Cmd::pack_mode`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PackMode {
    /// Reads through the pack opened by [`open_api`].
    Reads,
    /// `verify-audit`: the pack's chain under the pack instance's key.
    VerifiesChain,
    /// Opens `--db` itself (`doctor`, `pack`, `bench`, `hook`): refused with `--pack`.
    WorksOnDb,
    /// Writes to a database: refused with `--pack` (fork the pack first).
    Writes,
}

/// `--pack` before the dispatch (review e-3, SS-19): the pack is read-only and bound to its
/// own files, so only the readers reach [`cli::run`], `verify-audit` verifies the pack's chain
/// here, and every other subcommand is refused with a fixed text (exit 4) before any database
/// is opened. Without `--pack` this is [`cli::run`].
fn pack_dispatch(cli: Cli) -> ExitCode {
    let Some(pack) = cli.pack.clone() else {
        return cli::run(cli);
    };
    let cmd = cli.cmd.as_ref().unwrap_or(&Cmd::Serve { http: false });
    let name = cmd.name();
    let message = match cmd.pack_mode() {
        PackMode::Reads => return cli::run(cli),
        PackMode::VerifiesChain => {
            let format = if cli.json {
                cli::output::Format::Compact
            } else {
                cli::output::Format::Pretty
            };
            return match kaban_home() {
                Ok(home) => cli::verify_audit::run_pack(&pack, &home.join("keys"), format),
                Err(e) => {
                    cli::output::print_failure(&e);
                    ExitCode::from(cli::output::EXIT_INVALID)
                }
            };
        }
        PackMode::WorksOnDb => format!(
            "--pack is read-only and `{name}` works on --db: drop --pack (a pack is checked with `kaban pack verify <dir>`)"
        ),
        PackMode::Writes => format!(
            "--pack is read-only and `{name}` would write: fork the pack (`kaban pack fork <dir> <dest.db>`) and run it against --db <dest.db>"
        ),
    };
    cli::output::print_failure(&message);
    ExitCode::from(cli::output::EXIT_INVALID)
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

/// Lexically normalised absolute form of `p` (`.` and `..` folded, symlinks not followed).
fn normalized(p: &std::path::Path) -> PathBuf {
    use std::path::Component;
    let abs = std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf());
    let mut out = PathBuf::new();
    for c in abs.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// Refuse an `--audit-export` path that aliases the database or one of its `-wal`/`-shm`
/// sidecars (C2.5b, review P-2): by normalised name, by `(dev, inode)` identity (a hard link),
/// or because it is a symlink (whatever it points at). Appending audit lines to those files
/// would corrupt SQLite's bytes, and closing the export descriptor would release every POSIX
/// record lock this process holds on them (C2.5a).
///
/// # Errors
/// A message naming the option, never file contents.
fn check_audit_export_path(export: &std::path::Path, db: &std::path::Path) -> anyhow::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let mut protected: Vec<PathBuf> = Vec::with_capacity(3);
    for suffix in ["", "-wal", "-shm"] {
        let mut name = db.as_os_str().to_owned();
        name.push(suffix);
        protected.push(PathBuf::from(name));
    }
    let export_norm = normalized(export);
    if protected.iter().any(|p| normalized(p) == export_norm) {
        anyhow::bail!(
            "--audit-export must not name the database or its -wal/-shm sidecar ({})",
            export.display()
        );
    }
    match std::fs::symlink_metadata(export) {
        Ok(meta) if meta.file_type().is_symlink() => {
            anyhow::bail!(
                "--audit-export must not be a symlink ({})",
                export.display()
            );
        }
        Ok(meta) => {
            let ident = (meta.dev(), meta.ino());
            let aliased = protected
                .iter()
                .filter_map(|p| std::fs::symlink_metadata(p).ok())
                .any(|m| (m.dev(), m.ino()) == ident);
            if aliased {
                anyhow::bail!(
                    "--audit-export is the same file as the database or a sidecar ({})",
                    export.display()
                );
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(e).with_context(|| format!("--audit-export {}", export.display()));
        }
    }
    Ok(())
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
    /// `--audit-export` aliases the database (C2.5b, P-2) or `--workspace` is not a name.
    Config(anyhow::Error),
    /// The database or its key could not be opened.
    Store(kaban_core::StoreError),
    /// The pack named by `--pack` does not verify or cannot be opened read-only.
    Pack(kaban_store_sqlite::pack::PackError),
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
            Self::Pack(e) => cli::output::print_failure(&format_args!("opening pack: {e}")),
        }
    }
}

/// Assemble the service for `serve` and `rescan`: the audit-export alias check, the code-graph
/// provider (its key read, never created; a loose key is a config error before the store is
/// opened), the store with its key, the optional export and the bound workspace. With
/// `--pack` (SS-18, SS-19; review e-4) the store is the verified pack opened immutable and the
/// code side is the pack's own: a provider over its bundled `snapshots/` with the head pinned
/// to the manifest's base commit, never the live repository's provider, and no provider at
/// all (the code tools report absence) when the pack bundles no snapshot. `serving` says this
/// process is the server (SS-21): the mode-B projection loop runs beside the write barrier's
/// drain; a one-shot command drives its own projection through the drain and exits.
fn open_api(
    cli: Cli,
    policy: kaban_policy::Policy,
    serving: bool,
) -> Result<kaban_api::KabanApi, OpenError> {
    if let Some(export) = &cli.audit_export
        && let Err(e) = check_audit_export_path(export, &cli.db)
    {
        return Err(OpenError::Config(e));
    }
    let home = kaban_home().map_err(OpenError::Other)?;
    // the code side of the join (stage1-plan CG5.2, G1-F8, G1-S7): the only place a provider
    // is built; hooks (`assemble_api` callers) never get one, and a pack brings its own
    let cwd = std::env::current_dir().map_err(|e| OpenError::Other(e.into()))?;
    let codegraph = match &cli.pack {
        Some(_) => None,
        None => codegraph::wire(
            &home,
            repo_root(&cwd).as_deref(),
            &policy,
            &kaban_api::GitRunner::default(),
        )
        .map_err(|e| OpenError::Config(e.into()))?,
    };
    let keys_dir = home.join("keys");
    // the one engine configuration, loaded before the store so its `[er]` section reaches the
    // store's startup seam (E2.10 boundary review BR-7): an invalid section is a config error
    // (exit 4) naming the loader's category and the field path, never a value
    let engine = load_engine()?;
    let er = er_priors(&engine.er)?;
    // `--pack` (SS-19): the verified pack opened immutable; no trigram deployment, no startup
    // ontology load, since neither may write. Its bundled snapshots (when any) are the code
    // side, pinned to the manifest's base (SS-18): the provider walks its directory from `/`,
    // so the pack path is made absolute first. Its captured text index (when the manifest
    // selects one) is attached as it stands, read only (T6 review t6-2)
    let mut bundled = None;
    let store = if let Some(pack) = &cli.pack {
        let pack = std::path::absolute(pack).unwrap_or_else(|_| pack.clone());
        let (store, manifest) =
            kaban_store_sqlite::SqliteStore::open_pack(&pack, cli::pack::ADMISSION)
                .map_err(OpenError::Pack)?;
        let store = store
            .with_er_priors(er)
            .with_recall_config(engine.recall.clone())
            .map_err(OpenError::Store)?;
        let store = Arc::new(store);
        if !manifest.snapshots.is_empty() {
            bundled = Some((
                pack.join(kaban_store_sqlite::pack::SNAPSHOT_DIR),
                manifest.base_commit,
            ));
        }
        store
    } else {
        // the store owns its text index (spec 05 OWN-M): opened, admitted or rebuilt by the
        // open, published by the store; a serving process runs the store's projector thread
        // (SS-21; the caller says which, since `cli.cmd` was taken by the dispatch: fix11),
        // every other command publishes its own commits inline and exits
        let store = kaban_store_sqlite::SqliteStore::open_as(&cli.db, &keys_dir, &cli.actor_id)
            .map_err(OpenError::Store)?
            .with_er_priors(er)
            .with_recall_config(engine.recall.clone())
            .map_err(OpenError::Store)?;
        Arc::new(store.serving(serving).map_err(OpenError::Store)?)
    };
    let from_pack = cli.pack.is_some();
    // the one registry of this process (engine-plan E1.5): the service reads every mixin
    // decision from it, and `kaban ontology load` loads through it
    let registry = kaban_api::OntologyRegistry::new(ontology_root()).map_err(open_error)?;
    let api = assemble_api_with(cli, policy, store, engine)?.with_registry(Arc::new(registry));
    let api = match (codegraph, bundled) {
        (Some(wiring), _) => api.with_snapshot_provider(wiring.dir, wiring.provider),
        (None, Some((snapshots, base_commit))) => api
            .with_snapshot_provider(
                snapshots.clone(),
                Arc::new(kaban_codegraph::DiscoverProvider::new(snapshots)),
            )
            .with_pinned_head(&base_commit),
        (None, None) => api,
    };
    // the startup ontology load (engine-spec EN-F2): `.kaban/ontology/*.yaml` of the enclosing
    // repository in sorted path order; hooks (`assemble_api` callers) skip it, as they never
    // create or migrate anything either
    if !from_pack {
        load_ontologies(&api)?;
    }
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

/// [`open_api`] after the store is open: the migration log line, the optional export and the
/// bound workspace (core-spec S6; the same rule for the MCP server and the hooks, M4-32). The
/// `--audit-export` alias check is the caller's: it belongs before the store is opened.
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
    let export = cli
        .audit_export
        .map(|p| kaban_policy::AuditExport::new(p, signer.clone()));
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
        export,
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
        Ok(cli) => pack_dispatch(cli),
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
