//! `kaban review [--ontology <ns>] [--kind <kind>] [--status <pending>] [--limit <n>]`
//! (engine-spec EN-F14a; engine-plan E2.5): every pending item of the bound workspace with
//! its one sentence and its first next action, so a person works through them in one
//! sitting. `--status` is a closed `value_enum` of the pending kinds: a settled word
//! (`accepted`) is a clap error, never a variant. A CLI-only verb that goes through
//! [`crate::cli::seam::run_tool`] like `explain`, so every string is content and the result is
//! gated on its way out.

use crate::cli::output::Format;
use crate::cli::seam::run_tool;
use kaban_api::{KabanApi, PendingStatusKind, ReviewQuery};
use serde_json::{Map, Value, json};
use std::process::ExitCode;

/// The pending statuses `--status` accepts (the wire words of `PendingStatusKind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum StatusFilter {
    /// Two claims disagree and a human must pick.
    Unresolved,
    /// A reconciliation left the item for review.
    Candidate,
    /// The record's file and database disagree at the same rev.
    Conflict,
    /// A `CodeRef` whose symbol the snapshot no longer holds.
    Stale,
    /// A record awaiting acceptance.
    Proposed,
    /// A discovered `x:` kind awaiting promotion or pruning.
    Discovered,
}

impl From<StatusFilter> for PendingStatusKind {
    fn from(s: StatusFilter) -> Self {
        match s {
            StatusFilter::Unresolved => Self::Unresolved,
            StatusFilter::Candidate => Self::Candidate,
            StatusFilter::Conflict => Self::Conflict,
            StatusFilter::Stale => Self::Stale,
            StatusFilter::Proposed => Self::Proposed,
            StatusFilter::Discovered => Self::Discovered,
        }
    }
}

/// The command's arguments as parsed by clap (caller data, not judged yet).
#[derive(clap::Args)]
pub(crate) struct Flags {
    /// Only items whose kind belongs to this ontology namespace (`infra`, `x`, ...)
    #[arg(long, value_name = "NS")]
    ontology: Option<String>,
    /// Only items of this stored kind (`infra:Vendor`, `x:Org`, `Decision`)
    #[arg(long, value_name = "KIND")]
    kind: Option<String>,
    /// Only items of this pending status
    #[arg(long, value_enum)]
    status: Option<StatusFilter>,
    /// At most this many items (default 50, max 500)
    #[arg(long)]
    limit: Option<usize>,
}

/// Run the command and print its one JSON document.
pub(crate) fn run(api: &KabanApi, flags: Flags, format: Format) -> ExitCode {
    let mut args = Map::new();
    if let Some(ns) = flags.ontology {
        args.insert("ontology".into(), Value::String(ns));
    }
    if let Some(kind) = flags.kind {
        args.insert("kind".into(), Value::String(kind));
    }
    if let Some(status) = flags.status {
        let kind: PendingStatusKind = status.into();
        args.insert("status".into(), json!(kind));
    }
    if let Some(limit) = flags.limit {
        args.insert("limit".into(), Value::from(limit));
    }
    run_tool(
        api,
        "review",
        Value::Object(args),
        &[],
        format,
        |api, query: ReviewQuery| api.review(&query),
    )
}
