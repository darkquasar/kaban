//! `kaban hook post-commit` and the shipped git hooks (core-spec F9b, F13, §7; core-plan C5.3
//! as amended by `reviews/m5-prep.md` M5-12, M5-14, M5-15, M5-29 and `reviews/m5-prep-dispositions.md`
//! OWN-23), black-box through the binary over a real temporary git repository. The hook has no
//! envelope: git gives a post-commit hook no JSON and may leave stdin on a terminal, so the
//! handler must answer without reading stdin. It stamps `commit_sha = HEAD` on the unstamped
//! nodes of the bound workspace, once, and prints `{"stamped": n}`; exit 0 always.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod hook_common;

use hook_common::{HookScratch, REPO, accepted_decision, hook};
use kaban_api::RememberRequest;
use serde_json::json;
use std::process::Command;

const FIXED_DATE: &str = "2026-10-26T08:16:02Z";

/// Turn the scratch's fake repository (a bare `.git` directory) into a real one with one commit,
/// so `git rev-parse HEAD` answers; returns HEAD.
fn git_init(s: &HookScratch) -> String {
    git(s, &["init", "-q", "-b", "main"]);
    git(s, &["config", "user.email", "fixture@example.invalid"]);
    git(s, &["config", "user.name", "fixture"]);
    std::fs::write(s.repo().join("README.md"), "# fixture\n").unwrap();
    commit(s, "initial")
}

