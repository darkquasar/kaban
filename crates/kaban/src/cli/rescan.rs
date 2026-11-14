//! `kaban rescan`: run the rescan job in the foreground (core-spec S4; core-plan C2.7) and print
//! its report as JSON. Under a policy whose `effective_hash()` differs from
//! `index_meta.policy_hash` every node and edge is re-judged in batches of 500 (one transaction
//! each); otherwise only the rows the change log names after the `policy_scan_seq` watermark
//! are. Reject hits tombstone a node or delete an edge, redact hits are masked in place, and
//! `policy_hash` records the policy once the walk completes. Nothing to do when every row is
//! already judged (`status: up_to_date`).
//!
//! Exit codes (core-spec §6): 0 ok, 4 policy/config error, 5 store error.

use crate::cli::output::{EXIT_STORE_ERROR, Format, print_error, print_failure, print_json};
use kaban_api::{ApiError, KabanApi};
use std::process::ExitCode;
use tokio_util::sync::CancellationToken;

/// Run the job to completion (no cancellation: the foreground command owns the terminal) and
/// print the outcome.
///
/// # Errors
/// Only I/O or serialization failures of the report; store errors are reported on stderr with
/// exit code 5.
pub(crate) fn run(api: &KabanApi, format: Format) -> anyhow::Result<ExitCode> {
    match api.rescan_if_policy_changed(&CancellationToken::new()) {
        Ok(outcome) => {
            if let Err(e) = print_json(&outcome, format) {
                print_failure(&e);
                return Ok(ExitCode::from(crate::cli::output::EXIT_FAILURE));
            }
            Ok(ExitCode::SUCCESS)
        }
        Err(e @ (ApiError::Store(_) | ApiError::NotFound(_))) => {
            print_error(&format!("error: rescan failed: {e}"));
            Ok(ExitCode::from(EXIT_STORE_ERROR))
        }
        Err(e) => Err(e.into()),
    }
}
