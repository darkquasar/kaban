//! Host hook handlers (core-spec F8, F8b, S1, S4, S6, §11; core-plan C4.3 to C4.5 as amended
//! by `reviews/m4-prep-dispositions.md` M4-6 to M4-18, M4-21, M4-29 to M4-32): what
//! `kaban hook <event> --host <host>` does once the CLI has read the host's stdin envelope.
//!
//! ```text
//!   stdin envelope (Value)        allowlist (M4-7)          seam             store
//!   +------------------+   =>   +--------------+   =>   +-------------+   =>   +--------+
//!   | host JSON object | -----> | Selected     | -----> | prepare_args| -----> | why /  |
//!   | (never stored,   |        | session_id,  |        | typed parse |        | remem- |
//!   |  never scanned   |        | paths, cmd,  |        | call        |        | ber /  |
//!   |  as a whole)     |        | error, ...   |        | gate_resp.  |        | session|
//!   +------------------+        +--------------+        +-------------+        +--------+
//!                                                              |
//!                                                              v
//!                                 one JSON document on stdout, `{}` when nothing applies
//! ```
//!
//! Rules every handler follows:
//! - Only the allowlisted envelope fields are read ([`Selected`]); file contents, transcripts
//!   and `transcript_path` are never read, stored or scanned (M4-7).
//! - The target tool's raw request is built from the selected strings and goes through
//!   `prepare_args` before any typed parse; the typed operation re-gates every field inside
//!   its own transaction (S1); results pass `gate_response` (S4) and the final document passes
//!   the unregistered `hook` scope, where every string is content (M4-3).
//! - A hook never fails: every refusal, error or unexpected envelope ends as `{}` with one
//!   fixed stderr line ([`HookOutcome::note`]); the CLI always exits 0 (M4-8, PR-9).
//! - The workspace is the process's bound workspace (M4-32); the envelope `cwd` only resolves
//!   relative paths.
//! - One event has no host and no envelope: [`HookEvent::PostCommit`] (C5.3, M5-14) is the git
//!   `post-commit` hook; the CLI never reads stdin for it and the handler reads only the
//!   repository root, `HEAD` and the bound workspace (M5-15, OWN-23).

use crate::api::KabanApi;
use crate::error::ApiError;
use crate::explain::{Addressable, PendingStatus};
use crate::review::ReviewQuery;
use crate::types::{
    RememberRequest, RememberResponse, WhyRequest, WhyResponse, WriteOutcome, WriteReceipt,
};
use kaban_core::{
    EvidenceRun, FailurePatch, PendingFailuresQuery, PendingScope, SessionEdit, SessionEditKind,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::fmt::Write as _;
use std::path::{Component, Path, PathBuf};

/// Which host sent the envelope (core-plan C4.3 `--host`; `serde-enum-representation`: the
/// kebab-case wire names are the CLI values).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HookHost {
    /// Claude Code (`.claude/settings.json` hooks).
    ClaudeCode,
    /// Codex CLI (`hooks.json` or `[hooks]` in `config.toml`).
    Codex,
    /// pi (the `hooks/pi/kaban-hooks.ts` extension builds the envelope).
    Pi,
}

/// The kaban hook events (`kaban hook <event>`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HookEvent {
    /// Session briefing (<= 5 items) as `additionalContext`.
    SessionStart,
    /// Governing records for the file about to be edited as `additionalContext`.
    PreToolUse,
    /// A command finished: success evidence for a pending Failure (C4.4); on Codex and pi also
    /// the failure signal (non-zero exit, `isError`).
    PostToolUse,
    /// A command failed: record a Failure with `pending_fix: true`.
    PostToolUseFailure,
    /// A subagent finished: record an Episode when a summary is present.
    SubagentStop,
    /// The agent is about to stop: the one-time "how was it fixed" prompt (C4.4).
    Stop,
    /// The agent's reply: `{"failure_id", "fix"}` (C4.4).
    Fix,
    /// The git `post-commit` hook (C5.3, M5-14, M5-15/OWN-23): stamp `HEAD` on the unstamped
    /// nodes of the bound workspace. No host, no envelope.
    PostCommit,
}

impl HookEvent {
    /// Whether the event's handler writes (a Failure, an episode, a fix, commit stamps): the
    /// events whose refusal is an SS-10 `rejected` outcome.
    fn writes(self) -> bool {
        matches!(
            self,
            Self::PostToolUseFailure
                | Self::PostToolUse
                | Self::SubagentStop
                | Self::Stop
                | Self::Fix
                | Self::PostCommit
        )
    }

    /// The host's own name for this event, which the output echoes as `hookEventName` and the
    /// envelope must carry as `hook_event_name` (M4-8). `None`: the host has no such event.
    fn host_name(self, host: HookHost) -> Option<&'static str> {
        match (host, self) {
            (HookHost::Pi, Self::SessionStart) => Some("session_start"),
            (HookHost::Pi, Self::PreToolUse) => Some("tool_call"),
            (HookHost::Pi, Self::PostToolUse | Self::PostToolUseFailure) => {
                Some("tool_execution_end")
            }
            (HookHost::Pi, Self::Stop) => Some("agent_before_settle"),
            // pi has no subagent event of its own; `fix` is the agent's reply, not a host event;
            // `post-commit` comes from git, which has no envelope at all
            (HookHost::Pi, Self::SubagentStop) | (_, Self::Fix | Self::PostCommit) => None,
            (_, Self::SessionStart) => Some("SessionStart"),
            (_, Self::PreToolUse) => Some("PreToolUse"),
            // Codex has no PostToolUseFailure: its PostToolUse carries the failure signal
            (_, Self::PostToolUse) | (HookHost::Codex, Self::PostToolUseFailure) => {
                Some("PostToolUse")
            }
            (HookHost::ClaudeCode, Self::PostToolUseFailure) => Some("PostToolUseFailure"),
            (_, Self::SubagentStop) => Some("SubagentStop"),
            (_, Self::Stop) => Some("Stop"),
        }
    }
}

/// What a hook prints and says: `output` is the one JSON document for stdout (`{}` when the
/// hook has nothing to inject or record), `note` one fixed-or-gated stderr line for the
/// operator's debug log. Never an error: the CLI exits 0 whatever happened (M4-8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookOutcome {
    /// The stdout document.
    pub output: Value,
    /// One stderr line, when something is worth saying (every refusal says why in fixed words).
    pub note: Option<String>,
    /// The receipt of the event's write (spec 05 SS-10): `committed` when it stored,
    /// `rejected` when a write-bearing event was refused, `None` when the event writes nothing.
    /// Typed only: the stdout document is the host's protocol and does not carry it.
    pub receipt: Option<WriteReceipt>,
}

