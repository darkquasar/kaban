//! Restart recovery and source-handle authority through independent CLI processes.
#![cfg(feature = "ingest")]
#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;
use common::Scratch;
use serde_json::Value;
use std::process::{Command, Output, Stdio};

fn run(s: &Scratch, root: &std::path::Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_kaban"))
        .env("KABAN_HOME", s.home())
        .env("RUST_LOG", "warn")
        .current_dir(root)
        .args(["--db", s.db().to_str().unwrap(), "--json"])
        .args(args)
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

#[test]
fn restarted_next_reopens_source_and_finished_retry_never_uses_a_cached_authority() {
    let s = Scratch::new("kaban-e310-restart", "source");
    let root = s.0.join("repo");
    std::fs::create_dir_all(root.join(".git")).unwrap();
    std::fs::create_dir_all(root.join("docs")).unwrap();
    std::fs::write(root.join("docs/a.md"), "# Source\n\nHarmless source.\n").unwrap();
    let out = run(&s, &root, &["ingest", "job", "brief", "docs/a.md"]);
    assert!(out.status.success(), "{out:?}");
    let prepared: Value = serde_json::from_slice(&out.stdout).unwrap();
    let job = prepared["job"].as_str().unwrap();
    let next = run(&s, &root, &["ingest", "job", "next", job, "--chunk", "0"]);
    assert!(next.status.success(), "{next:?}");
    let page: Value = serde_json::from_slice(&next.stdout).unwrap();
    assert!(page["chunk"]["text"].as_str().unwrap().contains("Harmless"));
    let finished = run(&s, &root, &["ingest", "job", "finish", job]);
    assert!(finished.status.success(), "{finished:?}");
    let repeated = run(&s, &root, &["ingest", "job", "finish", job]);
    assert_eq!(repeated.stdout, finished.stdout);
    std::fs::write(root.join("docs/a.md"), "# Source\n\nEdited source.\n").unwrap();
    assert_eq!(
        run(&s, &root, &["ingest", "job", "finish", job])
            .status
            .code(),
        Some(4)
    );
}

#[cfg(unix)]
#[test]
fn same_final_inode_under_an_escaping_intermediate_symlink_is_refused() {
    let s = Scratch::new("kaban-e310-restart", "symlink");
    let root = s.0.join("repo");
    std::fs::create_dir_all(root.join(".git")).unwrap();
    std::fs::create_dir_all(root.join("docs")).unwrap();
    std::fs::write(root.join("docs/a.md"), "# Source\n\nHarmless source.\n").unwrap();
    let out = run(&s, &root, &["ingest", "job", "brief", "docs/a.md"]);
    assert!(out.status.success());
    let prepared: Value = serde_json::from_slice(&out.stdout).unwrap();
    let job = prepared["job"].as_str().unwrap();
    let outside = s.0.join("outside");
    std::fs::rename(root.join("docs"), &outside).unwrap();
    std::os::unix::fs::symlink(&outside, root.join("docs")).unwrap();
    for args in [
        vec!["ingest", "job", "next", job, "--chunk", "0"],
        vec!["ingest", "job", "finish", job],
    ] {
        let refused = run(&s, &root, &args);
        assert_eq!(refused.status.code(), Some(4));
        assert_eq!(refused.stdout, [] as [u8; 0]);
    }
}
