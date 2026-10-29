//! `kaban-index` binary (stage1-spec G1-F7, G1-S5.8, G1-S8, §7; stage1-plan CG4.6). The one
//! JSON report document goes to stdout (the only permitted stdout site of this crate, G1-E2 of
//! the plan); fixed texts and logs go to stderr. Verbs: `index`, `gc`, `status`,
//! `purge-derived`, `doctor`, `trust`, `untrust`, and the hidden `ns-helper` (CG4.4). An
//! invocation without a verb prints the scaffold report.
//!
//! ```text
//!   index: identity (CG4.1) ==> trust revalidate ==> config + per-run overrides (CG4.2)
//!      ==> HEAD == <commit>, tree clean ==> already_indexed? ==> languages, admitted indexers,
//!      nested Cargo roots named (backlog 17), toolchains (U2 RU2-2/3) ==> probes, select
//!      (CG4.3/4.4), resources (registry, module cache, toolchain) ==> Scratch
//!      ==> per language: [revalidate ==> run_indexer (run A; Go run B --skip-tests): the
//!          indexer and the checkout re-checked by identity (U2 RU2-3/5)]
//!      ==> Scratch::remove (U2 RU2-7) ==> decode_and_gate, admit, project_root ==> Rows::derive
//!      ==> publish_admitted
//!      ==> Report { commit, repo_id, languages, counts, snapshot_bytes, wall_ms_per_indexer,
//!                   peak_rss_bytes_per_indexer, failure_per_indexer, skipped_targets_per_indexer,
//!                   nested_workspaces,
//!                   policy_scan, policy_redacted_meta_per_indexer,
//!                   policy_skipped_source_per_indexer, source_classified_by_path_per_indexer,
//!                   isolation, isolation_detail, resources, cgo_enabled, failed, overrides,
//!                   warnings, exit, gc }
//!   every stderr line and every string of the report ==> one gating pass (U2 RU2-8) ==> print
//! ```
//!
//! Exit codes (G1-F7): 0 ok (`already_indexed` included); 2 usage error (a fixed diagnostic
//! naming the kind of argument error, never the argument); 3 indexer not found; 4 refused
//! (trust, identity, commit, dirty tree, config, `repo_path_refused`, an
//! indexer in untrusted mode or location, a changed indexer, a replaced checkout, a
//! repository-local toolchain, a failed git query under `gc`); 5 snapshot write error
//! (`published_unsynced`, the scratch and its cleanup, a corrupt existing snapshot); 6 indexer
//! failed (`output_cap`, `quota_exceeded`, `quota_unobservable`, a non-zero exit,
//! `project_root_mismatch`, an undecodable index, a refused output file); 7 indexer timed out;
//! 8 isolation refused (`required` without an isolating tier, a self-check mismatch). Every
//! text on stderr is fixed: no path, commit argument, indexer output or config value is echoed
//! (G1-S8), and whatever is printed passes the policy in effect first (U2 RU2-8).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::Context;
use clap::Parser;
use kaban_core::config::{ConfigError, EngineConfig, Isolation};
use kaban_core::process::{InheritedFds, ProcessError};
use kaban_index::cli::{Cli, GC_KEEP_DEFAULT, GcArgs, IndexArgs, NsHelperArgs, Verb};
use kaban_index::detect::{self, DetectErrorClass, Detected, IndexerOverride, Lang};
use kaban_index::doctor::{self, DoctorReport};
use kaban_index::gc::{self, Reason};
use kaban_index::identity_cli::{self, AdmittedDir, IdentityCliError};
use kaban_index::isolate::namespaces::{self, HELPER_EXIT, Overlay, SELFCHECK_FAILED_TEXT};
use kaban_index::isolate::{
    self, ISOLATION_NONE, ISOLATION_NONE_TEXT, REQUIRED_REFUSED_TEXT, Scratch, Selection, Tier,
};
use kaban_index::language;
use kaban_index::runner::{
    IndexerSpec, IsolationRun, NsRun, OUTPUT_CAP, QuotaCaps, RunClass, RunError, RunFailure,
    gate_line, run_indexer,
};
use kaban_index::status::{self, Purged, SnapshotState, SnapshotStatus, TrustState};
use kaban_index::tools::{self, TOOLCHAIN_ROOT_REFUSED, Toolchain};
use kaban_index::trust::{self, TRUST_TEXT, TrustError};
use kaban_index::writer::{self, Commit, Meta, Provenance, PublishError, Rows};
use kaban_index::{IndexError, admit_index, decode_and_gate};
use kaban_policy::Policy;
use serde::Serialize;
use serde_json::Value;

/// The fixed system tool directories appended to the pinned tool path (the shell and the
/// coreutils an indexer's toolchain calls; bound read-only inside the namespaces view).
const SYSTEM_TOOL_DIRS: [&str; 2] = ["/usr/bin", "/bin"];
/// The exit code of an argument error (clap's usage exit).
const USAGE_EXIT: u8 = 2;

/// The JSON report printed to stdout (G1-F7).
#[derive(Serialize, Default)]
struct Report {
    /// The verb that ran (`scaffold` without one, `usage` on an argument error).
    verb: &'static str,
    /// Whether the verb succeeded.
    ok: bool,
    /// The fixed error class when it did not (`already_indexed` on the one successful case
    /// that wrote nothing).
    #[serde(skip_serializing_if = "Option::is_none")]
    class: Option<String>,
    /// A fixed reason class refining `class` (`project_root_mismatch`, `index_decode_failed`,
    /// the `output_*` reasons).
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
    /// `<repo-id>` once computed (a storage key, never a path).
    #[serde(skip_serializing_if = "Option::is_none")]
    repo_id: Option<String>,
    /// The indexed commit.
    #[serde(skip_serializing_if = "Option::is_none")]
    commit: Option<String>,
    /// The languages the snapshot holds.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    languages: Vec<&'static str>,
    /// The row and derivation counters of the snapshot.
    #[serde(skip_serializing_if = "Option::is_none")]
    counts: Option<BTreeMap<String, u64>>,
    /// The published file's size.
    #[serde(skip_serializing_if = "Option::is_none")]
    snapshot_bytes: Option<u64>,
    /// Wall time per language (both Go runs summed).
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    wall_ms_per_indexer: BTreeMap<&'static str, u128>,
    /// Peak RSS of the indexer per language (RV-10; `null` when never sampled).
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    peak_rss_bytes_per_indexer: BTreeMap<&'static str, Option<u64>>,
    /// How a language's run died when its class is `indexer_failed` or `indexer_timeout`
    /// (backlog 22): the signal or exit code, the peak RSS, the observed scratch, the quota
    /// fact and the wall time; the stderr line says the same in words.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    failure_per_indexer: BTreeMap<&'static str, RunFailure>,
    /// Cargo targets the Rust producer left out because their `required-features` are not
    /// active (DF-2; nothing to act on): the names from its `semantic skipped targets=` line.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    skipped_targets_per_indexer: BTreeMap<&'static str, Vec<String>>,
    /// Nested Cargo roots below the root workspace that a Rust run does not index (backlog 17,
    /// `detect::nested_workspaces`): relative paths, also the snapshot's
    /// `coverage:nested_workspaces_not_indexed` and `coverage:nested_workspace_paths` rows.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    nested_workspaces: Vec<String>,
    /// `index.policy_scan` in effect (DF-3): `false` (the default) means the content policy
    /// did not scan the code-graph output at all; `true` means hits were masked and counted.
    #[serde(skip_serializing_if = "Option::is_none")]
    policy_scan: Option<bool>,
    /// With `policy_scan`: the rule ids behind the masked meta values per language
    /// (`counts:policy_redacted_meta`); rule ids only, never a value.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    policy_redacted_meta_per_indexer: BTreeMap<&'static str, Vec<String>>,
    /// With `policy_scan`: the rule ids behind the Rust files classified by their path only
    /// (`counts:policy_skipped_source`); rule ids only.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    policy_skipped_source_per_indexer: BTreeMap<&'static str, Vec<String>>,
    /// Rust files over the classifier's per-file bound, classified by their path only
    /// (`counts:source_classified_by_path`, DF-3), per language.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    source_classified_by_path_per_indexer: BTreeMap<&'static str, u64>,
    /// The tier that ran (`meta.isolation`).
    #[serde(skip_serializing_if = "Option::is_none")]
    isolation: Option<&'static str>,
    /// Its detail (`meta.isolation_detail`).
    #[serde(skip_serializing_if = "Option::is_none")]
    isolation_detail: Option<String>,
    /// `meta.resources`: the kinds of admitted read-only resources the runs were given.
    #[serde(skip_serializing_if = "Option::is_none")]
    resources: Option<Vec<&'static str>>,
    /// The effective cgo setting of a Go run as a fixed word (U2 RU2-12; `runner::cgo_setting`).
    #[serde(skip_serializing_if = "Option::is_none")]
    cgo_enabled: Option<&'static str>,
    /// Under `--partial`: the languages that failed with their fixed class.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    failed: BTreeMap<&'static str, String>,
    /// Every `index.*` key whose value differs from the default (SD-11), per-run overrides
    /// applied.
    overrides: Vec<(String, String)>,
    /// Fixed warning classes (`policy_skipped_row`, `policy_dropped`, `grammar_skipped_row`,
    /// `isolation_none`, `partial`, `toolchain_root_refused`, `gc_failed`).
    warnings: Vec<String>,
    /// The process exit code.
    exit: u8,
    /// The `doctor` findings.
    #[serde(skip_serializing_if = "Option::is_none")]
    doctor: Option<DoctorReport>,
    /// The `status` findings.
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<StatusReport>,
    /// The `gc` plan and outcome (also on a successful `index`, which applies `gc --keep 5`).
    #[serde(skip_serializing_if = "Option::is_none")]
    gc: Option<GcReport>,
    /// What `purge-derived` removed.
    #[serde(skip_serializing_if = "Option::is_none")]
    purged: Option<Purged>,
}

