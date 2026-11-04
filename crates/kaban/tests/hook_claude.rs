//! `kaban hook` with Claude Code envelopes (core-spec F8, S1, S6; core-plan C4.3 as amended by
//! `reviews/m4-prep-dispositions.md` M4-6 to M4-10, M4-13, M4-17, M4-18, M4-29, M4-32),
//! black-box through the binary with the invented-data fixtures under
//! `tests/fixtures/hooks/claude-code/`. Every hook exits 0 and prints one JSON document.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod hook_common;

use hook_common::{HookScratch, accepted_decision, fixture, hook, session_rows, set};
use serde_json::{Value, json};

/// Hits the builtin credential rule `aws-access-key-id` (always reject).
const CANARY: &str = "AKIAQWERTYUIOPASDFGH";

/// Plan slice: `pre-tool-use` output is valid `hookSpecificOutput` JSON carrying the `why`
/// context for the edited file (path normalized against the repository root, M4-18); the
/// attempted edit is recorded as a `session_edits` row; the `old_string`/`new_string` file
/// content in the envelope (carrying a credential canary) is neither scanned nor stored
/// (M4-7: allowlisted intake), so the hook still answers and the canary reaches nothing.
#[test]
fn pre_tool_use_injects_the_governing_decisions_for_the_edited_path_as_hook_specific_output() {
    // Arrange
    let s = HookScratch::new("pre");
    let api = s.api();
    let governing = accepted_decision(
        &api,
        "Use WAL mode for every SQLite store",
        &["src/store/**"],
        None,
    );
    let _elsewhere = accepted_decision(&api, "The CLI prints JSON only", &["src/cli/**"], None);
    drop(api);
    let mut env = fixture("claude-code", "pre-tool-use-edit");
    set(&mut env, "/cwd", json!(s.repo().to_str().unwrap()));
    set(
        &mut env,
        "/tool_input/file_path",
        json!(s.repo().join("src/store/sqlite.rs").to_str().unwrap()),
    );
    assert!(
        env["tool_input"]["new_string"]
            .as_str()
            .unwrap()
            .contains(CANARY),
        "fixture: the file content carries the canary"
    );

    // Act
    let run = hook(&s, "pre-tool-use", None, &env);

    // Assert
    assert_eq!(run.code(), Some(0), "stderr: {}", run.stderr());
    let out = run.json();
    let hso = &out["hookSpecificOutput"];
    assert_eq!(hso["hookEventName"], json!("PreToolUse"), "{out}");
    let context = hso["additionalContext"]
        .as_str()
        .expect("additionalContext");
    assert!(
        context.contains("Use WAL mode for every SQLite store") && context.contains(&governing),
        "{context}"
    );
    assert!(
        !context.contains("The CLI prints JSON only"),
        "a record scoped elsewhere must not be injected: {context}"
    );
    assert!(
        context.contains("src/store/sqlite.rs"),
        "the repository-relative path names the target: {context}"
    );
    assert!(
        !run.stdout().contains(CANARY) && !run.stderr().contains(CANARY),
        "the file content leaked: {} {}",
        run.stdout(),
        run.stderr()
    );
    assert!(
        !String::from_utf8_lossy(&s.db_bytes()).contains(CANARY),
        "the file content was persisted"
    );
    assert_eq!(
        session_rows(&s, "sess-claude-0001"),
        vec![("edit".to_owned(), "src/store/sqlite.rs".to_owned())],
        "the attempted edit is recorded once, repository-relative"
    );
    // stderr says nothing on the happy path
    assert!(run.stderr().trim().is_empty(), "{}", run.stderr());
}

