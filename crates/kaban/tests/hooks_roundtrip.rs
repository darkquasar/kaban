//! Hook round trip per host (core-spec F4 (iii), F7, F8, F8b, S1; core-plan C4.6 as amended by
//! `reviews/m4-prep-dispositions.md` M4-3, M4-4, M4-7, M4-8): the whole CLI seam, black-box
//! through the built binary over the shipped fixtures under `tests/fixtures/hooks/<host>/`. A
//! Decision is seeded with `kaban remember` (the CLI, not the API), then one session runs
//! `session-start` (nothing to brief: a Decision is never briefed), `pre-tool-use` (nothing
//! injected), the host's failure event, `kaban recall` by the error string (the pending Failure
//! with the session id), `session-start` again (the briefing names the Failure), `stop` twice (one prompt, then silence), `hook fix`, and `recall
//! --hops 1` again (`pending_fix: false` and the `FIXED_BY` neighbour). Every hook call exits 0
//! and prints exactly one JSON document; the fixtures' file-content canary reaches no output
//! and no database byte. The per-event field checks live in `hook_claude.rs`, `hook_hosts.rs`
//! and `hook_pairing.rs` and are not repeated here.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod hook_common;

use hook_common::{HookScratch, Run, fixture, hook, set};
use serde_json::{Value, json};
use std::process::{Command, Output, Stdio};

/// The file-content canary every edit fixture carries (`new_string`, the patch body, `newText`):
/// hits the builtin credential rule `aws-access-key-id`, and must reach nothing (M4-7).
const CANARY: &str = "AKIAQWERTYUIOPASDFGH";
const DECISION: &str = "Use WAL mode for every SQLite store";
const SCOPE: &str = "src/store/**";
const FIX: &str = "renamed open_v2 back to open; the method was never added";

/// What differs between the hosts: the `--host` value, the fixtures of each step, the event
/// the failure fixture is sent to, the recall query matching the failure text, and the host's
/// own event names echoed in `hookSpecificOutput.hookEventName` (M4-8).
struct Host {
    name: &'static str,
    session: &'static str,
    session_start: &'static str,
    edit: &'static str,
    /// JSON pointer of the edited path in the edit fixture, and the value to set there
    /// (`None`: the fixture's path is already repository-relative and stays).
    edit_path: Option<(&'static str, &'static str)>,
    failure: &'static str,
    failure_event: &'static str,
    recall_query: &'static str,
    stop: &'static str,
    session_start_name: &'static str,
}

const CLAUDE: Host = Host {
    name: "claude-code",
    session: "sess-roundtrip-claude",
    session_start: "session-start",
    edit: "pre-tool-use-edit",
    edit_path: Some(("/tool_input/file_path", "src/store/sqlite.rs")),
    failure: "post-tool-use-failure-bash",
    failure_event: "post-tool-use-failure",
    recall_query: "nightjar-store FAILED",
    stop: "stop",
    session_start_name: "SessionStart",
};

const CODEX: Host = Host {
    name: "codex",
    session: "sess-roundtrip-codex",
    session_start: "session-start",
    edit: "pre-tool-use-apply-patch",
    edit_path: None, // the `*** Update File:` headers name `src/store/*.rs`
    failure: "post-tool-use-bash-failed",
    failure_event: "post-tool-use", // Codex has no failure event: the `Exit code:` marker
    recall_query: "pytest failed",
    stop: "stop",
    session_start_name: "SessionStart",
};

const PI: Host = Host {
    name: "pi",
    session: "sess-roundtrip-pi",
    session_start: "session-start",
    edit: "tool-call-edit",
    edit_path: None, // `input.path` is repository-relative already
    failure: "tool-execution-end-bash-error",
    failure_event: "post-tool-use", // pi forwards one event with `isError`
    recall_query: "npm failed",
    stop: "agent-before-settle",
    session_start_name: "session_start",
};

