//! `kaban hook <event> [--host claude-code|codex|pi] --stdin-json` (core-spec F8, F8b, §7;
//! core-plan C4.3 to C4.5 as amended by `reviews/m4-prep-dispositions.md` M4-4, M4-6, M4-8,
//! M4-9, M4-32): the process side of a host hook. The handlers themselves live in
//! `kaban_api::hooks`; this module owns what makes a hook safe for the host:
//!
//! - **exit 0 always** (M4-8, PR-9): every refusal, store error, panic or deadline ends as one
//!   JSON document on stdout (`{}` when there is nothing to say) and one fixed stderr line;
//!   exit 2, which hosts read as "block", is never produced;
//! - **the 1.8 s watchdog** (M4-9): a thread that prints `{}` and exits 0 if the handler has
//!   not printed by then, so the host's `timeout: 2` is never reached and a missing GNU
//!   `timeout` in the wrapper does not matter;
//! - **the hook open** (M4-6): `SqliteStore::open_for_hook` refuses a missing or unmigrated
//!   database (a hook never creates or migrates one) and waits at most 1 s for a lock;
//! - **the bounded envelope read** (M4-4): the same 1 MiB reader as `--stdin-json`, whose
//!   failures here are `{}` and exit 0;
//! - **the workspace** (M4-32): bound exactly like the MCP server (`--workspace`,
//!   `KABAN_WORKSPACE`, else the enclosing repository's root name).
//!
//! The envelope always comes from stdin (`--stdin-json` is accepted for the recipes' sake and
//! is the only body form). One event has no envelope: `post-commit` (C5.3, M5-14) comes from
//! git, which sends no JSON and may leave stdin on a terminal, so the dispatcher never reads
//! stdin for it (`--stdin-json` is accepted and ignored, `--host` is irrelevant).

use crate::Cli;
use crate::cli::output::{self, Format};
use clap::ValueEnum;
use kaban_api::{HookEvent, HookHost, HookOutcome, KabanApi};
use serde_json::Value;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// How long a handler may run before the watchdog answers for it (M4-9; the host budget is
/// 2 s).
const WATCHDOG: Duration = Duration::from_millis(1800);

/// Set by whoever prints first (the handler or the watchdog); the other side prints nothing.
static PRINTED: AtomicBool = AtomicBool::new(false);

/// Debug-only test seam (M4-29): `KABAN_HOOK_STALL_MS` makes the handler sleep first, so the
/// watchdog's behaviour can be observed without a slow database. Compiled out of release
/// builds.
#[cfg(debug_assertions)]
fn test_stall() {
    if let Some(ms) = std::env::var("KABAN_HOOK_STALL_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
    {
        std::thread::sleep(Duration::from_millis(ms));
    }
}
#[cfg(not(debug_assertions))]
fn test_stall() {}

/// `kaban hook <event>` values (the kebab-case wire names of [`HookEvent`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum EventArg {
    SessionStart,
    PreToolUse,
    PostToolUse,
    PostToolUseFailure,
    SubagentStop,
    Stop,
    Fix,
    /// The git post-commit hook: no host, no envelope (M5-14).
    PostCommit,
}

impl From<EventArg> for HookEvent {
    fn from(e: EventArg) -> Self {
        match e {
            EventArg::SessionStart => Self::SessionStart,
            EventArg::PreToolUse => Self::PreToolUse,
            EventArg::PostToolUse => Self::PostToolUse,
            EventArg::PostToolUseFailure => Self::PostToolUseFailure,
            EventArg::SubagentStop => Self::SubagentStop,
            EventArg::Stop => Self::Stop,
            EventArg::Fix => Self::Fix,
            EventArg::PostCommit => Self::PostCommit,
        }
    }
}

/// `--host` values.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum HostArg {
    #[default]
    ClaudeCode,
    Codex,
    Pi,
}

impl From<HostArg> for HookHost {
    fn from(h: HostArg) -> Self {
        match h {
            HostArg::ClaudeCode => Self::ClaudeCode,
            HostArg::Codex => Self::Codex,
            HostArg::Pi => Self::Pi,
        }
    }
}

/// Print `doc` as the hook's one document unless the watchdog already answered.
fn print_once(doc: &Value, format: Format) {
    if PRINTED.swap(true, Ordering::SeqCst) {
        return;
    }
    if output::print_json(doc, format).is_err() {
        // serialization of a plain `Value` does not fail; the fallback keeps the contract
        output::print_json(&Value::Object(serde_json::Map::new()), format).ok();
    }
}

