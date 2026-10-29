//! `kaban import [--repo <root>]` (core-spec F13; core-plan C5.1 as amended by
//! `reviews/m5-prep.md` M5-13, M5-16): the explicit full import of `<root>/.kaban/` (every
//! record and tombstone file, then the absence pass) into the bound workspace. Nothing is
//! written to the repository. The request (`{repo}`) and the `ImportReport` go through the
//! seam under the unregistered scope `import`; stdout carries the one JSON document. Exit 0
//! on a completed pass (per-file problems are counts), 2 when the report is withheld, 4 for a
//! request or environment failure, 5 for a store failure.

use crate::Cli;
use crate::cli::output::Format;
use crate::cli::seam;
use kaban_api::sync::ImportReport;
use serde::Deserialize;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// The request the seam scans: the repository root.
#[derive(Debug, Deserialize)]
struct ImportArgs {
    repo: PathBuf,
}

/// Run `import` in the repository at `root`.
pub(crate) fn run(cli: Cli, root: &Path, format: Format) -> ExitCode {
    let api = match super::open_service(cli) {
        Ok(api) => api,
        Err(code) => return code,
    };
    seam::run_write_tool::<ImportArgs, ImportReport, _>(
        &api,
        "import",
        json!({ "repo": root.to_string_lossy() }),
        &[],
        format,
        |api, args| {
            kaban_api::sync::import(api, &args.repo, "import").map_err(kaban_api::ApiError::from)
        },
    )
}