impl HookOutcome {
    fn empty() -> Self {
        Self {
            output: Value::Object(Map::new()),
            note: None,
            receipt: None,
        }
    }
    fn silent(note: impl Into<String>) -> Self {
        Self {
            output: Value::Object(Map::new()),
            note: Some(note.into()),
            receipt: None,
        }
    }
    /// `{}` on stdout and the receipt of the write the event made.
    fn committed(receipt: WriteReceipt) -> Self {
        Self {
            output: Value::Object(Map::new()),
            note: None,
            receipt: Some(receipt),
        }
    }
    /// `{}` on stdout, the receipt of the write the event committed and the note of the step
    /// that failed after it (SS-10: a post-commit failure is said, never a rejection).
    fn committed_with_note(receipt: WriteReceipt, note: String) -> Self {
        Self {
            output: Value::Object(Map::new()),
            note: Some(note),
            receipt: Some(receipt),
        }
    }
}

/// Most `why` targets one pre-tool-use may name (the `WhyRequest` limit, M4-18).
const MAX_TARGETS: usize = 16;
/// Governing records injected per edited path.
const WHY_LIMIT: usize = 3;
/// The coding-agent adapter kinds the hooks write, declared in the shipped `adr.yaml`
/// (engine-spec EN-F4 "retained as the adapter kinds of `adr.yaml`"): the stored names of a
/// failed command, a subagent episode and a fix. One place instead of a literal per site; a
/// registry test pins that the shipped ontology declares them under `adr`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdapterKinds {
    /// A failed command (`pending_fix`, paired with its fix through `FIXED_BY`).
    pub failure: &'static str,
    /// A subagent summary.
    pub episode: &'static str,
    /// The fix of a failure.
    pub improvement: &'static str,
}

/// The `adr.yaml` adapter kinds under their stored (bare) names.
pub const ADAPTER_KINDS: AdapterKinds = AdapterKinds {
    failure: "Failure",
    episode: "Episode",
    improvement: "Improvement",
};

/// Items in a session briefing (core-spec F8).
pub const BRIEFING_ITEMS: usize = 5;
/// Pending Failures a briefing shows at most (the rest are accepted records).
const BRIEFING_FAILURES: usize = 2;
/// Most characters of injected context (Codex `additionalContextLimit` 1500 in the recipe).
const MAX_CONTEXT_CHARS: usize = 1500;
/// Line-boundary truncation caps of a captured failure (M4-10, after the full-string scan).
const STACK_HEAD_LINES: usize = 20;
const STACK_HEAD_BYTES: usize = 2 * 1024;
const ERROR_TAIL_LINES: usize = 40;
const ERROR_TAIL_BYTES: usize = 4 * 1024;
const COMMAND_BYTES: usize = 2 * 1024;
const TEXT_COMMAND_CHARS: usize = 200;
const TEXT_ERROR_CHARS: usize = 300;
/// `session_edits` rows older than this are pruned at session-start (M4-13).
const SESSION_EDITS_RETENTION_S: i64 = 7 * 24 * 3600;
/// Rows one prune deletes at most (bounded for the hook budget).
const PRUNE_BATCH: usize = 500;
/// Most `session_edits` rows one lookup reads.
const SESSION_ROWS: usize = 1000;
/// Most pending Failures one success or Stop considers.
const PENDING_LIMIT: usize = 32;
/// Domain separation of the command fingerprint (M4-11; the same pattern as M3-17).
const FINGERPRINT_DOMAIN: &str = "kaban-cmd/v1\0";
/// kaban's own MCP tools as a host sees them: never captured as failures (core-plan C4.3).
const OWN_TOOL_PREFIX: &str = "mcp__kaban__";
/// Fixed stderr lines (never caller data).
const NOTE_REFUSED: &str = "refused by the denylist policy; nothing stored";
const NOTE_INVALID: &str = "the selected envelope fields do not form a valid request";
const NOTE_STORE: &str = "store error; nothing stored";
const NOTE_EVENT: &str = "hook_event_name is absent or not the event this handler serves";
const NOTE_NOT_FOUND: &str = "failure_id names no Failure of this workspace";
const NOTE_FIX_FIELDS: &str = "fix needs {\"failure_id\", \"fix\"}";
const NOTE_WITHHELD: &str = "output withheld by the denylist policy";
/// SS-10: the write committed and is not searchable yet; the host must not replay it.
const NOTE_PENDING_SEARCH: &str = "stored; not yet searchable, do not replay";
const NOTE_NO_REPO: &str = "not inside a git repository; nothing stamped";
const NOTE_NO_HEAD: &str = "git gave no HEAD; nothing stamped";
/// The session pairing failed after the Failure committed (SS-10: the record stands, the
/// receipt says `committed`; the pairing is best effort, M4-14).
const NOTE_PAIRING_FAILED: &str = "failure stored; its session pairing failed";
/// The Failure's patch failed after the Improvement and its `FIXED_BY` edge committed (SS-10:
/// the receipt says `committed`; the edge alone resolves the Failure, M4-14).
const NOTE_PATCH_FAILED: &str = "fix stored and linked; the failure's patch failed";
/// An evidence write failed after an earlier one of the same event committed (SS-10: the
/// receipt covers what committed).
const NOTE_EVIDENCE_PARTIAL: &str = "evidence recorded for some pending failures, not all";

/// The allowlisted envelope fields (M4-7): everything a handler may look at. Built by
/// [`select`] from direct lookups; the rest of the envelope is never visited. Unknown fields
/// are ignored by construction (plan slice 5).
#[derive(Debug, Default)]
struct Selected {
    session_id: Option<String>,
    host_event: Option<String>,
    cwd: Option<String>,
    tool_name: Option<String>,
    /// Paths about to be edited (pre-tool-use).
    paths: Vec<String>,
    /// The shell command (post-tool-use, post-tool-use-failure).
    command: Option<String>,
    /// The host's error output for a failed command.
    error_text: Option<String>,
    /// Whether the host reports the command as failed (Codex `Exit code:` marker, pi
    /// `isError`); `None` when the host does not say (Claude Code: the event itself says).
    failed: Option<bool>,
    /// Subagent summary (M4-31, settled live: `last_assistant_message`, then `summary`).
    summary: Option<String>,
    /// Claude Code `Stop`: set when a Stop hook already blocked once (its own loop guard).
    stop_hook_active: Option<bool>,
    /// `hook fix`: the agent's reply (M4-15).
    failure_id: Option<String>,
    fix: Option<String>,
}

