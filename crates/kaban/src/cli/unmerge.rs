//! `kaban unmerge <ID> [--reason <TEXT>]`: reverse a merge (engine-spec EN-F51 (5); engine-plan
//! E2.10). The `node_identity` rows naming the node are deleted, the identity claim behind each
//! becomes `deprecated` under the reason and its group is re-decided; both nodes stay. A human
//! action (the actor is the CLI); `--stdin-json` takes `{id, reason?}` instead of the flags. The
//! command goes through the seam like `forget` (core-spec S1, S4; OWN-10): the id is a control
//! field validated by the service, the reason is content and is gated before it is stored.

use crate::cli::ToolArgs;
use crate::cli::output::{EXIT_INVALID, Format, print_error};
use crate::cli::seam::run_write_tool;
use kaban_api::KabanApi;
use kaban_core::Actor;
use serde_json::{Map, Value};
use std::process::ExitCode;

/// The command's arguments as parsed by clap (caller data, not judged yet).
#[derive(clap::Args)]
pub(crate) struct Flags {
    /// Node id: the merged node or its canonical (omit with --stdin-json)
    id: Option<String>,
    /// Why the merge is reversed (stored as the identity claim's rank reason; default "unmerged")
    #[arg(long)]
    reason: Option<String>,
}

/// The typed request the seam parses.
#[derive(Debug, serde::Deserialize)]
struct UnmergeRequest {
    id: String,
    #[serde(default)]
    reason: Option<String>,
}

impl ToolArgs for Flags {
    const NAME: &'static str = "unmerge";

    fn any_given(&self) -> bool {
        self.id.is_some() || self.reason.is_some()
    }

    fn into_raw(self) -> Result<Value, ExitCode> {
        let Some(id) = self.id else {
            print_error("error: unmerge needs the node id (or --stdin-json)");
            return Err(ExitCode::from(EXIT_INVALID));
        };
        let mut args = Map::new();
        args.insert("id".into(), Value::String(id));
        if let Some(reason) = self.reason {
            args.insert("reason".into(), Value::String(reason));
        }
        Ok(Value::Object(args))
    }
}

/// Reverse the merge named by the raw request (flags or `--stdin-json`) through the seam and
/// print the result as JSON.
pub(crate) fn run(api: &KabanApi, raw: Value, format: Format) -> ExitCode {
    run_write_tool(
        api,
        "unmerge",
        raw,
        &[],
        format,
        |api, request: UnmergeRequest| {
            api.unmerge(&request.id, request.reason.as_deref(), Actor::HumanCli)
        },
    )
}