/// Print `{}` unless something was printed already.
fn print_empty(format: Format) {
    print_once(&Value::Object(serde_json::Map::new()), format);
}

/// An empty outcome with the fixed stderr line of a hook refusal (never caller data).
fn refused(event: EventArg, what: &str) -> HookOutcome {
    let name = event
        .to_possible_value()
        .map(|v| v.get_name().to_owned())
        .unwrap_or_default();
    HookOutcome {
        output: Value::Object(serde_json::Map::new()),
        note: Some(format!("kaban hook {name}: {what}")),
        receipt: None,
    }
}

/// Run the hook to completion: always exit 0 (see the module docs).
pub(crate) fn run(cli: Cli, event: EventArg, host: HostArg) -> ExitCode {
    let format = if cli.json {
        Format::Compact
    } else {
        Format::Pretty
    };
    // a panic anywhere below prints its fixed line, never the panic message (which could
    // quote a value), and the process still answers `{}` and exits 0
    std::panic::set_hook(Box::new(|_| {
        output::print_error("kaban hook: internal error");
    }));
    std::thread::spawn(move || {
        std::thread::sleep(WATCHDOG);
        print_empty(format);
        std::process::exit(0);
    });
    let outcome =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handle(cli, event, host)));
    match outcome {
        // the receipt stays typed (SS-10): the stdout document is the host's protocol
        Ok(HookOutcome {
            output: doc,
            note: line,
            receipt: _,
        }) => {
            if let Some(line) = line {
                output::print_error(&line);
            }
            print_once(&doc, format);
        }
        Err(_) => print_empty(format),
    }
    ExitCode::SUCCESS
}

/// Read the envelope, open the store for the hook, bind the workspace and run the handler.
/// Every failure is an empty outcome with a fixed note.
fn handle(cli: Cli, event: EventArg, host: HostArg) -> HookOutcome {
    let empty = |what: &str| refused(event, what);
    // the envelope first (an oversized or malformed body fails before the database is touched);
    // `post-commit` has none and stdin may never close (M5-14): it is not read at all
    let envelope = if event == EventArg::PostCommit {
        Value::Object(serde_json::Map::new())
    } else {
        let Ok(envelope) = read_envelope() else {
            return empty("the envelope on stdin is not one JSON object of at most 1 MiB");
        };
        envelope
    };
    test_stall();
    let policy = match cli.policy.as_deref() {
        Some(p) => match kaban_policy::Policy::load(p) {
            Ok(policy) => policy,
            Err(_) => return empty("the policy file does not load"),
        },
        None => kaban_policy::Policy::empty(),
    };
    let Ok(keys_dir) = crate::kaban_home().map(|home| home.join("keys")) else {
        return empty("KABAN_HOME/HOME is not set");
    };
    // M4-6: a hook fired in a repository without a kaban database must not create one
    if !cli.db.exists() {
        return empty(
            "the database does not exist; hooks never create one (run kaban serve or kaban doctor there first)",
        );
    }
    let store =
        match kaban_store_sqlite::SqliteStore::open_for_hook(&cli.db, &keys_dir, &cli.actor_id) {
            Ok(store) => store,
            Err(e) => {
                // the store error's own message names a class, never content (RM-1)
                return empty(&format!("database not usable by a hook: {e}"));
            }
        };
    let api: KabanApi = match crate::assemble_api(cli, policy, store) {
        Ok(api) => api,
        Err(_) => return empty("the service could not be assembled"),
    };
    api.hook(host.into(), event.into(), &envelope)
}

/// The envelope: one JSON object of at most 1 MiB on stdin (the `--stdin-json` reader's
/// rules). Its diagnostics are not printed here: the hook has its own fixed line.
fn read_envelope() -> Result<Value, ()> {
    use std::io::Read;
    let limit = crate::cli::seam::STDIN_JSON_LIMIT;
    let mut buf = Vec::with_capacity(8 * 1024);
    let cap = u64::try_from(limit).unwrap_or(u64::MAX).saturating_add(1);
    std::io::stdin()
        .lock()
        .take(cap)
        .read_to_end(&mut buf)
        .map_err(|_| ())?;
    if buf.len() > limit {
        return Err(());
    }
    match serde_json::from_slice::<Value>(&buf) {
        Ok(value @ Value::Object(_)) => Ok(value),
        _ => Err(()),
    }
}
