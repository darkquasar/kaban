//! CLI adapter (core-spec F7; core-plan C4.1 as amended by M4-1 to M4-5, M4-25). [`run`] is the
//! dispatch: it maps every subcommand of [`crate::Cmd`] to its module, loads the policy, opens the
//! service through [`crate::open_api`] and turns every failure into the exit codes of core-spec
//! §6 (one mapping, [`output::exit_code`]). Subcommands that map to a tool go through
//! [`seam::run_tool`] (M4-3); stdout carries exactly one JSON document, written by [`output`]
//! only. The tool subcommands `remember`, `recall`, `forget` and `stats` (C4.1b) take their
//! request from flags ([`ToolArgs`]) or from `--stdin-json` through [`run_tool_command`]. The
//! unshipped flags and modes exit 4 through [`stubs`]; `ingest` (docs-ingest DI1.6, behind the
//! `ingest` feature) runs the document pipeline as the tool `index_docs`.

pub(crate) mod doctor;
pub(crate) mod forget;
pub(crate) mod hook;
#[cfg(feature = "ingest")]
pub(crate) mod ingest;
pub(crate) mod ontology;
pub(crate) mod output;
pub(crate) mod recall;
pub(crate) mod reindex;
pub(crate) mod remember;
pub(crate) mod rescan;
pub(crate) mod scan;
pub(crate) mod seam;
pub(crate) mod stats;
pub(crate) mod stubs;

use crate::{Cli, Cmd, OpenError};
use clap::error::ErrorKind;
use output::{EXIT_FAILURE, EXIT_INVALID, Format};
use serde_json::Value;
use std::process::ExitCode;

/// The clap arguments of a subcommand that maps to a tool (`remember`, `recall`, `forget`,
/// `stats`; C4.1b). They build the raw request the MCP adapter would receive; the request body
/// comes from exactly one place, the flags or `--stdin-json` (M4-4), which
/// [`run_tool_command`] enforces for every implementor.
pub(crate) trait ToolArgs {
    /// The subcommand's own name, for its fixed messages (never caller data).
    const NAME: &'static str;
    /// Whether any argument was given (then `--stdin-json` is refused).
    fn any_given(&self) -> bool;
    /// The raw request as one JSON object. A missing required argument or a structured flag
    /// that does not parse is reported with a fixed text and maps to exit 4; the content of an
    /// argument is never echoed here, because the seam has not judged it yet.
    fn into_raw(self) -> Result<Value, ExitCode>;
}

/// Why `Cli::try_parse` failed, as a fixed text keyed by the error kind (M4-2): clap's own
/// message would echo the argument value (a pasted secret, M3 C3.10), so no value, argument
/// name or subcommand name from the command line is printed. `--help` and `--version` are not
/// failures and are handled by the caller.
fn parse_error_text(kind: ErrorKind) -> &'static str {
    match kind {
        ErrorKind::InvalidValue | ErrorKind::ValueValidation => {
            "an argument value is invalid for its option"
        }
        ErrorKind::UnknownArgument => "an argument is not recognized",
        ErrorKind::InvalidSubcommand => "the subcommand is not recognized",
        ErrorKind::NoEquals => "an option that takes a value needs `=` or a following value",
        ErrorKind::TooManyValues | ErrorKind::TooFewValues | ErrorKind::WrongNumberOfValues => {
            "an option received the wrong number of values"
        }
        ErrorKind::ArgumentConflict => "two of the given arguments cannot be used together",
        ErrorKind::MissingRequiredArgument => "a required argument is missing",
        ErrorKind::MissingSubcommand | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand => {
            "a subcommand is required"
        }
        ErrorKind::InvalidUtf8 => "an argument is not valid UTF-8",
        ErrorKind::Io | ErrorKind::Format => "the command line could not be processed",
        // `DisplayHelp`/`DisplayVersion` are handled before this text is used; the enum is
        // `non_exhaustive`
        _ => "the command line is invalid",
    }
}

/// Finish a `Cli::try_parse` failure: help and version print their fixed text and exit 0,
/// every other kind prints [`parse_error_text`] with a pointer to `--help` and exits 4.
pub(crate) fn parse_failure(e: &clap::Error) -> ExitCode {
    match e.kind() {
        ErrorKind::DisplayHelp | ErrorKind::DisplayVersion => match output::print_clap_help(e) {
            Ok(()) => ExitCode::SUCCESS,
            Err(_) => ExitCode::from(EXIT_FAILURE),
        },
        kind => {
            output::print_error(&format!(
                "error: {}; run `kaban --help` for usage",
                parse_error_text(kind)
            ));
            ExitCode::from(EXIT_INVALID)
        }
    }
}

