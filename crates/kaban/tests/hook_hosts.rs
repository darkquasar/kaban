//! The `--host codex` and `--host pi` handler variants (core-spec F8, §11; core-plan C4.5 as
//! amended by `reviews/m4-prep-dispositions.md` M4-21, M4-30, M4-31; the plan's "host-specific
//! envelope differences are covered by critical-tdd fixtures in the handler tests"), black-box
//! with the invented-data fixtures under `tests/fixtures/hooks/{codex,pi}/`, aligned to the
//! live field tables of `hooks/HOST-EVENTS.md` (W2-D1). Codex has no `PostToolUseFailure`
//! and its `tool_response` is a plain string: an `Exit code: <n>` marker in it is the only
//! failure signal (no marker: evidence-only). pi's extension forwards `tool_execution_end`
//! with `isError` and `result.content[].text`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod hook_common;

use hook_common::{HookScratch, fixture, hook, session_rows, set};
use serde_json::{Value, json};

const CANARY: &str = "AKIAQWERTYUIOPASDFGH";

fn in_repo(s: &HookScratch, host: &str, name: &str) -> Value {
    let mut env = fixture(host, name);
    set(&mut env, "/cwd", json!(s.repo().to_str().unwrap()));
    env
}

/// `(text, props)` of every active Failure, oldest first.
fn failures(s: &HookScratch) -> Vec<(String, Value)> {
    let conn = s.raw();
    let mut st = conn
        .prepare(
            "SELECT kaban_body(text), props FROM node WHERE kind = 'Failure' AND lifecycle = 'active' ORDER BY rid",
        )
        .unwrap();
    st.query_map([], |r| {
        let props: String = r.get(1)?;
        Ok((r.get(0)?, serde_json::from_str(&props).unwrap()))
    })
    .unwrap()
    .collect::<Result<_, _>>()
    .unwrap()
}

fn empty() -> Value {
    Value::Object(serde_json::Map::new())
}

/// Codex `PostToolUse` whose string response carries `Exit code: 1` records a Failure
/// (command, last line, the response string as `error_text`, `pending_fix`); the object
/// spelling `exitCode: 2` the plan assumed is still accepted; `Exit code: 0` records nothing
/// new and is the success evidence of the first one.
#[test]
fn codex_post_tool_use_with_a_non_zero_exit_records_a_failure_and_a_zero_exit_is_evidence() {
    let s = HookScratch::new("codex-exit");
    drop(s.api());
    let failed = in_repo(&s, "codex", "post-tool-use-bash-failed");
    assert!(
        failed["tool_response"].is_string(),
        "HOST-EVENTS §2: a string"
    );
    let run = hook(&s, "post-tool-use", Some("codex"), &failed);
    assert_eq!(run.code(), Some(0), "{}", run.stderr());
    assert_eq!(run.json(), empty(), "{}", run.stdout());
    // SS-10 permits this fixed notice while a committed write awaits search visibility.
    let stderr = run.stderr();
    assert!(
        stderr.trim().is_empty()
            || stderr.trim()
                == "kaban hook post-tool-use: stored; not yet searchable, do not replay",
        "{stderr}"
    );
    let got = failures(&s);
    assert_eq!(got.len(), 1, "{got:?}");
    assert!(
        got[0]
            .0
            .starts_with("\"pytest -q tests/test_store.py\" failed: 1 failed, 11 passed"),
        "{}",
        got[0].0
    );
    assert_eq!(got[0].1["pending_fix"], json!(true));
    assert!(
        got[0].1["error_text"]
            .as_str()
            .unwrap()
            .starts_with("Exit code: 1"),
        "the whole response string is the error text: {}",
        got[0].1
    );
    assert_eq!(got[0].1["command_fingerprint"].as_str().unwrap().len(), 64);

    // the object spelling the plan assumed, another command
    let mut camel = failed.clone();
    set(&mut camel, "/tool_input/command", json!("make lint"));
    camel["tool_response"] = json!({"exitCode": 2, "output": "lint: 3 errors"});
    assert_eq!(
        hook(&s, "post-tool-use", Some("codex"), &camel).code(),
        Some(0)
    );
    let got = failures(&s);
    assert_eq!(got.len(), 2, "{got:?}");
    assert!(
        got[1].0.starts_with("\"make lint\" failed: lint: 3 errors"),
        "{}",
        got[1].0
    );

    // a zero exit of the first command: no new Failure, evidence on the pending one
    let ok = in_repo(&s, "codex", "post-tool-use-bash-ok");
    let run = hook(&s, "post-tool-use", Some("codex"), &ok);
    assert_eq!(run.code(), Some(0), "{}", run.stderr());
    let got = failures(&s);
    assert_eq!(got.len(), 2, "no Failure on success: {got:?}");
    assert_eq!(
        got[0].1["evidence_runs"].as_array().map(Vec::len),
        Some(1),
        "{}",
        got[0].1
    );
    assert!(
        got[1].1.get("evidence_runs").is_none(),
        "another command: {}",
        got[1].1
    );
    assert_eq!(
        session_rows(&s, "sess-codex-0001")
            .iter()
            .filter(|(k, _)| k == "failure")
            .count(),
        2
    );
}

