//! Subcommands and flags whose milestone has not landed (M4-1, M4-4): `promote`, `serve --http`
//! and `--global-db` are accepted by the parser and exit 4 with "not available in this build",
//! so a hook or script that calls them fails loudly instead of succeeding silently (core-plan
//! C4.1). The M5 subcommands `export`, `sync`, `rebuild` and `resolve` have their typed shapes
//! (M5-16) and call [`not_available`] from their own placeholder modules until the C5.1b and
//! C5.2 lanes fill them; `ingest-decisions` stays a stub until the assisted ingestion spec
//! lands (OWN-26).

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
