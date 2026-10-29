//! The CLI side of the field-class seam (core-spec S1, S4; OWN-10; M4-3): every subcommand that
//! maps to a tool runs through [`run_tool`], which does what the MCP adapter does around a
//! tool call and nothing else:
//!
//! 1. the raw request is one JSON tree scanned by `KabanApi::prepare_args` before any typed
//!    parse, so a serde error can never echo a secret (a rejection reclaims the secret leaves
//!    the caller placed with `RawSecret` back into zeroizing memory);
//! 2. a typed parse failure becomes `ApiError::Invalid` with the fixed text of shared
//!    [`crate::scrub::typed_argument_failure`] (the class and the type-derived parts, no
//!    caller byte, RW3-1);
//! 3. the result passes `KabanApi::gate_response` and must still deserialize as the declared
//!    response type (R3-9 reshape check), otherwise it is withheld;
//! 4. every error text passes `KabanApi::gate_response_text`;
//! 5. the one JSON document is printed by [`crate::cli::output`]; a serializer failure on the
//!    way (building the value or printing it) is one fixed line and exit 5, the error object
//!    never formatted (R4-3: its message could quote the value).
//!
//! Tool names are the MCP names, so the control registry applies unchanged; a CLI-only
//! operation (`doctor`, `hook`) has no registry entry and its walk treats every string as
//! content (fails closed).
//!
//! [`read_stdin_json`] is the bounded reader behind `--stdin-json` (1 MiB, M4-4).

use crate::cli::output::{
    self, EXIT_INVALID, EXIT_PENDING_SEARCH, EXIT_REJECTED, EXIT_STORE_ERROR, Format,
    SERIALIZATION_FAILED,
};
use crate::scrub::{OnRefusal, rejected_receipt_document, typed_argument_failure};
use kaban_api::{ApiError, KabanApi, RawSecret};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::io::Read;
use std::process::ExitCode;

/// Printed instead of a result that hits a reject rule of the policy (a stored value that
/// predates the rule, for example). Fixed on purpose, like the MCP adapter's text.
const RESPONSE_WITHHELD: &str =
    "error: response withheld: the result contains a value the denylist policy forbids";

/// Printed instead of a result a redaction left outside its declared shape (R3-9). Fixed.
const RESPONSE_RESHAPED: &str =
    "error: response withheld: a denylist redaction changed the declared response shape";

/// Printed instead of an error message that hits a reject rule. Fixed on purpose.
const ERROR_WITHHELD: &str = "error: error details withheld (denylist hit)";

/// Most bytes `--stdin-json` reads (M4-4).
pub(crate) const STDIN_JSON_LIMIT: usize = 1024 * 1024;

/// Run `tool` on `raw` through the seam and print its one JSON document. `secret_keys` names
/// the top-level leaves the caller moved in with `RawSecret::place`; on a rejection they are
/// reclaimed before the tree is dropped. A read: a refusal prints the error text alone.
pub(crate) fn run_tool<Req, Resp, F>(
    api: &KabanApi,
    tool: &str,
    raw: Value,
    secret_keys: &[&str],
    format: Format,
    call: F,
) -> ExitCode
where
    Req: DeserializeOwned,
    Resp: Serialize + DeserializeOwned,
    F: FnOnce(&KabanApi, Req) -> Result<Resp, ApiError>,
{
    run_tool_with(
        api,
        tool,
        raw,
        secret_keys,
        format,
        OnRefusal::ErrorOnly,
        call,
    )
}

