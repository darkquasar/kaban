//! `kaban doctor` (core-spec §7; core-plan C4.2 as amended by M4-19, M4-20, M4-25): open the
//! database as it is on disk (`SqliteStore::open_diagnostic`: nothing created, migrated,
//! seeded or tightened, the key never created), run `KabanApi::doctor` and print the report as
//! the one JSON document. The report goes through the seam like a tool result
//! ([`seam::run_tool`] with the unregistered scope `doctor`, so every string is content and the
//! walk fails closed; M4-3). The code-graph provider is wired as `serve` wires it (read-only:
//! the codegraph key is read, never created) so `stale_anchors` sees the current snapshot.
//! Exit 0 when every check passes, 5 when one fails or the database cannot be opened; a
//! withheld report is 2 like every other withheld result.

use crate::Cli;
use crate::cli::output::{EXIT_INVALID, EXIT_STORE_ERROR, Format, print_failure, store_exit_code};
use crate::cli::seam;
use crate::codegraph;
use kaban_api::{ApiConfig, DoctorContext, DoctorReport, GitRunner, KabanApi};
use kaban_core::WorkspaceId;
use kaban_store_sqlite::SqliteStore;
use serde::Deserialize;
use serde_json::json;
use std::cell::Cell;
use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;

/// The request the seam scans: the two flags, no caller string.
#[derive(Debug, Deserialize)]
struct DoctorArgs {
    vacuum: bool,
    priors: bool,
    optimize: bool,
}

/// Run the checks against `cli.db` with the audit key under `keys_dir` and print the report.
pub(crate) fn run(
    cli: &Cli,
    policy: kaban_policy::Policy,
    keys_dir: &Path,
    vacuum: bool,
    priors: bool,
    optimize: bool,
    format: Format,
) -> ExitCode {
    let engine = match crate::load_engine() {
        Ok(engine) => Arc::new(engine),
        Err(error) => {
            error.report(&cli.db);
            return ExitCode::from(EXIT_INVALID);
        }
    };
    // the store's own refusal of a missing file names the file-mode rule; doctor says why
    if std::fs::symlink_metadata(&cli.db).is_err() {
        print_failure(&format_args!(
            "database {} does not exist (doctor never creates one)",
            cli.db.display()
        ));
        return ExitCode::from(EXIT_STORE_ERROR);
    }
    let (store, files) = match SqliteStore::open_diagnostic(&cli.db, keys_dir, &cli.actor_id) {
        Ok(opened) => opened,
        Err(e) => {
            print_failure(&format_args!(
                "opening database {} for diagnosis: {e}",
                cli.db.display()
            ));
            return ExitCode::from(store_exit_code(&e));
        }
    };
    // spec 05 SS-21: the diagnostic open attached the store's index as it exists, so the
    // report reads its payload watermark (no loop, no drain, nothing created: doctor
    // publishes nothing and a missing index is reported, never built, T4 review t4-9)
    let store = Arc::new(store);
    // the same binding rule as the server (S6; `open_api`): the workspace only stamps the
    // audit rows a gate rejection would write
    let workspace = match &cli.workspace {
        Some(ws) if ws == WorkspaceId::ALL || ws.is_empty() => {
            print_failure(&"--workspace must name one workspace (\"*\" is a read selector)");
            return ExitCode::from(EXIT_INVALID);
        }
        Some(ws) => WorkspaceId::new(ws.clone()),
        None => std::env::current_dir()
            .map(|cwd| crate::default_workspace(&cwd))
            .unwrap_or_default(),
    };
    // the code side as `serve` wires it (`codegraph::wire`: the key is read, never created),
    // so `stale_anchors` resolves the snapshot of the current head; a refused key is used for
    // nothing and reported in that check
    let repo_root = std::env::current_dir()
        .ok()
        .and_then(|cwd| crate::repo_root(&cwd));
    let repo = repo_root.as_deref().and_then(crate::repo_name);
    let (wiring, refused) = match crate::kaban_home() {
        Ok(home) => {
            match codegraph::wire(&home, repo_root.as_deref(), &policy, &GitRunner::default()) {
                Ok(wiring) => (wiring, None),
                Err(refused) => (None, Some(refused)),
            }
        }
        Err(_) => (None, None),
    };
    let schema_version = store.migration().to;
    let signer = store.signer();
    let api = KabanApi::with_config(
        store,
        policy,
        signer,
        None,
        ApiConfig {
            workspace,
            repo_root,
            repo,
            engine,
            ..ApiConfig::default()
        },
    );
    let api = match wiring {
        Some(wiring) => api.with_snapshot_provider(wiring.dir, wiring.provider),
        None => api,
    };
    // `run_tool` owns the gate and the printing; the verdict decides the exit code on top of
    // it (0 printed and ok, 5 printed and not ok; a withheld report keeps the seam's 2)
    let verdict = Cell::new(true);
    let code = seam::run_tool::<DoctorArgs, DoctorReport, _>(
        &api,
        "doctor",
        json!({ "vacuum": vacuum, "priors": priors, "optimize": optimize }),
        &[],
        format,
        |api, args| {
            let context = DoctorContext {
                files,
                schema_version,
                keys_dir: keys_dir.to_path_buf(),
                vacuum: args.vacuum,
                priors: args.priors,
            };
            let mut report = if args.optimize {
                api.doctor_optimize(&context)?
            } else {
                api.doctor(&context)
            };
            if let Some(refused) = refused
                && let Some(row) = report.checks.iter_mut().find(|c| c.name == "stale_anchors")
            {
                row.ok = false;
                row.detail = format!("not checked: {refused}");
                report.ok = false;
            }
            verdict.set(report.ok);
            Ok(report)
        },
    );
    if code == ExitCode::SUCCESS && !verdict.get() {
        ExitCode::from(EXIT_STORE_ERROR)
    } else {
        code
    }
}