/// The `status` half of the report.
#[derive(Serialize)]
struct StatusReport {
    trust: TrustState,
    identity: &'static str,
    snapshots: Vec<SnapshotStatus>,
}

/// The `gc` half of the report.
#[derive(Serialize)]
struct GcReport {
    keep: Vec<String>,
    collect: Vec<(String, Reason)>,
    removed: Vec<String>,
    swept_leftovers: usize,
    dry_run: bool,
}

fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) => return usage_error(&e),
    };
    if let Some(Verb::NsHelper(args)) = cli.verb {
        return ns_helper(args);
    }
    // the policy before anything is printed: every later text passes through it (U2 RU2-8)
    let (policy, outcome) = match load_policy(cli.policy.as_deref()) {
        Ok(policy) => {
            let outcome = run(&policy, cli);
            (policy, outcome)
        }
        Err(failure) => (Policy::empty(), Err(failure)),
    };
    let (report, code) = match outcome {
        Ok(report) => {
            let code = report.exit;
            (report, code)
        }
        Err(failure) => {
            say(&policy, &failure.text);
            (failure.report, failure.exit)
        }
    };
    print_report(&policy, &report, code)
}

/// An argument error (U2 RU2-8): `--help` and `--version` print as clap renders them
/// (kaban-built text); every other kind is a fixed diagnostic naming the kind only, since the
/// offending argument and its value are caller input (exit 2).
fn usage_error(e: &clap::Error) -> ExitCode {
    use clap::error::ErrorKind;
    match e.kind() {
        ErrorKind::DisplayHelp | ErrorKind::DisplayVersion => {
            if e.print().is_err() {
                return ExitCode::from(1);
            }
            ExitCode::SUCCESS
        }
        kind => {
            let policy = Policy::empty();
            say(
                &policy,
                &format!("usage error ({kind:?}); see kaban-index --help"),
            );
            let report = Report {
                verb: "usage",
                ok: false,
                class: Some("usage_error".to_owned()),
                exit: USAGE_EXIT,
                ..Report::default()
            };
            print_report(&policy, &report, USAGE_EXIT)
        }
    }
}

/// The one stderr site of the user-facing verbs (U2 RU2-8): every line passes through the
/// policy in effect before it is printed (a reject hit prints `[removed: <rule id>]`, a redact
/// hit the masked line); the fixed texts pass unchanged.
/// The Cargo targets the Rust producer reported as skipped (`semantic skipped targets=a,b` in
/// the gated run log; DF-2), in its order; empty when there were none or the log is unreadable.
fn skipped_targets(log: &Path) -> Vec<String> {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| line.strip_prefix("semantic skipped targets="))
        .flat_map(|names| names.split(','))
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .collect()
}

#[allow(clippy::print_stderr)]
fn say(policy: &Policy, text: &str) {
    for line in text.lines() {
        eprintln!("{}", gate_line(policy, line));
    }
}

