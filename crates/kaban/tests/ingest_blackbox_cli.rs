//! Structured finish through real CLI JSON pipes, including retained byte retries.
#![cfg(feature = "ingest")]
#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;
use common::Scratch;
use serde_json::{Value, json};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

fn run(s: &Scratch, root: &Path, args: &[&str], body: Option<&Value>) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_kaban"))
        .env("KABAN_HOME", s.home())
        .env("RUST_LOG", "warn")
        .current_dir(root)
        .args(["--db", s.db().to_str().unwrap(), "--json"])
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    if let Some(body) = body {
        // A subcommand that refuses `--stdin-json` exits (code 4) without reading stdin; under
        // host load it can do so before this write lands, which then fails with EPIPE. That
        // ordering is not the behaviour under test: callers assert on the exit status.
        if let Err(e) = child
            .stdin
            .take()
            .unwrap()
            .write_all(body.to_string().as_bytes())
        {
            assert_eq!(
                e.kind(),
                std::io::ErrorKind::BrokenPipe,
                "writing the request body: {e}"
            );
        }
    }
    child.wait_with_output().unwrap()
}
fn value(out: &Output) -> Value {
    assert!(out.status.success(), "{out:?}");
    serde_json::from_slice(&out.stdout).unwrap()
}

#[test]
fn cli_finish_retains_exact_output_and_rechecks_authority() {
    let s = Scratch::new("kaban-e310-cli", "finish");
    let root = s.0.join("repo");
    std::fs::create_dir_all(root.join(".git")).unwrap();
    std::fs::write(root.join("a.md"), "# Source\n\nHarmless body.\n").unwrap();
    let prepared = value(&run(&s, &root, &["ingest", "job", "brief", "a.md"], None));
    let job = prepared["job"].as_str().unwrap();
    let body = json!({"records":[{"local_id":"r","kind":"Fact","text":"Source-supported record"}]});
    assert_eq!(
        value(&run(
            &s,
            &root,
            &["ingest", "job", "submit", job, "--stdin-json"],
            Some(&body)
        ))["created"],
        1
    );
    let first = run(&s, &root, &["ingest", "job", "finish", job], None);
    let report = value(&first);
    assert_eq!(report["status"], "finished");
    assert_eq!(report["created"], 1);
    let second = run(&s, &root, &["ingest", "job", "finish", job], None);
    value(&second);
    assert_eq!(second.stdout, first.stdout);
    let forged = run(
        &s,
        &root,
        &["ingest", "job", "finish", &"f".repeat(64)],
        None,
    );
    assert_eq!(forged.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&forged.stderr).contains("job_revoked"));
    let foreign = run(
        &s,
        &root,
        &["--workspace", "foreign", "ingest", "job", "finish", job],
        None,
    );
    assert_eq!(foreign.status.code(), Some(3));
    let malformed = run(
        &s,
        &root,
        &["ingest", "job", "finish", "AKIAQWERTYUIOPASDFGH"],
        None,
    );
    assert_eq!(malformed.status.code(), Some(2));
    assert!(!String::from_utf8_lossy(&malformed.stderr).contains("AKIAQWERTYUIOPASDFGH"));
    let body_refused = run(
        &s,
        &root,
        &["ingest", "job", "finish", job, "--stdin-json"],
        Some(&json!({})),
    );
    assert_eq!(body_refused.status.code(), Some(4));
    let forgotten = run(&s, &root, &["forget", "--document", "a.md"], None);
    value(&forgotten);
    assert_eq!(
        run(&s, &root, &["ingest", "job", "finish", job], None)
            .status
            .code(),
        Some(3)
    );
}
