//! The C4.1b tool subcommands `remember`, `why`, `recall` and `forget` (core-spec F7, §6, S1;
//! core-plan C4.1 as amended by M4-3, M4-4, M4-5, M4-20), black-box through the binary: every
//! exit code of core-spec §6 reproduced once (a denylist rejection 2 with the value never
//! echoed, an unknown id 3, a policy, request or usage error 4, a read-only database directory
//! 5), one JSON document on stdout per subcommand in both layouts (pretty by default, compact
//! with `--json`), the `--stdin-json` body alternative, and clap's fixed texts for the new flags.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::Scratch;
use serde_json::Value;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output, Stdio};

/// Hits the builtin credential rule `aws-access-key-id` (always reject) in every view.
const SECRET_CANARY: &str = "AKIAQWERTYUIOPASDFGH";
/// A string no policy rule matches: its absence proves no code path formatted the value.
const ARG_CANARY: &str = "CANARY-cli-value-9c2d";
const UNKNOWN_ID: &str = "01a17a03-5cbc-7000-8000-000000000000";
const WORKSPACE: &str = "default";

fn kaban(s: &Scratch) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_kaban"));
    cmd.env("KABAN_HOME", s.home())
        .env("RUST_LOG", "warn")
        .args(["--db", s.db().to_str().unwrap(), "--workspace", WORKSPACE])
        .stdin(Stdio::null());
    cmd
}

fn run(s: &Scratch, args: &[&str]) -> Output {
    kaban(s).args(args).output().unwrap()
}

fn with_stdin(s: &Scratch, args: &[&str], body: &[u8]) -> Output {
    let mut child = kaban(s)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // a subcommand that refuses the body (flags and `--stdin-json` together) may exit before
    // reading stdin; the resulting EPIPE on our side is not a failure of the thing under test
    let _ = child.stdin.take().unwrap().write_all(body);
    child.wait_with_output().unwrap()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn text_of(out: &Output) -> String {
    format!("{}{}", String::from_utf8_lossy(&out.stdout), stderr(out))
}

/// stdout of a successful run as its one JSON document.
fn document(out: &Output) -> Value {
    assert_eq!(out.status.code(), Some(0), "{}", stderr(out));
    let text = String::from_utf8(out.stdout.clone()).unwrap();
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("one JSON document: {e}: {text}"))
}

/// `value` with every `checked_at` leaf removed: `why` stamps each currency with the time of
/// the call, which differs between two runs of the same request.
fn without_timestamps(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .filter(|(key, _)| key.as_str() != "checked_at")
                .map(|(key, v)| (key.clone(), without_timestamps(v)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(without_timestamps).collect()),
        other => other.clone(),
    }
}

/// Run `args` pretty (default) and compact (`--json`): the compact output is one line, the
/// pretty output more than one, and both parse to the same document (timestamps aside), which
/// is returned.
fn both_layouts(s: &Scratch, args: &[&str]) -> Value {
    let pretty = run(s, args);
    let value = document(&pretty);
    assert!(
        String::from_utf8_lossy(&pretty.stdout).lines().count() > 1,
        "{args:?}: pretty by default"
    );
    let compact = kaban(s).arg("--json").args(args).output().unwrap();
    let again = document(&compact);
    assert_eq!(
        String::from_utf8_lossy(&compact.stdout).lines().count(),
        1,
        "{args:?}: --json is one line"
    );
    assert_eq!(
        without_timestamps(&again),
        without_timestamps(&value),
        "{args:?}: the same document in both layouts"
    );
    value
}

fn count_nodes(s: &Scratch, kind: &str) -> i64 {
    let conn = rusqlite::Connection::open(s.db()).unwrap();
    conn.query_row("SELECT count(*) FROM node WHERE kind = ?", [kind], |r| {
        r.get(0)
    })
    .unwrap()
}