/// `HOST-EVENTS.md` §2: a Codex `Bash` response string without the `Exit code:` marker (the
/// observed one-line shape) is never guessed to be a failure (evidence-only, spec §11); a
/// marker anywhere in the string decides; `mcp__kaban__*` tools are never captured.
#[test]
fn codex_without_an_exit_code_marker_records_no_failure() {
    let s = HookScratch::new("codex-heuristic");
    drop(s.api());
    let mut env = in_repo(&s, "codex", "post-tool-use-bash-failed");
    env["tool_response"] = json!("bash: cargo: command not found");
    assert_eq!(
        hook(&s, "post-tool-use", Some("codex"), &env).code(),
        Some(0)
    );
    assert_eq!(failures(&s).len(), 0, "no marker, no guess");
    env["tool_response"] = json!("Output:\nbash: cargo: command not found\nExit code: 127");
    assert_eq!(
        hook(&s, "post-tool-use", Some("codex"), &env).code(),
        Some(0)
    );
    assert_eq!(failures(&s).len(), 1);
    assert!(
        failures(&s)[0].0.ends_with("Exit code: 127"),
        "{}",
        failures(&s)[0].0
    );
    let mut quiet = env.clone();
    set(&mut quiet, "/tool_input/command", json!("ls"));
    quiet["tool_response"] = json!("a\nb\nExit code: 0");
    assert_eq!(
        hook(&s, "post-tool-use", Some("codex"), &quiet).code(),
        Some(0)
    );
    assert_eq!(failures(&s).len(), 1, "a zero marker is a success");
    let mut own = env.clone();
    set(&mut own, "/tool_name", json!("mcp__kaban__remember"));
    own["tool_response"] = json!("Exit code: 1\nrejected");
    assert_eq!(
        hook(&s, "post-tool-use", Some("codex"), &own).code(),
        Some(0)
    );
    assert_eq!(
        failures(&s).len(),
        1,
        "kaban's own tools are never captured"
    );
}

/// Codex `PreToolUse` on `apply_patch` (`tool_input.command` holds the patch, HOST-EVENTS §2):
/// the `*** Update File:` / `*** Add File:` headers name the targets (the patch body, carrying
/// a canary, is never read as content) and are recorded as the session's edits with nothing
/// injected, and Codex `Stop` carries the prompt as `decision: block` (M4-16 switch:
/// `systemMessage` did not reach the model live).
#[test]
fn codex_apply_patch_paths_come_from_the_patch_headers_and_stop_uses_decision_block() {
    let s = HookScratch::new("codex-patch");
    drop(s.api());
    let env = in_repo(&s, "codex", "pre-tool-use-apply-patch");
    assert!(
        env["tool_input"]["command"].is_string(),
        "HOST-EVENTS §2 shape"
    );
    let run = hook(&s, "pre-tool-use", Some("codex"), &env);
    assert_eq!(run.code(), Some(0), "{}", run.stderr());
    assert_eq!(run.json(), empty(), "{}", run.stdout());
    assert!(!run.stdout().contains(CANARY) && !run.stderr().contains(CANARY));
    assert!(!String::from_utf8_lossy(&s.db_bytes()).contains(CANARY));
    assert_eq!(
        session_rows(&s, "sess-codex-0001"),
        vec![
            ("edit".to_owned(), "src/store/sqlite.rs".to_owned()),
            ("edit".to_owned(), "src/store/hooks.rs".to_owned()),
        ]
    );
    // a pending Failure, then Stop on Codex
    let failed = in_repo(&s, "codex", "post-tool-use-bash-failed");
    assert_eq!(
        hook(&s, "post-tool-use", Some("codex"), &failed).code(),
        Some(0)
    );
    let run = hook(&s, "stop", Some("codex"), &in_repo(&s, "codex", "stop"));
    assert_eq!(run.code(), Some(0), "{}", run.stderr());
    let out = run.json();
    assert_eq!(out["decision"], json!("block"), "{out}");
    let reason = out["reason"].as_str().expect("reason");
    assert!(
        reason.starts_with("kaban: how was \"pytest -q tests/test_store.py\" fixed?"),
        "{reason}"
    );
    assert!(
        out.get("systemMessage").is_none(),
        "systemMessage never reached the model live: {out}"
    );
    // briefing on Codex: the same hookSpecificOutput shape
    let run = hook(
        &s,
        "session-start",
        Some("codex"),
        &in_repo(&s, "codex", "session-start"),
    );
    assert_eq!(
        run.json()["hookSpecificOutput"]["hookEventName"],
        json!("SessionStart")
    );
}