/// `kaban --db <db> --json <args>` run from inside the scratch repository (so the workspace is
/// the repository's name, like the hooks), with `KABAN_HOME` on the scratch and no stdin.
fn cli(s: &HookScratch, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_kaban"))
        .env("KABAN_HOME", s.home())
        .env("RUST_LOG", "warn")
        .env_remove("KABAN_WORKSPACE")
        .current_dir(s.repo())
        .args(["--db", s.db().to_str().unwrap(), "--json"])
        .args(args)
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

/// stdout of a successful CLI run as its one JSON document.
fn document(out: &Output) -> Value {
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{stderr}");
    let text = String::from_utf8_lossy(&out.stdout);
    serde_json::from_str(text.trim())
        .unwrap_or_else(|e| panic!("stdout is not one JSON document: {e}: {text:?}"))
}

/// A Decision scoped to `SCOPE`, seeded through the CLI as a person would; the database is
/// created by this first command.
fn seed_decision(s: &HookScratch) -> String {
    let remembered = document(&cli(
        s,
        &[
            "remember",
            "Decision",
            DECISION,
            "--path-scope",
            SCOPE,
            "--props",
            r#"{"rationale": "because the team agreed"}"#,
        ],
    ));
    assert_eq!(remembered["status"], json!("created"), "{remembered}");
    remembered["id"].as_str().unwrap().to_owned()
}

/// The fixture `name` of `host` bound to the scratch repository and the round trip's session.
fn envelope(s: &HookScratch, host: &Host, name: &str) -> Value {
    let mut env = fixture(host.name, name);
    set(&mut env, "/cwd", json!(s.repo().to_str().unwrap()));
    set(&mut env, "/session_id", json!(host.session));
    env
}

/// Run one hook call and check the contract every call shares: exit 0, exactly one JSON
/// document, the canary in neither stdout nor stderr (hooks/README.md "Contract").
fn hook_call(s: &HookScratch, host: &Host, event: &str, env: &Value) -> (Run, Value) {
    let run = hook(s, event, Some(host.name), env);
    assert_eq!(
        run.code(),
        Some(0),
        "{}/{event}: stderr: {}",
        host.name,
        run.stderr()
    );
    let out = run.json();
    assert!(
        !run.stdout().contains(CANARY) && !run.stderr().contains(CANARY),
        "{}/{event}: the file content leaked: {} {}",
        host.name,
        run.stdout(),
        run.stderr()
    );
    (run, out)
}

fn empty() -> Value {
    Value::Object(serde_json::Map::new())
}

/// `kaban recall <query> --full-text --hops <hops>` from the CLI; the cards.
fn recall(s: &HookScratch, query: &str, hops: &str) -> Vec<Value> {
    let doc = document(&cli(s, &["recall", query, "--full-text", "--hops", hops]));
    doc["results"].as_array().cloned().expect("results")
}

fn the_failure(cards: &[Value]) -> &Value {
    let failures: Vec<&Value> = cards
        .iter()
        .filter(|c| c["kind"] == json!("Failure"))
        .collect();
    assert_eq!(failures.len(), 1, "exactly one Failure card: {cards:?}");
    failures[0]
}

fn round_trip(host: &Host) {
    // Arrange: a Decision through the CLI seam
    let s = HookScratch::new(&format!("roundtrip-{}", host.name));
    let decision = seed_decision(&s);

    // session-start: nothing pending, and a Decision is never briefed
    let start = envelope(&s, host, host.session_start);
    let (_, out) = hook_call(&s, host, "session-start", &start);
    assert_eq!(out, empty(), "{out}");

    // pre-tool-use: the edit is recorded, nothing is injected
    let mut edit = envelope(&s, host, host.edit);
    if let Some((pointer, relative)) = host.edit_path {
        set(
            &mut edit,
            pointer,
            json!(s.repo().join(relative).to_str().unwrap()),
        );
    }
    assert!(
        edit.to_string().contains(CANARY),
        "fixture: the edit content carries the canary"
    );
    let (_, out) = hook_call(&s, host, "pre-tool-use", &edit);
    assert_eq!(out, empty(), "{out}");

    // the failure event: nothing to say, a pending Failure recorded with the session id
    let (_, out) = hook_call(
        &s,
        host,
        host.failure_event,
        &envelope(&s, host, host.failure),
    );
    assert_eq!(out, empty(), "{out}");
    let cards = recall(&s, host.recall_query, "0");
    let failure = the_failure(&cards);
    assert_eq!(failure["props"]["pending_fix"], json!(true), "{failure}");
    assert_eq!(
        failure["provenance"]["session_id"],
        json!(host.session),
        "{failure}"
    );
    let failure_id = failure["id"].as_str().unwrap().to_owned();

    // session-start again: the briefing names the pending Failure, not the Decision
    let (_, out) = hook_call(&s, host, "session-start", &start);
    let hso = &out["hookSpecificOutput"];
    assert_eq!(
        hso["hookEventName"],
        json!(host.session_start_name),
        "{out}"
    );
    let briefing = hso["additionalContext"]
        .as_str()
        .expect("additionalContext");
    assert!(
        briefing.contains(&failure_id) && !briefing.contains(&decision),
        "{briefing}"
    );

    // stop: the one-time prompt names the Failure; the second stop is silent
    let stop = envelope(&s, host, host.stop);
    let (_, out) = hook_call(&s, host, "stop", &stop);
    assert_eq!(out["decision"], json!("block"), "{out}");
    assert!(
        out["reason"].as_str().unwrap().contains(&failure_id),
        "{out}"
    );
    let (_, out) = hook_call(&s, host, "stop", &stop);
    assert_eq!(out, empty(), "second stop of the session: {out}");

    // hook fix, then recall shows the resolution and its FIXED_BY neighbour
    let reply = json!({"session_id": host.session, "failure_id": failure_id, "fix": FIX});
    let (run, out) = hook_call(&s, host, "fix", &reply);
    assert_eq!(
        out["status"],
        json!("fixed"),
        "{out}; stderr: {}",
        run.stderr()
    );
    assert_eq!(out["failure_id"], json!(failure_id), "{out}");
    let cards = recall(&s, host.recall_query, "1");
    let failure = the_failure(&cards);
    assert_eq!(failure["props"]["pending_fix"], json!(false), "{failure}");
    assert_eq!(failure["props"]["fix"], json!(FIX), "{failure}");
    let improvement = cards
        .iter()
        .find(|c| c["kind"] == json!("Improvement"))
        .unwrap_or_else(|| panic!("the FIXED_BY neighbour is in the result: {cards:?}"));
    assert_eq!(improvement["text"], json!(FIX), "{improvement}");
    assert_eq!(
        improvement["why"]
            .as_array()
            .map(|w| w.contains(&json!("hop1:FIXED_BY"))),
        Some(true),
        "reached through the FIXED_BY edge: {improvement}"
    );

    // the file-content canary reached no database byte either
    assert!(
        !String::from_utf8_lossy(&s.db_bytes()).contains(CANARY),
        "the file content was persisted"
    );
}

#[test]
fn claude_code_round_trip_briefs_injects_context_captures_a_failure_prompts_once_and_pairs_the_fix()
{
    round_trip(&CLAUDE);
}

#[test]
fn codex_round_trip_briefs_injects_context_captures_a_failure_prompts_once_and_pairs_the_fix() {
    round_trip(&CODEX);
}

#[test]
fn pi_round_trip_briefs_injects_context_captures_a_failure_prompts_once_and_pairs_the_fix() {
    round_trip(&PI);
}
