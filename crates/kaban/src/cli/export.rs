//! `kaban export [--format agents-md|claude-md|markdown] [--write <file>] [--ontology
//! <name>[,...]|all]` (core-spec F9a, S1, S4, §7; core-plan C5.2 as amended by
//! `reviews/m5-prep.md` M5-3, M5-16, M5-17; engine-plan E1.6, OWN-46): render the accepted
//! records of the exportable kinds of the selected ontologies (default all; the shipped
//! ontology: Decisions and Conventions) into the managed marker block and print it, or splice
//! it into a user file. The request (`{format, write, ontologies}`) and the report go through
//! the seam under the unregistered scope `export` (M4-3: every string is content, the walk
//! fails closed); the block itself is gated as one string before a byte is written or printed
//! (M5-3). Without `--write` stdout carries `{"format", "records", "block"}`, with it
//! `{"outcome", "path", "records"}`. Exit 0 on a completed run, 2 when the block is withheld
//! (a reject hit, or a redact hit: the fixed note names `kaban rescan`) or `--ontology` names
//! an unloaded ontology (`ontology_unknown`), 4 for a malformed marker pair or an unusable
//! path, 5 for a store failure.

use crate::Cli;
use crate::cli::output::{self, Format};
use crate::cli::seam;
use kaban_api::ApiError;
use kaban_api::export::{
    ExportError, ExportOpts, ExportReport, REDACT_WITHHELD_NOTE, WithheldCause, export,
};
use serde_json::json;
use std::cell::Cell;
use std::path::PathBuf;
use std::process::ExitCode;

/// The export block formats (core-spec F9a).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub(crate) enum FormatArg {
    /// A block for `AGENTS.md`, with the session-start instruction line.
    #[default]
    AgentsMd,
    /// A block for `CLAUDE.md` (same body as `agents-md`).
    ClaudeMd,
    /// The plain block without the instruction line.
    Markdown,
}

impl FormatArg {
    /// The spelling the seam parses into `ExportFormat` (kaban's own enum text, not caller
    /// data).
    const fn as_str(self) -> &'static str {
        match self {
            Self::AgentsMd => "agents-md",
            Self::ClaudeMd => "claude-md",
            Self::Markdown => "markdown",
        }
    }
}

/// Run `export`: open the service, build the one raw request from the flags and hand it to
/// the seam; a redact-class withholding adds its fixed note after the seam's own line.
pub(crate) fn run(
    cli: Cli,
    block: FormatArg,
    write: Option<PathBuf>,
    ontology: Option<&str>,
    format: Format,
) -> ExitCode {
    let api = match super::open_service(cli) {
        Ok(api) => api,
        Err(code) => return code,
    };
    let redact_hit = Cell::new(false);
    let code = seam::run_tool::<ExportOpts, ExportReport, _>(
        &api,
        "export",
        json!({
            "format": block.as_str(),
            "write": write.map(|p| p.to_string_lossy().into_owned()),
            "ontologies": super::sync::ontology_names(ontology),
        }),
        &[],
        format,
        |api, opts| {
            export(api, &opts).map_err(|e| {
                if matches!(
                    e,
                    ExportError::Withheld {
                        cause: WithheldCause::Redact,
                        ..
                    }
                ) {
                    redact_hit.set(true);
                }
                ApiError::from(e)
            })
        },
    );
    if redact_hit.get() {
        output::print_error(&format!("note: {REDACT_WITHHELD_NOTE}"));
    }
    code
}
