//! `kaban why --path <P> | --query <Q>`: the governing records for paths and queries (core-spec
//! F2, F7; core-plan C4.1b). Each `--path` and `--query` is one target of the `why` tool
//! (1..=16 per call); the targets are sent paths first, in the order given, then queries.
//! `--stdin-json` takes the whole `WhyRequest` wire object instead of any flag. The command goes
//! through the seam like the MCP adapter (core-spec S1, S4; OWN-10; M4-3).

use crate::cli::ToolArgs;
use crate::cli::output::Format;
use crate::cli::seam::run_tool;
use kaban_api::{KabanApi, WhyRequest};
use serde_json::{Map, Value, json};
use std::process::ExitCode;

/// The command's arguments as parsed by clap (caller data, not judged yet).
#[derive(clap::Args)]
pub(crate) struct Flags {
    /// A repository-relative path whose governing records are wanted; repeat for several
    #[arg(long = "path", value_name = "PATH")]
    paths: Vec<String>,
    /// A free-text query matched against the records; repeat for several
    #[arg(long = "query", value_name = "QUERY")]
    queries: Vec<String>,
    /// Max records per target (default 10, max 100)
    #[arg(long)]
    limit: Option<usize>,
    /// Workspace to read (default: the bound workspace; `*` for all)
    #[arg(long)]
    workspace_id: Option<String>,
}

impl ToolArgs for Flags {
    const NAME: &'static str = "why";

    fn any_given(&self) -> bool {
        !self.paths.is_empty()
            || !self.queries.is_empty()
            || self.limit.is_some()
            || self.workspace_id.is_some()
    }

    fn into_raw(self) -> Result<Value, ExitCode> {
        let targets = self
            .paths
            .into_iter()
            .map(|path| json!({ "path": path }))
            .chain(
                self.queries
                    .into_iter()
                    .map(|query| json!({ "query": query })),
            )
            .collect();
        let mut args = Map::new();
        args.insert("targets".into(), Value::Array(targets));
        if let Some(limit) = self.limit {
            args.insert("limit".into(), Value::from(limit));
        }
        if let Some(workspace_id) = self.workspace_id {
            args.insert("workspace_id".into(), Value::String(workspace_id));
        }
        Ok(Value::Object(args))
    }
}

/// Answer the raw request (flags or `--stdin-json`) through the seam and print the result as
/// JSON.
pub(crate) fn run(api: &KabanApi, raw: Value, format: Format) -> ExitCode {
    run_tool(api, "why", raw, &[], format, |api, request: WhyRequest| {
        api.why(request)
    })
}