/// pi: `tool_execution_end` with `isError: true` for `bash` records a Failure (the result's
/// `content[].text` is the error output); `isError: false` records nothing new and is
/// evidence; `tool_call` for `edit` records the edit and injects nothing (path from
/// `input.path`, `args.path` accepted too, the edit content never read); `agent_before_settle`
/// carries the prompt in the `decision: block` shape for the extension's one continuation.
#[test]
fn pi_is_error_records_a_failure_and_the_pi_events_echo_their_own_names() {
    let s = HookScratch::new("pi");
    drop(s.api());

    let error = in_repo(&s, "pi", "tool-execution-end-bash-error");
    let run = hook(&s, "post-tool-use", Some("pi"), &error);
    assert_eq!(run.code(), Some(0), "{}", run.stderr());
    assert_eq!(run.json(), empty(), "{}", run.stdout());
    let got = failures(&s);
    assert_eq!(got.len(), 1, "{got:?}");
    assert!(
        got[0]
            .0
            .starts_with("\"npm test -- --runInBand\" failed: npm ERR! Test failed."),
        "{}",
        got[0].0
    );
    assert!(
        got[0].1["stack_head"]
            .as_str()
            .unwrap()
            .starts_with("FAIL src/store.test.ts")
    );
    // the same payload through the failure event name is accepted too (pi forwards one event)
    let run = hook(&s, "post-tool-use-failure", Some("pi"), &error);
    assert_eq!(run.code(), Some(0));
    assert_eq!(
        failures(&s).len(),
        1,
        "the same occurrence dedups to the existing Failure"
    );

    let ok = in_repo(&s, "pi", "tool-execution-end-bash-ok");
    assert_eq!(hook(&s, "post-tool-use", Some("pi"), &ok).code(), Some(0));
    let got = failures(&s);
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got[0].1["evidence_runs"].as_array().map(Vec::len), Some(1));

    let edit = in_repo(&s, "pi", "tool-call-edit");
    let run = hook(&s, "pre-tool-use", Some("pi"), &edit);
    assert_eq!(run.code(), Some(0), "{}", run.stderr());
    assert_eq!(run.json(), empty(), "{}", run.stdout());
    assert!(
        !run.stdout().contains(CANARY) && !String::from_utf8_lossy(&s.db_bytes()).contains(CANARY)
    );
    // `tool_execution_start` shape: the path under `args`
    let mut args_shape = edit.clone();
    let input = args_shape.as_object_mut().unwrap().remove("input").unwrap();
    args_shape["args"] = input;
    set(&mut args_shape, "/toolName", json!("write"));
    let run = hook(&s, "pre-tool-use", Some("pi"), &args_shape);
    assert_eq!(run.code(), Some(0), "{}", run.stderr());
    assert_eq!(run.json(), empty(), "{}", run.stdout());
    let edits: Vec<_> = session_rows(&s, "sess-pi-0001")
        .into_iter()
        .filter(|(kind, _)| kind == "edit")
        .collect();
    assert_eq!(
        edits,
        vec![
            ("edit".to_owned(), "src/store/sqlite.rs".to_owned()),
            ("edit".to_owned(), "src/store/sqlite.rs".to_owned()),
        ],
        "both shapes name the edited path"
    );

    let run = hook(
        &s,
        "stop",
        Some("pi"),
        &in_repo(&s, "pi", "agent-before-settle"),
    );
    assert_eq!(run.code(), Some(0), "{}", run.stderr());
    let out = run.json();
    assert_eq!(out["decision"], json!("block"), "{out}");
    assert!(
        out["reason"]
            .as_str()
            .unwrap()
            .contains("npm test -- --runInBand")
    );
    let run = hook(
        &s,
        "session-start",
        Some("pi"),
        &in_repo(&s, "pi", "session-start"),
    );
    assert_eq!(
        run.json()["hookSpecificOutput"]["hookEventName"],
        json!("session_start")
    );

    // a Claude Code envelope handed to the pi variant is not its event: `{}` and a note
    let run = hook(
        &s,
        "post-tool-use",
        Some("pi"),
        &in_repo(&s, "claude-code", "post-tool-use-bash"),
    );
    assert_eq!(run.json(), empty());
    assert!(run.stderr().contains("hook_event_name"), "{}", run.stderr());
}
