//! `kaban resolve <id> --take file|db [--repo <root>]` (core-spec F13; core-plan C5.1 as
//! amended by `reviews/m5-prep.md` M5-16, M5-31, M5-32): resolve an equal-rev conflict
//! between a record's `.kaban/*.md` file and the database. `--take db` bumps the record above
//! the file and rewrites the file; `--take file` imports the file at a rev above both sides
//! and re-serializes it; both clear the `conflicts` row and print the projected record as the
//! one JSON `ResolveReport`. The id is caller data: it travels in the raw request the seam
//! scans under the unregistered scope `resolve` and is never echoed before the seam judged
//! it. Exit 0 ok, 2 withheld, 3 when the id has no conflict row, 4 when `--take file` asks
//! for a governance move the spec forbids (or another request error), 5 store error.
//!
//! `kaban resolve <id> --winner <value> [--group <key>]` (engine-plan E2.5; EN-F5a, EN-F12c;
//! wave-3 review RW3-3): a human names the winner of one of the node's outstanding candidate
//! groups, the one `--group` names (the key `kaban review` lists on the item and the rendered
//! action carries) or the node's only one; the value and the key are content under the scope
//! `resolve` and travel through the same seam. Exit 0 ok, 2 withheld, 3 when the node has no
//! candidate group or the key is not one of its groups, 4 for an agent actor, several groups
//! with no `--group` (`ambiguous_group`) or another request error, 5 store error.
//!
//! `kaban resolve <coderef-id> --symbol <key>` (engine-plan E2.5, SO-6; stage1-spec §4.2): a
//! human re-targets a stale `CodeRef` to the normalised key its symbol has now; the key is
//! content under the scope `resolve` and travels through the same seam. Exit 0 ok, 2
//! withheld, 3 when the id names no active `CodeRef`, 4 for an agent actor, a versioned or
//! `local` key (`symbol_invalid`) or another request error, 5 store error.

use crate::Cli;
use crate::cli::output::Format;
use crate::cli::seam;
use kaban_api::sync::ResolveReport;
use serde::Deserialize;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// Which side of an equal-rev conflict wins (core-spec F13 `--take file|db`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum Take {
    /// The `.kaban/*.md` file is imported at a rev above both sides.
    File,
    /// The database record is kept, its rev bumped, and the file rewritten.
    Db,
}

impl From<Take> for kaban_api::sync::Take {
    fn from(t: Take) -> Self {
        match t {
            Take::File => Self::File,
            Take::Db => Self::Db,
        }
    }
}

/// The request the seam scans: the id (content under this scope), the side and the root.
#[derive(Debug, Deserialize)]
struct ResolveArgs {
    id: String,
    take: kaban_api::sync::Take,
    repo: PathBuf,
}

/// Run `resolve` for `id` in the repository at `root`.
pub(crate) fn run(cli: Cli, id: &str, take: Take, root: &Path, format: Format) -> ExitCode {
    let api = match super::open_service(cli) {
        Ok(api) => api,
        Err(code) => return code,
    };
    let take: kaban_api::sync::Take = take.into();
    seam::run_tool::<ResolveArgs, ResolveReport, _>(
        &api,
        "resolve",
        json!({ "id": id, "take": take, "repo": root.to_string_lossy() }),
        &[],
        format,
        |api, args| {
            // a string that is not a node id names no node (D2): not found, echoing the
            // gated form the seam already judged
            let id = kaban_core::NodeId::parse(&args.id)
                .map_err(|_| kaban_api::ApiError::NotFound(args.id.clone()))?;
            kaban_api::sync::resolve(api, &args.repo, &id, args.take)
                .map_err(kaban_api::ApiError::from)
        },
    )
}

/// The `--winner` request the seam scans: the id, the value and the group key (content under
/// this scope).
#[derive(Debug, Deserialize)]
struct WinnerArgs {
    id: String,
    winner: String,
    group: Option<String>,
}

/// Run `resolve <id> --winner <value> [--group <key>]`: a human claim (`Actor::HumanCli`, the
/// CLI is the one human surface; the MCP side has no such tool).
pub(crate) fn run_winner(
    cli: Cli,
    id: &str,
    winner: &str,
    group: Option<&str>,
    format: Format,
) -> ExitCode {
    let api = match super::open_service(cli) {
        Ok(api) => api,
        Err(code) => return code,
    };
    seam::run_write_tool::<WinnerArgs, kaban_api::ResolveWinnerResponse, _>(
        &api,
        "resolve",
        json!({ "id": id, "winner": winner, "group": group }),
        &[],
        format,
        |api, args| {
            let id = kaban_core::NodeId::parse(&args.id)
                .map_err(|_| kaban_api::ApiError::NotFound(args.id.clone()))?;
            api.resolve_winner(
                &id,
                &args.winner,
                args.group.as_deref(),
                kaban_core::Actor::HumanCli,
            )
        },
    )
}

/// The `--symbol` request the seam scans: the id and the key (content under this scope).
#[derive(Debug, Deserialize)]
struct SymbolArgs {
    id: String,
    symbol: String,
}

/// Run `resolve <coderef-id> --symbol <key>`: a human re-target claim (`Actor::HumanCli`,
/// the CLI is the one human surface; the MCP side has no such tool).
pub(crate) fn run_symbol(cli: Cli, id: &str, key: &str, format: Format) -> ExitCode {
    let api = match super::open_service(cli) {
        Ok(api) => api,
        Err(code) => return code,
    };
    seam::run_write_tool::<SymbolArgs, kaban_api::ResolveSymbolResponse, _>(
        &api,
        "resolve",
        json!({ "id": id, "symbol": key }),
        &[],
        format,
        |api, args| {
            let id = kaban_core::NodeId::parse(&args.id)
                .map_err(|_| kaban_api::ApiError::NotFound(args.id.clone()))?;
            api.resolve_symbol(&id, &args.symbol, kaban_core::Actor::HumanCli)
        },
    )
}