/// Load `--policy` (builtin rules only when absent); a file that does not load is reported
/// with the policy error's own message (rule ids and field names, core-spec S7) and is a
/// config error (exit 4) for every subcommand.
fn load_policy(path: Option<&std::path::Path>) -> Result<kaban_policy::Policy, ExitCode> {
    match path {
        Some(p) => kaban_policy::Policy::load(p).map_err(|e| {
            output::print_failure(&format_args!("loading policy {}: {e}", p.display()));
            ExitCode::from(EXIT_INVALID)
        }),
        None => Ok(kaban_policy::Policy::empty()),
    }
}

/// Open the service for a subcommand (`serving`: the server's open, see [`crate::open_api`]),
/// reporting a failure with its outer message (RM-1) and choosing its exit code: a refused
/// configuration or environment is 4, a store or key failure 5.
fn open_or_exit(
    cli: Cli,
    policy: kaban_policy::Policy,
    serving: bool,
) -> Result<kaban_api::KabanApi, ExitCode> {
    let db = cli.db.clone();
    crate::open_api(cli, policy, serving).map_err(|e| {
        e.report(&db);
        ExitCode::from(match e {
            OpenError::Config(_) | OpenError::Other(_) => EXIT_INVALID,
            OpenError::Rejected(_) => output::EXIT_REJECTED,
            OpenError::Store(e) => output::store_exit_code(&e),
        })
    })
}

/// The key directory for the commands that open a store themselves (`open_api` resolves its
/// own); an unset `KABAN_HOME`/`HOME` is a config error.
fn keys_dir() -> Result<std::path::PathBuf, ExitCode> {
    crate::kaban_home()
        .map(|home| home.join("keys"))
        .map_err(|e| {
            output::print_failure(&e);
            ExitCode::from(EXIT_INVALID)
        })
}

/// Load the policy, then open the service for a one-shot command (the order the messages are
/// most useful in).
fn open_service(cli: Cli) -> Result<kaban_api::KabanApi, ExitCode> {
    let policy = load_policy(cli.policy.as_deref())?;
    open_or_exit(cli, policy, false)
}

/// [`open_service`] for the serving process (`kaban serve`, or `kaban` alone): the mode-B
/// projection loop runs beside the write barrier's drain (SS-21).
fn open_server(cli: Cli) -> Result<kaban_api::KabanApi, ExitCode> {
    let policy = load_policy(cli.policy.as_deref())?;
    open_or_exit(cli, policy, true)
}

