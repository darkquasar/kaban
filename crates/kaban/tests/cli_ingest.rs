//! `kaban ingest <paths...>` and `kaban forget --document`, black-box through the binary
//! (docs-ingest-spec D7, D10; docs-ingest-plan.md DI1.6): exit codes per core-spec §6, the
//! `--dry-run` report without writes, idempotency of a second run, the recall chunk card
//! (`path > heading path`, hop-1 parent Section and the Decision linked `EVIDENCE_FOR`,
//! `--kind Chunk` excludes notes), the document tombstone, and the canary.
#![cfg(feature = "ingest")] // the feature-off nextest step (CG7.3, scripts/ci/ci.sh) builds this binary too
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::Scratch;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// A reject-class canary (builtin `aws-access-key-id`): must reach neither stream nor disk.
const SECRET_CANARY: &str = "AKIAQWERTYUIOPASDFGH";

/// A repository scratch (`<root>/.git`) with a `docs/` folder.
fn repo(s: &Scratch) -> PathBuf {
    let root = s.0.join("repo");
    std::fs::create_dir_all(root.join(".git")).unwrap();
    std::fs::create_dir_all(root.join("docs")).unwrap();
    std::fs::write(
        root.join("alpha.md"),
        "# Alpha\n\n## Storage\n\nThe nightjar service uses WAL mode for concurrent readers.\n\nSee [[docs/beta]] and #ops.\n",
    )
    .unwrap();
    std::fs::write(
        root.join("docs/beta.md"),
        "# Beta\n\nBeta body about libssl linking order.\n",
    )
    .unwrap();
    root
}

fn kaban(s: &Scratch, cwd: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_kaban"));
    cmd.env("KABAN_HOME", s.home())
        .env("RUST_LOG", "warn")
        .current_dir(cwd)
        .args(["--db", s.db().to_str().unwrap(), "--json"])
        .stdin(Stdio::null());
    cmd
}

fn run(s: &Scratch, cwd: &Path, args: &[&str]) -> Output {
    kaban(s, cwd).args(args).output().unwrap()
}