fn string_at<'a>(v: &'a Value, pointer: &str) -> Option<&'a str> {
    v.pointer(pointer).and_then(Value::as_str)
}

fn owned_at(v: &Value, pointer: &str) -> Option<String> {
    string_at(v, pointer).map(str::to_owned)
}

/// Codex `apply_patch` paths: the `*** Update File:` / `*** Add File:` headers of the patch
/// text (core-plan C4.5; `HOST-EVENTS.md` §2: the patch is `tool_input.command`; `input` and
/// `patch` are kept as fallbacks).
fn patch_paths(tool_input: &Value) -> Vec<String> {
    let patch = ["/command", "/input", "/patch"]
        .iter()
        .find_map(|p| string_at(tool_input, p))
        .or_else(|| tool_input.as_str());
    patch
        .map(|text| {
            text.lines()
                .filter_map(|line| {
                    line.strip_prefix("*** Update File: ")
                        .or_else(|| line.strip_prefix("*** Add File: "))
                })
                .map(|p| p.trim().to_owned())
                .collect()
        })
        .unwrap_or_default()
}

/// The exit code a host's tool response reports, if any. Codex (`HOST-EVENTS.md` §2) sends
/// `tool_response` as a plain string that carries an `Exit code: <n>` marker for `apply_patch`
/// and no exit field at all; the object spellings `exit_code`/`exitCode` the plan assumed are
/// kept as fallbacks. A string without the marker reports nothing: the Codex `Bash` path is
/// then evidence-only (spec §11 fallback), never a guessed failure.
fn exit_code(response: &Value) -> Option<i64> {
    if let Some(text) = response.as_str() {
        return text.lines().find_map(|line| {
            let rest = line.trim().strip_prefix("Exit code:")?;
            rest.trim().parse::<i64>().ok()
        });
    }
    ["/exit_code", "/exitCode", "/metadata/exit_code"]
        .iter()
        .find_map(|p| response.pointer(p).and_then(Value::as_i64))
}

/// Read the allowlisted fields of `envelope` for `host`.
fn select(host: HookHost, envelope: &Value) -> Selected {
    let mut s = Selected {
        session_id: owned_at(envelope, "/session_id"),
        host_event: owned_at(envelope, "/hook_event_name"),
        cwd: owned_at(envelope, "/cwd"),
        stop_hook_active: envelope.get("stop_hook_active").and_then(Value::as_bool),
        failure_id: owned_at(envelope, "/failure_id"),
        fix: owned_at(envelope, "/fix"),
        ..Selected::default()
    };
    match host {
        HookHost::ClaudeCode | HookHost::Codex => {
            s.tool_name = owned_at(envelope, "/tool_name");
            let input = envelope.get("tool_input").unwrap_or(&Value::Null);
            if let Some(p) = string_at(input, "/file_path") {
                s.paths.push(p.to_owned());
            }
            if host == HookHost::Codex {
                s.paths.extend(patch_paths(input));
            }
            s.command = owned_at(input, "/command");
            let response = envelope.get("tool_response").unwrap_or(&Value::Null);
            // Claude Code PostToolUseFailure: an `error` field (string or `{message}`); Codex
            // PostToolUse: the response's stderr/output
            s.error_text = owned_at(envelope, "/error")
                .or_else(|| owned_at(envelope, "/error/message"))
                .or_else(|| owned_at(response, "/error"))
                .or_else(|| owned_at(response, "/stderr").filter(|t| !t.is_empty()))
                .or_else(|| owned_at(response, "/output"))
                .or_else(|| response.as_str().map(str::to_owned));
            // no exit code (Claude Code never sends one; a Codex string without the marker):
            // the event itself decides, never a guess
            s.failed = exit_code(response).map(|code| code != 0);
            // M4-31 settled live: the summary field is `last_assistant_message`
            s.summary = owned_at(envelope, "/last_assistant_message")
                .or_else(|| owned_at(envelope, "/summary"));
        }
        HookHost::Pi => {
            // the kaban pi extension forwards the event fields it observed (E4 §3.3)
            s.tool_name =
                owned_at(envelope, "/toolName").or_else(|| owned_at(envelope, "/tool_name"));
            // `tool_call` carries `input`, `tool_execution_start` `args` (HOST-EVENTS §3); the
            // extension attaches the matching `tool_call` input to `tool_execution_end`
            let input = envelope
                .get("input")
                .or_else(|| envelope.get("args"))
                .unwrap_or(&Value::Null);
            if let Some(p) = string_at(input, "/path") {
                s.paths.push(p.to_owned());
            }
            s.command = owned_at(input, "/command");
            // pi's `AgentToolResult { content: [{type: "text", text}], details }` (E4 §3.3),
            // or a plain string / `{error|text|output}` when the extension flattens it
            let result = envelope.get("result").unwrap_or(&Value::Null);
            s.error_text = result
                .as_str()
                .map(str::to_owned)
                .or_else(|| owned_at(result, "/content/0/text"))
                .or_else(|| owned_at(result, "/error"))
                .or_else(|| owned_at(result, "/text"))
                .or_else(|| owned_at(result, "/output"));
            s.failed = envelope.get("isError").and_then(Value::as_bool);
            s.summary = owned_at(envelope, "/summary");
        }
    }
    s
}

/// Whether `tool_name` is the host's file-edit tool.
fn is_edit_tool(host: HookHost, tool_name: &str) -> bool {
    match host {
        HookHost::ClaudeCode => {
            matches!(tool_name, "Edit" | "Write" | "MultiEdit" | "NotebookEdit")
        }
        HookHost::Codex => matches!(tool_name, "apply_patch" | "Edit" | "Write"),
        HookHost::Pi => matches!(tool_name, "edit" | "write"),
    }
}

/// Whether `tool_name` is the host's shell tool (the only one captured as a Failure).
fn is_shell_tool(host: HookHost, tool_name: &str) -> bool {
    match host {
        HookHost::ClaudeCode | HookHost::Codex => tool_name == "Bash",
        HookHost::Pi => tool_name == "bash",
    }
}

