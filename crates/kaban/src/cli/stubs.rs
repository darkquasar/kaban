//! Flags and modes whose milestone has not landed (M4-1, M4-4): `serve --http`, `--global-db`
//! and `bench cg` without the `bench` feature are accepted by the parser and exit 4 with "not
//! available in this build", so a hook or script that calls them fails loudly instead of
//! succeeding silently (core-plan C4.1).

use crate::cli::output::{EXIT_INVALID, print_error};
use std::process::ExitCode;

/// Report `what` (kaban's own subcommand or flag name, never caller data) as not shipped, with
/// the milestone that ships it, and exit 4.
pub(crate) fn not_available(what: &str, milestone: &str) -> ExitCode {
    print_error(&format!(
        "error: {what} is not available in this build ({milestone})"
    ));
    ExitCode::from(EXIT_INVALID)
}
