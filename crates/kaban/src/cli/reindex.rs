//! `kaban reindex [text|graph|all]` (spec 11 PC-7): rebuild derived indexes from this
//! database's rows, through the same request/result gates as other CLI calls; canonical
//! records and repository files are never changed.
use crate::Cli;
use crate::cli::{output::Format, seam};
use kaban_api::{ReindexReport, ReindexTarget};
use serde::Deserialize;
use serde_json::json;
use std::process::ExitCode;

/// The indexes `reindex` rebuilds.
#[derive(Clone, Copy, clap::ValueEnum)]
pub(crate) enum Which {
    /// The text index
    Text,
    /// The memory graph
    Graph,
    /// Every derived index
    All,
}

impl From<Which> for ReindexTarget {
    fn from(which: Which) -> Self {
        match which {
            Which::Text => Self::Text,
            Which::Graph => Self::Graph,
            Which::All => Self::All,
        }
    }
}

/// The request the seam scans: the one target, no caller string.
#[derive(Deserialize)]
struct ReindexArgs {
    which: ReindexTarget,
}

/// `reindex` arguments.
#[derive(clap::Args)]
pub(crate) struct Flags {
    /// Which derived indexes to rebuild
    #[arg(value_enum, default_value_t = Which::All)]
    which: Which,
}

/// Rebuild the indexes `flags` names against the bound database.
pub(crate) fn run(cli: Cli, flags: &Flags, format: Format) -> ExitCode {
    let api = match super::open_service(cli) {
        Ok(api) => api,
        Err(code) => return code,
    };
    let which = ReindexTarget::from(flags.which);
    seam::run_tool::<ReindexArgs, ReindexReport, _>(
        &api,
        "reindex",
        json!({ "which": which }),
        format,
        |api, args| api.reindex(args.which),
    )
}
