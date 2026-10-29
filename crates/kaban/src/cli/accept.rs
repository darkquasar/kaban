//! `kaban accept <id>`: a person accepts a governed record on the CLI (core-spec F4 (i), actor
//! `human-cli`), or redeems the confirmation token an agent received over MCP
//! (`--confirmation-token <t> --confirmed-by-user`, F4 (iii), `signer_kind: human-via-chat`).
//! The token is read from argv (visible to `ps`, as the spec prescribes) into zeroizing memory
//! and never printed or logged; the result is the same JSON the MCP tool returns. The stdin
//! route `--token-stdin --confirmed-by-user` (owner decision OWN-20) reads the token as one
//! line from stdin instead ([`token_from_stdin`]: at most 1 KiB, trimmed, never echoed), so it
//! never appears in the process arguments; it fills the same raw leaf and is judged the same
//! way. With `--stdin-json` the whole request is the JSON object on stdin instead (the
//! `AcceptRequest` wire shape), and no argument flag may be given.
//!
//! The CLI is an adapter like the MCP server and goes through the same seam (core-spec S1, S4;
//! OWN-10; review R3-1): [`crate::cli::seam::run_tool`] scans the raw request, token included,
//! before the typed parse, gates the success value and every error text, and prints the result.

use crate::cli::output::{EXIT_INVALID, Format, print_error, print_failure};
use crate::cli::seam::run_tool;
use kaban_api::{AcceptRequest, Caller, KabanApi, RawSecret};
use kaban_core::Actor;
use serde_json::{Map, Value};
use std::io::{BufRead, Read};
use std::process::ExitCode;

/// Key of the token in the raw request (the `AcceptRequest` wire field).
const TOKEN_KEY: &str = "confirmation_token";

/// Most bytes `--token-stdin` reads (one line; a token is 64 hex digits).
const TOKEN_STDIN_LIMIT: usize = 1024;

/// Read the `--token-stdin` token: the first line of stdin, at most [`TOKEN_STDIN_LIMIT`] bytes,
/// surrounding whitespace trimmed, into a [`RawSecret`] (zeroizing; the line buffer it is
/// trimmed from is a plain `String`, dropped here). Every failure prints a fixed text (the line
/// is caller data and is never echoed) and maps to exit 4.
pub(crate) fn token_from_stdin() -> Result<RawSecret, ExitCode> {
    let mut line = String::new();
    let limit = u64::try_from(TOKEN_STDIN_LIMIT).unwrap_or(u64::MAX);
    let read = std::io::stdin()
        .lock()
        .take(limit.saturating_add(1))
        .read_line(&mut line);
    if let Err(e) = read {
        print_failure(&format_args!("reading --token-stdin: {}", e.kind()));
        return Err(ExitCode::from(EXIT_INVALID));
    }
    if line.len() > TOKEN_STDIN_LIMIT {
        print_error("error: --token-stdin line exceeds 1 KiB");
        return Err(ExitCode::from(EXIT_INVALID));
    }
    let token = line.trim();
    if token.is_empty() {
        print_error("error: --token-stdin: no token on stdin");
        return Err(ExitCode::from(EXIT_INVALID));
    }
    Ok(RawSecret::new(token))
}

/// The command's arguments as parsed by clap (the token is raw: not judged yet).
pub(crate) struct Args {
    /// Node id.
    pub(crate) id: String,
    /// Acceptance reason.
    pub(crate) reason: Option<String>,
    /// Acceptance scope.
    pub(crate) path_scope: Vec<String>,
    /// Declared signer.
    pub(crate) signer: Option<String>,
    /// Token to redeem, as pasted.
    pub(crate) confirmation_token: Option<RawSecret>,
    /// The user confirmed in chat.
    pub(crate) confirmed_by_user: bool,
}

impl Args {
    /// The raw request as the MCP adapter would receive it: the token leaf is moved in, not
    /// copied.
    fn into_raw(self) -> Value {
        let mut args = Map::new();
        args.insert("id".into(), Value::String(self.id));
        if let Some(reason) = self.reason {
            args.insert("reason".into(), Value::String(reason));
        }
        args.insert(
            "path_scope".into(),
            Value::Array(self.path_scope.into_iter().map(Value::String).collect()),
        );
        if let Some(signer) = self.signer {
            args.insert("signer".into(), Value::String(signer));
        }
        if let Some(token) = self.confirmation_token {
            token.place(&mut args, TOKEN_KEY);
        }
        args.insert(
            "confirmed_by_user".into(),
            Value::Bool(self.confirmed_by_user),
        );
        Value::Object(args)
    }
}

/// Run the acceptance from argument flags and print the result as JSON.
pub(crate) fn run(api: &KabanApi, args: Args, format: Format) -> ExitCode {
    run_raw(api, args.into_raw(), format)
}

/// Run the acceptance from a raw request body (`--stdin-json`) and print the result as JSON.
pub(crate) fn run_raw(api: &KabanApi, raw: Value, format: Format) -> ExitCode {
    let caller = Caller {
        kind: Actor::HumanCli,
        host: None,
        session: None,
    };
    run_tool(
        api,
        "accept",
        raw,
        &[TOKEN_KEY],
        format,
        |api, request: AcceptRequest| api.accept(request, &caller),
    )
}