/// Lexically fold `.` and `..` (symlinks not followed), `\` normalized to `/` first (M4-18).
fn lexical(p: &str) -> PathBuf {
    let mut out = PathBuf::new();
    for c in Path::new(&p.replace('\\', "/")).components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// The repository-relative, `/`-separated form of an edited path (M4-18): relative paths
/// resolve against the envelope `cwd` (else the repository root); a path outside the
/// repository, or any path when no repository is known, is skipped.
fn repo_relative(path: &str, cwd: Option<&str>, repo_root: &Path) -> Option<String> {
    let candidate = lexical(path);
    let absolute = if candidate.is_absolute() {
        candidate
    } else {
        lexical(&format!(
            "{}/{}",
            cwd.unwrap_or(&repo_root.to_string_lossy()),
            candidate.to_string_lossy()
        ))
    };
    let root = lexical(&repo_root.to_string_lossy());
    let rel = absolute.strip_prefix(&root).ok()?;
    let rel = rel.to_string_lossy();
    (!rel.is_empty()).then(|| rel.into_owned())
}

/// The fingerprint's normal form of a (gated) command (M4-11): trimmed, ASCII whitespace runs
/// collapsed to one space; no Unicode normalization.
fn normalize_command(command: &str) -> String {
    command
        .split_ascii_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Unix seconds now.
fn now_ts() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// The first `max_chars` characters of `s`, cut at a character boundary.
fn head_chars(s: &str, max_chars: usize) -> &str {
    match s.char_indices().nth(max_chars) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

/// The first `lines` lines of `s`, at most `bytes` (whole lines only).
fn head_lines(s: &str, lines: usize, bytes: usize) -> String {
    let mut out = String::new();
    for line in s.lines().take(lines) {
        if out.len() + line.len() + 1 > bytes {
            break;
        }
        out.push_str(line);
        out.push('\n');
    }
    out.trim_end().to_owned()
}

/// The last `lines` lines of `s`, at most `bytes` (whole lines only).
fn tail_lines(s: &str, lines: usize, bytes: usize) -> String {
    let all: Vec<&str> = s.lines().collect();
    let mut kept: Vec<&str> = Vec::new();
    let mut size = 0usize;
    for line in all.iter().rev().take(lines) {
        if size + line.len() + 1 > bytes {
            break;
        }
        size += line.len() + 1;
        kept.push(line);
    }
    kept.reverse();
    kept.join("\n")
}

/// The spelling `kaban explain <address>` accepts for `a` (`explain::parse_address`: a node
/// id, `e:<id>`, a claim key, `x:Kind`, `<path>:<offset>`).
fn address_text(a: &Addressable) -> String {
    match a {
        Addressable::Node(id) => id.to_string(),
        Addressable::Edge(id) => format!("e:{id}"),
        Addressable::Assertion(key) => key.0.clone(),
        Addressable::DiscoveredKind(label) => label.clone(),
        Addressable::SpanRef { path, offset } => format!("{path}:{offset}"),
    }
}

/// `{"hookSpecificOutput":{"hookEventName":…,"additionalContext":…}}` (core-plan C4.3).
fn hook_specific(host_event: &str, context: &str) -> Value {
    json!({
        "hookSpecificOutput": {
            "hookEventName": host_event,
            "additionalContext": context,
        }
    })
}

/// Cut `text` to [`MAX_CONTEXT_CHARS`] at a line boundary.
fn bounded_context(text: &str) -> String {
    if text.chars().count() <= MAX_CONTEXT_CHARS {
        return text.to_owned();
    }
    let mut out = String::new();
    for line in text.lines() {
        if out.chars().count() + line.chars().count() + 1 > MAX_CONTEXT_CHARS {
            break;
        }
        out.push_str(line);
        out.push('\n');
    }
    out.trim_end().to_owned()
}

impl KabanApi {
    /// Run the hook `event` for `host` on the envelope the CLI read from stdin. Infallible by
    /// contract (M4-8): every error becomes `{}` plus a note.
    #[must_use]
    pub fn hook(&self, host: HookHost, event: HookEvent, envelope: &Value) -> HookOutcome {
        let selected = select(host, envelope);
        let prefix = format!("kaban hook {}:", event_name(event));
        // the envelope must be the host's own event for this handler (M4-8); `fix` has none
        let host_event = match event.host_name(host) {
            Some(expected) if selected.host_event.as_deref() == Some(expected) => Some(expected),
            Some(_) => return HookOutcome::silent(format!("{prefix} {NOTE_EVENT}")),
            None => None,
        };
        let outcome = match event {
            HookEvent::SessionStart => self.hook_session_start(&selected, host_event),
            HookEvent::PreToolUse => self.hook_pre_tool_use(host, &selected, host_event),
            HookEvent::PostToolUseFailure => self.hook_failure(host, &selected, true),
            HookEvent::PostToolUse => self.hook_post_tool_use(host, &selected),
            HookEvent::SubagentStop => self.hook_subagent_stop(host, &selected),
            HookEvent::Stop => self.hook_stop(host, &selected),
            HookEvent::Fix => self.hook_fix(&selected),
            HookEvent::PostCommit => self.hook_post_commit(),
        };
        match outcome {
            Ok(outcome) => self.gate_outcome(&prefix, self.settle_hook(&prefix, outcome)),
            Err(e) => {
                let mut refused = HookOutcome::silent(format!("{prefix} {}", note_for(&e)));
                // a write-bearing event that failed did not commit its mutation (SS-10
                // `rejected`); the receipt is best effort, the note is the contract
                if event.writes() {
                    refused.receipt = Some(self.rejected_receipt());
                }
                refused
            }
        }
    }

    /// One write barrier per hook call (SS-10), inside the hook's budget: a committed write
    /// whose position the search reader does not cover yet is said so on stderr
    /// ([`NOTE_PENDING_SEARCH`]), never claimed searchable and never replayed; the exit
    /// status stays 0 (M4-8). An event that wrote nothing has no receipt and waits for nothing.
    fn settle_hook(&self, prefix: &str, mut outcome: HookOutcome) -> HookOutcome {
        if let Some(receipt) = outcome.receipt.take() {
            let settled = self.await_searchable_in_hook(receipt);
            if settled.outcome == WriteOutcome::CommittedNotYetSearchable {
                outcome.note = Some(match outcome.note.take() {
                    Some(note) => format!("{note}; {NOTE_PENDING_SEARCH}"),
                    None => format!("{prefix} {NOTE_PENDING_SEARCH}"),
                });
            }
            outcome.receipt = Some(settled);
        }
        outcome
    }

    /// The last gate before the document reaches stdout (M4-3: the `hook` scope has no
    /// registry entry, so every string is content). A reject hit withholds the document.
    fn gate_outcome(&self, prefix: &str, outcome: HookOutcome) -> HookOutcome {
        match self.gate_response("hook", &outcome.output) {
            Ok(clean) => HookOutcome {
                output: clean,
                note: outcome.note,
                receipt: outcome.receipt,
            },
            Err(_) => HookOutcome::silent(format!("{prefix} {NOTE_WITHHELD}")),
        }
    }

    /// Run `tool` on `raw` through the seam: the pre-parse scan, the typed parse, the call.
    fn hook_tool<Req, Resp, F>(&self, tool: &str, raw: Value, call: F) -> Result<Resp, ApiError>
    where
        Req: DeserializeOwned,
        Resp: Serialize + DeserializeOwned,
        F: FnOnce(&Self, Req) -> Result<Resp, ApiError>,
    {
        self.prepare_args(tool, &raw)?;
        // the parse error text is never printed by a hook (fixed notes only)
        let request = serde_json::from_value::<Req>(raw)
            .map_err(|_| ApiError::Invalid(NOTE_INVALID.into()))?;
        let response = call(self, request)?;
        // S4: the response gate plus the R3-9 reshape check, like every adapter
        let value =
            serde_json::to_value(&response).map_err(|_| ApiError::Invalid(NOTE_INVALID.into()))?;
        let clean = self.gate_response(tool, &value)?;
        serde_json::from_value::<Resp>(clean).map_err(|_| ApiError::Rejected {
            field: "response".into(),
            rules: vec!["reshaped".into()],
        })
    }

    /// Gate a hook-level string (a session id, a path) under the `hook` scope and flush its
    /// audit entries; `None` on a reject hit (the caller skips the row, M4-13).
    fn hook_gate(&self, field: &str, s: &str) -> Option<String> {
        let mut ctx = self.write_ctx(None);
        self.gate(&mut ctx, "hook", field, s, &mut Vec::new())
            .ok()
            .and_then(|gated| self.flush_audit(&mut ctx).ok().map(|()| gated))
    }

    /// Record one `session_edits` row for a gated session id (best effort: a store error is
    /// logged as a note by the caller, never fatal).
    fn record_session(
        &self,
        session_id: &str,
        kind: SessionEditKind,
        path: &str,
    ) -> Result<(), ApiError> {
        self.store
            .session_edit_record(&SessionEdit {
                session_id: session_id.to_owned(),
                kind,
                path: path.to_owned(),
                ts: now_ts(),
            })
            .map_err(ApiError::from)
    }

    /// `pre-tool-use`: the governing records for the paths about to be edited, injected as
    /// `additionalContext`; the attempted edits recorded as `edit` rows (M4-18).
    fn hook_pre_tool_use(
        &self,
        host: HookHost,
        s: &Selected,
        host_event: Option<&str>,
    ) -> Result<HookOutcome, ApiError> {
        let (Some(host_event), Some(tool)) = (host_event, s.tool_name.as_deref()) else {
            return Ok(HookOutcome::empty());
        };
        if !is_edit_tool(host, tool) {
            return Ok(HookOutcome::empty());
        }
        let Some(repo_root) = self.config.repo_root.as_deref() else {
            return Ok(HookOutcome::empty());
        };
        let mut paths: Vec<String> = s
            .paths
            .iter()
            .filter_map(|p| repo_relative(p, s.cwd.as_deref(), repo_root))
            .collect();
        paths.dedup();
        paths.truncate(MAX_TARGETS);
        if paths.is_empty() {
            return Ok(HookOutcome::empty());
        }
        let raw = json!({
            "targets": paths.iter().map(|p| json!({"path": p})).collect::<Vec<_>>(),
            "limit": WHY_LIMIT,
        });
        let why: WhyResponse = self.hook_tool("why", raw, |api, req: WhyRequest| api.why(req))?;
        // the attempted edits, gated (the `why` targets were scanned above; a redact-class
        // path is stored in its masked form, a reject-class one skipped)
        if let Some(session) = s
            .session_id
            .as_deref()
            .and_then(|v| self.hook_gate("session_id", v))
        {
            for p in &paths {
                if let Some(gated) = self.hook_gate("path", p) {
                    self.record_session(&session, SessionEditKind::Edit, &gated)?;
                }
            }
        }
        if why.results.is_empty() {
            return Ok(HookOutcome::empty());
        }
        let mut text = String::from("kaban: records governing this edit (why):\n");
        for card in &why.results {
            let body = card
                .node
                .summary
                .as_deref()
                .or(card.node.text.as_deref())
                .unwrap_or("");
            let _ = writeln!(
                text,
                "- {} [{}, {}, currency {}] {} (id {})",
                card.matched_target,
                card.node.kind,
                card.governance.as_str(),
                currency_name(card.currency.state),
                head_chars(body.lines().next().unwrap_or(""), TEXT_COMMAND_CHARS),
                card.node.id
            );
        }
        Ok(HookOutcome {
            output: hook_specific(host_event, &bounded_context(text.trim_end())),
            note: None,
            receipt: None,
        })
    }

    /// `session-start`: the briefing (<= [`BRIEFING_ITEMS`]: accepted Decisions and
    /// Conventions by importance, pending Failures; M4-17), the `kaban review:` section (OD-E-6,
    /// EN-F49: up to `[briefing] max_candidates` pending candidates and unresolved groups, each
    /// with its `kaban explain` address; `0` removes it) and the bounded prune of old
    /// `session_edits` rows (M4-13).
    fn hook_session_start(
        &self,
        s: &Selected,
        host_event: Option<&str>,
    ) -> Result<HookOutcome, ApiError> {
        let Some(host_event) = host_event else {
            return Ok(HookOutcome::empty());
        };
        // housekeeping first, bounded, never fatal
        let _ = self
            .store
            .session_edits_prune(now_ts() - SESSION_EDITS_RETENTION_S, PRUNE_BATCH);
        let workspace = self.config.workspace.as_str().to_owned();
        let session = s.session_id.clone().unwrap_or_default();
        let failures = self
            .store
            .pending_failures(&kaban_core::PendingFailuresQuery {
                workspace_id: workspace.clone(),
                session_id: session,
                scope: kaban_core::PendingScope::Workspace,
                limit: BRIEFING_FAILURES,
            })?;
        // the kinds whose records govern paths, from the loaded ontologies (EN-F4, E1.6)
        let governing = self.registry().governing_kinds();
        let governing: Vec<&str> = governing.iter().map(String::as_str).collect();
        let records = self.store.briefing(
            &workspace,
            &governing,
            BRIEFING_ITEMS.saturating_sub(failures.len()),
        )?;
        let governed = self.governed();
        let mut lines: Vec<String> = Vec::with_capacity(BRIEFING_ITEMS);
        for node in records.iter().chain(failures.iter()) {
            if lines.len() == BRIEFING_ITEMS {
                break;
            }
            // S4: the projected (masked) card, a hidden record skipped
            let Some(card) = self.project(node, false, None) else {
                continue;
            };
            let body = card
                .summary
                .as_deref()
                .or(card.text.as_deref())
                .unwrap_or("");
            let state = if node.kind == ADAPTER_KINDS.failure {
                "pending fix".to_owned()
            } else {
                card.governance.as_str().to_owned()
            };
            // the kind's display label (`governed.display.label`, EN-F4), the stored name for
            // a kind without one
            let label = governed
                .display(&node.kind)
                .map_or(card.kind.as_str(), |d| d.label.as_str());
            lines.push(format!(
                "- [{label}, {state}] {} (id {})",
                head_chars(body.lines().next().unwrap_or(""), TEXT_COMMAND_CHARS),
                card.id
            ));
        }
        // OD-E-6 (EN-F49; engine-plan E2.8): one bounded page of pending review items in
        // review order (review rows, conflicts, proposed records, stale anchors, discovered
        // kinds), the candidate statuses kept, each with the address `kaban explain` takes;
        // the lines count against `MAX_CONTEXT_CHARS` after the records
        let max_candidates =
            usize::try_from(self.config.engine.briefing.max_candidates).unwrap_or(usize::MAX);
        let mut review: Vec<String> = Vec::new();
        if max_candidates > 0 {
            let page = self.review(&ReviewQuery {
                limit: max_candidates,
                ..ReviewQuery::default()
            })?;
            for item in page.items {
                if !matches!(
                    item.status,
                    PendingStatus::Candidate(_)
                        | PendingStatus::Unresolved(_)
                        | PendingStatus::Discovered { .. }
                ) {
                    continue;
                }
                review.push(format!(
                    "- [review] {} (kaban explain {})",
                    item.sentence,
                    address_text(&item.subject)
                ));
            }
        }
        if lines.is_empty() && review.is_empty() {
            return Ok(HookOutcome::empty());
        }
        let mut sections: Vec<String> = Vec::with_capacity(2);
        if !lines.is_empty() {
            sections.push(format!("kaban briefing:\n{}", lines.join("\n")));
        }
        if !review.is_empty() {
            sections.push(format!("kaban review:\n{}", review.join("\n")));
        }
        let text = sections.join("\n");
        Ok(HookOutcome {
            output: hook_specific(host_event, &bounded_context(&text)),
            note: None,
            receipt: None,
        })
    }

    /// `post-tool-use-failure` (and the failure branch of Codex/pi `post-tool-use`): record a
    /// `Failure { pending_fix: true, command, error_text, stack_head }` through `remember`
    /// (M4-10: the full selected strings are scanned first, truncation at line boundaries
    /// comes after).
    fn hook_failure(
        &self,
        host: HookHost,
        s: &Selected,
        from_failure_event: bool,
    ) -> Result<HookOutcome, ApiError> {
        let Some(tool) = s.tool_name.as_deref() else {
            return Ok(HookOutcome::empty());
        };
        if tool.starts_with(OWN_TOOL_PREFIX) || !is_shell_tool(host, tool) {
            return Ok(HookOutcome::empty());
        }
        let (Some(command), Some(error_text)) = (s.command.as_deref(), s.error_text.as_deref())
        else {
            return Ok(HookOutcome::empty());
        };
        let event = if from_failure_event {
            "post-tool-use-failure"
        } else {
            "post-tool-use"
        };
        // 1. the full strings through the pre-parse scan (a credential anywhere rejects)
        let full = json!({
            "kind": ADAPTER_KINDS.failure,
            "text": "",
            "session_id": s.session_id,
            "props": {"command": command, "error_text": error_text},
        });
        self.prepare_args("remember", &full)?;
        // 2. truncate at line boundaries only now (M4-10)
        let first_cmd_line = command.lines().next().unwrap_or("").trim();
        let last_err_line = error_text
            .lines()
            .rev()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or("");
        let text = format!(
            "\"{}\" failed: {}",
            head_chars(first_cmd_line, TEXT_COMMAND_CHARS),
            head_chars(last_err_line, TEXT_ERROR_CHARS)
        );
        // the fingerprint is keyed over the gated command (S1: scanned before hashing; M4-11)
        let fingerprint = self.command_fingerprint(command)?;
        let raw = json!({
            "kind": ADAPTER_KINDS.failure,
            "text": text,
            "session_id": s.session_id,
            "props": {
                "command": head_lines(command, usize::MAX, COMMAND_BYTES),
                "error_text": tail_lines(error_text, ERROR_TAIL_LINES, ERROR_TAIL_BYTES),
                "stack_head": head_lines(error_text, STACK_HEAD_LINES, STACK_HEAD_BYTES),
                "pending_fix": true,
                "command_fingerprint": fingerprint,
            },
        });
        // 3. the typed write re-gates every field inside its transaction (S1)
        let stored: RememberResponse =
            self.hook_tool("remember", raw, |api, req: RememberRequest| {
                api.remember_unsettled(req)
            })?;
        // 4. pair the occurrence (created or existing, M4-14) with the session: a second
        // write after the Failure committed, so its failure is said, never a rejection (SS-10)
        if let Some(session) = s
            .session_id
            .as_deref()
            .and_then(|v| self.hook_gate("session_id", v))
            && let Err(e) = self.record_session(&session, SessionEditKind::Failure, &stored.id)
        {
            tracing::warn!(error = %e, "the session pairing failed after the Failure committed");
            return Ok(HookOutcome::committed_with_note(
                stored.receipt,
                format!("kaban hook {event}: {NOTE_PAIRING_FAILED}"),
            ));
        }
        Ok(HookOutcome::committed(stored.receipt))
    }

    /// `HMAC(audit key, "kaban-cmd/v1\0" + normalized gated command)` as 64 hex digits
    /// (M4-11): a stub that cannot confirm a guess (S1), the same for every occurrence of the
    /// command in any session. Rejected when the command hits a reject rule.
    fn command_fingerprint(&self, command: &str) -> Result<String, ApiError> {
        let gated = self
            .hook_gate("command", command)
            .ok_or_else(|| ApiError::Rejected {
                field: "command".into(),
                rules: vec!["denylist".into()],
            })?;
        let mut data = FINGERPRINT_DOMAIN.to_owned();
        data.push_str(&normalize_command(&gated));
        Ok(self.signer.hmac_hex(data.as_bytes()))
    }

    /// The pending Failures of a session (M4-14), newest first.
    fn session_pending(&self, session: &str) -> Result<Vec<kaban_core::Node>, ApiError> {
        Ok(self.store.pending_failures(&PendingFailuresQuery {
            workspace_id: self.config.workspace.as_str().to_owned(),
            session_id: session.to_owned(),
            scope: PendingScope::Session,
            limit: PENDING_LIMIT,
        })?)
    }

    /// A successful command: evidence for every pending Failure of the session with the same
    /// fingerprint (M4-14): `{ts, files_edited_since}` from the session's `edit` rows after
    /// the Failure's `failure` row.
    fn hook_success(&self, s: &Selected) -> Result<HookOutcome, ApiError> {
        let (Some(command), Some(session)) = (s.command.as_deref(), s.session_id.as_deref()) else {
            return Ok(HookOutcome::empty());
        };
        let Some(session) = self.hook_gate("session_id", session) else {
            return Ok(HookOutcome::empty());
        };
        let fingerprint = self.command_fingerprint(command)?;
        let pending: Vec<kaban_core::Node> = self
            .session_pending(&session)?
            .into_iter()
            .filter(|n| n.props["command_fingerprint"].as_str() == Some(fingerprint.as_str()))
            .collect();
        if pending.is_empty() {
            return Ok(HookOutcome::empty());
        }
        let failure_rows =
            self.store
                .session_edits(&session, SessionEditKind::Failure, 0, SESSION_ROWS)?;
        let now = now_ts();
        // one receipt covers every evidence update that committed (SS-10): each is its own
        // transaction, so a failure after the first commit is said, never a rejection
        let mut committed: Option<WriteReceipt> = None;
        for node in pending {
            let since = failure_rows
                .iter()
                .filter(|r| r.path == node.id)
                .map(|r| r.ts)
                .min()
                .unwrap_or(0);
            let ctx = self.write_ctx(None);
            let written = self
                .store
                .session_edits(&session, SessionEditKind::Edit, since, SESSION_ROWS)
                .map_err(ApiError::from)
                .and_then(|rows| {
                    let mut files: Vec<String> = rows.into_iter().map(|r| r.path).collect();
                    files.sort();
                    files.dedup();
                    let id = Self::node_id(&node.id)?;
                    Ok(self.store.update_failure_props(
                        &id,
                        &FailurePatch {
                            evidence_run: Some(EvidenceRun {
                                ts: now,
                                files_edited_since: files,
                            }),
                            ..FailurePatch::default()
                        },
                        &ctx,
                    )?)
                });
            match (written, committed.take()) {
                (Ok(_), _) => committed = Some(self.committed_receipt(ctx.terminal_seq())),
                (Err(e), None) => return Err(e),
                (Err(e), Some(receipt)) => {
                    tracing::warn!(error = %e, "an evidence write failed after another committed");
                    return Ok(HookOutcome::committed_with_note(
                        receipt,
                        format!("kaban hook post-tool-use: {NOTE_EVIDENCE_PARTIAL}"),
                    ));
                }
            }
        }
        Ok(committed.map_or_else(HookOutcome::empty, HookOutcome::committed))
    }

    /// `post-tool-use`: on Codex and pi the failure signal decides (non-zero exit, `isError`);
    /// a success is evidence for a pending Failure (C4.4).
    fn hook_post_tool_use(&self, host: HookHost, s: &Selected) -> Result<HookOutcome, ApiError> {
        let Some(tool) = s.tool_name.as_deref() else {
            return Ok(HookOutcome::empty());
        };
        if tool.starts_with(OWN_TOOL_PREFIX) || !is_shell_tool(host, tool) {
            return Ok(HookOutcome::empty());
        }
        match (host, s.failed) {
            (HookHost::Codex | HookHost::Pi, Some(true)) => self.hook_failure(host, s, false),
            _ => self.hook_success(s),
        }
    }

    /// `subagent-stop`: an Episode when a summary is present (M4-31).
    fn hook_subagent_stop(&self, _host: HookHost, s: &Selected) -> Result<HookOutcome, ApiError> {
        let Some(summary) = s
            .summary
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
        else {
            return Ok(HookOutcome::empty());
        };
        let raw = json!({
            "kind": ADAPTER_KINDS.episode,
            "text": summary,
            "session_id": s.session_id,
        });
        let stored: RememberResponse =
            self.hook_tool("remember", raw, |api, req: RememberRequest| {
                api.remember_unsettled(req)
            })?;
        Ok(HookOutcome::committed(stored.receipt))
    }

    /// `stop`: the one-time "how was it fixed" prompt (core-spec F8b; M4-16). With pending
    /// Failures in the session and no `prompted` marker yet, the marker is written and the
    /// prompt is built from the **projected** (masked) Failure: its id and both reply routes
    /// (`kaban hook fix` or an MCP `link FIXED_BY`). Every host reads
    /// `{"decision":"block","reason":…}`: the live capture (`HOST-EVENTS.md` §2) showed Codex
    /// delivers `decision: block` to the model and not `systemMessage`, so M4-16's bounded
    /// switch applies; pi's extension turns the same document into one continuation. The exit
    /// status is 0 either way (PR-9). The second Stop of the session prints `{}`; so does one
    /// whose `stop_hook_active` is set (the hosts' own loop guard).
    fn hook_stop(&self, _host: HookHost, s: &Selected) -> Result<HookOutcome, ApiError> {
        if s.stop_hook_active == Some(true) {
            return Ok(HookOutcome::empty());
        }
        let Some(session) = s
            .session_id
            .as_deref()
            .and_then(|v| self.hook_gate("session_id", v))
        else {
            return Ok(HookOutcome::empty());
        };
        let pending = self.session_pending(&session)?;
        if pending.is_empty() {
            return Ok(HookOutcome::empty());
        }
        let prompted = self
            .store
            .session_edits(&session, SessionEditKind::Prompted, 0, 1)?;
        if !prompted.is_empty() {
            return Ok(HookOutcome::empty());
        }
        // the first pending Failure the read path lets through (S4)
        let Some((card, more)) = pending.iter().enumerate().find_map(|(i, n)| {
            self.project(n, true, None)
                .map(|c| (c, pending.len() - i - 1))
        }) else {
            return Ok(HookOutcome::empty());
        };
        // the marker before the prompt: a crash after this point loses one prompt, never loops
        self.record_session(&session, SessionEditKind::Prompted, "")?;
        let command = card
            .props
            .as_ref()
            .and_then(|p| p.get("command"))
            .and_then(Value::as_str)
            .map(|c| c.lines().next().unwrap_or("").trim().to_owned())
            .filter(|c| !c.is_empty())
            .or_else(|| card.text.clone())
            .unwrap_or_default();
        let others = if more > 0 {
            format!(" ({more} more pending)")
        } else {
            String::new()
        };
        let reason = format!(
            "kaban: how was \"{}\" fixed? Failure {}{others}. Reply in one line with \
             `kaban hook fix --stdin-json` and the body {{\"failure_id\":\"{}\",\"fix\":\"<one line>\"}}, \
             or link FIXED_BY from {} to an Improvement, or say 'unresolved'.",
            head_chars(&command, TEXT_COMMAND_CHARS),
            card.id,
            card.id,
            card.id
        );
        Ok(HookOutcome {
            output: json!({"decision": "block", "reason": reason}),
            note: None,
            receipt: None,
        })
    }

    /// `post-commit` (M5-14, M5-15/OWN-23; OWN-65): `commit_sha = HEAD` on every unstamped node
    /// of the bound workspace whose kind is in the registry's stamping set (effective
    /// `index.stamp: commit`, engine-spec §3.1; the shipped document kinds are `none`) and
    /// whose `repo` is unset or this repository's, bumping `rev` so the next `sync`
    /// re-serializes the record with its provenance (once per node, so it converges). Outside
    /// a repository, or when git gives no HEAD within its bound, `{}` with a fixed note. No
    /// `changes(paths)` rows and no currency cache (OWN-23).
    fn hook_post_commit(&self) -> Result<HookOutcome, ApiError> {
        let Some(root) = self.config.repo_root.as_deref() else {
            return Ok(HookOutcome::silent(format!(
                "kaban hook post-commit: {NOTE_NO_REPO}"
            )));
        };
        let Some(sha) = self.git.head(root) else {
            return Ok(HookOutcome::silent(format!(
                "kaban hook post-commit: {NOTE_NO_HEAD}"
            )));
        };
        let kinds = self.registry.stamp_kinds();
        let kinds: Vec<&str> = kinds.iter().map(String::as_str).collect();
        let stamped = self.store.stamp_commit(
            self.config.workspace.as_str(),
            &sha,
            self.config.repo.as_deref(),
            &kinds,
        )?;
        Ok(HookOutcome {
            output: json!({"stamped": stamped}),
            note: None,
            // the stamping committed: its receipt holds at the head (no store outcome
            // carries the stamping's own sequence)
            receipt: Some(self.committed_receipt(None)),
        })
    }

    /// `fix`: the agent's reply `{"failure_id", "fix"}` (M4-15): an `Improvement` with the fix
    /// text linked `FIXED_BY` from the Failure through the atomic, workspace-checked
    /// `remember`, then the Failure's props patched (`fix`, `pending_fix: false`) in one
    /// transaction. Should the patch fail, the edge alone already resolves the Failure (M4-14).
    fn hook_fix(&self, s: &Selected) -> Result<HookOutcome, ApiError> {
        let (Some(failure_id), Some(fix)) = (s.failure_id.as_deref(), s.fix.as_deref()) else {
            return Err(ApiError::Invalid(NOTE_FIX_FIELDS.into()));
        };
        if fix.trim().is_empty() {
            return Err(ApiError::Invalid(NOTE_FIX_FIELDS.into()));
        }
        let raw = json!({
            "kind": ADAPTER_KINDS.improvement,
            "text": fix,
            "session_id": s.session_id,
            "links": [{"rel": "FIXED_BY", "from": failure_id}],
        });
        let stored: RememberResponse =
            self.hook_tool("remember", raw, |api, req: RememberRequest| {
                api.remember_unsettled(req)
            })?;
        // the fix text stored in props is the gated form (the write above gated the same
        // string as the Improvement's text; a redaction is reported there). The patch is a
        // second transaction after the Improvement committed: should it fail, the outcome is
        // the Improvement's `committed` receipt and a note, never a rejection (SS-10)
        let mut ctx = self.write_ctx(None);
        let patched = self
            .gate(&mut ctx, "remember", "props", fix, &mut Vec::new())
            .and_then(|gated_fix| {
                let id = Self::node_id(failure_id)?;
                Ok(self.store.update_failure_props(
                    &id,
                    &FailurePatch {
                        fix: Some(gated_fix),
                        pending_fix: Some(false),
                        evidence_run: None,
                    },
                    &ctx,
                )?)
            });
        let node = match patched {
            Ok(node) => node,
            Err(e) => {
                tracing::warn!(error = %e, "the Failure's patch failed after the Improvement committed");
                return Ok(HookOutcome::committed_with_note(
                    stored.receipt,
                    format!("kaban hook fix: {NOTE_PATCH_FAILED}"),
                ));
            }
        };
        Ok(HookOutcome {
            output: json!({
                "status": "fixed",
                "failure_id": node.id,
                "improvement_id": stored.id,
                "pending_fix": false,
            }),
            note: None,
            // the Failure's patch follows the `remember` in the same call: its position
            // covers both
            receipt: Some(self.committed_receipt(ctx.terminal_seq())),
        })
    }
}

/// The wire name of a currency state (core-spec F2 vocabulary).
fn currency_name(state: crate::currency::CurrencyState) -> &'static str {
    use crate::currency::CurrencyState;
    match state {
        CurrencyState::Active => "active",
        CurrencyState::NeedsReview => "needs_review",
        CurrencyState::Stale => "stale",
        CurrencyState::Uncheckable => "uncheckable",
    }
}

/// The kebab-case name of an event, for notes.
fn event_name(event: HookEvent) -> &'static str {
    match event {
        HookEvent::SessionStart => "session-start",
        HookEvent::PreToolUse => "pre-tool-use",
        HookEvent::PostToolUse => "post-tool-use",
        HookEvent::PostToolUseFailure => "post-tool-use-failure",
        HookEvent::SubagentStop => "subagent-stop",
        HookEvent::Stop => "stop",
        HookEvent::Fix => "fix",
        HookEvent::PostCommit => "post-commit",
    }
}

/// The fixed note for a failed hook: a class, never the error's text (which may quote a field
/// name at most, but hooks say nothing variable).
fn note_for(e: &ApiError) -> &'static str {
    match e {
        ApiError::Rejected { .. } => NOTE_REFUSED,
        ApiError::NotFound(_) => NOTE_NOT_FOUND,
        ApiError::Invalid(text) if text == NOTE_FIX_FIELDS => NOTE_FIX_FIELDS,
        ApiError::Invalid(_) | ApiError::Policy(_) => NOTE_INVALID,
        ApiError::Store(_) => NOTE_STORE,
    }
}