/// Every string of the report through the policy (U2 RU2-8): configuration values, the
/// override list, snapshot metadata read back by `status`, the live paths, the version lines.
fn gate_json(policy: &Policy, value: &mut Value) {
    match value {
        Value::String(s) => {
            let gated = gate_line(policy, s);
            if gated != *s {
                *s = gated;
            }
        }
        Value::Array(items) => items.iter_mut().for_each(|v| gate_json(policy, v)),
        Value::Object(map) => map.values_mut().for_each(|v| gate_json(policy, v)),
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

/// Print the report as the one stdout document, every string gated, and exit with `code`.
fn print_report(policy: &Policy, report: &Report, code: u8) -> ExitCode {
    let mut value = match serde_json::to_value(report).context("serialise the JSON report") {
        Ok(value) => value,
        Err(e) => {
            say(policy, &format!("{e:#}"));
            return ExitCode::from(1);
        }
    };
    gate_json(policy, &mut value);
    // The JSON report is the one stdout document of this binary (G1-F7; plan §4 E2).
    #[allow(clippy::print_stdout)]
    {
        println!("{value}");
    }
    ExitCode::from(code)
}

/// A failed verb: the fixed text for stderr, the report and the exit code.
struct Failure {
    text: String,
    report: Report,
    exit: u8,
}

impl Failure {
    fn new(verb: &'static str, class: &str, text: String, exit: u8) -> Self {
        Self {
            text,
            report: Report {
                verb,
                ok: false,
                class: Some(class.to_owned()),
                exit,
                ..Report::default()
            },
            exit,
        }
    }
}

/// The helper: no JSON report (its stdout and stderr are the indexer's captured streams, gated
/// by the parent before any byte is kept). A run never returns on success (`exec`); the probe
/// prints `overlay=<x>`; every failure prints the fixed self-check text and exits 8 (S5.2),
/// whatever the policy.
#[allow(clippy::print_stderr)]
fn ns_helper(args: NsHelperArgs) -> ExitCode {
    let opts = match args.into_opts() {
        Ok(opts) => opts,
        Err(e) => {
            eprintln!("{SELFCHECK_FAILED_TEXT}");
            eprintln!("ns-helper: arguments: {e}");
            return ExitCode::from(HELPER_EXIT);
        }
    };
    match namespaces::run_helper(&opts) {
        Ok(finding) => {
            // the probe's one fixed line, parsed back by the parent (G1-E2: this binary's
            // stdout is kaban-built text only)
            #[allow(clippy::print_stdout)]
            {
                println!("{finding}");
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("{SELFCHECK_FAILED_TEXT}");
            eprintln!("ns-helper: {} ({})", e.class(), error_chain(&e));
            ExitCode::from(HELPER_EXIT)
        }
    }
}

/// `display: source: source` of an error chain (kaban-built texts and errno names only).
fn error_chain(e: &dyn std::error::Error) -> String {
    let mut text = e.to_string();
    let mut source = e.source();
    while let Some(s) = source {
        text.push_str(": ");
        text.push_str(&s.to_string());
        source = s.source();
    }
    text
}

fn run(policy: &Policy, cli: Cli) -> Result<Report, Box<Failure>> {
    match cli.verb {
        None => Ok(Report {
            verb: "scaffold",
            ok: true,
            ..Report::default()
        }),
        Some(Verb::Index(args)) => index_verb(policy, args),
        Some(Verb::Gc(args)) => gc_verb(policy, args),
        Some(Verb::Status { repo }) => status_verb(policy, repo),
        Some(Verb::PurgeDerived { repo }) => purge_verb(policy, repo),
        Some(Verb::Trust { repo }) => trust_verb(policy, repo),
        Some(Verb::Untrust { repo }) => untrust_verb(policy, repo),
        Some(Verb::Doctor { repo, indexer }) => doctor_verb(policy, repo.as_deref(), &indexer),
        // dispatched in `main` before any report; kept for the exhaustive match
        Some(Verb::NsHelper(_)) => Err(Box::new(Failure::new(
            "ns-helper",
            "usage",
            "ns-helper runs without a report".to_owned(),
            4,
        ))),
    }
}

fn load_policy(path: Option<&Path>) -> Result<Policy, Box<Failure>> {
    match path {
        Some(p) => Policy::load(p).map_err(|e| {
            Box::new(Failure::new(
                "policy",
                "policy_invalid",
                format!("loading policy: {e}"),
                4,
            ))
        }),
        None => Ok(Policy::empty()),
    }
}

/// Everything the repository verbs share: resolve, gate, key, `<repo-id>`, admitted directory,
/// and the canonical kaban data directory (a forbidden root for tools, U2 RU2-2/3).
struct Located {
    resolved: identity_cli::Resolved,
    repo_id: String,
    dir: AdmittedDir,
    data_dir: PathBuf,
}

fn locate(
    verb: &'static str,
    policy: &Policy,
    repo: Option<PathBuf>,
) -> Result<Located, Box<Failure>> {
    let repo = repo.map_or_else(
        || {
            std::env::current_dir()
                .map_err(|_| Box::new(cli_failure(verb, &IdentityCliError::NoHome)))
        },
        Ok,
    )?;
    let resolved = identity_cli::resolve(&repo).map_err(|e| Box::new(cli_failure(verb, &e)))?;
    let home = identity_cli::kaban_home().map_err(|e| Box::new(cli_failure(verb, &e)))?;
    let key = identity_cli::open_key(&home).map_err(|e| Box::new(cli_failure(verb, &e)))?;
    let repo_id = identity_cli::repo_id_gated(policy, &key, &resolved.canonical_common_dir)
        .map_err(|e| Box::new(cli_failure(verb, &e)))?;
    let dir = identity_cli::admit_repo_dir(&home, &repo_id)
        .map_err(|e| Box::new(cli_failure(verb, &e)))?;
    let data_dir = std::fs::canonicalize(&home).unwrap_or(home);
    Ok(Located {
        resolved,
        repo_id,
        dir,
        data_dir,
    })
}

fn cli_failure(verb: &'static str, e: &IdentityCliError) -> Failure {
    let class = match e {
        IdentityCliError::RepoPathRefused => "repo_path_refused",
        IdentityCliError::Identity(_) | IdentityCliError::NotARepository => "trust_refused",
        IdentityCliError::GitUntrusted => "git_untrusted",
        _ => "env",
    };
    Failure::new(verb, class, e.to_string(), e.exit_code())
}

fn trust_failure(verb: &'static str, e: &TrustError) -> Failure {
    Failure::new(verb, e.class(), e.to_string(), e.exit_code())
}

fn trust_verb(policy: &Policy, repo: Option<PathBuf>) -> Result<Report, Box<Failure>> {
    let located = locate("trust", policy, repo)?;
    trust::trust(
        &located.dir.handle,
        &located.resolved.identity,
        env!("CARGO_PKG_VERSION"),
    )
    .map_err(|e| Box::new(trust_failure("trust", &e)))?;
    // the live path, scanned, never persisted (G1-S6)
    say(
        policy,
        &format!(
            "repository: {}",
            identity_cli::display_path(policy, &located.resolved.canonical_repo)
        ),
    );
    for line in TRUST_TEXT {
        say(policy, line);
    }
    Ok(Report {
        verb: "trust",
        ok: true,
        repo_id: Some(located.repo_id),
        ..Report::default()
    })
}

fn untrust_verb(policy: &Policy, repo: Option<PathBuf>) -> Result<Report, Box<Failure>> {
    let located = locate("untrust", policy, repo)?;
    trust::untrust(&located.dir.handle).map_err(|e| Box::new(trust_failure("untrust", &e)))?;
    Ok(Report {
        verb: "untrust",
        ok: true,
        repo_id: Some(located.repo_id),
        ..Report::default()
    })
}

fn parse_indexer_overrides(
    verb: &'static str,
    indexer: &[String],
) -> Result<Vec<IndexerOverride>, Box<Failure>> {
    indexer
        .iter()
        .map(|arg| IndexerOverride::parse(arg))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| {
            Box::new(Failure::new(
                verb,
                "indexer_override_invalid",
                e.to_string(),
                4,
            ))
        })
}

fn doctor_verb(
    policy: &Policy,
    repo: Option<&Path>,
    indexer: &[String],
) -> Result<Report, Box<Failure>> {
    let overrides = parse_indexer_overrides("doctor", indexer)?;
    let home = identity_cli::kaban_home().map_err(|e| Box::new(cli_failure("doctor", &e)))?;
    let report = doctor::run(&home, policy, repo, &overrides);
    say(policy, &report.render());
    let ok = matches!(report.config, doctor::ConfigStatus::Ok);
    if ok {
        Ok(Report {
            verb: "doctor",
            ok: true,
            doctor: Some(report),
            ..Report::default()
        })
    } else {
        Err(Box::new(Failure {
            text: "config invalid".to_owned(),
            report: Report {
                verb: "doctor",
                ok: false,
                class: Some("config_invalid".to_owned()),
                doctor: Some(report),
                exit: 4,
                ..Report::default()
            },
            exit: 4,
        }))
    }
}

// ---------------------------------------------------------------------------------------------
// status, purge-derived, gc

fn status_verb(policy: &Policy, repo: Option<PathBuf>) -> Result<Report, Box<Failure>> {
    let located = locate("status", policy, repo)?;
    let state = status::trust_state(&located.dir.handle, &located.resolved.identity);
    let identity = match state {
        TrustState::Revoked | TrustState::Malformed => "mismatch (mark revoked)",
        TrustState::Trusted | TrustState::NotTrusted | TrustState::Io => "resolved",
    };
    say(
        policy,
        &format!(
            "repository: {}",
            identity_cli::display_path(policy, &located.resolved.canonical_repo)
        ),
    );
    say(policy, &format!("repo_id: {}", located.repo_id));
    say(policy, &format!("trust: {}", state.as_str()));
    say(policy, &format!("identity: {identity}"));
    let snapshots = status::snapshots(&located.dir);
    if snapshots.is_empty() {
        say(policy, "snapshots: none");
    }
    for s in &snapshots {
        let or = |v: &Option<String>| v.clone().unwrap_or_else(|| "?".to_owned());
        say(
            policy,
            &format!(
                "snapshot {}: isolation={} isolation_detail={} resources={} languages={} bytes={} state={}",
                s.commit,
                or(&s.isolation),
                or(&s.isolation_detail),
                or(&s.resources),
                or(&s.languages),
                s.bytes,
                s.state.as_str()
            ),
        );
    }
    Ok(Report {
        verb: "status",
        ok: true,
        repo_id: Some(located.repo_id),
        status: Some(StatusReport {
            trust: state,
            identity,
            snapshots,
        }),
        ..Report::default()
    })
}

fn purge_verb(policy: &Policy, repo: Option<PathBuf>) -> Result<Report, Box<Failure>> {
    let located = locate("purge-derived", policy, repo)?;
    let purged = status::purge_derived(&located.dir).map_err(|e| {
        Box::new(Failure::new(
            "purge-derived",
            "purge_failed",
            e.to_string(),
            5,
        ))
    })?;
    say(
        policy,
        &format!(
            "purged: {} files, {} directories (the trust mark is kept)",
            purged.files, purged.directories
        ),
    );
    Ok(Report {
        verb: "purge-derived",
        ok: true,
        repo_id: Some(located.repo_id),
        purged: Some(purged),
        ..Report::default()
    })
}

/// `gc`: a failed git query aborts the collection with a fixed class (U2 RU2-11); nothing is
/// deleted when `HEAD` or the ordering could not be read. A successful `--ignore-missing` run
/// that prints nothing is an empty order, not a failure.
fn gc_verb(policy: &Policy, args: GcArgs) -> Result<Report, Box<Failure>> {
    let located = locate("gc", policy, args.repo)?;
    let dir = &located.dir;
    let repo = &located.resolved.canonical_repo;
    let git_failed = || {
        Box::new(Failure::new(
            "gc",
            "git_query_failed",
            "a git query failed; nothing collected".to_owned(),
            4,
        ))
    };
    let head = identity_cli::git(repo, &["rev-parse", "--verify", "HEAD^{commit}"])
        .map(|s| s.trim().to_owned())
        .map_err(|_| git_failed())?;
    let Retention { mut plan, swept } =
        retention(dir, repo, &head, args.keep).map_err(|_| git_failed())?;
    if args.prune_corrupt {
        for c in gc::corrupt(dir, &plan.keep) {
            plan.keep.retain(|k| *k != c);
            plan.collect.push((c, Reason::Corrupt));
        }
    }
    if args.stale_policy {
        for c in gc::stale(dir, &plan.keep, policy.effective_hash()) {
            plan.keep.retain(|k| *k != c);
            plan.collect.push((c, Reason::StalePolicy));
        }
    }
    for c in &plan.keep {
        say(policy, &format!("gc: keep {c}"));
    }
    for (c, reason) in &plan.collect {
        say(
            policy,
            &format!(
                "gc: collect {c} ({}){}",
                reason.as_str(),
                if args.dry_run { " [dry-run]" } else { "" }
            ),
        );
    }
    let removed = if args.dry_run {
        Vec::new()
    } else {
        gc::apply(dir, &plan.collect)
            .map_err(|e| Box::new(Failure::new("gc", "gc_failed", e.to_string(), 5)))?
    };
    Ok(Report {
        verb: "gc",
        ok: true,
        repo_id: Some(located.repo_id),
        gc: Some(GcReport {
            keep: plan.keep,
            collect: plan.collect,
            removed,
            swept_leftovers: swept,
            dry_run: args.dry_run,
        }),
        ..Report::default()
    })
}

/// The retention plan shared by `gc` and `index` (after a successful write), with the dead
/// runs' leftovers swept on the way.
struct Retention {
    plan: gc::Plan,
    swept: usize,
}

/// The snapshots with a final name ordered by commit date (one
/// `git log --format=%H --no-walk --ignore-missing <names>`), the leftovers swept, and the plan
/// keeping `head`'s snapshot plus the `keep` most recent. A failed git query is the fixed class
/// `git_query_failed` and nothing is planned (U2 RU2-11).
fn retention(
    dir: &AdmittedDir,
    repo: &Path,
    head: &str,
    keep: usize,
) -> Result<Retention, &'static str> {
    let snapshots = status::final_commits(&dir.path);
    let order: Vec<String> = if snapshots.is_empty() {
        Vec::new()
    } else {
        let mut git_args = vec!["log", "--format=%H", "--no-walk", "--ignore-missing"];
        git_args.extend(snapshots.iter().map(String::as_str));
        identity_cli::git(repo, &git_args)
            .map(|out| out.lines().map(str::to_owned).collect())
            .map_err(|_| "git_query_failed")?
    };
    let swept = gc::sweep_leftovers(dir);
    Ok(Retention {
        plan: gc::plan(&snapshots, Some(head), &order, keep),
        swept,
    })
}

/// The fixed warning class of an `index` run whose retention step failed (the snapshot stands).
const GC_FAILED: &str = "gc_failed";

/// After a successful `index` write: the `gc --keep 5` retention over the final names, the
/// snapshot just written (HEAD's) kept first by construction. A failure never fails the run:
/// its class is traced at warn and recorded as the warning `gc_failed`; nothing is collected.
fn retain_after_write(
    ctx: &mut Ctx,
    dir: &AdmittedDir,
    repo: &Path,
    head: &str,
) -> Option<GcReport> {
    let outcome = retention(dir, repo, head, GC_KEEP_DEFAULT).and_then(|r| {
        gc::apply(dir, &r.plan.collect)
            .map(|removed| (r, removed))
            .map_err(|_| GC_FAILED)
    });
    match outcome {
        Ok((Retention { plan, swept }, removed)) => Some(GcReport {
            keep: plan.keep,
            collect: plan.collect,
            removed,
            swept_leftovers: swept,
            dry_run: false,
        }),
        Err(class) => {
            tracing::warn!(
                class,
                "gc after index failed; the snapshot stands, nothing collected"
            );
            ctx.warn(GC_FAILED);
            None
        }
    }
}

// ---------------------------------------------------------------------------------------------
// index

/// What the `index` report carries whatever the outcome (filled as the pipeline advances).
#[derive(Default, Clone)]
struct Ctx {
    repo_id: Option<String>,
    commit: Option<String>,
    isolation: Option<&'static str>,
    isolation_detail: Option<String>,
    resources: Option<Vec<&'static str>>,
    cgo_enabled: Option<&'static str>,
    overrides: Vec<(String, String)>,
    warnings: Vec<String>,
    failed: BTreeMap<&'static str, String>,
    wall_ms: BTreeMap<&'static str, u128>,
    peak_rss: BTreeMap<&'static str, Option<u64>>,
    failures: BTreeMap<&'static str, RunFailure>,
    skipped_targets: BTreeMap<&'static str, Vec<String>>,
    nested_workspaces: Vec<String>,
    policy_scan: Option<bool>,
}

impl Ctx {
    fn report(&self, exit: u8) -> Report {
        Report {
            verb: "index",
            ok: exit == 0,
            repo_id: self.repo_id.clone(),
            commit: self.commit.clone(),
            isolation: self.isolation,
            isolation_detail: self.isolation_detail.clone(),
            resources: self.resources.clone(),
            cgo_enabled: self.cgo_enabled,
            overrides: self.overrides.clone(),
            warnings: self.warnings.clone(),
            failed: self.failed.clone(),
            wall_ms_per_indexer: self.wall_ms.clone(),
            peak_rss_bytes_per_indexer: self.peak_rss.clone(),
            failure_per_indexer: self.failures.clone(),
            skipped_targets_per_indexer: self.skipped_targets.clone(),
            nested_workspaces: self.nested_workspaces.clone(),
            policy_scan: self.policy_scan,
            exit,
            ..Report::default()
        }
    }

    fn fail(&self, class: &str, text: &str, exit: u8) -> Box<Failure> {
        let mut report = self.report(exit);
        report.class = Some(class.to_owned());
        Box::new(Failure {
            text: text.to_owned(),
            report,
            exit,
        })
    }

    fn fail_with_reason(&self, class: &str, reason: &str, text: &str, exit: u8) -> Box<Failure> {
        let mut failure = self.fail(class, text, exit);
        failure.report.reason = Some(reason.to_owned());
        failure
    }

    fn warn(&mut self, class: &str) {
        if !self.warnings.iter().any(|w| w == class) {
            self.warnings.push(class.to_owned());
        }
    }
}

/// The index bytes one language produced (run A, and Go's run B).
struct LangOutput {
    lang: Lang,
    a: Vec<u8>,
    b: Option<Vec<u8>>,
    /// The validated coverage sidecar beside run A's output (R-1), when the producer wrote one.
    coverage: Option<writer::Coverage>,
}

/// A language's run ended in a fixed class (the Go runs stop at the first), with the fixed
/// reason when the class is `indexer_failed` for a refused output file (U2 RU2-6) and the
/// stderr line: the class text, or how the indexer died when the runner knows it (backlog 22).
struct LangFailure {
    class: RunClass,
    reason: Option<&'static str>,
    text: String,
}

/// What a language's runs are given beyond the indexer (U2 RU2-2): the pinned tool path and
/// the toolchain's read-only resources.
#[derive(Clone)]
struct LangTools {
    tool_dirs: Vec<PathBuf>,
    resource_dirs: Vec<PathBuf>,
    driver: language::Driver,
}

#[allow(clippy::too_many_lines)] // the §2 pipeline in its spec order, one place to read it
fn index_verb(policy: &Policy, args: IndexArgs) -> Result<Report, Box<Failure>> {
    const VERB: &str = "index";
    let mut ctx = Ctx::default();
    let overrides_cli = parse_indexer_overrides(VERB, &args.indexer)?;
    let located = locate(VERB, policy, args.repo)?;
    ctx.repo_id = Some(located.repo_id.clone());
    let repo = located.resolved.canonical_repo.clone();

    // preconditions: trusted and the identity revalidates (G1-S6)
    trust::revalidate(&located.dir.handle, &located.resolved.identity)
        .map_err(|e| Box::new(trust_failure(VERB, &e)))?;

    // config in effect with the per-run overrides (OWN-29, G1-S5.5, SD-11)
    let home = identity_cli::kaban_home().map_err(|e| Box::new(cli_failure(VERB, &e)))?;
    let mut cfg = load_config(&ctx, &home.join("config.yaml"))?;
    if let Some(isolation) = args.isolation {
        cfg.index.isolation = isolation.into();
    }
    if let Some(timeout) = args.timeout {
        cfg.index.deadline_secs = timeout.as_secs();
    }
    ctx.overrides = cfg
        .effective_overrides()
        .into_iter()
        .filter(|(k, _)| k.starts_with("index."))
        .collect();
    // DF-3 (owner decision): the content policy scans the code-graph output only when
    // `index.policy_scan` is on; stderr and the report keep the loaded policy (U2 RU2-8)
    ctx.policy_scan = Some(cfg.index.policy_scan);
    let scan_off = Policy::disabled();
    let scan: &Policy = if cfg.index.policy_scan {
        policy
    } else {
        &scan_off
    };

    // the commit resolves and is the checked-out HEAD; tracked files are unchanged (RS-5)
    let head = rev_parse(&repo, "HEAD").ok_or_else(|| {
        ctx.fail(
            "commit_not_checked_out",
            "HEAD does not resolve to a commit; indexing refused",
            4,
        )
    })?;
    let target = if args.commit == "HEAD" {
        head.clone()
    } else {
        rev_parse(&repo, &args.commit).ok_or_else(|| {
            ctx.fail(
                "commit_not_checked_out",
                "the commit does not resolve in this repository; indexing refused",
                4,
            )
        })?
    };
    let commit = match Commit::parse(&head) {
        Some(c) if target == head => c,
        _ => {
            return Err(ctx.fail(
                "commit_not_checked_out",
                "the commit is not the checked-out HEAD; indexing refused",
                4,
            ));
        }
    };
    ctx.commit = Some(commit.to_string());
    let porcelain = identity_cli::git(&repo, &["status", "--porcelain", "--untracked-files=no"])
        .map_err(|_| {
            ctx.fail(
                "tree_dirty",
                "git status could not be read; indexing refused",
                4,
            )
        })?;
    if !porcelain.trim().is_empty() {
        return Err(ctx.fail(
            "tree_dirty",
            "tracked files are modified; commit or stash them first; indexing refused",
            4,
        ));
    }

    // a snapshot name already present is never overwritten (a marked one is re-verified)
    if let Some(state) = status::existing(&located.dir, &commit) {
        return match state {
            SnapshotState::Ok | SnapshotState::Reverified => {
                say(
                    policy,
                    "already indexed: the snapshot of this commit exists; nothing written",
                );
                let mut report = ctx.report(0);
                report.class = Some("already_indexed".to_owned());
                Ok(report)
            }
            SnapshotState::TooNew => Err(ctx.fail(
                "snapshot_too_new",
                "a snapshot of this commit exists with a newer schema; upgrade kaban-index",
                5,
            )),
            SnapshotState::Corrupt
            | SnapshotState::UnsyncedCorrupt
            | SnapshotState::Refused => Err(ctx.fail(
                "snapshot_corrupt",
                "a snapshot of this commit exists but failed re-verification; run kaban-index gc --prune-corrupt",
                5,
            )),
        };
    }

    // languages and indexers (G1-F20, G1-F7 --indexer): an indexer is admitted from the object
    // (owner, mode, identity) and never from under the checkout or the kaban data directory
    // (U2 RU2-3)
    let path_env = std::env::var_os("PATH");
    let detected = detect::languages(
        &repo,
        &overrides_cli,
        path_env.as_ref(),
        &[located.data_dir.as_path()],
    )
    .map_err(|_| {
        ctx.fail(
            "unsupported_in_stage_1",
            "no supported language marker at the repository root (go.mod, package.json, tsconfig.json)",
            4,
        )
    })?;
    let wanted: Vec<Detected> = if args.languages.is_empty() {
        detected
    } else {
        let mut out = Vec::new();
        for lang in &args.languages {
            let d = detected.iter().find(|d| d.lang == *lang).ok_or_else(|| {
                ctx.fail(
                    "language_not_detected",
                    "a requested language has no marker at the repository root",
                    4,
                )
            })?;
            if !out.iter().any(|o: &Detected| o.lang == d.lang) {
                out.push(d.clone());
            }
        }
        out
    };
    // a Rust run covers the root workspace only: the nested Cargo roots it leaves out are named
    // in the report and the snapshot (backlog 17), never silently absent
    if wanted.iter().any(|d| d.lang == Lang::Rust) {
        ctx.nested_workspaces = detect::nested_workspaces(&repo)
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
    }
    for d in &wanted {
        match &d.indexer {
            Ok(_) => {}
            Err(DetectErrorClass::IndexerNotFound) => {
                return Err(ctx.fail(
                    "indexer_not_found",
                    "indexer not found for a detected language; install it or pass --indexer <lang>=<path>",
                    3,
                ));
            }
            Err(DetectErrorClass::IndexerUntrustedMode) => {
                return Err(ctx.fail(
                    "indexer_untrusted_mode",
                    "the indexer is not a regular file owned by you or root, or is writable by group or other; refused",
                    4,
                ));
            }
            Err(DetectErrorClass::IndexerUntrustedLocation) => {
                return Err(ctx.fail(
                    "indexer_untrusted_location",
                    "the indexer lies inside the repository or the kaban data directory; refused",
                    4,
                ));
            }
            Err(DetectErrorClass::IndexerOverrideInvalid) => {
                return Err(ctx.fail(
                    "indexer_override_invalid",
                    "the --indexer override is malformed or relative",
                    4,
                ));
            }
        }
    }

    // Select policy before any producer/compiler probe. Required isolation failure must
    // never fall back to a native tool execution.
    let helper = namespaces::self_exe()
        .map_err(|_| ctx.fail("env", "the kaban-index binary path could not be read", 4))?;
    let probes = if cfg.index.isolation == Isolation::Off {
        Vec::new()
    } else {
        isolate::probe_all(&cfg.index, &helper)
    };
    let selection: Selection = isolate::select(cfg.index.isolation, &probes).map_err(|e| {
        ctx.fail(
            "isolation_unavailable",
            REQUIRED_REFUSED_TEXT,
            e.exit_code(),
        )
    })?;
    ctx.isolation = Some(selection.tier.as_str());
    ctx.isolation_detail = Some(selection.detail.0.clone());
    if selection.warning.is_some() {
        say(policy, ISOLATION_NONE_TEXT);
        ctx.warn(ISOLATION_NONE);
    }
    let probe_site = if selection.tier == Tier::Namespaces {
        doctor::ProbeSite::View {
            helper: &helper,
            resources: &[],
        }
    } else {
        doctor::ProbeSite::Native
    };

    // the language toolchains (U2 RU2-2): the `go`/`node` found on PATH is admitted like an
    // indexer, its directory joins the pinned PATH, its root is bound only when narrow; a
    // repository-local or untrusted toolchain refuses the run before anything starts
    let caller_home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .and_then(|h| std::fs::canonicalize(h).ok());
    let rule = tools::RootRule {
        checkout: &repo,
        home: caller_home.as_deref(),
        data_dir: &located.data_dir,
    };
    let mut lang_tools: BTreeMap<Lang, LangTools> = BTreeMap::new();
    for d in &wanted {
        let Ok(resolved) = &d.indexer else {
            continue;
        };
        let toolchain = tools::toolchain(d.lang, path_env.as_ref(), &rule).map_err(|_| {
            ctx.fail(
                "toolchain_refused",
                "the language toolchain found on PATH lies inside the repository or the kaban data directory, or is not a trusted executable; refused",
                4,
            )
        })?;
        if toolchain.as_ref().is_some_and(|t| t.root_refused) {
            ctx.warn(TOOLCHAIN_ROOT_REFUSED);
        }
        let driver = language::Driver::admit(
            d.lang,
            &repo,
            resolved,
            toolchain.as_ref(),
            &located.data_dir,
            probe_site,
        )
        .map_err(|refusal| ctx.fail(refusal.class, &refusal.text, 4))?;
        // the driver's own resources (Rust: the linker's helper directory) join the toolchain's
        let mut resource_dirs = toolchain
            .as_ref()
            .map(Toolchain::resource_dirs)
            .unwrap_or_default();
        resource_dirs.extend(driver.resource_dirs());
        lang_tools.insert(
            d.lang,
            LangTools {
                tool_dirs: pinned_tool_dirs(driver.tool_dirs(), &resolved.path, toolchain.as_ref()),
                resource_dirs,
                driver,
            },
        );
    }

    // provenance of what was bound: derived from the admitted resources (gofix1-7)
    let mut resources: Vec<&'static str> = lang_tools
        .values()
        .flat_map(|tools| tools.driver.resource_kinds())
        .collect();
    resources.sort_unstable();
    resources.dedup();
    ctx.resources = Some(resources.clone());
    let overlay = Overlay::from_detail(&selection.detail.0).unwrap_or(Overlay::None);

    // the run's scratch: the only writable place; deleted on every exit path (RS2-7)
    let scratch = Scratch::create(&located.dir).map_err(|_| {
        ctx.fail(
            "scratch",
            "the scratch directory could not be created in the data directory",
            5,
        )
    })?;
    let common_dir = Path::new(&located.resolved.canonical_common_dir);
    let common_outside = (!common_dir.starts_with(&repo)).then_some(common_dir);
    let deadline = std::time::Duration::from_secs(cfg.index.deadline_secs);
    let quota = QuotaCaps {
        file_bytes: cfg.index.quota.file_bytes,
        total_bytes: cfg.index.quota.run_dir_bytes,
    };

    // per language: revalidate immediately before every spawn (G1-S5.8), run, keep the bytes
    let mut outputs: Vec<LangOutput> = Vec::new();
    for d in &wanted {
        let Ok(resolved) = &d.indexer else {
            continue;
        };
        let lang = d.lang;
        let Some(tools) = lang_tools.get(&lang) else {
            continue;
        };
        let mut bytes: Vec<Vec<u8>> = Vec::new();
        let mut coverage: Option<writer::Coverage> = None;
        let mut failure: Option<LangFailure> = None;
        let invocations = tools.driver.prepare(&repo, &scratch, selection.tier != Tier::None, commit.as_str())
            .map_err(|class| ctx.fail(class, "Rust run not prepared: the admitted toolchain or linker changed since doctor, the rust-toolchain file asks for something not installed, or the scratch could not be written", 4))?;
        for invocation in invocations {
            let slot = invocation.slot;
            let file = invocation.file;
            // G1-S5.8: the identity is recomputed (not reused) immediately before every spawn,
            // run B included; a mismatch revokes the mark and refuses the run
            let live = identity_cli::resolve(&repo).map_err(|e| {
                let mut f = Box::new(cli_failure(VERB, &e));
                let class = f.report.class.clone();
                f.report = ctx.report(f.exit);
                f.report.class = class;
                f
            })?;
            trust::revalidate(&located.dir.handle, &live.identity).map_err(|e| {
                let mut f = Box::new(trust_failure(VERB, &e));
                f.report = ctx.report(f.exit);
                f.report.class = Some(e.class().to_owned());
                f
            })?;
            let isolation = match selection.tier {
                Tier::None => IsolationRun::None,
                Tier::Namespaces => IsolationRun::Namespaces(
                    NsRun::prepare(&helper, common_outside)
                        .map_err(|_| {
                            ctx.fail(
                                "isolation_unavailable",
                                "the unshare handle could not be resolved for the run",
                                8,
                            )
                        })?
                        .with_overlay(overlay),
                ),
                Tier::Container => IsolationRun::Container,
            };
            // r1-3: the coverage sidecar belongs to exactly one invocation. One a previous
            // invocation left (a producer that wrote it and then failed, skipped under
            // `--partial`) is removed through the owned `out` handle before this one spawns,
            // so the sidecar read after this run's output is the one this run wrote
            scratch.remove_output("coverage.json").map_err(|_| {
                ctx.fail(
                    "scratch",
                    "the scratch output directory could not be cleared before the run",
                    5,
                )
            })?;
            let report = run_indexer(IndexerSpec {
                lang,
                run: slot,
                indexer: &resolved.path,
                indexer_identity: Some(resolved.identity),
                args: invocation.args,
                repo_root: &repo,
                checkout: Some(&located.resolved.repo_handle),
                tool_dirs: tools.tool_dirs.clone(),
                resource_dirs: tools.resource_dirs.clone(),
                scratch: &scratch,
                deadline,
                output_cap: OUTPUT_CAP,
                quota,
                data_dir: &located.dir,
                commit: commit.clone(),
                policy,
                inherited_fds: InheritedFds::Refuse,
                go: tools.driver.go_run(),
                linker: tools.driver.linker(),
                isolation,
            })
            .map_err(|e| run_error(&ctx, &e))?;
            *ctx.wall_ms.entry(lang.name()).or_insert(0) += report.wall.as_millis();
            let peak = ctx.peak_rss.entry(lang.name()).or_insert(None);
            *peak = (*peak).max(report.peak_rss_bytes);
            for w in &report.warnings {
                ctx.warn(w);
            }
            if let Some(cgo) = report.cgo_enabled {
                ctx.cgo_enabled = Some(cgo);
            }
            if lang == Lang::Rust && report.class.is_none() {
                let skipped = skipped_targets(&report.log_path);
                if !skipped.is_empty() {
                    ctx.skipped_targets.insert(lang.name(), skipped);
                }
            }
            if let Some(class) = report.class {
                // how the indexer died (backlog 22): in the report and in the stderr line
                let how = report.failure();
                if let Some(how) = how {
                    ctx.failures.insert(lang.name(), how);
                }
                failure = Some(LangFailure {
                    class,
                    reason: None,
                    text: failure_text(lang, class, how, &report.log_path),
                });
                break;
            }
            // the output file through the owned directory handle, no link followed, a
            // regular file within the per-file quota (U2 RU2-6); exit 0 without it, or with
            // anything else there, is the indexer's failure with the fixed reason
            match scratch.read_output(file, quota.file_bytes) {
                Ok(b) => bytes.push(b),
                Err(e) => {
                    failure = Some(LangFailure {
                        class: RunClass::IndexerFailed,
                        reason: Some(e.reason()),
                        text: class_text(RunClass::IndexerFailed).to_owned(),
                    });
                    break;
                }
            }
            // the producer's coverage sidecar beside run A's output (R-1): absent for Go and
            // TypeScript (nothing published, no error); present, it is this invocation's (the
            // directory was cleared of it before the spawn), read through the same handle and
            // validated, and one that fails validation is ignored with a warning
            if bytes.len() == 1 {
                let read: Result<Option<writer::Coverage>, ()> =
                    match scratch.read_output("coverage.json", writer::Coverage::MAX_BYTES) {
                        Ok(sidecar) => writer::Coverage::parse(&sidecar).map(Some).map_err(|_| ()),
                        Err(isolate::OutputError::Missing) => Ok(None),
                        Err(_) => Err(()),
                    };
                coverage = read.unwrap_or_else(|()| {
                    ctx.warn("coverage_invalid");
                    None
                });
            }
        }
        match failure {
            None => {
                let mut it = bytes.into_iter();
                let a = it.next().unwrap_or_default();
                outputs.push(LangOutput {
                    lang,
                    a,
                    b: it.next(),
                    coverage,
                });
            }
            // a fatal class (an isolation refusal, an unobservable quota) ends the run
            // whatever `--partial` says (U2 RU2-9): nothing is published
            Some(LangFailure {
                class,
                reason,
                text,
            }) => {
                if args.partial && wanted.len() > 1 && !class.is_fatal() {
                    ctx.failed.insert(lang.name(), class.as_str().to_owned());
                    ctx.warn("partial");
                } else {
                    return Err(match reason {
                        Some(reason) => {
                            ctx.fail_with_reason(class.as_str(), reason, &text, class.exit_code())
                        }
                        None => ctx.fail(class.as_str(), &text, class.exit_code()),
                    });
                }
            }
        }
    }

    // the raw files go before anything is reported (RS2-7, U2 RU2-7): the explicit, checked
    // removal through handles; a failure is a write error, nothing is published
    scratch.remove().map_err(|_| {
        ctx.fail(
            "scratch_cleanup_failed",
            "the run's raw files could not be removed from the data directory; nothing published",
            5,
        )
    })?;
    if outputs.is_empty() {
        return Err(ctx.fail(
            "indexer_failed",
            "every requested language failed; nothing written (see the gated logs)",
            6,
        ));
    }
    if outputs.len() > 1 {
        return Err(ctx.fail(
            "multi_language_not_in_this_release",
            "more than one language succeeded; a snapshot holds one language in this release: pass --languages <one>",
            4,
        ));
    }
    let output = &outputs[0];

    // decode and gate (CG3.1), project root (RS-5), derive (CG3.2/3.3), publish (CG3.2)
    let a = decode_and_gate(&output.a, scan).map_err(|e| gate_failure(&ctx, &e))?;
    let b = output
        .b
        .as_deref()
        .map(|bytes| decode_and_gate(bytes, scan))
        .transpose()
        .map_err(|e| gate_failure(&ctx, &e))?;
    // the root is validated as an identity parsed before the gate (df23-6): a meta hit on its
    // text is masked and counted by admission below, never a refusal
    for gated in std::iter::once(&a).chain(b.as_ref()) {
        kaban_index::derive::check_project_root(&gated.metadata().project_root_identity, &repo)
            .map_err(|_| {
                ctx.fail_with_reason(
                    "indexer_failed",
                    "project_root_mismatch",
                    "the index names another project root; the snapshot is refused",
                    6,
                )
            })?;
    }
    let admitted_a = admit_index(&a);
    let admitted_b = b.as_ref().map(admit_index);
    let mut rows = Rows::derive_committed(&admitted_a, admitted_b.as_ref(), scan, &repo, &commit)
        .map_err(|e| gate_failure(&ctx, &e))?;
    // CG8.2: the snippets come from `git show <commit>:<path>` through the pinned git (kaban's
    // own child, outside the isolation tier, after the indexers and before publish; never the
    // working tree), gated as free fields before they are rows
    rows.set_snippets(kaban_index::snippets::collect(
        &repo,
        commit.as_str(),
        &rows,
        scan,
    ));
    if let Some(coverage) = output.coverage.clone() {
        rows.set_coverage(coverage);
    }
    if output.lang == Lang::Rust {
        rows.set_nested_workspaces(ctx.nested_workspaces.clone());
    }
    for w in rows.warnings() {
        let text = serde_json::to_value(w)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_default();
        if !text.is_empty() {
            ctx.warn(&text);
        }
    }
    if rows.counts().grammar_skipped_rows > 0 {
        ctx.warnings.push("grammar_skipped_row".to_owned());
    }
    if rows.counts().snippet_skipped > 0 {
        ctx.warn("snippet_skipped");
    }
    let by_path = rows.counts().source_classified_by_path;
    if by_path > 0 {
        say(
            policy,
            &format!(
                "{by_path} committed Rust files over the classifier bound were classified by their path only"
            ),
        );
    }
    let meta = Meta {
        commit: commit.clone(),
        repo_id: located.repo_id.clone(),
        policy_version: scan.effective_hash().to_owned(),
        provenance: Provenance {
            isolation: selection.tier,
            isolation_detail: selection.detail.0.clone(),
            resources,
            container_image_digest: None,
        },
    };
    let published =
        writer::publish_admitted(&located.dir.handle, &rows, &meta, scan, writer::dir_fsync)
            .map_err(|e| publish_failure(&ctx, &e))?;
    let snapshot_bytes = published.bytes;
    if published.policy_redacted_meta > 0 {
        ctx.warn("policy_redacted_meta");
    }
    let mut counts: BTreeMap<String, u64> = rows
        .counts()
        .entries()
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v))
        .collect();
    counts.insert("files".to_owned(), rows.files().len() as u64);
    counts.insert("symbols".to_owned(), rows.symbols().len() as u64);
    counts.insert("edges".to_owned(), rows.edges().len() as u64);
    counts.insert("occurrences".to_owned(), rows.occurrences().len() as u64);
    counts.insert("snippets".to_owned(), rows.snippets().len() as u64);
    counts.insert("snippet_skipped".to_owned(), rows.counts().snippet_skipped);
    counts.insert(
        "policy_redacted_meta".to_owned(),
        published.policy_redacted_meta,
    );

    // snapshots must not accumulate (owner decision): the `gc --keep 5` retention after the
    // write, reported in the existing `gc` half; its failure is a warning, never the run's
    let gc_report = retain_after_write(&mut ctx, &located.dir, &repo, commit.as_str());
    let mut report = ctx.report(0);
    report.languages = vec![output.lang.name()];
    if !published.policy_redacted_meta_rules.is_empty() {
        report.policy_redacted_meta_per_indexer.insert(
            output.lang.name(),
            published.policy_redacted_meta_rules.clone(),
        );
    }
    if !rows.policy_skipped_source_rules().is_empty() {
        report.policy_skipped_source_per_indexer.insert(
            output.lang.name(),
            rows.policy_skipped_source_rules().to_vec(),
        );
    }
    if by_path > 0 {
        report
            .source_classified_by_path_per_indexer
            .insert(output.lang.name(), by_path);
    }
    report.counts = Some(counts);
    report.snapshot_bytes = Some(snapshot_bytes);
    report.gc = gc_report;
    Ok(report)
}