fn run_stdin(s: &Scratch, cwd: &Path, args: &[&str], stdin: &str) -> Output {
    use std::io::Write;
    let mut child = kaban(s, cwd)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // the child may exit before reading everything: EPIPE is not a failure here
    let _ = child.stdin.take().unwrap().write_all(stdin.as_bytes());
    child.wait_with_output().unwrap()
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn json(out: &Output) -> Value {
    serde_json::from_str(&stdout(out)).unwrap_or_else(|e| panic!("{e}: {}", stdout(out)))
}

fn code(out: &Output) -> i32 {
    out.status.code().unwrap()
}

#[test]
fn job_submit_reads_bounded_stdin_and_repeats_through_shared_report() {
    let s = Scratch::new("kaban-cli-submit", "public");
    let root = repo(&s);
    let prepared = run(&s, &root, &["ingest", "job", "brief", "alpha.md"]);
    assert_eq!(code(&prepared), 0, "{prepared:?}");
    let prepared = json(&prepared);
    let job = prepared["job"].as_str().unwrap();
    let body = serde_json::json!({"records":[{"local_id":"r","kind":"Fact","text":"Source-supported fact"}]}).to_string();
    let first = run_stdin(
        &s,
        &root,
        &["ingest", "job", "submit", job, "--stdin-json"],
        &body,
    );
    assert_eq!(code(&first), 0, "{first:?}");
    assert_eq!(json(&first)["created"], 1);
    let repeat = run_stdin(
        &s,
        &root,
        &["ingest", "job", "submit", job, "--stdin-json"],
        &body,
    );
    assert_eq!(code(&repeat), 0, "{repeat:?}");
    assert_eq!(json(&repeat)["unchanged"], 1);
    assert_eq!(code(&run(&s, &root, &["ingest", "job", "submit", job])), 4);
    assert_eq!(
        code(&run_stdin(
            &s,
            &root,
            &["ingest", "job", "submit", job, "--stdin-json"],
            "{\"accepted\":true}"
        )),
        4
    );
    assert_eq!(
        code(&run_stdin(
            &s,
            &root,
            &["ingest", "job", "brief", "alpha.md", "--stdin-json"],
            "{}"
        )),
        4
    );
}

#[test]
fn oversized_submit_body_is_refused_before_database_creation() {
    let s = Scratch::new("kaban-cli-submit", "oversized");
    let root = repo(&s);
    let output = run_stdin(
        &s,
        &root,
        &["ingest", "job", "submit", &"a".repeat(64), "--stdin-json"],
        &" ".repeat(1024 * 1024 + 1),
    );
    assert_eq!(code(&output), 4);
    assert!(
        !s.db().exists(),
        "bounded reader runs before opening service/database"
    );
}

#[test]
fn ingest_commits_the_tree_dry_run_writes_nothing_and_a_second_run_skips() {
    // Arrange
    let s = Scratch::new("kaban-cli-ingest", "run");
    let root = repo(&s);

    // Act
    let dry = run(&s, &root, &["ingest", ".", "--dry-run"]);
    let first = run(&s, &root, &["ingest", "."]);
    let second = run(&s, &root, &["ingest", "alpha.md", "docs"]);

    // Assert
    assert_eq!(code(&dry), 0, "{dry:?}");
    let dry = json(&dry);
    assert_eq!(dry["dry_run"], true);
    assert_eq!(dry["outbox_rows"], 0, "dry run writes nothing: {dry}");
    let first = json(&first);
    assert_eq!(first["documents"], 2, "{first}");
    assert!(first["outbox_rows"].as_u64().unwrap() > 0);
    let second = json(&second);
    assert_eq!(second["documents"], 0, "{second}");
    assert_eq!(second["outbox_rows"], 0);
    assert_eq!(second["skipped"], 2);
    // the index status knows the run
    let status = run(&s, &root, &["recall", "nightjar", "--kind", "Document"]);
    assert_eq!(code(&status), 0);
}

#[test]
fn a_path_outside_the_workspace_root_exits_4_and_no_path_exits_4() {
    // Arrange
    let s = Scratch::new("kaban-cli-ingest", "outside");
    let root = repo(&s);
    let outside = s.0.join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("x.md"), "# X\n").unwrap();

    // Act
    let out = run(&s, &root, &["ingest", outside.to_str().unwrap()]);
    let none = run(&s, &root, &["ingest"]);

    // Assert
    assert_eq!(code(&out), 4, "{out:?}");
    assert_eq!(stdout(&out), "", "no JSON document on a refusal");
    assert_eq!(code(&none), 4, "{none:?}");
}

#[test]
fn job_next_unknown_invalid_selector_and_canary_fail_with_safe_exit_classes() {
    let s = Scratch::new("kaban-cli-ingest", "job-next");
    let root = repo(&s);
    let job = "a".repeat(64);
    let unknown = run(&s, &root, &["ingest", "job", "next", &job, "--chunk", "0"]);
    assert_eq!(code(&unknown), 3, "{unknown:?}");
    assert!(String::from_utf8_lossy(&unknown.stderr).contains("job_revoked"));
    let invalid = run(
        &s,
        &root,
        &["ingest", "job", "next", &job, "--chunks", "9..2"],
    );
    assert_eq!(code(&invalid), 4, "{invalid:?}");
    let canary = run(
        &s,
        &root,
        &["ingest", "job", "next", SECRET_CANARY, "--chunk", "0"],
    );
    assert_eq!(code(&canary), 2, "{canary:?}");
    assert!(!stdout(&canary).contains(SECRET_CANARY));
    assert!(!String::from_utf8_lossy(&canary.stderr).contains(SECRET_CANARY));
}

