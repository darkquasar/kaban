//! `kaban scan`: validate a policy file and scan stdin, printing the verdict (no persistence).
//! A policy that fails to load (strict schema, unanchored allow regex, bad rule regex) exits
//! with code 4 and a message that names the field or rule id, never a term value (core-spec S7).

use crate::cli::output::{EXIT_INVALID, Format, print_error, print_json};
use kaban_policy::Policy;
use std::path::Path;
use std::process::ExitCode;

/// Load the policy (builtin only when `path` is `None`), read stdin and print the verdict.
///
/// # Errors
/// Only I/O or serialization failures; policy errors are reported on stderr with exit code 4.
pub(crate) fn run(path: Option<&Path>, format: Format) -> anyhow::Result<ExitCode> {
    let policy = match path {
        Some(p) => match Policy::load(p) {
            Ok(policy) => policy,
            Err(e) => {
                print_error(&format!(
                    "error: policy {} failed to load: {e}",
                    p.display()
                ));
                return Ok(ExitCode::from(EXIT_INVALID));
            }
        },
        None => Policy::empty(),
    };
    let mut s = String::new();
    std::io::Read::read_to_string(&mut std::io::stdin(), &mut s)?;
    print_json(&policy.scan(&s), format)?;
    Ok(ExitCode::SUCCESS)
}