#[test]
fn remember_why_recall_and_forget_print_one_json_document_in_both_layouts() {
    let s = Scratch::new("kaban-cli", "documents");
    // remember: a Decision with a path scope and typed props from the flags (pretty), and the
    // same request again (compact): a duplicate is reported, not stored twice
    let decision_args = [
        "remember",
        "Decision",
        "store files under src/store use WAL mode",
        "--path-scope",
        "src/store/**",
        "--props",
        r#"{"rationale": "durability under concurrent readers"}"#,
        "--tag",
        "storage",
        "--importance",
        "0.9",
    ];
    let created = document(&run(&s, &decision_args));
    assert_eq!(created["status"], "created", "{created}");
    assert_eq!(created["kind"], "Decision");
    let id = created["id"].as_str().unwrap().to_owned();
    let again = kaban(&s)
        .arg("--json")
        .args(decision_args)
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&again.stdout).lines().count(),
        1,
        "--json is one line"
    );
    let existing = document(&again);
    assert_eq!(existing["status"], "existing", "{existing}");
    assert_eq!(existing["id"], id.as_str());
    assert_eq!(count_nodes(&s, "Decision"), 1);
    // a second record of another kind, from the --stdin-json body (the wire object)
    let fact = document(&with_stdin(
        &s,
        &["remember", "--stdin-json"],
        br#"{"kind": "Fact", "text": "the build needs two jobs", "tags": ["build"]}"#,
    ));
    assert_eq!(fact["status"], "created", "{fact}");
    let fact_id = fact["id"].as_str().unwrap().to_owned();

    // why: by path from the flags, and by the same target from a body
    let why = both_layouts(&s, &["why", "--path", "src/store/sqlite.rs"]);
    let results = why["results"].as_array().unwrap();
    assert_eq!(results.len(), 1, "{why}");
    assert_eq!(results[0]["node"]["id"], id.as_str());
    assert_eq!(results[0]["matched_by"], "path_scope");
    let why_body = document(&with_stdin(
        &s,
        &["why", "--stdin-json"],
        br#"{"targets": [{"path": "src/store/sqlite.rs"}], "limit": 5}"#,
    ));
    assert_eq!(
        without_timestamps(&why_body["results"]),
        without_timestamps(&why["results"])
    );

    // recall: a query from the flags with a kind filter
    let recall = both_layouts(&s, &["recall", "WAL mode", "--kind", "Decision"]);
    let hits = recall["results"].as_array().unwrap();
    assert!(hits.iter().any(|hit| hit["id"] == id.as_str()), "{recall}");

    // forget: the default mode (pretty), then an explicit mode on the other record (compact)
    let forgotten = document(&run(&s, &["forget", &fact_id]));
    assert_eq!(forgotten["lifecycle"], "tombstoned", "{forgotten}");
    let archived = kaban(&s)
        .args(["--json", "forget", &id, "--mode", "archive"])
        .output()
        .unwrap();
    let archived = document(&archived);
    assert_eq!(archived["lifecycle"], "archived", "{archived}");
    // the tombstoned record is out of every default read
    let after = document(&run(&s, &["--json", "recall", "two jobs"]));
    assert!(
        after["results"]
            .as_array()
            .unwrap()
            .iter()
            .all(|hit| hit["id"] != fact_id.as_str()),
        "{after}"
    );
}

#[test]
fn a_denylist_hit_in_the_request_exits_2_and_the_value_is_never_echoed() {
    let s = Scratch::new("kaban-cli", "rejected");
    let out = run(
        &s,
        &[
            "remember",
            "Fact",
            &format!("the deploy key is {SECRET_CANARY}"),
        ],
    );
    let text = text_of(&out);
    assert_eq!(out.status.code(), Some(2), "rejected by policy: {text}");
    assert!(
        !text.contains(SECRET_CANARY),
        "the rejected value was echoed: {text}"
    );
    assert!(
        text.contains("aws-access-key-id") || text.contains("credential-key"),
        "the rejection names its rule ids: {text}"
    );
    assert!(out.stdout.is_empty(), "stdout stays empty on error");
    assert_eq!(count_nodes(&s, "Fact"), 0, "nothing was stored");
}

#[test]
fn an_unknown_id_exits_3() {
    let s = Scratch::new("kaban-cli", "not-found");
    let out = run(&s, &["forget", UNKNOWN_ID, "--mode", "archive"]);
    assert_eq!(out.status.code(), Some(3), "{}", text_of(&out));
    assert!(out.stdout.is_empty(), "stdout stays empty on error");
}

