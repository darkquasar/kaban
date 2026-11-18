//! `kaban bench query` (spec 11): registered by T6; its owning task fills it.

use std::process::ExitCode;

/// Not delivered yet: refused with exit 4.
pub(crate) fn run() -> ExitCode {
    super::not_delivered("query")
}