/// `<home>/config.yaml` loaded (absent: defaults); a load error is `config_invalid`, exit 4,
/// with the field path only.
fn load_config(ctx: &Ctx, path: &Path) -> Result<EngineConfig, Box<Failure>> {
    kaban_core::config::load(path).map_err(|e| {
        let (class, detail) = match e {
            ConfigError::Io { class } => ("io", class.to_owned()),
            ConfigError::Parse { path } => ("parse", path),
            ConfigError::Range { path } => ("range", path),
            ConfigError::NotAvailable { path } => {
                ("not_available_in_this_release", path.to_owned())
            }
        };
        ctx.fail(
            "config_invalid",
            &format!("config invalid ({class}: {detail})"),
            4,
        )
    })
}

/// `git rev-parse --verify <spec>^{commit}` in `repo`, or `None`.
fn rev_parse(repo: &Path, spec: &str) -> Option<String> {
    let spec = format!("{spec}^{{commit}}");
    identity_cli::git(repo, &["rev-parse", "--verify", "--end-of-options", &spec])
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| s.len() == 40)
}

/// The pinned tool path of a run (S5.3, U2 RU2-2): the driver's own directories `first` (the
/// module-cache Go toolchain's `bin`, G2), the indexer's directory, the directory of the
/// admitted toolchain executable (`go`, `node`) and the fixed system tool directories that
/// exist; executable directories only (the toolchain's root is a read-only resource of the
/// view, never on `PATH`).
fn pinned_tool_dirs(
    first: Vec<PathBuf>,
    indexer: &Path,
    toolchain: Option<&Toolchain>,
) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = first;
    if let Some(parent) = indexer.parent() {
        dirs.push(parent.to_path_buf());
    }
    if let Some(t) = toolchain {
        dirs.push(t.bin_dir.clone());
    }
    dirs.extend(
        SYSTEM_TOOL_DIRS
            .iter()
            .map(PathBuf::from)
            .filter(|p| p.is_dir()),
    );
    let mut seen = std::collections::BTreeSet::new();
    dirs.retain(|d| seen.insert(d.clone()));
    dirs
}

