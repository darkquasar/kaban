//! CLI adapter (core-spec F7; core-plan C4.1 as amended by M4-1 to M4-5, M4-25). [`run`] is the
//! dispatch: it maps every subcommand of [`crate::Cmd`] to its module, loads the policy, opens the
//! service through [`crate::open_api`] and turns every failure into the exit codes of core-spec
//! §6 (one mapping, [`output::exit_code`]). Subcommands that map to a tool go through
//! [`seam::run_tool`] (M4-3); stdout carries exactly one JSON document, written by [`output`]
//! only. The tool subcommands `remember`, `why`, `recall` and `forget` (C4.1b) take their
//! request from flags ([`ToolArgs`]) or from `--stdin-json` through [`run_tool_command`]. The
//! unshipped subcommands exit 4 through [`stubs`]; the M5 subcommands `sync`, `import`,
//! `resolve` and `export` have their typed shapes (M5-16) and placeholder modules that exit 4
//! the same way until the C5.1b and C5.2 lanes fill them; `ingest-decisions` is typed and
//! stays a stub (OWN-26); `ingest` (docs-ingest DI1.6, behind the `ingest` feature) runs the
//! document pipeline as the tool `index_docs`.

pub(crate) mod accept;
pub(crate) mod doctor;
pub(crate) mod explain;
pub(crate) mod export;
pub(crate) mod forget;
pub(crate) mod governance_history;
pub(crate) mod hook;
pub(crate) mod import;
#[cfg(feature = "ingest")]
pub(crate) mod ingest;
pub(crate) mod ontology;
pub(crate) mod output;
pub(crate) mod pack;
pub(crate) mod protect;
pub(crate) mod recall;
pub(crate) mod reindex;
pub(crate) mod remember;
pub(crate) mod rescan;
pub(crate) mod resolve;
pub(crate) mod review;
pub(crate) mod scan;
pub(crate) mod seam;
pub(crate) mod stats;
pub(crate) mod stubs;
pub(crate) mod sync;
pub(crate) mod unmerge;
pub(crate) mod verify_audit;
pub(crate) mod why;

use crate::{Cli, Cmd, OpenError};
use clap::error::ErrorKind;
use output::{EXIT_FAILURE, EXIT_INVALID, Format};
use serde_json::Value;
use std::process::ExitCode;

