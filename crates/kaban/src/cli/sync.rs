//! `kaban sync [--stage] [--digest-only] [--ontology <name>[,...]|all] [--repo <root>]
//! [--deadline <s>]` (core-spec F13, F9b; core-plan C5.1 as amended by `reviews/m5-prep.md`
//! M5-16, M5-19; R5-9; engine-plan E1.6, OWN-46): move a pre-engine layout once, import the
//! files another clone advanced, write the stale records of every ontology, the selected
//! per-ontology digests and the `.gitignore` under `<root>/.kaban/`, stage them with
//! `--stage`, then run the default sink's bookkeeping. The request (`{stage, digest_only, ontologies,
//! repo}`) and the report go through the seam under the unregistered scope `sync` (M4-3:
//! every string is content, the walk fails closed); stdout carries the one JSON `SyncReport`.
//! Exit 0 on a completed run (per-file problems are counts in the report), 2 when the report
//! is withheld or `--ontology` names an unloaded ontology (`ontology_unknown`), 4 for a
//! request or environment failure, 5 for a store failure.
//!
//! **The deadline (R5-9).** `--deadline <s>` (env `KABAN_SYNC_DEADLINE`) is a hard wall-clock
//! bound honoured by the process itself, so the detached sync the Stop hooks start is bounded
//! on every host, with or without GNU `timeout` or `setsid`: a watchdog thread prints
//! [`DEADLINE_LINE`] on stderr and exits 4 when the run has not finished by then. Nothing
//! half-written survives it: record files, the digest and the `.gitignore` land by rename,
//! the store is transactional, and the repository `flock` is released with the process; the
//! next sync picks up the dirty set. Without the flag the run is unbounded, as before.

use crate::Cli;
use crate::cli::output::Format;
use crate::cli::seam;
use kaban_api::export::OntologySelection;
use kaban_api::sync::{SyncOpts, SyncReport};
use serde::Deserialize;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

/// The fixed stderr line of a run stopped by its deadline (never caller data).
pub(crate) const DEADLINE_LINE: &str =
    "kaban sync: the deadline passed; the run stopped, the next sync continues";

/// The exit status of a run stopped by its deadline (§6: a request or environment failure).
const DEADLINE_EXIT: i32 = 4;

/// Arm the deadline: a detached thread that ends the process with [`DEADLINE_LINE`] and exit
/// 4 once `seconds` have passed. A run that finishes first exits through `main` as usual and
/// the thread dies with it.
fn arm_deadline(seconds: u64) {
    std::thread::Builder::new()
        .name("sync-deadline".into())
        .spawn(move || {
            std::thread::sleep(Duration::from_secs(seconds));
            crate::cli::output::print_error(DEADLINE_LINE);
            std::process::exit(DEADLINE_EXIT);
        })
        .ok(); // a thread that cannot be spawned leaves the run unbounded, as without the flag
}

/// The request the seam scans: the flags, the ontology names and the repository root.
#[derive(Debug, Deserialize)]
struct SyncArgs {
    stage: bool,
    #[serde(default)]
    digest_only: bool,
    #[serde(default)]
    ontologies: Vec<String>,
    repo: PathBuf,
}

/// The `sync` flags as clap parsed them.
#[derive(Debug, Clone, Default)]
pub(crate) struct Flags {
    /// `--stage`.
    pub(crate) stage: bool,
    /// `--digest-only`.
    pub(crate) digest_only: bool,
    /// `--ontology <name>[,<name>...]|all`, unparsed.
    pub(crate) ontology: Option<String>,
    /// `--deadline <s>`.
    pub(crate) deadline: Option<u64>,
}

/// The names of an `--ontology` value: comma-separated, trimmed, empty entries dropped
/// (`all` or no flag is the empty list, which selects everything).
pub(crate) fn ontology_names(flag: Option<&str>) -> Vec<String> {
    flag.map(|s| {
        s.split(',')
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .map(str::to_owned)
            .collect()
    })
    .unwrap_or_default()
}

/// Run `sync` in the repository at `root`, bounded by `flags.deadline` seconds when given.
pub(crate) fn run(cli: Cli, flags: &Flags, root: &Path, format: Format) -> ExitCode {
    if let Some(seconds) = flags.deadline {
        arm_deadline(seconds);
    }
    let api = match super::open_service(cli) {
        Ok(api) => api,
        Err(code) => return code,
    };
    seam::run_write_tool::<SyncArgs, SyncReport, _>(
        &api,
        "sync",
        json!({
            "stage": flags.stage,
            "digest_only": flags.digest_only,
            "ontologies": ontology_names(flags.ontology.as_deref()),
            "repo": root.to_string_lossy(),
        }),
        &[],
        format,
        |api, args| {
            kaban_api::sync::sync(
                api,
                &args.repo,
                &SyncOpts {
                    stage: args.stage,
                    digest_only: args.digest_only,
                    ontologies: OntologySelection::from_names(args.ontologies),
                },
            )
            .map_err(kaban_api::ApiError::from)
        },
    )
}
