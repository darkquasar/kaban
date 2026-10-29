//! `kaban stats`: the same gated response as MCP stats and `kaban://stats` (OWN-89).

use crate::cli::{ToolArgs, output::Format, seam::run_tool};
use kaban_api::{KabanApi, StatsRequest};
use serde_json::{Value, json};
use std::process::ExitCode;

/// Optional workspace selector; the global `--workspace` remains the bound workspace.
#[derive(clap::Args)]
pub(crate) struct Flags {
    /// Workspace to count (default: bound workspace; `*` selects all)
    #[arg(long)]
    workspace_id: Option<String>,
}

impl ToolArgs for Flags {
    const NAME: &'static str = "stats";
    fn any_given(&self) -> bool {
        self.workspace_id.is_some()
    }
    fn into_raw(self) -> Result<Value, ExitCode> {
        Ok(self
            .workspace_id
            .map_or_else(|| json!({}), |workspace| json!({"workspace_id":workspace})))
    }
}

/// Scan arguments and the response through the shared tool seam, then print JSON.
pub(crate) fn run(api: &KabanApi, raw: Value, format: Format) -> ExitCode {
    run_tool(
        api,
        "stats",
        raw,
        &[],
        format,
        |api, request: StatsRequest| api.stats(request),
    )
}