/// [`run_tool`] for a public write (spec 05 SS-10): a refusal keeps its reason-specific status
/// and error line and prints the typed `rejected` receipt after it, on stderr as
/// `receipt: {...}` (stdout stays empty on error, the CLI's contract), the same receipt
/// object the MCP adapter attaches to its error reply.
pub(crate) fn run_write_tool<Req, Resp, F>(
    api: &KabanApi,
    tool: &str,
    raw: Value,
    secret_keys: &[&str],
    format: Format,
    call: F,
) -> ExitCode
where
    Req: DeserializeOwned,
    Resp: Serialize + DeserializeOwned,
    F: FnOnce(&KabanApi, Req) -> Result<Resp, ApiError>,
{
    run_tool_with(
        api,
        tool,
        raw,
        secret_keys,
        format,
        OnRefusal::RejectedReceipt,
        call,
    )
}

/// The seam behind [`run_tool`] and [`run_write_tool`]: `on_refusal` says what a refusal
/// prints beside its error (a tool that writes on some arguments only decides per call).
pub(crate) fn run_tool_with<Req, Resp, F>(
    api: &KabanApi,
    tool: &str,
    mut raw: Value,
    secret_keys: &[&str],
    format: Format,
    on_refusal: OnRefusal,
    call: F,
) -> ExitCode
where
    Req: DeserializeOwned,
    Resp: Serialize + DeserializeOwned,
    F: FnOnce(&KabanApi, Req) -> Result<Resp, ApiError>,
{
    let outcome = match api.prepare_args(tool, &raw) {
        // the typed parse consumes the tree (a secret leaf moves into its zeroizing typed
        // form); its failure is the fixed text (no caller byte) and still gated below
        Ok(()) => serde_json::from_value::<Req>(raw)
            .map_err(|e| ApiError::Invalid(typed_argument_failure(&e.to_string())))
            .and_then(|request| call(api, request)),
        Err(e) => {
            if let Value::Object(args) = &mut raw {
                for key in secret_keys {
                    drop(RawSecret::reclaim(args, key));
                }
            }
            Err(e)
        }
    };
    match outcome {
        Ok(resp) => {
            let Ok(value) = serde_json::to_value(&resp) else {
                return serialization_failure();
            };
            // the last gate before the result reaches the terminal (S1 "before returning",
            // S4): every content string masked, or the whole result withheld on a reject hit
            let Ok(clean) = api.gate_response(tool, &value) else {
                output::print_error(RESPONSE_WITHHELD);
                return ExitCode::from(EXIT_REJECTED);
            };
            if Resp::deserialize(&clean).is_err() {
                output::print_error(RESPONSE_RESHAPED);
                return ExitCode::from(EXIT_REJECTED);
            }
            if output::print_json(&clean, format).is_err() {
                return serialization_failure();
            }
            // the typed receipt is printed whole; status 6 says the write stands but is not
            // searchable yet (SS-10), so a script never replays it
            if output::pending_search(&clean) {
                return ExitCode::from(EXIT_PENDING_SEARCH);
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            // error text is free text: masked for redact rules, replaced on a reject hit
            let text = api.gate_response_text(&e.to_string()).map_or_else(
                |_| ERROR_WITHHELD.to_owned(),
                |safe| format!("error: {safe}"),
            );
            output::print_error(&text);
            // a refused write still answers with its typed outcome (SS-10 `rejected`)
            if on_refusal == OnRefusal::RejectedReceipt
                && let Some(document) = rejected_receipt_document(api, tool)
            {
                output::print_error(&format!("receipt: {document}"));
            }
            output::exit_code(&e)
        }
    }
}

/// The one answer to a serializer failure inside the seam (R4-3): the fixed
/// [`SERIALIZATION_FAILED`] line on stderr and exit 5. Takes no error on purpose: a
/// `serde_json::Error` message can quote the value it refused, so there is nothing to gate
/// and nothing to print.
fn serialization_failure() -> ExitCode {
    output::print_error(SERIALIZATION_FAILED);
    ExitCode::from(EXIT_STORE_ERROR)
}

/// Read the `--stdin-json` request body: at most [`STDIN_JSON_LIMIT`] bytes, one JSON object.
/// Every failure prints a fixed text (the body is caller data and is never echoed) and maps to
/// exit 4.
pub(crate) fn read_stdin_json() -> Result<Value, ExitCode> {
    let mut buf = Vec::with_capacity(8 * 1024);
    let limit = u64::try_from(STDIN_JSON_LIMIT).unwrap_or(u64::MAX);
    if let Err(e) = std::io::stdin()
        .lock()
        .take(limit.saturating_add(1))
        .read_to_end(&mut buf)
    {
        output::print_failure(&format_args!("reading --stdin-json: {}", e.kind()));
        return Err(ExitCode::from(EXIT_INVALID));
    }
    if buf.len() > STDIN_JSON_LIMIT {
        output::print_error("error: --stdin-json body exceeds 1 MiB");
        return Err(ExitCode::from(EXIT_INVALID));
    }
    // a parse error would quote the body, so the failure text is fixed
    if let Ok(value @ Value::Object(_)) = serde_json::from_slice::<Value>(&buf) {
        Ok(value)
    } else {
        output::print_error("error: --stdin-json body is not one JSON object");
        Err(ExitCode::from(EXIT_INVALID))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion
mod tests {
    use super::*;
    use kaban_store_sqlite::SqliteStore;
    use serde::{Deserialize, Deserializer, Serializer};

    /// Text only the failing serializer's message carries; it must reach neither stream.
    const CANARY: &str = "serializer-canary-7f3a9c";
    /// Set in the child process that actually runs the seam (the parent captures its streams).
    const CHILD_ENV: &str = "KABAN_SEAM_SERIALIZER_CHILD";

    /// A response type whose `Serialize` always fails with a message carrying the canary.
    struct Unserializable;
    impl Serialize for Unserializable {
        fn serialize<S: Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
            Err(serde::ser::Error::custom(format!(
                "the response value refused to serialize: {CANARY}"
            )))
        }
    }
    impl<'de> Deserialize<'de> for Unserializable {
        fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            serde::de::IgnoredAny::deserialize(d)?;
            Ok(Self)
        }
    }

    /// Review R4-3 (core-spec S1, S4): a serializer failure inside the seam is printed as one
    /// fixed content-free line and exits 5; the serializer's own message (which could carry
    /// caller-derived text) reaches neither stdout nor stderr. The seam prints to the real
    /// streams, so the test re-runs itself as a child process and reads what it printed.
    #[test]
    fn a_failing_serializer_prints_the_fixed_text_and_exits_5_without_its_message() {
        if std::env::var_os(CHILD_ENV).is_some() {
            let store = std::sync::Arc::new(SqliteStore::open_memory().unwrap());
            let api = KabanApi::new(
                store.clone(),
                kaban_policy::Policy::empty(),
                store.signer(),
                None,
            );
            let code = run_tool::<Value, Unserializable, _>(
                &api,
                "doctor",
                serde_json::json!({}),
                &[],
                Format::Compact,
                |_, _| Ok(Unserializable),
            );
            // `ExitCode` has no public integer view: the process exit encodes the comparison
            std::process::exit(if code == ExitCode::from(EXIT_STORE_ERROR) {
                5
            } else {
                99
            });
        }
        let name = format!(
            "{}::a_failing_serializer_prints_the_fixed_text_and_exits_5_without_its_message",
            module_path!().trim_start_matches("kaban::")
        );
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &name, "--nocapture"])
            .env(CHILD_ENV, "1")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            !stdout.contains(CANARY) && !stderr.contains(CANARY),
            "the serializer message leaked: stdout {stdout:?} stderr {stderr:?}"
        );
        assert_eq!(
            stderr.trim_end(),
            SERIALIZATION_FAILED,
            "exactly the fixed line on stderr"
        );
        assert!(
            !stdout.contains("error:") && !stdout.contains('{'),
            "no error text and no document on stdout: {stdout:?}"
        );
        assert_eq!(out.status.code(), Some(5), "exit 5: {stdout:?} {stderr:?}");
    }
}
