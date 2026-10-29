//! `kaban explain <address>` (engine-spec EN-F14a; engine-plan E2.5): the one-stop status of
//! anything addressable (a node id, `e:<edge id>`, a claim key, `x:Kind`, `<path>:<offset>`),
//! and the one human action of E2.3, `--deprecate <reason>` on a claim key (engine-spec
//! EN-F12b): the claim's rank becomes `deprecated` and its group is re-decided without its
//! vote. Both branches go through [`crate::cli::seam::run_tool`] as `accept` does, so the raw
//! request is scanned before the typed parse (the address is content first, then grammar,
//! the E2.6 order), every string is content and the result is gated on its way out; the
//! deprecation runs with actor `human-cli`. An address that names nothing exits 3.

use crate::cli::output::Format;
use crate::cli::seam::{run_tool, run_write_tool};
use kaban_api::{DeprecateRequest, ExplainStatusRequest, KabanApi};
use kaban_core::Actor;
use serde_json::json;
use std::process::ExitCode;

/// The command's arguments as parsed by clap (caller data, not judged yet).
#[derive(clap::Args)]
pub(crate) struct Flags {
    /// What to explain: a node id, `e:<edge id>`, a claim key (64 hex digits, as `traverse`
    /// cards and `doctor` list it), a discovered kind (`x:Org`) or `<path>:<offset>`
    pub(crate) address: String,
    /// Deprecate the claim (the address must be a claim key) with this reason: it no longer
    /// votes in its group and is listed as dissent; the group is re-decided at once. Without
    /// this flag `explain` prints the status document: the status with its reason sentence,
    /// the group's explanation where one exists (the candidates in decision order with their
    /// lineages and weights, the margin and the gates), the history and the next actions
    #[arg(long, value_name = "REASON")]
    pub(crate) deprecate: Option<String>,
}

/// Run the command and print its one JSON document.
pub(crate) fn run(api: &KabanApi, flags: Flags, format: Format) -> ExitCode {
    let Some(reason) = flags.deprecate else {
        return run_tool(
            api,
            "explain",
            json!({"address": flags.address}),
            &[],
            format,
            |api, request: ExplainStatusRequest| api.explain_status(request),
        );
    };
    run_write_tool(
        api,
        "explain",
        json!({"key": flags.address, "reason": reason}),
        &[],
        format,
        |api, request: DeprecateRequest| api.deprecate(request, Actor::HumanCli),
    )
}
