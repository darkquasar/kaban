//! The one place in the binary that writes to stdout outside the MCP transport, the CLI's
//! stderr error channel, and the one exit-code mapping (core-spec §6; M4-2). `clippy::print_stdout`
//! is denied workspace-wide (core-plan.md §17, E2); the allows below are the single justified
//! exception: CLI subcommands print their one JSON document (or clap's help) and exit, and never
//! run concurrently with the stdio MCP server in the same process.

use kaban_api::ApiError;
use kaban_core::StoreError;
use serde::Serialize;
use std::process::ExitCode;

/// Exit codes of core-spec §6: 0 ok, 2 rejected by policy (also a withheld response), 3 not
/// found, 4 policy/config or request error (also a usage error), 5 store error. 1 is kept for a
/// runtime failure the spec gives no code (a transport or serialization error). A write's
/// receipt (spec 05 SS-10, `kaban_api::WriteOutcome`) maps onto the same table: `committed`
/// is 0, `rejected` is the refusing error's own code and `committed_not_yet_searchable` is 6
/// (the write stands and must not be replayed; a `set -e` script handles 6 explicitly).
pub(crate) const EXIT_FAILURE: u8 = 1;
pub(crate) const EXIT_REJECTED: u8 = 2;
pub(crate) const EXIT_NOT_FOUND: u8 = 3;
pub(crate) const EXIT_INVALID: u8 = 4;
pub(crate) const EXIT_STORE_ERROR: u8 = 5;
pub(crate) const EXIT_PENDING_SEARCH: u8 = 6;

/// Whether a write's response carries a `committed_not_yet_searchable` receipt (SS-10): the
/// receipt sits at the response's top level or, in a composite report (`sync`), one level
/// down; nothing else in a response is a receipt.
pub(crate) fn pending_search(response: &serde_json::Value) -> bool {
    let pending = |v: &serde_json::Value| {
        v.get("receipt")
            .and_then(|r| r.get("outcome"))
            .and_then(serde_json::Value::as_str)
            == Some("committed_not_yet_searchable")
    };
    pending(response)
        || response
            .as_object()
            .is_some_and(|fields| fields.values().any(pending))
}

/// Printed by the tool seam instead of a result its serializer refused (review R4-3). Fixed on
/// purpose: a serializer's message may quote the value, so the error is never formatted.
pub(crate) const SERIALIZATION_FAILED: &str = "error: response could not be serialized";

/// How the one JSON document on stdout is laid out: pretty (default) or compact single-line
/// (`--json`, M4-4). Both parse as one value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Format {
    Pretty,
    Compact,
}

/// Print a value as one JSON document followed by a newline.
///
/// # Errors
/// Serialization failure (never for the plain data types passed here).
#[allow(clippy::print_stdout)] // CLI result channel; the MCP server never reaches this path
pub(crate) fn print_json<T: Serialize>(v: &T, format: Format) -> serde_json::Result<()> {
    let text = match format {
        Format::Pretty => serde_json::to_string_pretty(v)?,
        Format::Compact => serde_json::to_string(v)?,
    };
    println!("{text}");
    Ok(())
}

/// [`print_json`] in the default layout, for the bench report (`bench` prints its one document
/// pretty whatever `--json` says: the benchmark facade is not part of the C4.1 adapter).
///
/// # Errors
/// Serialization failure.
pub(crate) fn print_json_pretty<T: Serialize>(v: &T) -> anyhow::Result<()> {
    print_json(v, Format::Pretty)?;
    Ok(())
}

/// Print clap's help or version text (fixed text kaban declares, no argument value) on the
/// stream clap chose for it (stdout for `--help`/`--version`).
///
/// # Errors
/// The write failed.
pub(crate) fn print_clap_help(e: &clap::Error) -> std::io::Result<()> {
    e.print()
}

/// Print a user-facing CLI error line to stderr (exit codes are chosen by the caller).
#[allow(clippy::print_stderr)] // CLI error channel; `tracing` may be filtered off by RUST_LOG
pub(crate) fn print_error(msg: &str) {
    eprintln!("{msg}");
}

/// Print a failure at a CLI boundary as `error: <message>`, where the message is the plain
/// `Display` of `e`: its outer text only, never a chained `{:#}` or `{:?}` rendering, so a
/// source deeper in the chain (a storage driver's message, which may quote SQL text or bound
/// values) cannot reach the terminal (review RM-1, core-spec S1). For an `anyhow::Error` this
/// is the outermost context; for a typed error its own message.
pub(crate) fn print_failure(e: &dyn std::fmt::Display) {
    print_error(&format!("error: {e}"));
}

/// The exit code of a service error (M4-2; the orphan rule rules out `From<&ApiError> for
/// ExitCode` here): `Rejected` 2, `NotFound` 3, `Invalid` and `Policy` 4, a request-shaped
/// store error 4, every other store error 5.
pub(crate) fn exit_code(e: &ApiError) -> ExitCode {
    ExitCode::from(match e {
        ApiError::Rejected { .. } => EXIT_REJECTED,
        ApiError::NotFound(_) => EXIT_NOT_FOUND,
        ApiError::Invalid(_) | ApiError::Policy(_) => EXIT_INVALID,
        ApiError::Store(store) => store_exit_code(store),
    })
}

/// Exit code of a store error on its own: the request-shaped variants (the caller named an
/// unknown kind, a bad link, an invalid id or transition, ...) are 4, `NotFound` is 3, and the
/// backend, audit, file-mode, lease and migration failures are 5 (through the fallback
/// arm: the enum is `non_exhaustive`).
pub(crate) fn store_exit_code(e: &StoreError) -> u8 {
    match e {
        StoreError::UnknownKind(_)
        | StoreError::UnknownRel(_)
        | StoreError::InvalidName(..)
        | StoreError::EndpointNotAllowed { .. }
        | StoreError::BadLink
        | StoreError::SelfLink(_)
        | StoreError::KindMismatch { .. }
        | StoreError::WorkspaceMismatch { .. }
        | StoreError::Builtin(_)
        | StoreError::InvalidTransition(_)
        | StoreError::InvalidId(_)
        // engine-plan E1.8 (EN-F5, EN-F7): request-shaped like the kind errors above; the
        // E1.9a promotion refusals (RE3-3, RE3-4) are fixed classes of the same shape
        | StoreError::KindNotAllowed
        | StoreError::HumanOnly(_)
        | StoreError::PromoteConflict
        | StoreError::PromoteTargetIncompatible
        | StoreError::Ontology(_)
        // OWN-86 (3): a refused `supersedes` is request-shaped, a fixed class
        | StoreError::SupersedesInvalid(_)
        // OWN-P: a database of another schema version is refused with guidance, request-shaped
        | StoreError::SchemaUnsupported { .. } => EXIT_INVALID,
        StoreError::NotFound(_) => EXIT_NOT_FOUND,
        _ => EXIT_STORE_ERROR,
    }
}

/// Exit code of an `anyhow::Error` a subcommand returned: the typed service or store error in
/// its chain decides when there is one, otherwise the generic failure code.
pub(crate) fn exit_code_of(e: &anyhow::Error) -> ExitCode {
    if let Some(api) = e.downcast_ref::<ApiError>() {
        exit_code(api)
    } else if let Some(store) = e.downcast_ref::<StoreError>() {
        ExitCode::from(store_exit_code(store))
    } else {
        ExitCode::from(EXIT_FAILURE)
    }
}
