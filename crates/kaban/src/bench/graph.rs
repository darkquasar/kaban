//! `kaban bench graph` (spec 11): registered by T6; its owning task fills it.

use std::process::ExitCode;

/// The graph bench's arguments (empty until its owning task fills them).
#[derive(clap::Args)]
pub(crate) struct Args {}

/// Not delivered yet: refused with exit 4.
pub(crate) fn run(_args: &Args) -> ExitCode {
    super::not_delivered("graph")
}