/// Run the parsed command line to completion and return the process exit code. Nothing here
/// returns an error to the caller: every failure is printed with its outer message and mapped
/// (review RM-1).
pub(crate) fn run(mut cli: Cli) -> ExitCode {
    let t0 = std::time::Instant::now();
    let format = if cli.json {
        Format::Compact
    } else {
        Format::Pretty
    };
    if cli.global_db.is_some() {
        return stubs::not_available("--global-db", "M6");
    }
    let cmd = cli.cmd.take().unwrap_or(Cmd::Serve { http: false });
    if cli.stdin_json && !cmd.reads_body() {
        output::print_error("error: --stdin-json is not accepted by this subcommand");
        return ExitCode::from(EXIT_INVALID);
    }
    match cmd {
        // `scan` validates the policy itself (exit 4 on error) instead of failing like a server
        Cmd::Scan => finish(scan::run(cli.policy.as_deref(), format)),
        Cmd::Bench {
            n,
            edges_per_node,
            chunks,
            schema_version,
            cg,
        } => {
            let (Ok(policy), Ok(keys)) = (load_policy(cli.policy.as_deref()), keys_dir()) else {
                return ExitCode::from(EXIT_INVALID);
            };
            if matches!(
                &cg,
                Some(crate::BenchCg::Recall(_) | crate::BenchCg::Edges(_))
            ) {
                let engine = match crate::load_engine() {
                    Ok(engine) => engine,
                    Err(error) => {
                        error.report(&cli.db);
                        return ExitCode::from(EXIT_INVALID);
                    }
                };
                return crate::bench::recall_runtime::dispatch(
                    &cli.db,
                    &keys,
                    &cli.actor_id,
                    policy,
                    &engine,
                    cg.as_ref(),
                );
            }
            // the code-graph bench (stage1-plan §18, CG7.3) lives behind the `bench` feature
            if let Some(cg) = cg {
                #[cfg(feature = "bench")]
                {
                    return crate::bench::cg::dispatch(
                        &cli.db,
                        &keys,
                        &cli.actor_id,
                        policy,
                        cg,
                        format,
                    );
                }
                #[cfg(not(feature = "bench"))]
                {
                    let _ = cg;
                    return stubs::not_available("bench cg", "feature bench");
                }
            }
            finish(
                crate::bench::run(
                    &cli.db,
                    &keys,
                    &cli.actor_id,
                    policy,
                    cli.policy.as_deref(),
                    &crate::bench::Shape {
                        notes: n,
                        edges_per_node,
                        chunks,
                        schema_version,
                    },
                )
                .map(|()| ExitCode::SUCCESS),
            )
        }
        // the explicit job: a policy that does not load is a config error (exit 4), the store
        // decides exit 5 inside `rescan::run`
        Cmd::Rescan => match open_service(cli) {
            Ok(api) => finish(rescan::run(&api, format)),
            Err(code) => code,
        },
        Cmd::Remember(flags) => run_tool_command(cli, format, flags, remember::run),
        Cmd::Recall(flags) => run_tool_command(cli, format, flags, recall::run),
        Cmd::Stats(flags) => run_tool_command(cli, format, flags, stats::run),
        Cmd::Forget(flags) => run_tool_command(cli, format, flags, forget::run),
        Cmd::Serve { http: true } => stubs::not_available("serve --http", "M7"),
        Cmd::Serve { http: false } => match open_server(cli) {
            Ok(api) => {
                tracing::info!(
                    startup_ms = t0.elapsed().as_millis(),
                    workspace = %api.workspace(),
                    "kaban ready"
                );
                crate::codegraph::announce();
                finish(
                    tokio::runtime::Builder::new_multi_thread()
                        .enable_all()
                        .build()
                        .map_err(anyhow::Error::from)
                        .and_then(|rt| rt.block_on(crate::mcp::serve_stdio(api)))
                        .map(|()| ExitCode::SUCCESS),
                )
            }
            Err(code) => code,
        },
        // its own open (diagnostic, never creating or migrating); a policy that does not load
        // is a config error like everywhere else
        Cmd::Doctor {
            vacuum,
            priors,
            optimize,
        } => match (load_policy(cli.policy.as_deref()), keys_dir()) {
            (Ok(policy), Ok(keys)) => {
                doctor::run(&cli, policy, &keys, vacuum, priors, optimize, format)
            }
            (Err(code), _) | (_, Err(code)) => code,
        },
        // every hook path exits 0 (M4-8); the module owns the open, the watchdog and the body
        Cmd::Hook { event, host } => hook::run(cli, event, host),
        Cmd::Reindex { command } => reindex::run(cli, &command, format),
        // E1.3: the registry runs through the tool seam like every tool subcommand
        Cmd::Ontology { cmd } => match open_service(cli) {
            Ok(api) => ontology::run(&api, cmd, format),
            Err(code) => code,
        },
        // DI1.6: the document pipeline runs through the tool seam as `index_docs`
        #[cfg(feature = "ingest")]
        Cmd::Ingest(flags) => {
            let body = if flags.is_submit() {
                if !cli.stdin_json {
                    output::print_error("error: ingest job submit requires --stdin-json");
                    return ExitCode::from(EXIT_INVALID);
                }
                match seam::read_stdin_json() {
                    Ok(raw) => Some(raw),
                    Err(code) => return code,
                }
            } else {
                None
            };
            match open_service(cli) {
                Ok(api) => {
                    if let Some(raw) = body {
                        ingest::run_submit(&api, flags, raw, format)
                    } else {
                        ingest::run(&api, flags, format)
                    }
                }
                Err(code) => code,
            }
        }
    }
}

/// Run a tool subcommand: take its raw request from the flags, or from `--stdin-json` when no
/// flag is given (both at once exit 4; an oversized body fails before the database is opened),
/// open the service and hand the request to the module's seam call.
fn run_tool_command<A: ToolArgs>(
    cli: Cli,
    format: Format,
    args: A,
    run: fn(&kaban_api::KabanApi, Value, Format) -> ExitCode,
) -> ExitCode {
    let raw = if cli.stdin_json {
        if args.any_given() {
            output::print_error(&format!(
                "error: {} takes either --stdin-json or its arguments, not both",
                A::NAME
            ));
            return ExitCode::from(EXIT_INVALID);
        }
        match seam::read_stdin_json() {
            Ok(raw) => raw,
            Err(code) => return code,
        }
    } else {
        match args.into_raw() {
            Ok(raw) => raw,
            Err(code) => return code,
        }
    };
    match open_service(cli) {
        Ok(api) => run(&api, raw, format),
        Err(code) => code,
    }
}

/// Finish a subcommand that reports unexpected failures as `anyhow::Error`: print the outer
/// message only and map the typed error in its chain to an exit code (RM-1, M4-2).
fn finish(outcome: anyhow::Result<ExitCode>) -> ExitCode {
    outcome.unwrap_or_else(|e| {
        output::print_failure(&e);
        output::exit_code_of(&e)
    })
}