#[test]
fn structured_cli_mcp_brief_next_parity_preview_directory_and_negatives() {
    use common::Server;
    use serde_json::json;
    use std::time::Duration;
    let s = Scratch::new("kaban-cli-ingest", "structured-parity");
    let root = repo(&s);
    let preview = run(
        &s,
        &root,
        &["ingest", "alpha.md", "--mode", "structured", "--dry-run"],
    );
    assert_eq!(code(&preview), 0, "{preview:?}");
    let preview = json(&preview);
    assert_eq!(preview["document"]["indexed"], false);
    let unknown = run(
        &s,
        &root,
        &[
            "ingest",
            "job",
            "next",
            preview["job"].as_str().unwrap(),
            "--chunk",
            "0",
        ],
    );
    assert_eq!(code(&unknown), 3, "preview opens no job");
    let first = run(&s, &root, &["ingest", "job", "brief", "alpha.md"]);
    assert_eq!(code(&first), 0, "{first:?}");
    let first = json(&first);
    assert_eq!(first["job"], preview["job"]);
    assert_eq!(first["document"]["indexed"], true);
    let job = first["job"].as_str().unwrap();
    let cli_next = run(&s, &root, &["ingest", "job", "next", job, "--chunk", "0"]);
    assert_eq!(code(&cli_next), 0, "{cli_next:?}");
    let mut server = Server::start_at(&s, &root);
    let id = server.request(
        "tools/call",
        &json!({"name":"ingest_brief","arguments":{"path":"alpha.md"}}),
    );
    let response = server.response(id, Duration::from_secs(10)).unwrap();
    assert_ne!(response["result"]["isError"], true, "{response}");
    assert_eq!(response["result"]["structuredContent"], first);
    let id = server.request(
        "tools/call",
        &json!({"name":"ingest_next","arguments":{"job":job,"n":0}}),
    );
    let response = server.response(id, Duration::from_secs(10)).unwrap();
    assert_eq!(response["result"]["structuredContent"], json(&cli_next));
    for args in [
        json!({"job":"a".repeat(64),"n":0}),
        json!({"job":job,"n":0,"chunks":"0..1"}),
        json!({"job":SECRET_CANARY,"n":0}),
    ] {
        let id = server.request(
            "tools/call",
            &json!({"name":"ingest_next","arguments":args}),
        );
        let response = server.response(id, Duration::from_secs(10)).unwrap();
        assert!(
            response["result"]["isError"] == true || response.get("error").is_some(),
            "{response}"
        );
        assert!(!response.to_string().contains(SECRET_CANARY));
    }
    server.finish();
    let directory = run(
        &s,
        &root,
        &["ingest", ".", "--mode", "structured", "--dry-run"],
    );
    assert_eq!(code(&directory), 0, "{directory:?}");
    let briefs: Vec<Value> = stdout(&directory)
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(briefs.len(), 2);
    assert_eq!(briefs[0]["document"]["path"], "alpha.md");
    assert_eq!(briefs[1]["document"]["path"], "docs/beta.md");
    std::fs::write(root.join("unsupported.rs"), "fn main() {}\n").unwrap();
    assert_eq!(
        code(&run(
            &s,
            &root,
            &["ingest", "unsupported.rs", "--mode", "structured"]
        )),
        4
    );
    let parse = run(&s, &root, &["parse"]);
    assert_eq!(code(&parse), 4);
    assert_eq!(
        code(&run(
            &s,
            &root,
            &["ingest", "job", "brief", "alpha.md", "--with", "invalid"]
        )),
        4
    );
}