#[test]
fn a_policy_request_or_usage_error_exits_4_with_a_fixed_text() {
    let s = Scratch::new("kaban-cli", "invalid");
    // a policy file that does not load is a config error for every subcommand
    let policy = s.0.join("policy.yaml");
    std::fs::write(&policy, "rules:\n  - id: broken\n    nonsense: 1\n").unwrap();
    let bad_policy = kaban(&s)
        .args(["--policy", policy.to_str().unwrap(), "recall", "anything"])
        .output()
        .unwrap();
    assert_eq!(bad_policy.status.code(), Some(4), "{}", stderr(&bad_policy));
    assert!(stderr(&bad_policy).contains("loading policy"));

    // the body and the argument flags are exclusive, for each tool subcommand
    for args in [
        &["remember", "--stdin-json", "--tag", "t"][..],
        &["why", "--stdin-json", "--limit", "3"][..],
        &["recall", "--stdin-json", "--full-text"][..],
        &["forget", "--stdin-json", "--mode", "archive"][..],
    ] {
        let both = with_stdin(&s, args, b"{}");
        assert_eq!(both.status.code(), Some(4), "{args:?}: {}", stderr(&both));
        assert!(stderr(&both).contains("not both"), "{args:?}");
    }

    // a missing required argument has a fixed text pointing at --stdin-json
    for args in [&["remember", "Fact"][..], &["recall"][..], &["forget"][..]] {
        let missing = run(&s, args);
        assert_eq!(
            missing.status.code(),
            Some(4),
            "{args:?}: {}",
            stderr(&missing)
        );
        assert!(stderr(&missing).contains("--stdin-json"), "{args:?}");
    }

    // caller data that fails before or at the typed parse is never echoed: a structured flag
    // that is not JSON, a value clap cannot parse for its option, and a mode the wire shape does
    // not know (its serde text is scrubbed)
    let cases: [&[&str]; 4] = [
        &["remember", "Fact", "t", "--props", ARG_CANARY],
        &["recall", "q", "--limit", ARG_CANARY],
        &["remember", "Fact", "t", "--importance", ARG_CANARY],
        &["forget", UNKNOWN_ID, "--mode", ARG_CANARY],
    ];
    for args in cases {
        let out = run(&s, args);
        let text = text_of(&out);
        assert_eq!(out.status.code(), Some(4), "{args:?}: {text}");
        assert!(!text.contains(ARG_CANARY), "{args:?}: echoed: {text}");
        assert!(text.starts_with("error: "), "{args:?}: {text}");
        assert!(out.stdout.is_empty(), "{args:?}: stdout stays empty");
    }

    // a request the service refuses (no target) is 4 through the seam as well
    let no_target = run(&s, &["why", "--limit", "3"]);
    assert_eq!(no_target.status.code(), Some(4), "{}", stderr(&no_target));
    assert!(stderr(&no_target).contains("targets"));
}

/// Restores the directory mode on drop, so the scratch can be removed whatever the outcome.
struct RestoreMode<'a>(&'a Path);
impl Drop for RestoreMode<'_> {
    fn drop(&mut self) {
        let _ = std::fs::set_permissions(self.0, std::fs::Permissions::from_mode(0o700));
    }
}

#[test]
fn a_database_that_cannot_be_opened_for_writing_exits_5() {
    let s = Scratch::new("kaban-cli", "read-only");
    // a database that exists, with its key, created by a first run
    let seeded = run(&s, &["--json", "recall", "anything"]);
    assert_eq!(seeded.status.code(), Some(0), "{}", stderr(&seeded));
    for sidecar in ["kaban.db-wal", "kaban.db-shm"] {
        let _ = std::fs::remove_file(s.0.join(sidecar));
    }
    // M4-20: a read-only parent directory, where the WAL sidecar cannot be created; a process
    // that ignores directory modes (root) uses a symlinked database path instead, which the
    // store refuses as not a regular file: both are store errors
    let dir = s.0.clone();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).unwrap();
    let restore = RestoreMode(&dir);
    let modes_enforced = std::fs::write(dir.join("probe"), b"").is_err();
    let out = if modes_enforced {
        run(&s, &["recall", "anything"])
    } else {
        drop(restore);
        let _ = std::fs::remove_file(dir.join("probe"));
        let link = dir.join("link.db");
        std::os::unix::fs::symlink(s.db(), &link).unwrap();
        Command::new(env!("CARGO_BIN_EXE_kaban"))
            .env("KABAN_HOME", s.home())
            .env("RUST_LOG", "warn")
            .args(["--db", link.to_str().unwrap(), "recall", "anything"])
            .stdin(Stdio::null())
            .output()
            .unwrap()
    };
    let text = text_of(&out);
    assert_eq!(out.status.code(), Some(5), "store error: {text}");
    assert!(text.starts_with("error: opening database"), "{text}");
    assert!(out.stdout.is_empty(), "stdout stays empty on error");
}
