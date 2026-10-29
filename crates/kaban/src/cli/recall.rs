//! `kaban recall <QUERY>`: search memories from the command line (core-spec F3, F7; core-plan
//! C4.1b). The flags mirror the `recall` tool's scalar and list fields; the structured `ref`
//! filter and `enrich` list are reachable through `--stdin-json`, which takes the whole
//! `RecallRequest` wire object instead of any flag. The command goes through the seam like the
//! MCP adapter (core-spec S1, S4; OWN-10; M4-3).

use crate::cli::ToolArgs;
use crate::cli::output::{EXIT_INVALID, Format, print_error};
use crate::cli::seam::run_tool;
use kaban_api::{KabanApi, RecallRequest};
use serde_json::{Map, Value};
use std::process::ExitCode;

/// The command's arguments as parsed by clap (caller data, not judged yet).
#[derive(clap::Args)]
#[allow(clippy::struct_excessive_bools)] // Independent user switches, not mutually exclusive states.
pub(crate) struct Flags {
    /// Free-text query: keywords, identifiers, error strings (omit with --stdin-json)
    query: Option<String>,
    /// Retrieval sources: fts,fuzzy (sim is reserved)
    #[arg(long, value_delimiter = ',')]
    layers: Vec<String>,
    /// Also search fuzzy matches when lexical retrieval hits
    #[arg(long)]
    fuzzy: bool,
    /// Restrict to this kind; repeat for several
    #[arg(long = "kind", value_name = "KIND")]
    kinds: Vec<String>,
    /// Require at least one of these tags; repeat for several
    #[arg(long = "tag", value_name = "TAG")]
    tags: Vec<String>,
    /// Workspace to search (default: the bound workspace; `*` for all)
    #[arg(long)]
    workspace_id: Option<String>,
    /// Max results (default 10, max 100)
    #[arg(long)]
    limit: Option<usize>,
    /// Graph expansion hops 0..2 (default 1)
    #[arg(long)]
    hops: Option<u8>,
    /// Also return superseded (archived) records reached through graph expansion
    #[arg(long, default_value_t = false)]
    include_superseded: bool,
    /// Also return archived records of any governance reached through graph expansion
    #[arg(long, default_value_t = false)]
    include_archived: bool,
    /// Return full text instead of summaries
    #[arg(long, default_value_t = false)]
    full_text: bool,
}

impl ToolArgs for Flags {
    const NAME: &'static str = "recall";

    fn any_given(&self) -> bool {
        self.query.is_some()
            || !self.layers.is_empty()
            || self.fuzzy
            || !self.kinds.is_empty()
            || !self.tags.is_empty()
            || self.workspace_id.is_some()
            || self.limit.is_some()
            || self.hops.is_some()
            || self.include_superseded
            || self.include_archived
            || self.full_text
    }

    fn into_raw(self) -> Result<Value, ExitCode> {
        let Some(query) = self.query else {
            print_error("error: recall needs the query (or --stdin-json)");
            return Err(ExitCode::from(EXIT_INVALID));
        };
        let strings =
            |values: Vec<String>| Value::Array(values.into_iter().map(Value::String).collect());
        let mut args = Map::new();
        args.insert("query".into(), Value::String(query));
        if !self.layers.is_empty() {
            args.insert("layers".into(), strings(self.layers));
        }
        args.insert("fuzzy".into(), Value::Bool(self.fuzzy));
        args.insert("kinds".into(), strings(self.kinds));
        args.insert("tags".into(), strings(self.tags));
        if let Some(workspace_id) = self.workspace_id {
            args.insert("workspace_id".into(), Value::String(workspace_id));
        }
        if let Some(limit) = self.limit {
            args.insert("limit".into(), Value::from(limit));
        }
        if let Some(hops) = self.hops {
            args.insert("hops".into(), Value::from(hops));
        }
        args.insert(
            "include_superseded".into(),
            Value::Bool(self.include_superseded),
        );
        args.insert(
            "include_archived".into(),
            Value::Bool(self.include_archived),
        );
        args.insert("full_text".into(), Value::Bool(self.full_text));
        Ok(Value::Object(args))
    }
}

/// Run the raw request (flags or `--stdin-json`) through the seam and print the result as
/// JSON.
pub(crate) fn run(api: &KabanApi, raw: Value, format: Format) -> ExitCode {
    run_tool(
        api,
        "recall",
        raw,
        &[],
        format,
        |api, request: RecallRequest| api.recall(request),
    )
}
