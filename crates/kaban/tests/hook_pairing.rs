//! Failure -> fix pairing (core-spec F8b; core-plan C4.4 as amended by
//! `reviews/m4-prep-dispositions.md` M4-10 to M4-16), black-box through `kaban hook` with the
//! Claude Code fixtures: a later success of the same command is evidence with the files edited
//! in between, `hook fix` clears `pending_fix` and links `FIXED_BY`, the Stop prompt is asked once
//! per session and an MCP `link FIXED_BY` counts as resolution.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod hook_common;

use hook_common::{HookScratch, fixture, hook, session_rows, set};
use kaban_core::{MemoryStore, NodeId};
use kaban_store_sqlite::SqliteStore;
use serde_json::{Value, json};

const SESSION: &str = "sess-claude-0003";
const COMMAND: &str = "cargo test -p nightjar-store --lib";

fn in_repo(s: &HookScratch, host: &str, name: &str) -> Value {
    let mut env = fixture(host, name);
    set(&mut env, "/cwd", json!(s.repo().to_str().unwrap()));
    env
}

/// The one Failure node of the scratch database.
fn the_failure(s: &HookScratch) -> kaban_core::Node {
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let id: String = s
        .raw()
        .query_row(
            "SELECT id FROM node WHERE kind = 'Failure' AND lifecycle = 'active'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    store
        .get(&NodeId::parse(&id).unwrap(), false)
        .unwrap()
        .unwrap()
}

fn is_hex64(s: &str) -> bool {
    s.len() == 64
        && s.chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}

/// Plan slice 1: a failed command, an edit, then the same command succeeding: the Failure
/// carries a keyed `command_fingerprint` (M4-11: HMAC with the audit key and the
/// `kaban-cmd/v1` domain, never a bare sha256) and gains one `evidence_runs` entry listing the
/// file edited in between; a success of another command adds nothing; `pending_fix` stays.
#[test]
fn a_later_success_of_the_same_command_records_evidence_with_the_files_edited_since() {
    // Arrange
    let s = HookScratch::new("pair-evidence");
    drop(s.api());
    let failure_env = in_repo(&s, "claude-code", "post-tool-use-failure-bash");
    let mut edit_env = in_repo(&s, "claude-code", "pre-tool-use-edit");
    set(&mut edit_env, "/session_id", json!(SESSION));
    set(
        &mut edit_env,
        "/tool_input/file_path",
        json!(s.repo().join("src/store/sqlite.rs").to_str().unwrap()),
    );
    let success_env = in_repo(&s, "claude-code", "post-tool-use-bash");
    let mut other_env = success_env.clone();
    set(
        &mut other_env,
        "/tool_input/command",
        json!("cargo fmt --all"),
    );

    // Act
    for (event, env) in [
        ("post-tool-use-failure", &failure_env),
        ("pre-tool-use", &edit_env),
        ("post-tool-use", &other_env),
        ("post-tool-use", &success_env),
    ] {
        let run = hook(&s, event, None, env);
        assert_eq!(run.code(), Some(0), "{event}: {}", run.stderr());
        assert!(run.stderr().trim().is_empty(), "{event}: {}", run.stderr());
    }

    // Assert
    let failure = the_failure(&s);
    let props = &failure.props;
    let fingerprint = props["command_fingerprint"].as_str().expect("fingerprint");
    assert!(is_hex64(fingerprint), "{fingerprint}");
    let normalized = COMMAND; // already trimmed, single spaces
    assert_ne!(
        fingerprint,
        kaban_policy::sha256_hex(normalized.as_bytes()),
        "a bare sha256 would let anyone confirm a guess (S1, M4-11)"
    );
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    assert_eq!(
        fingerprint,
        store
            .signer()
            .hmac_hex(format!("kaban-cmd/v1\0{normalized}").as_bytes()),
        "HMAC(audit key, domain + normalized command)"
    );
    let runs = props["evidence_runs"].as_array().expect("evidence_runs");
    assert_eq!(runs.len(), 1, "exactly one matching success: {props}");
    assert_eq!(
        runs[0]["files_edited_since"],
        json!(["src/store/sqlite.rs"])
    );
    assert!(runs[0]["ts"].as_i64().unwrap() > 1_793_004_407);
    assert_eq!(
        props["pending_fix"],
        json!(true),
        "evidence is not a conclusion"
    );
    // the failure row that pairs the Failure with the session (M4-14), then the edit
    assert_eq!(
        session_rows(&s, SESSION),
        vec![
            ("failure".to_owned(), failure.id.clone()),
            ("edit".to_owned(), "src/store/sqlite.rs".to_owned()),
        ]
    );
}

/// `FIXED_BY` edges out of `id`: `(rel, destination kind, destination text)`.
fn fixed_by(s: &HookScratch, id: &str) -> Vec<(String, String, String)> {
    let conn = s.raw();
    let mut st = conn
        .prepare(
            "SELECT e.rel, d.kind, kaban_body(d.text) FROM edge e
             JOIN node src ON src.rid = e.src JOIN node d ON d.rid = e.dst
             WHERE src.id = ? AND e.rel = 'FIXED_BY' ORDER BY e.id",
        )
        .unwrap();
    st.query_map([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

/// Plan slice 2: `kaban hook fix --stdin-json {"failure_id","fix"}` stores the fix text,
/// links the Failure `FIXED_BY` a new `Improvement` and clears `pending_fix` (M4-15); the
/// Failure then leaves the pending set, so a Stop afterwards has nothing to ask. Negatives: an
/// unknown id and a credential in the fix text store nothing and exit 0.
#[test]
fn hook_fix_sets_the_fix_text_links_fixed_by_to_an_improvement_and_clears_pending() {
    let s = HookScratch::new("pair-fix");
    drop(s.api());
    let failure_env = in_repo(&s, "claude-code", "post-tool-use-failure-bash");
    assert_eq!(
        hook(&s, "post-tool-use-failure", None, &failure_env).code(),
        Some(0)
    );
    let failure = the_failure(&s);

    // negatives first: nothing changes
    let unknown = json!({"failure_id": "01a17a03-5cbc-7000-8000-000000000000", "fix": "renamed"});
    let run = hook(&s, "fix", None, &unknown);
    assert_eq!(run.code(), Some(0), "{}", run.stderr());
    assert_eq!(
        run.json(),
        Value::Object(serde_json::Map::new()),
        "{}",
        run.stdout()
    );
    assert!(run.stderr().contains("kaban hook fix:"), "{}", run.stderr());
    let secret = json!({"failure_id": failure.id, "fix": "set the key to AKIAQWERTYUIOPASDFGH"});
    let run = hook(&s, "fix", None, &secret);
    assert_eq!(run.code(), Some(0));
    assert!(
        run.stderr().contains("refused by the denylist policy"),
        "{}",
        run.stderr()
    );
    assert_eq!(
        fixed_by(&s, &failure.id).len(),
        0,
        "no edge after the negatives"
    );
    assert_eq!(the_failure(&s).props["pending_fix"], json!(true));

    // Act
    let reply = json!({
        "session_id": SESSION,
        "failure_id": failure.id,
        "fix": "renamed open_v2 back to open; the method was never added",
    });
    let run = hook(&s, "fix", None, &reply);

    // Assert
    assert_eq!(run.code(), Some(0), "{}", run.stderr());
    assert!(run.stderr().trim().is_empty(), "{}", run.stderr());
    let out = run.json();
    assert_eq!(out["status"], json!("fixed"), "{out}");
    assert_eq!(out["failure_id"], json!(failure.id));
    let improvement = out["improvement_id"].as_str().expect("improvement_id");
    assert!(NodeId::parse(improvement).is_ok(), "{out}");
    let fixed = the_failure(&s);
    assert_eq!(fixed.props["pending_fix"], json!(false), "{}", fixed.props);
    assert_eq!(
        fixed.props["fix"],
        json!("renamed open_v2 back to open; the method was never added")
    );
    assert_eq!(
        fixed.rev,
        failure.rev + 1,
        "one props update bumps rev once"
    );
    let edges = fixed_by(&s, &failure.id);
    assert_eq!(edges.len(), 1, "{edges:?}");
    assert_eq!(edges[0].1, "Improvement");
    assert_eq!(
        edges[0].2,
        "renamed open_v2 back to open; the method was never added"
    );
    // resolved: the Stop hook has nothing to ask
    let stop_env = in_repo(&s, "claude-code", "stop");
    let run = hook(&s, "stop", None, &stop_env);
    assert_eq!(run.code(), Some(0));
    assert_eq!(
        run.json(),
        Value::Object(serde_json::Map::new()),
        "{}",
        run.stdout()
    );
}

/// Plan slice 3 (M4-16): with a pending Failure in the session, `stop` prints the one-time
/// prompt as `{"decision":"block","reason":…}` built from the projected Failure (its id, both
/// reply routes), exit 0, never exit 2; the second `stop` of the same session prints `{}`
/// (the `prompted` marker row); a Stop with Claude's own `stop_hook_active: true` also prints
/// `{}`; another session with no pending Failure prints `{}`.
#[test]
fn stop_prompts_once_per_session_with_the_failure_id_and_both_reply_routes() {
    let s = HookScratch::new("pair-stop");
    drop(s.api());
    let failure_env = in_repo(&s, "claude-code", "post-tool-use-failure-bash");
    assert_eq!(
        hook(&s, "post-tool-use-failure", None, &failure_env).code(),
        Some(0)
    );
    let failure = the_failure(&s);
    let stop_env = in_repo(&s, "claude-code", "stop");

    // first Stop: the prompt
    let run = hook(&s, "stop", None, &stop_env);
    assert_eq!(run.code(), Some(0), "{}", run.stderr());
    let out = run.json();
    assert_eq!(out["decision"], json!("block"), "{out}");
    let reason = out["reason"].as_str().expect("reason");
    assert!(reason.starts_with("kaban: how was "), "{reason}");
    assert!(reason.contains(&failure.id), "{reason}");
    assert!(
        reason.contains("cargo test -p nightjar-store --lib"),
        "{reason}"
    );
    assert!(
        reason.contains("kaban hook fix") && reason.contains("FIXED_BY"),
        "{reason}"
    );
    assert!(reason.contains("unresolved"), "{reason}");
    assert_eq!(
        session_rows(&s, SESSION)
            .into_iter()
            .filter(|(k, _)| k == "prompted")
            .count(),
        1
    );

    // second Stop of the same session: silent, still pending
    let run = hook(&s, "stop", None, &stop_env);
    assert_eq!(run.code(), Some(0), "{}", run.stderr());
    assert_eq!(
        run.json(),
        Value::Object(serde_json::Map::new()),
        "{}",
        run.stdout()
    );
    assert_eq!(the_failure(&s).props["pending_fix"], json!(true));

    // Claude's own loop guard is honoured too
    let mut active = in_repo(&s, "claude-code", "stop");
    set(&mut active, "/session_id", json!("sess-claude-0099"));
    set(&mut active, "/stop_hook_active", json!(true));
    let run = hook(&s, "stop", None, &active);
    assert_eq!(
        run.json(),
        Value::Object(serde_json::Map::new()),
        "{}",
        run.stdout()
    );

    // a session without pending Failures is never prompted
    let mut other = in_repo(&s, "claude-code", "stop");
    set(&mut other, "/session_id", json!("sess-claude-0042"));
    let run = hook(&s, "stop", None, &other);
    assert_eq!(run.code(), Some(0));
    assert_eq!(
        run.json(),
        Value::Object(serde_json::Map::new()),
        "{}",
        run.stdout()
    );
    assert_eq!(
        session_rows(&s, "sess-claude-0042").len(),
        0,
        "no marker without a prompt"
    );
}

/// M4-14: a `FIXED_BY` edge made through the ordinary `link` tool (as the MCP adapter would)
/// resolves the Failure without touching its props: the Stop prompt is not shown, the
/// briefing no longer lists it, and a Failure fixed through props only (`pending_fix: false`
/// without an edge) is resolved as well.
#[test]
fn a_fixed_by_link_made_through_the_link_tool_counts_as_resolution() {
    let s = HookScratch::new("pair-link");
    drop(s.api());
    let failure_env = in_repo(&s, "claude-code", "post-tool-use-failure-bash");
    assert_eq!(
        hook(&s, "post-tool-use-failure", None, &failure_env).code(),
        Some(0)
    );
    let failure = the_failure(&s);
    let api = s.api();
    let improvement = api
        .remember(kaban_api::RememberRequest {
            kind: "Improvement".into(),
            text: "added the missing method".into(),
            ..Default::default()
        })
        .unwrap()
        .id;
    api.link(kaban_api::LinkRequest {
        from: failure.id.clone(),
        rel: "FIXED_BY".into(),
        to: improvement,
        confidence: None,
        props: None,
    })
    .unwrap();
    drop(api);
    assert_eq!(
        the_failure(&s).props["pending_fix"],
        json!(true),
        "props untouched"
    );

    let run = hook(&s, "stop", None, &in_repo(&s, "claude-code", "stop"));
    assert_eq!(run.code(), Some(0), "{}", run.stderr());
    assert_eq!(
        run.json(),
        Value::Object(serde_json::Map::new()),
        "{}",
        run.stdout()
    );
    let run = hook(
        &s,
        "session-start",
        None,
        &in_repo(&s, "claude-code", "session-start"),
    );
    assert_eq!(run.code(), Some(0));
    assert!(!run.stdout().contains(&failure.id), "{}", run.stdout());
}