/// The fixed text of a run class.
const fn class_text(class: RunClass) -> &'static str {
    match class {
        RunClass::IndexerFailed => "indexer failed; the gated log is in the data directory",
        RunClass::IndexerTimeout => "indexer timed out; the gated log is in the data directory",
        RunClass::OutputCap => "indexer output exceeded the cap; the run was killed",
        RunClass::QuotaExceeded => "indexer exceeded the scratch quota; the run was killed",
        RunClass::QuotaUnobservable => "the scratch could not be measured; the run was killed",
        RunClass::MonitorFailed | RunClass::MonitorSilent | RunClass::DescendantsSurvived => {
            "indexer run did not end cleanly; the gated log is in the data directory"
        }
        RunClass::IsolationSelfcheckFailed => SELFCHECK_FAILED_TEXT,
    }
}

/// The signal of the kernel's OOM killer and of every group kill: a `9` with no quota breach
/// and no deadline is a kill kaban did not issue.
const SIGKILL: i32 = 9;

/// The stderr line of a run that ended `indexer_failed` or `indexer_timeout` (backlog 22): how
/// the indexer died in plain words, with the resources the monitors saw, so a kill by the host
/// (signal 9, no quota breach: probably its OOM killer) is told from a crash (an exit code) and
/// from kaban's deadline. The class text when the runner has no failure for the class.
fn failure_text(lang: Lang, class: RunClass, how: Option<RunFailure>, log: &Path) -> String {
    let Some(how) = how else {
        return class_text(class).to_owned();
    };
    let lang = lang.name();
    let secs = how.wall_ms / 1000;
    let log = format!("see the gated log {}", log.display());
    let resources = format!(
        "peak RSS {}, scratch {}, {}",
        how.peak_rss_bytes
            .map_or_else(|| "n/a".to_owned(), human_bytes),
        human_bytes(how.observed_scratch_bytes),
        if how.quota_breached {
            "quota breached"
        } else {
            "no quota breach"
        }
    );
    match (class, how.signal, how.exit_code) {
        (RunClass::IndexerTimeout, ..) => format!(
            "{lang} indexer timed out: killed by the deadline after {secs} s ({resources}; {log})"
        ),
        (_, Some(signal), _) => {
            let hint = if signal == SIGKILL && !how.quota_breached {
                "the host was probably out of memory".to_owned()
            } else {
                log
            };
            format!(
                "{lang} indexer failed: killed by signal {signal} after {secs} s ({resources}; {hint})"
            )
        }
        (_, None, Some(code)) => {
            format!("{lang} indexer failed: exited with code {code} after {secs} s ({log})")
        }
        (_, None, None) => format!(
            "{lang} indexer failed: no exit status within the join bound after {secs} s ({resources}; {log})"
        ),
    }
}

