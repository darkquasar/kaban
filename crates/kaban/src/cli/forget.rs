//! `kaban forget <ID> [--mode <MODE>]`: tombstone (default), invalidate, archive or purge a
//! memory from the command line (core-spec F5, F7; core-plan C4.1b). `--stdin-json` takes the
//! whole `ForgetRequest` wire object instead of any flag. The actor is the agent (M4-5). The
//! command goes through the seam like the MCP adapter (core-spec S1, S4; OWN-10; M4-3): the id
//! is a control field validated by the service, anything else in its place is content and is
//! gated before the lookup; a mode the wire shape does not know is refused by the typed parse
//! with its value scrubbed. `--document <path>` (docs-ingest-spec D10, DI1.6; behind the
//! `ingest` feature) tombstones an ingested Document with its sections and chunks instead: the
//! path is content, gated before any identity is derived from it (PR-2).

use crate::cli::ToolArgs;
use crate::cli::output::{EXIT_INVALID, Format, print_error};
use crate::cli::seam::run_write_tool;
use kaban_api::{ForgetRequest, KabanApi};
use serde_json::{Map, Value};
use std::process::ExitCode;

/// The command's arguments as parsed by clap (caller data, not judged yet).
#[derive(clap::Args)]
pub(crate) struct Flags {
    /// Node id (omit with --stdin-json)
    id: Option<String>,
    /// tombstone (default: hidden, never re-proposed) | invalidate (tombstone plus governance
    /// `invalidated` with an event) | archive | purge (destructive: removes text, index entries
    /// and edges; the id never reappears)
    #[arg(long)]
    mode: Option<String>,
    /// Reject this machine layer's claims and clear its DB projections, preserving history
    #[arg(long,conflicts_with_all=["id","mode"])]
    layer: Option<String>,
    /// Tombstone the ingested document at this workspace-relative path, with its sections
    /// and chunks (instead of a node id)
    #[cfg(feature = "ingest")]
    #[arg(long, conflicts_with_all = ["id", "mode"])]
    document: Option<String>,
}

/// `forget --document`'s typed request.
#[cfg(feature = "ingest")]
#[derive(Debug, serde::Deserialize)]
struct ForgetDocumentRequest {
    document: String,
}

impl ToolArgs for Flags {
    const NAME: &'static str = "forget";

    fn any_given(&self) -> bool {
        if self.layer.is_some() {
            return true;
        }
        #[cfg(feature = "ingest")]
        if self.document.is_some() {
            return true;
        }
        self.id.is_some() || self.mode.is_some()
    }

    fn into_raw(self) -> Result<Value, ExitCode> {
        if let Some(layer) = self.layer {
            let mut args = Map::new();
            args.insert("layer".into(), Value::String(layer));
            #[cfg(feature = "ingest")]
            if let Some(document) = self.document {
                args.insert("document".into(), Value::String(document));
            }
            return Ok(Value::Object(args));
        }
        #[cfg(feature = "ingest")]
        if let Some(document) = self.document {
            let mut args = Map::new();
            args.insert("document".into(), Value::String(document));
            return Ok(Value::Object(args));
        }
        let Some(id) = self.id else {
            print_error("error: forget needs the node id (or --stdin-json)");
            return Err(ExitCode::from(EXIT_INVALID));
        };
        let mut args = Map::new();
        args.insert("id".into(), Value::String(id));
        if let Some(mode) = self.mode {
            args.insert("mode".into(), Value::String(mode));
        }
        Ok(Value::Object(args))
    }
}

/// Forget the node named by the raw request (flags or `--stdin-json`) through the seam and
/// print the result as JSON.
pub(crate) fn run(api: &KabanApi, raw: Value, format: Format) -> ExitCode {
    if raw.get("layer").is_some() {
        #[derive(serde::Deserialize)]
        struct Request {
            layer: kaban_core::Layer,
            #[serde(default)]
            document: Option<String>,
        }
        return run_write_tool(
            api,
            "forget_layer",
            raw,
            &[],
            format,
            |api, request: Request| api.forget_layer(request.layer, request.document.as_deref()),
        );
    }
    #[cfg(feature = "ingest")]
    if raw.get("document").is_some() {
        return run_write_tool(
            api,
            "forget",
            raw,
            &[],
            format,
            |api, request: ForgetDocumentRequest| {
                let policy = api.policy_handle();
                let policy = policy
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let ctx = kaban_core::WriteCtx::new(
                    api.policy_version(),
                    api.workspace().clone(),
                    kaban_core::Actor::Agent,
                );
                kaban_ingest::forget_document(
                    api.store().as_ref(),
                    &policy,
                    &ctx,
                    &request.document,
                )
            },
        );
    }
    run_write_tool(
        api,
        "forget",
        raw,
        &[],
        format,
        |api, request: ForgetRequest| api.forget(request),
    )
}