#[test]
fn recall_returns_chunk_cards_with_the_path_and_heading_path_and_hop1_context() {
    // Arrange: ingest, then a Decision linked EVIDENCE_FOR from the storage chunk
    let s = Scratch::new("kaban-cli-ingest", "cards");
    let root = repo(&s);
    assert_eq!(code(&run(&s, &root, &["ingest", "."])), 0);
    let chunks = json(&run(
        &s,
        &root,
        &[
            "recall",
            "WAL mode concurrent",
            "--kind",
            "Chunk",
            "--hops",
            "0",
        ],
    ));
    let chunk = &chunks["results"][0];
    assert_eq!(chunk["kind"], "Chunk", "{chunks}");
    let chunk_id = chunk["id"].as_str().unwrap();
    let remember = run_stdin(
        &s,
        &root,
        &["remember", "--stdin-json"],
        &format!(
            r#"{{"kind":"Decision","text":"Use WAL mode for the nightjar store","props":{{"rationale":"concurrent readers"}},"links":[{{"rel":"EVIDENCE_FOR","from":"{chunk_id}"}}]}}"#
        ),
    );
    assert_eq!(code(&remember), 0, "{remember:?}");

    // Act
    let expanded = json(&run(&s, &root, &["recall", "WAL mode concurrent"]));
    let only_chunks = json(&run(
        &s,
        &root,
        &["recall", "WAL mode concurrent", "--kind", "Chunk"],
    ));

    // Assert: the card header is `path > heading path`
    assert_eq!(
        chunk["summary"], "alpha.md > Alpha > Storage",
        "chunk card header: {chunk}"
    );
    let kinds: Vec<&str> = expanded["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["kind"].as_str().unwrap())
        .collect();
    assert!(
        kinds.contains(&"Section"),
        "hop-1 parent Section: {kinds:?}"
    );
    assert!(
        kinds.contains(&"Decision"),
        "hop-1 Decision linked EVIDENCE_FOR: {kinds:?}"
    );
    assert!(
        only_chunks["results"]
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c["kind"] == "Chunk"),
        "kinds:[Chunk] excludes notes: {only_chunks}"
    );
}

#[test]
fn forget_document_tombstones_the_document_and_its_chunks_and_an_unknown_path_exits_3() {
    // Arrange
    let s = Scratch::new("kaban-cli-ingest", "forget");
    let root = repo(&s);
    assert_eq!(code(&run(&s, &root, &["ingest", "."])), 0);

    // Act
    let forgotten = run(&s, &root, &["forget", "--document", "docs/beta.md"]);
    let gone = run(&s, &root, &["recall", "libssl linking", "--kind", "Chunk"]);
    let unknown = run(&s, &root, &["forget", "--document", "nope.md"]);

    // Assert
    assert_eq!(code(&forgotten), 0, "{forgotten:?}");
    let v = json(&forgotten);
    assert_eq!(v["tombstoned"], 3, "document + section + chunk: {v}");
    assert_eq!(json(&gone)["count"], 0, "the chunk is hidden from recall");
    assert_eq!(code(&unknown), 3, "{unknown:?}");
}

#[test]
fn a_secret_in_a_document_never_reaches_stdout_stderr_or_the_database() {
    // Arrange
    let s = Scratch::new("kaban-cli-ingest", "canary");
    let root = repo(&s);
    std::fs::write(
        root.join("docs/keys.md"),
        format!("# Keys\n\nThe key is {SECRET_CANARY} in this paragraph.\n"),
    )
    .unwrap();

    // Act
    let out = run(&s, &root, &["ingest", "."]);

    // Assert
    assert_eq!(code(&out), 0, "{out:?}");
    let text = format!("{}{}", stdout(&out), String::from_utf8_lossy(&out.stderr));
    assert!(!text.contains(SECRET_CANARY));
    let report = json(&out);
    assert_eq!(report["blocked_chunks"], 1, "{report}");
    let mut blob = Vec::new();
    for suffix in ["", "-wal", "-shm"] {
        if let Ok(b) = std::fs::read(format!("{}{suffix}", s.db().display())) {
            blob.extend(b);
        }
    }
    assert_ne!(blob.len(), 0, "database bytes read");
    assert!(
        !blob
            .windows(SECRET_CANARY.len())
            .any(|w| w == SECRET_CANARY.as_bytes()),
        "canary bytes in the database"
    );
}
