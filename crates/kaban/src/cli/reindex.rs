//! DB-only maintenance, through the same request/result gates as other CLI calls.
use crate::Cli;
use crate::cli::{output::Format, seam};
use serde_json::json;
use std::process::ExitCode;

#[derive(serde::Deserialize)]
struct LayerRequest {
    layer: kaban_core::Layer,
}

/// Retained reindex operations.
#[derive(clap::Subcommand)]
pub(crate) enum Command {
    /// Rebuild the text index from the database without changing canonical content
    Text,
    /// DB-only reset; generated links remain absent until explicit ingestion with the layer enabled
    Layer { name: String },
}

/// Execute a maintenance pass against the bound database, never repository files.
pub(crate) fn run(cli: Cli, command: &Command, format: Format) -> ExitCode {
    match command {
        Command::Layer { name } => {
            let api = match super::open_service(cli) {
                Ok(api) => api,
                Err(code) => return code,
            };
            seam::run_tool(
                &api,
                "reindex_layer",
                json!({"layer":name}),
                &[],
                format,
                |api, request: LayerRequest| {
                    let report = api.reindex_layer(request.layer)?;
                    super::output::print_error(
                        "DB-only reset: generated links remain absent until explicit ingestion with the layer enabled; rebuilding has not run.",
                    );
                    Ok(report)
                },
            )
        }
        Command::Text => {
            let api = match super::open_service(cli) {
                Ok(api) => api,
                Err(code) => return code,
            };
            seam::run_tool::<serde_json::Value, serde_json::Value, _>(
                &api,
                "reindex",
                json!({}),
                &[],
                format,
                |api, _| {
                    api.store()
                        .reindex_text()
                        .map(|rows| json!({"rows": rows}))
                        .map_err(kaban_api::ApiError::from)
                },
            )
        }
    }
}