/// Negative (M4-7): a path outside the repository is skipped and the hook has nothing to say:
/// `{}` and exit 0, no row.
#[test]
fn pre_tool_use_outside_the_repository_prints_an_empty_document() {
    let s = HookScratch::new("pre-outside");
    let api = s.api();
    accepted_decision(&api, "Use WAL mode", &["**"], None);
    drop(api);
    let mut env = fixture("claude-code", "pre-tool-use-edit");
    set(&mut env, "/cwd", json!(s.repo().to_str().unwrap()));
    set(&mut env, "/tool_input/file_path", json!("/etc/passwd"));
    let run = hook(&s, "pre-tool-use", None, &env);
    assert_eq!(run.code(), Some(0), "{}", run.stderr());
    assert_eq!(
        run.json(),
        Value::Object(serde_json::Map::new()),
        "{}",
        run.stdout()
    );
    assert_eq!(
        session_rows(&s, "sess-claude-0001"),
        Vec::<(String, String)>::new()
    );
}

/// Plan slice: the briefing is capped at 5 items (core-spec F8; M4-17): accepted Decisions by
/// importance, newest first, plus the workspace's pending Failures; a seventh record is left
/// out, a proposed Decision is never briefed, and the output is `hookSpecificOutput` for
/// `SessionStart`.
#[test]
fn session_start_briefing_is_capped_at_five_items_by_importance_with_pending_failures() {
    let s = HookScratch::new("brief");
    let api = s.api();
    let mut ids = Vec::new();
    for (i, importance) in [0.9, 0.1, 0.8, 0.5, 0.7, 0.6].iter().enumerate() {
        ids.push(accepted_decision(
            &api,
            &format!("decision number {i} importance {importance}"),
            &["src/**"],
            Some(*importance),
        ));
    }
    let proposed = api
        .remember(kaban_api::RememberRequest {
            kind: "Decision".into(),
            text: "a proposed decision stays out of the briefing".into(),
            props: Some(json!({"rationale": "not yet"})),
            importance: Some(1.0),
            ..Default::default()
        })
        .unwrap()
        .id;
    let failure = api
        .remember(kaban_api::RememberRequest {
            kind: "Failure".into(),
            text: "\"cargo test\" failed: 1 error".into(),
            props: Some(json!({"pending_fix": true, "command": "cargo test"})),
            ..Default::default()
        })
        .unwrap()
        .id;
    drop(api);
    let mut env = fixture("claude-code", "session-start");
    set(&mut env, "/cwd", json!(s.repo().to_str().unwrap()));

    let run = hook(&s, "session-start", None, &env);

    assert_eq!(run.code(), Some(0), "{}", run.stderr());
    let out = run.json();
    assert_eq!(
        out["hookSpecificOutput"]["hookEventName"],
        json!("SessionStart"),
        "{out}"
    );
    let context = out["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .expect("additionalContext");
    let items: Vec<&str> = context.lines().filter(|l| l.starts_with("- ")).collect();
    assert_eq!(items.len(), 5, "{context}");
    // the pending Failure is one item, the four most important accepted Decisions the rest
    assert!(
        items
            .iter()
            .any(|l| l.contains(&failure) && l.contains("pending fix")),
        "{context}"
    );
    for (i, id) in ids.iter().enumerate() {
        let expected = matches!(i, 0 | 2 | 4 | 5); // 0.9, 0.8, 0.7, 0.6
        assert_eq!(
            context.contains(id),
            expected,
            "decision {i}: expected {expected}: {context}"
        );
    }
    assert!(!context.contains(&proposed), "{context}");
    let first = items[0];
    assert!(first.contains(&ids[0]), "most important first: {context}");
}

/// Plan slice: a `PostToolUseFailure` payload records a `Failure { pending_fix: true, command,
/// error_text, stack_head }` visible to `recall` by its error string, with the host session
/// id; the hook prints `{}` and exits 0.
#[test]
fn post_tool_use_failure_records_a_failure_visible_to_recall_by_its_error_string() {
    let s = HookScratch::new("fail");
    drop(s.api()); // create and migrate the database
    let mut env = fixture("claude-code", "post-tool-use-failure-bash");
    set(&mut env, "/cwd", json!(s.repo().to_str().unwrap()));

    let run = hook(&s, "post-tool-use-failure", None, &env);

    assert_eq!(run.code(), Some(0), "{}", run.stderr());
    assert_eq!(
        run.json(),
        Value::Object(serde_json::Map::new()),
        "{}",
        run.stdout()
    );
    // SS-10 permits this fixed notice while a committed write awaits search visibility.
    let stderr = run.stderr();
    assert!(
        stderr.trim().is_empty()
            || stderr.trim()
                == "kaban hook post-tool-use-failure: stored; not yet searchable, do not replay",
        "{stderr}"
    );
    let api = s.api();
    let hits = api
        .recall(kaban_api::RecallRequest {
            // the note text carries the command's first line and the last error line
            query: "nightjar-store FAILED".into(),
            full_text: true,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(hits.results.len(), 1, "{hits:?}");
    let card = &hits.results[0];
    assert_eq!(card.kind, "Failure");
    let text = card.text.as_deref().unwrap_or("");
    assert!(
        text.starts_with("\"cargo test -p nightjar-store --lib\" failed: "),
        "{text}"
    );
    assert!(
        text.contains("1 error"),
        "the last non-empty error line: {text}"
    );
    let props = card.props.as_ref().expect("props");
    assert_eq!(props["pending_fix"], json!(true), "{props}");
    assert_eq!(
        props["command"],
        json!("cargo test -p nightjar-store --lib")
    );
    assert!(
        props["stack_head"]
            .as_str()
            .unwrap()
            .starts_with("error[E0599]"),
        "{props}"
    );
    assert!(
        props["error_text"].as_str().unwrap().ends_with("1 error"),
        "{props}"
    );
    assert_eq!(
        card.provenance.session_id.as_deref(),
        Some("sess-claude-0003")
    );
}

/// Plan slice (S1, M4-10): a payload with a credential in `error_text` is rejected before
/// anything is stored; the hook still exits 0 (never blocks) with a stderr note, prints `{}`,
/// and the canary reaches neither stdout, stderr nor the database.
#[test]
fn a_secret_in_the_error_text_is_rejected_the_hook_exits_0_and_nothing_is_stored() {
    let s = HookScratch::new("secret");
    drop(s.api());
    let mut env = fixture("claude-code", "post-tool-use-failure-bash");
    set(&mut env, "/cwd", json!(s.repo().to_str().unwrap()));
    set(
        &mut env,
        "/error",
        json!(format!(
            "error: aws rejected the key {CANARY}\nexit status 1"
        )),
    );
    let nodes_before: i64 = s
        .raw()
        .query_row("SELECT count(*) FROM node", [], |r| r.get(0))
        .unwrap();

    let run = hook(&s, "post-tool-use-failure", None, &env);

    assert_eq!(run.code(), Some(0), "{}", run.stderr());
    assert_eq!(
        run.json(),
        Value::Object(serde_json::Map::new()),
        "{}",
        run.stdout()
    );
    assert!(
        run.stderr().contains("refused by the denylist policy"),
        "{}",
        run.stderr()
    );
    assert!(!run.stdout().contains(CANARY) && !run.stderr().contains(CANARY));
    assert!(!String::from_utf8_lossy(&s.db_bytes()).contains(CANARY));
    let nodes_after: i64 = s
        .raw()
        .query_row("SELECT count(*) FROM node", [], |r| r.get(0))
        .unwrap();
    assert_eq!(nodes_after, nodes_before, "nothing stored");
    // the rejection itself is audited (fail-closed), with rule ids only
    let audit: i64 = s
        .raw()
        .query_row("SELECT count(*) FROM audit WHERE op = 'reject'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert!(audit >= 1);
}

/// Plan slice: an unknown extra field anywhere in the envelope is ignored (M4-7: allowlisted
/// intake), including one that carries a credential, since it is never read.
#[test]
fn unknown_envelope_fields_are_ignored_even_when_they_carry_a_credential() {
    let s = HookScratch::new("unknown");
    drop(s.api());
    let mut env = fixture("claude-code", "post-tool-use-failure-bash");
    set(&mut env, "/cwd", json!(s.repo().to_str().unwrap()));
    env["host_added_field"] = json!({"nested": CANARY, "count": 3});
    env["tool_input"]["unexpected"] = json!(CANARY);

    let run = hook(&s, "post-tool-use-failure", None, &env);

    assert_eq!(run.code(), Some(0), "{}", run.stderr());
    // SS-10 permits this fixed notice while a committed write awaits search visibility.
    let stderr = run.stderr();
    assert!(
        stderr.trim().is_empty()
            || stderr.trim()
                == "kaban hook post-tool-use-failure: stored; not yet searchable, do not replay",
        "{stderr}"
    );
    let failures: i64 = s
        .raw()
        .query_row(
            "SELECT count(*) FROM node WHERE kind = 'Failure'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(failures, 1, "the failure is still recorded");
    assert!(!String::from_utf8_lossy(&s.db_bytes()).contains(CANARY));
}

/// M4-6 negative: a hook fired where no database exists prints `{}`, exits 0, says so on
/// stderr and creates nothing.
#[test]
fn a_hook_without_a_database_prints_an_empty_document_and_creates_nothing() {
    let s = HookScratch::new("nodb");
    let mut env = fixture("claude-code", "session-start");
    set(&mut env, "/cwd", json!(s.repo().to_str().unwrap()));
    let run = hook(&s, "session-start", None, &env);
    assert_eq!(run.code(), Some(0), "{}", run.stderr());
    assert_eq!(run.json(), Value::Object(serde_json::Map::new()));
    assert!(run.stderr().contains("does not exist"), "{}", run.stderr());
    assert!(!s.db().exists(), "a hook must never create the database");
    // R4-2: the parent directory is writable, so a creating open would have succeeded here
    assert!(
        !std::fs::metadata(&s.0).unwrap().permissions().readonly(),
        "the scratch directory is writable: a creating open would succeed here"
    );
    assert!(
        !sidecar(&s.db(), "-wal").exists() && !sidecar(&s.db(), "-shm").exists(),
        "nothing at all was created in the writable directory"
    );
}

/// R4-2: a database path that is a symlink is refused like a missing one (`{}`, exit 0, one
/// fixed stderr line), nothing is created and the link target is neither tightened nor
/// written: the hook opens regular files only.
#[test]
fn a_hook_on_a_symlinked_database_path_prints_an_empty_document_and_leaves_the_target_alone() {
    use std::os::unix::fs::PermissionsExt;
    let s = HookScratch::new("symlink");
    let target = s.0.join("elsewhere.db");
    std::fs::write(&target, b"not a kaban database").unwrap();
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644)).unwrap();
    std::os::unix::fs::symlink(&target, s.db()).unwrap();
    let mut env = fixture("claude-code", "session-start");
    set(&mut env, "/cwd", json!(s.repo().to_str().unwrap()));

    let run = hook(&s, "session-start", None, &env);

    assert_eq!(run.code(), Some(0), "{}", run.stderr());
    assert_eq!(run.json(), Value::Object(serde_json::Map::new()));
    assert_eq!(run.stderr().lines().count(), 1, "{}", run.stderr());
    assert!(
        run.stderr().contains("database not usable by a hook"),
        "{}",
        run.stderr()
    );
    assert_eq!(mode(&target), 0o644, "the link target was not tightened");
    assert_eq!(
        std::fs::read(&target).unwrap(),
        b"not a kaban database",
        "the link target was not opened as a database"
    );
    assert!(
        !sidecar(&s.db(), "-wal").exists() && !sidecar(&target, "-wal").exists(),
        "nothing was created next to the link or its target"
    );
}

fn mode(path: &std::path::Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn sidecar(db: &std::path::Path, suffix: &str) -> std::path::PathBuf {
    let mut s = db.as_os_str().to_owned();
    s.push(suffix);
    std::path::PathBuf::from(s)
}

/// R4-2: the hook open is existing-only and best-effort: it never tightens the database or
/// its `-wal`/`-shm` sidecars (that is the server's job at its own open, C1.6), so loose modes
/// an operator left behind read the same after a hook call, which still answers normally: the
/// briefing names the seeded Decision and stderr is empty (RL-3: `{}` after a refusal would
/// not pass).
#[test]
fn a_hook_leaves_loose_database_and_sidecar_modes_exactly_as_it_found_them() {
    use std::os::unix::fs::PermissionsExt;
    const SEEDED: &str = "Loose modes are reported, never repaired by a hook";
    // Arrange: a live holder (the server) keeps the WAL sidecars on disk during the hook
    let s = HookScratch::new("loose-modes");
    let holder = s.api();
    let seeded = accepted_decision(&holder, SEEDED, &["**"], None);
    let (wal, shm) = (sidecar(&s.db(), "-wal"), sidecar(&s.db(), "-shm"));
    assert!(wal.exists() && shm.exists(), "WAL sidecars expected");
    for p in [&s.db(), &wal, &shm] {
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o644)).unwrap();
    }
    let mut env = fixture("claude-code", "session-start");
    set(&mut env, "/cwd", json!(s.repo().to_str().unwrap()));

    // Act
    let run = hook(&s, "session-start", None, &env);

    // Assert: the hook answered with the briefing (exit 0, the seeded Decision in
    // `additionalContext`, nothing on stderr) and touched no mode
    assert_eq!(run.code(), Some(0), "{}", run.stderr());
    let out = run.json();
    let context = out["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap_or_else(|| panic!("a usable hook briefs; got {out}"));
    assert!(
        context.contains(&seeded) && context.contains(SEEDED),
        "the briefing names the seeded Decision: {context}"
    );
    assert!(run.stderr().trim().is_empty(), "{}", run.stderr());
    assert_eq!(
        (mode(&s.db()), mode(&wal), mode(&shm)),
        (0o644, 0o644, 0o644),
        "a hook must not tighten the files it opens"
    );
    drop(holder);
}

/// RL-4: an existing `-wal` sidecar that is a symlink is refused like a symlinked database
/// path (`{}`, exit 0, one fixed stderr line); the link target is neither read as a WAL nor
/// written, nothing is created, and the database bytes stay as they were.
#[test]
fn a_hook_beside_a_symlinked_wal_sidecar_prints_an_empty_document_and_leaves_the_target_alone() {
    use std::os::unix::fs::PermissionsExt;
    // Arrange: a real database, closed cleanly (no sidecars), then a symlinked -wal beside it
    let s = HookScratch::new("wal-symlink");
    {
        let api = s.api();
        accepted_decision(&api, "A symlinked WAL is refused", &["**"], None);
    }
    let wal = sidecar(&s.db(), "-wal");
    assert!(!wal.exists(), "fixture: no -wal after a clean close");
    let db_before = std::fs::read(s.db()).unwrap();
    let decoy = s.0.join("decoy-wal");
    std::fs::write(&decoy, b"decoy bytes, not a WAL").unwrap();
    std::fs::set_permissions(&decoy, std::fs::Permissions::from_mode(0o644)).unwrap();
    std::os::unix::fs::symlink(&decoy, &wal).unwrap();
    let mut env = fixture("claude-code", "session-start");
    set(&mut env, "/cwd", json!(s.repo().to_str().unwrap()));

    // Act
    let run = hook(&s, "session-start", None, &env);

    // Assert
    assert_eq!(run.code(), Some(0), "{}", run.stderr());
    assert_eq!(
        run.json(),
        Value::Object(serde_json::Map::new()),
        "{}",
        run.stdout()
    );
    assert_eq!(run.stderr().lines().count(), 1, "{}", run.stderr());
    // the file-contract refusal (`FileMode`, its fixed class text), not SQLite failing on the
    // decoy after the link was followed (`storage backend error`)
    assert!(
        run.stderr().contains("database not usable by a hook")
            && run
                .stderr()
                .contains("could not be created or made owner-only"),
        "{}",
        run.stderr()
    );
    assert_eq!(
        std::fs::read(&decoy).unwrap(),
        b"decoy bytes, not a WAL",
        "the -wal link target was not written"
    );
    assert_eq!(
        mode(&decoy),
        0o644,
        "the -wal link target was not tightened"
    );
    assert_eq!(
        std::fs::read(s.db()).unwrap(),
        db_before,
        "the database was not opened: its bytes are unchanged"
    );
    assert!(
        !sidecar(&s.db(), "-shm").exists(),
        "nothing was created beside the refused database"
    );
}

/// M4-9, M4-29: when the handler is slow, the watchdog prints `{}` and the process exits 0
/// well under the 5 s ceiling (the host budget is 2 s). The stall is the debug-only test seam
/// `KABAN_HOOK_STALL_MS`.
#[test]
fn a_slow_handler_is_answered_by_the_watchdog_with_an_empty_document_and_exit_0() {
    let s = HookScratch::new("watchdog");
    drop(s.api());
    let mut env = fixture("claude-code", "session-start");
    set(&mut env, "/cwd", json!(s.repo().to_str().unwrap()));
    let run = hook_common::hook_with_env(
        &s,
        "session-start",
        None,
        &env,
        &[("KABAN_HOOK_STALL_MS", "20000")],
    );
    assert_eq!(run.code(), Some(0), "{}", run.stderr());
    assert_eq!(
        run.json(),
        Value::Object(serde_json::Map::new()),
        "{}",
        run.stdout()
    );
    assert!(
        run.elapsed < std::time::Duration::from_secs(5),
        "took {:?}",
        run.elapsed
    );
}

/// M4-31 (settled live, `HOST-EVENTS.md` §1): `SubagentStop` carries `last_assistant_message`,
/// which becomes an `Episode` with the session id; a payload without it records nothing.
#[test]
fn subagent_stop_records_an_episode_from_last_assistant_message() {
    let s = HookScratch::new("subagent");
    drop(s.api());
    let mut env = fixture("claude-code", "subagent-stop");
    set(&mut env, "/cwd", json!(s.repo().to_str().unwrap()));
    let run = hook(&s, "subagent-stop", None, &env);
    assert_eq!(run.code(), Some(0), "{}", run.stderr());
    assert_eq!(run.json(), Value::Object(serde_json::Map::new()));
    let episodes: Vec<(String, Option<String>)> = {
        use kaban_core::IngestStore;
        let store = kaban_store_sqlite::SqliteStore::open(&s.db(), &s.keys()).unwrap();
        store
            .list_nodes("Episode", hook_common::REPO, 0, 10)
            .unwrap()
            .into_iter()
            .map(|n| (n.text, n.session_id))
            .collect()
    };
    assert_eq!(
        episodes,
        vec![(
            "Reviewed the store module: the WAL retry loop needs a bound on its backoff."
                .to_owned(),
            Some("sess-claude-0005".to_owned())
        )]
    );
    let mut silent = env.clone();
    silent
        .as_object_mut()
        .unwrap()
        .remove("last_assistant_message");
    set(&mut silent, "/session_id", json!("sess-claude-0006"));
    assert_eq!(hook(&s, "subagent-stop", None, &silent).code(), Some(0));
    let count: i64 = s
        .raw()
        .query_row(
            "SELECT count(*) FROM node WHERE kind = 'Episode'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 1, "no summary field, no Episode");
}