fn git(s: &HookScratch, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(s.repo())
        .args(args)
        .env("GIT_AUTHOR_DATE", FIXED_DATE)
        .env("GIT_COMMITTER_DATE", FIXED_DATE)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

fn commit(s: &HookScratch, msg: &str) -> String {
    git(s, &["add", "-A"]);
    git(s, &["commit", "-q", "--allow-empty", "-m", msg]);
    git(s, &["rev-parse", "HEAD"])
}

/// `(commit_sha, rev)` of a node, straight from the database.
fn stamp_of(s: &HookScratch, id: &str) -> (Option<String>, i64) {
    s.raw()
        .query_row("SELECT commit_sha, rev FROM node WHERE id = ?", [id], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap()
}

/// Plan slice (M5-14, M5-15): `post-commit` stamps `HEAD` on every unstamped node of the bound
/// workspace (the accepted Decision and the session's note), bumps their `rev`, leaves a node of
/// another workspace alone, and a second run after another commit stamps only the node created
/// since, keeping the first stamp on the others (each node is stamped once).
#[test]
fn post_commit_stamps_the_unstamped_nodes_of_the_bound_workspace_once() {
    // Arrange
    let s = HookScratch::new("post-commit");
    let head1 = git_init(&s);
    let api = s.api();
    let decision = accepted_decision(
        &api,
        "Use WAL mode for every SQLite store",
        &["src/**"],
        None,
    );
    let note = api
        .remember(RememberRequest {
            kind: "Fact".into(),
            text: "The nightly job runs at 02:00 UTC".into(),
            session_id: Some("sess-INVENTED-1".into()),
            ..Default::default()
        })
        .unwrap()
        .id;
    let elsewhere = api
        .remember(RememberRequest {
            kind: "Fact".into(),
            text: "A note of another workspace".into(),
            workspace_id: Some("other-workspace".into()),
            ..Default::default()
        })
        .unwrap()
        .id;
    drop(api);
    let (_, note_rev_before) = stamp_of(&s, &note);

    // Act
    let run = hook(&s, "post-commit", None, &json!({}));

    // Assert
    assert_eq!(run.code(), Some(0), "stderr: {}", run.stderr());
    assert_eq!(
        run.json(),
        json!({"stamped": 2}),
        "stderr: {}",
        run.stderr()
    );
    assert_eq!(stamp_of(&s, &decision).0.as_deref(), Some(head1.as_str()));
    let (note_sha, note_rev) = stamp_of(&s, &note);
    assert_eq!(note_sha.as_deref(), Some(head1.as_str()));
    assert_eq!(note_rev, note_rev_before + 1, "every write bumps rev (F13)");
    assert_eq!(
        stamp_of(&s, &elsewhere).0,
        None,
        "S6: another workspace is never touched"
    );

    // a later commit stamps only what was created since; the first stamps stay
    std::fs::write(s.repo().join("CHANGES.md"), "second\n").unwrap();
    let head2 = commit(&s, "second");
    assert_ne!(head1, head2);
    let api = s.api();
    let later = api
        .remember(RememberRequest {
            kind: "Fact".into(),
            text: "Created after the first commit".into(),
            ..Default::default()
        })
        .unwrap()
        .id;
    drop(api);
    let run = hook(&s, "post-commit", None, &json!({}));
    assert_eq!(run.code(), Some(0), "stderr: {}", run.stderr());
    assert_eq!(
        run.json(),
        json!({"stamped": 1}),
        "stderr: {}",
        run.stderr()
    );
    assert_eq!(stamp_of(&s, &later).0.as_deref(), Some(head2.as_str()));
    assert_eq!(
        stamp_of(&s, &note).0.as_deref(),
        Some(head1.as_str()),
        "stamped once"
    );
    assert_eq!(stamp_of(&s, &decision).0.as_deref(), Some(head1.as_str()));

    // nothing left: a third run stamps nothing
    let run = hook(&s, "post-commit", None, &json!({}));
    assert_eq!(run.code(), Some(0));
    assert_eq!(run.json(), json!({"stamped": 0}));
    assert_eq!(REPO, "nightjar-service", "the workspace the scratch binds");
}

/// `kaban --db <db> hook post-commit --stdin-json` run from inside the repository with `stdin`
/// as given; returns `(exit code, stdout, stderr, elapsed)`. The `--stdin-json` flag is what
/// the recipe passes; the handler ignores it.
fn post_commit_with_stdin(
    s: &HookScratch,
    stdin: std::process::Stdio,
) -> (std::process::Child, std::time::Instant) {
    let child = Command::new(env!("CARGO_BIN_EXE_kaban"))
        .env("KABAN_HOME", s.home())
        .env("RUST_LOG", "warn")
        .env_remove("KABAN_WORKSPACE")
        .current_dir(s.repo())
        .args([
            "--db",
            s.db().to_str().unwrap(),
            "--json",
            "hook",
            "post-commit",
            "--stdin-json",
        ])
        .stdin(stdin)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    (child, std::time::Instant::now())
}

/// Plan slice (M5-14): git gives a post-commit hook no envelope and may leave stdin on a terminal.
/// With stdin closed (`</dev/null`, the recipe's redirection) and with a pipe whose writer never
/// sends EOF (a terminal-like stdin), the hook answers `{"stamped": n}` well under the host
/// budget instead of waiting for an envelope until the watchdog prints `{}`.
#[test]
fn post_commit_answers_without_an_envelope_with_stdin_closed_or_never_closing() {
    // Arrange
    let s = HookScratch::new("post-commit-stdin");
    let head = git_init(&s);
    let api = s.api();
    let first = accepted_decision(&api, "Decisions live in .kaban", &["docs/**"], None);
    drop(api);

    // Act: stdin closed
    let (child, started) = post_commit_with_stdin(&s, std::process::Stdio::null());
    let out = child.wait_with_output().unwrap();
    let elapsed = started.elapsed();

    // Assert
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert_eq!(out.status.code(), Some(0), "stderr: {stderr}");
    let doc: serde_json::Value =
        serde_json::from_str(stdout.trim()).unwrap_or_else(|e| panic!("{e}: {stdout:?}"));
    assert_eq!(doc, json!({"stamped": 1}), "stderr: {stderr}");
    assert!(elapsed < std::time::Duration::from_secs(5), "{elapsed:?}");
    assert_eq!(stamp_of(&s, &first).0.as_deref(), Some(head.as_str()));

    // Arrange: a second node, and a pipe the test keeps open (never EOF)
    let api = s.api();
    let second = api
        .remember(RememberRequest {
            kind: "Fact".into(),
            text: "Created before the never-closing run".into(),
            ..Default::default()
        })
        .unwrap()
        .id;
    drop(api);

    // Act: the handler must not read stdin, so the open writer must not matter
    let (mut child, started) = post_commit_with_stdin(&s, std::process::Stdio::piped());
    let held_open = child.stdin.take().unwrap();
    let out = child.wait_with_output().unwrap();
    let elapsed = started.elapsed();
    drop(held_open);

    // Assert
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert_eq!(out.status.code(), Some(0), "stderr: {stderr}");
    let doc: serde_json::Value =
        serde_json::from_str(stdout.trim()).unwrap_or_else(|e| panic!("{e}: {stdout:?}"));
    assert_eq!(doc, json!({"stamped": 1}), "stderr: {stderr}");
    assert!(
        elapsed < std::time::Duration::from_millis(1500),
        "the handler waited for stdin: {elapsed:?} (the watchdog answers at 1.8 s)"
    );
    assert_eq!(stamp_of(&s, &second).0.as_deref(), Some(head.as_str()));
    assert!(stderr.is_empty(), "nothing to say on a clean run: {stderr}");
}

/// Plan slice: outside a git repository the hook prints `{}` with one fixed stderr line and
/// stamps nothing; without a database it prints `{}`, says so, and creates nothing (M4-6, the
/// same open as every hook). Exit 0 in both cases (PR-9).
#[test]
fn post_commit_outside_a_repository_or_without_a_database_prints_an_empty_document() {
    // Arrange: a database with one unstamped node, but the hook runs from a directory that is
    // not inside any repository
    let s = HookScratch::new("post-commit-nogit");
    let api = s.api();
    let id = accepted_decision(
        &api,
        "Nothing to stamp without a repository",
        &["src/**"],
        None,
    );
    drop(api);
    let outside = s.0.clone();
    assert!(
        !outside.ancestors().any(|d| d.join(".git").exists()),
        "the scratch root must not be inside a repository"
    );

    // Act
    let out = Command::new(env!("CARGO_BIN_EXE_kaban"))
        .env("KABAN_HOME", s.home())
        .env("RUST_LOG", "warn")
        .env_remove("KABAN_WORKSPACE")
        .current_dir(&outside)
        .args([
            "--db",
            s.db().to_str().unwrap(),
            "--json",
            "hook",
            "post-commit",
            "--stdin-json",
        ])
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();

    // Assert
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert_eq!(out.status.code(), Some(0), "stderr: {stderr}");
    assert_eq!(stdout.trim(), "{}", "stderr: {stderr}");
    assert_eq!(
        stderr.trim(),
        "kaban hook post-commit: not inside a git repository; nothing stamped"
    );
    assert_eq!(stamp_of(&s, &id).0, None);

    // Act: no database at the given path (inside the repository this time)
    git_init(&s);
    let missing = s.0.join("absent").join("kaban.db");
    let out = Command::new(env!("CARGO_BIN_EXE_kaban"))
        .env("KABAN_HOME", s.home())
        .env("RUST_LOG", "warn")
        .env_remove("KABAN_WORKSPACE")
        .current_dir(s.repo())
        .args([
            "--db",
            missing.to_str().unwrap(),
            "--json",
            "hook",
            "post-commit",
            "--stdin-json",
        ])
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();

    // Assert
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert_eq!(out.status.code(), Some(0), "stderr: {stderr}");
    assert_eq!(stdout.trim(), "{}");
    assert!(
        stderr.contains("the database does not exist; hooks never create one"),
        "{stderr}"
    );
    assert!(
        !missing.exists() && !missing.parent().unwrap().exists(),
        "a hook never creates a database"
    );
}

/// `git -C <repo> <args>` with the shipped hooks (`core.hooksPath` set on the repository) and
/// the kaban environment the hooks read: `KABAN_BIN` (the built binary, or a missing path),
/// `KABAN_DB`, `KABAN_HOME`. Returns the output; the caller decides what success means.
fn git_with_hooks(s: &HookScratch, kaban_bin: &str, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .arg("-C")
        .arg(s.repo())
        .args(args)
        .env("GIT_AUTHOR_DATE", FIXED_DATE)
        .env("GIT_COMMITTER_DATE", FIXED_DATE)
        .env("KABAN_BIN", kaban_bin)
        .env("KABAN_DB", s.db())
        .env("KABAN_HOME", s.home())
        .env("RUST_LOG", "warn")
        .env_remove("KABAN_WORKSPACE")
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap()
}

fn show(s: &HookScratch, spec: &str) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(s.repo())
        .args(["show", spec])
        .output()
        .unwrap();
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Plan slices (C5.3; M5-12, M5-19, M5-29): with `core.hooksPath` pointing at `hooks/git`, a
/// commit contains `.kaban/adr/INDEX.md` naming the accepted Decision (pre-commit ran `sync
/// --stage`), the session's note carries the commit's sha afterwards (post-commit stamped it), a
/// second commit without record changes leaves the digest byte-identical (no sha, no timestamp
/// in it), and a missing `KABAN_BIN` never aborts a commit.
#[test]
fn the_git_hooks_serialize_into_the_commit_stamp_it_and_never_abort_a_commit() {
    // Arrange
    let s = HookScratch::new("git-hooks");
    git_init(&s);
    let hooks_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../hooks/git")
        .canonicalize()
        .unwrap();
    assert!(hooks_dir.join("pre-commit").exists() && hooks_dir.join("post-commit").exists());
    git(
        &s,
        &["config", "core.hooksPath", hooks_dir.to_str().unwrap()],
    );
    let api = s.api();
    let decision = accepted_decision(
        &api,
        "Store every decision under .kaban",
        &["crates/**"],
        None,
    );
    let note = api
        .remember(RememberRequest {
            kind: "Fact".into(),
            text: "The session's own note".into(),
            session_id: Some("sess-INVENTED-git".into()),
            ..Default::default()
        })
        .unwrap()
        .id;
    drop(api);
    std::fs::write(s.repo().join("src.rs"), "fn main() {}\n").unwrap();
    git(&s, &["add", "src.rs"]);

    // Act: the triggering commit (no pathspec: the as-is index the hook can stage into)
    let out = git_with_hooks(
        &s,
        env!("CARGO_BIN_EXE_kaban"),
        &["commit", "-q", "-m", "first"],
    );

    // Assert
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.stdout.is_empty() && out.stderr.is_empty(),
        "the hooks print nothing: {out:?}"
    );
    let head1 = git(&s, &["rev-parse", "HEAD"]);
    let listed = git(&s, &["show", "--name-only", "--format=", "HEAD"]);
    assert!(
        listed.lines().any(|l| l == ".kaban/adr/INDEX.md"),
        "{listed}"
    );
    assert!(
        listed
            .lines()
            .any(|l| l == format!(".kaban/adr/decisions/{decision}.md")),
        "{listed}"
    );
    assert!(listed.lines().any(|l| l == "src.rs"), "{listed}");
    let digest1 = show(&s, "HEAD:.kaban/adr/INDEX.md").expect("digest in the commit");
    assert!(
        digest1.contains("Store every decision under .kaban") && digest1.contains(&decision),
        "{digest1}"
    );
    assert_eq!(
        stamp_of(&s, &note).0.as_deref(),
        Some(head1.as_str()),
        "post-commit stamped the note"
    );
    assert_eq!(stamp_of(&s, &decision).0.as_deref(), Some(head1.as_str()));
    assert!(
        git(&s, &["status", "--porcelain", "--", ".kaban"]).is_empty(),
        "everything sync wrote was staged into the commit"
    );

    // Act: a second commit without record changes (only the stamps moved the records' rev)
    std::fs::write(s.repo().join("more.rs"), "fn more() {}\n").unwrap();
    git(&s, &["add", "more.rs"]);
    let out = git_with_hooks(
        &s,
        env!("CARGO_BIN_EXE_kaban"),
        &["commit", "-q", "-m", "second"],
    );

    // Assert
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let head2 = git(&s, &["rev-parse", "HEAD"]);
    assert_ne!(head1, head2);
    let digest2 = show(&s, "HEAD:.kaban/adr/INDEX.md").unwrap();
    assert_eq!(
        digest1, digest2,
        "the digest carries no sha and no timestamp (M5-12)"
    );
    let record2 = show(&s, &format!("HEAD:.kaban/adr/decisions/{decision}.md")).unwrap();
    assert!(
        record2.contains(&format!("commit_sha: {head1}")),
        "the re-serialized record carries the first commit's stamp: {record2}"
    );
    assert_eq!(
        stamp_of(&s, &note).0.as_deref(),
        Some(head1.as_str()),
        "stamped once"
    );

    // Act: a commit with a missing binary
    std::fs::write(s.repo().join("third.rs"), "fn third() {}\n").unwrap();
    git(&s, &["add", "third.rs"]);
    let out = git_with_hooks(
        &s,
        "/nonexistent/path/to/kaban",
        &["commit", "-q", "-m", "third"],
    );

    // Assert
    assert!(
        out.status.success(),
        "a missing KABAN_BIN never aborts the commit: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let listed = git(&s, &["show", "--name-only", "--format=", "HEAD"]);
    assert_eq!(listed.trim(), "third.rs", "{listed}");
}