/// Bytes for a human: `2.1 GB`, `340 MB`, `12 kB`, `0 B` (decimal units, one decimal for GB).
#[allow(clippy::cast_precision_loss)] // display only
fn human_bytes(bytes: u64) -> String {
    match bytes {
        b if b >= 1_000_000_000 => format!("{:.1} GB", b as f64 / 1e9),
        b if b >= 1_000_000 => format!("{} MB", b / 1_000_000),
        b if b >= 1_000 => format!("{} kB", b / 1_000),
        b => format!("{b} B"),
    }
}

/// Why no indexer ran: the class and exit code of a [`RunError`].
fn run_error(ctx: &Ctx, e: &RunError) -> Box<Failure> {
    match e {
        RunError::TierNotImplemented(_) => ctx.fail(
            "isolation_unavailable",
            "the selected tier is not in this release",
            8,
        ),
        RunError::Namespaces(_) => ctx.fail(
            "isolation_unavailable",
            "the namespaces tier could not be prepared for the run",
            8,
        ),
        RunError::Process(ProcessError::Env(class)) => ctx.fail(
            class,
            "the indexer was refused before the spawn (environment or descriptor policy)",
            4,
        ),
        RunError::Process(e @ ProcessError::InheritedFd { .. }) => ctx.fail(
            "inherited_fd",
            &format!("the indexer was refused before the spawn: the kaban-index process {e}"),
            4,
        ),
        RunError::Process(ProcessError::Spawn(_)) => {
            ctx.fail("indexer_not_found", "the indexer could not be started", 3)
        }
        RunError::Log(_) => ctx.fail(
            "log_write_failed",
            "the gated log could not be written in the data directory",
            5,
        ),
        RunError::IndexerChanged => ctx.fail(
            "indexer_changed",
            "the indexer is not the object admitted at the start; refused",
            4,
        ),
        RunError::CheckoutReplaced => ctx.fail(
            "checkout_replaced",
            "the repository directory is not the one admitted at the start; refused",
            4,
        ),
        RunError::ToolchainChanged => ctx.fail(
            "go_toolchain_changed",
            "the module-cache go toolchain is not the object admitted at the start; refused",
            4,
        ),
        _ => ctx.fail("indexer_failed", "the indexer run could not be recorded", 6),
    }
}