/// The clap arguments of a subcommand that maps to a tool (`remember`, `why`, `recall`,
/// `forget`; C4.1b). They build the raw request the MCP adapter would receive, exactly as
/// `accept` does; the request body comes from exactly one place, the flags or `--stdin-json`
/// (M4-4), which [`run_tool_command`] enforces for every implementor.
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
            OpenError::Pack(_) => output::EXIT_STORE_ERROR,
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
        Cmd::VerifyAudit => match keys_dir() {
            Ok(keys) => verify_audit::run(&cli.db, &keys, &cli.actor_id, format),
            Err(code) => code,
        },
        Cmd::Pack { cmd } => match (load_policy(cli.policy.as_deref()), keys_dir()) {
            (Ok(policy), Ok(keys)) => pack::run(&cli, &policy, &keys, cmd, format),
            (Err(code), _) | (_, Err(code)) => code,
        },
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
            // the paired `why` bench (EN-N7; engine-plan E2.7): no feature, no snapshot
            if let Some(crate::BenchCg::WhyPaired {
                before,
                after,
                samples,
                warmup,
                regression_pct,
                out,
            }) = &cg
            {
                return crate::bench::why_paired::dispatch(
                    &crate::bench::why_paired::Args {
                        before: before.clone(),
                        after: after.clone(),
                        samples: *samples,
                        warmup: *warmup,
                        regression_pct: *regression_pct,
                        out: out.clone(),
                    },
                    &keys,
                    &cli.actor_id,
                    cli.policy.as_deref(),
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
        Cmd::Accept {
            id,
            reason,
            path_scope,
            signer,
            confirmation_token,
            token_stdin,
            confirmed_by_user,
        } => {
            // the request body comes from exactly one place: the flags, or `--stdin-json`
            let flags_given = id.is_some()
                || reason.is_some()
                || !path_scope.is_empty()
                || signer.is_some()
                || confirmation_token.is_some()
                || token_stdin
                || confirmed_by_user;
            if cli.stdin_json {
                if flags_given {
                    output::print_error(
                        "error: accept takes either --stdin-json or its arguments, not both",
                    );
                    return ExitCode::from(EXIT_INVALID);
                }
                // read before opening the database: an oversized body fails fast
                let raw = match seam::read_stdin_json() {
                    Ok(raw) => raw,
                    Err(code) => return code,
                };
                return match open_service(cli) {
                    Ok(api) => accept::run_raw(&api, raw, format),
                    Err(code) => code,
                };
            }
            let Some(id) = id else {
                output::print_error("error: accept needs the node id (or --stdin-json)");
                return ExitCode::from(EXIT_INVALID);
            };
            // `--token-stdin` (OWN-20): the token line is read before the database is opened,
            // into the same raw leaf `--confirmation-token` fills (clap refuses both at once)
            let confirmation_token = if token_stdin {
                match accept::token_from_stdin() {
                    Ok(token) => Some(token),
                    Err(code) => return code,
                }
            } else {
                confirmation_token
            };
            match open_service(cli) {
                Ok(api) => accept::run(
                    &api,
                    accept::Args {
                        id,
                        reason,
                        path_scope,
                        signer,
                        confirmation_token,
                        confirmed_by_user,
                    },
                    format,
                ),
                Err(code) => code,
            }
        }
        Cmd::Remember(flags) => run_tool_command(cli, format, flags, remember::run),
        Cmd::Why(flags) => run_tool_command(cli, format, flags, why::run),
        Cmd::GovernanceHistory(flags) => {
            run_tool_command(cli, format, flags, governance_history::run)
        }
        // CLI-only (E2.3): the flags are the whole request, no `--stdin-json`
        Cmd::Explain(flags) => match open_service(cli) {
            Ok(api) => explain::run(&api, flags, format),
            Err(code) => code,
        },
        // CLI-only (E2.5): the pending items, one sentence and one action each
        Cmd::Review(flags) => match open_service(cli) {
            Ok(api) => review::run(&api, flags, format),
            Err(code) => code,
        },
        Cmd::Recall(flags) => run_tool_command(cli, format, flags, recall::run),
        Cmd::Stats(flags) => run_tool_command(cli, format, flags, stats::run),
        Cmd::Forget(flags) => run_tool_command(cli, format, flags, forget::run),
        Cmd::Protect(flags) => run_tool_command(cli, format, flags, protect::run),
        // E2.10: a human reverses a merge; exits as `forget`
        Cmd::Unmerge(flags) => run_tool_command(cli, format, flags, unmerge::run),
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
        // M5 shapes (M5-16): the repository-bound ones refuse to run outside a repository
        // before anything else; each module is a placeholder until its lane lands
        Cmd::Export {
            format: block,
            write,
            ontology,
        } => export::run(cli, block, write, ontology.as_deref(), format),
        Cmd::Sync {
            stage,
            digest_only,
            ontology,
            repo,
            deadline,
        } => match require_repo("sync", repo) {
            Ok(root) => {
                let opts = sync::Flags {
                    stage,
                    digest_only,
                    ontology,
                    deadline,
                };
                sync::run(cli, &opts, &root, format)
            }
            Err(code) => code,
        },
        Cmd::Import { repo } => match require_repo("import", repo) {
            Ok(root) => import::run(cli, &root, format),
            Err(code) => code,
        },
        Cmd::Reindex { command } => reindex::run(cli, &command, format),
        Cmd::Resolve {
            id,
            take,
            winner,
            symbol,
            group,
            repo,
        } => {
            let Some(id) = id else {
                output::print_error("error: resolve needs the node id");
                return ExitCode::from(EXIT_INVALID);
            };
            // clap holds exactly one of the three; `--winner` and `--symbol` need no repository
            match (take, winner, symbol) {
                (_, _, Some(key)) => resolve::run_symbol(cli, &id, &key, format),
                (_, Some(value), None) => {
                    resolve::run_winner(cli, &id, &value, group.as_deref(), format)
                }
                (Some(take), None, None) => match require_repo("resolve", repo) {
                    Ok(root) => resolve::run(cli, &id, take, &root, format),
                    Err(code) => code,
                },
                (None, None, None) => {
                    output::print_error("error: resolve needs --take, --winner or --symbol");
                    ExitCode::from(EXIT_INVALID)
                }
            }
        }
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
        Cmd::Promote { .. } => stubs::not_available("promote", "M6"),
        // OWN-26: the deterministic ADR parser is dropped; the assisted ingestion is specified
        // separately, so the typed shape stays a stub
        Cmd::IngestDecisions { .. } => {
            stubs::not_available("ingest-decisions", "assisted ingestion spec pending")
        }
    }
}

/// The repository root a repository-bound subcommand works in (M5-16): `--repo` when given
/// (it must hold a `.git` entry), else the git repository enclosing the current directory.
/// Outside a repository the fixed text is printed and the exit is 4; the given path is caller
/// data the seam has not judged and is never echoed.
fn require_repo(
    name: &str,
    repo: Option<std::path::PathBuf>,
) -> Result<std::path::PathBuf, ExitCode> {
    let root = match repo {
        Some(root) => root.join(".git").exists().then_some(root),
        None => std::env::current_dir()
            .ok()
            .and_then(|cwd| crate::repo_root(&cwd)),
    };
    root.ok_or_else(|| {
        output::print_error(&format!(
            "error: {name} needs a git repository (--repo <root>, or run it inside one)"
        ));
        ExitCode::from(EXIT_INVALID)
    })
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
