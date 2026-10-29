//! `kaban governance-history --id <ID> | --symbol <KEY> | --path <P>`: the `SUPERSEDES` chain
//! and the governance events of the records one selector reaches (stage1-spec G1-F13; v4
//! CG6.2). Exactly one selector; `--stdin-json` takes the whole `GovernanceHistoryRequest`
//! wire object instead of any flag. The command goes through the seam like the MCP adapter
//! (core-spec S1, S4; OWN-10; M4-3); `cli/why.rs` is the model.

use crate::cli::ToolArgs;
use crate::cli::output::Format;
use crate::cli::seam::run_tool;
use kaban_api::{GovernanceHistoryRequest, KabanApi};
use serde_json::{Map, Value};
use std::process::ExitCode;

/// The command's arguments as parsed by clap (caller data, not judged yet).
#[derive(clap::Args)]
pub(crate) struct Flags {
    /// One record by id (archived records included)
    #[arg(long, value_name = "ID")]
    id: Option<String>,
    /// A normalised SCIP symbol key: the records `why --symbol` reaches, superseded included
    #[arg(long, value_name = "KEY")]
    symbol: Option<String>,
    /// A repository-relative path: the records whose `path_scope` governs it, superseded included
    #[arg(long, value_name = "PATH")]
    path: Option<String>,
    /// Max records (default 20, max 100)
    #[arg(long)]
    limit: Option<usize>,
    /// Workspace to read (default: the bound workspace; `*` for all)
    #[arg(long)]
    workspace_id: Option<String>,
}

impl ToolArgs for Flags {
    const NAME: &'static str = "governance_history";

    fn any_given(&self) -> bool {
        self.id.is_some()
            || self.symbol.is_some()
            || self.path.is_some()
            || self.limit.is_some()
            || self.workspace_id.is_some()
    }

    fn into_raw(self) -> Result<Value, ExitCode> {
        let mut args = Map::new();
        for (key, value) in [
            ("id", self.id),
            ("symbol", self.symbol),
            ("path", self.path),
            ("workspace_id", self.workspace_id),
        ] {
            if let Some(value) = value {
                args.insert(key.into(), Value::String(value));
            }
        }
        if let Some(limit) = self.limit {
            args.insert("limit".into(), Value::from(limit));
        }
        Ok(Value::Object(args))
    }
}

/// Answer the raw request (flags or `--stdin-json`) through the seam and print the result as
/// JSON.
pub(crate) fn run(api: &KabanApi, raw: Value, format: Format) -> ExitCode {
    run_tool(
        api,
        "governance_history",
        raw,
        &[],
        format,
        |api, request: GovernanceHistoryRequest| api.governance_history(request),
    )
}