/// A decode or gate failure: a refused source classification or an undecodable index
/// (`indexer_failed`, reason `index_decode_failed`, exit 6). A policy hit is masked and
/// counted, never a failure (DF-3).
fn gate_failure(ctx: &Ctx, e: &IndexError) -> Box<Failure> {
    match e {
        IndexError::SourceClassification => ctx.fail_with_reason(
            "indexer_failed", "source_classification_refused",
            "committed Rust source could not be classified within its bounds; the snapshot is refused", 6,
        ),
        _ => ctx.fail_with_reason(
            "indexer_failed",
            "index_decode_failed",
            "the indexer output is not a SCIP index; the snapshot is refused",
            6,
        ),
    }
}

/// A publication failure (CG3.2 classes): `published_unsynced` 5 (the snapshot exists and will
/// be re-verified by the next `status` or `index`), every other 5.
fn publish_failure(ctx: &Ctx, e: &PublishError) -> Box<Failure> {
    match e {
        PublishError::Unsynced { .. } => ctx.fail(
            "published_unsynced",
            "the snapshot was published but the directory fsync failed; it is marked for re-verification by the next status or index",
            5,
        ),
        PublishError::Exists => ctx.fail(
            "snapshot_exists",
            "a snapshot of this commit appeared during the run; nothing overwritten",
            5,
        ),
        other => ctx.fail(other.class(), "the snapshot could not be written", 5),
    }
}
