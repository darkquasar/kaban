//! `kaban scan` as a policy validator (core-plan C1.3): exit 4 on a policy that fails to load,
//! verdict JSON on stdout otherwise, never the offending value.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

use std::io::Write;
use std::process::{Command, Stdio};

fn kaban() -> Command {
    Command::new(env!("CARGO_BIN_EXE_kaban"))
}

/// A policy file in a private temp dir, removed on drop (also when the test panics).
struct TmpPolicy {
    dir: std::path::PathBuf,
    path: std::path::PathBuf,
}

impl TmpPolicy {
    fn new(name: &str, body: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("kaban-cli-scan-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("policy.yaml");
        std::fs::write(&path, body).unwrap();
        Self { dir, path }
    }
    fn arg(&self) -> &str {
        self.path.to_str().unwrap()
    }
}

impl Drop for TmpPolicy {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn scan_with_invalid_policy_exits_4_and_names_the_field() {
    let p = TmpPolicy::new(
        "bad",
        "allow:\n  - regexes: ['^x$']\n    stopwords: [example]\n",
    );
    let out = kaban()
        .args(["--policy", p.arg(), "scan"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(4), "{out:?}");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("stopwords") && err.contains("allow"), "{err}");
    assert!(
        !err.contains("example"),
        "term values must not be echoed: {err}"
    );
    assert!(out.stdout.is_empty(), "stdout stays empty on error");
}

#[test]
fn scan_with_unanchored_allow_exits_4() {
    let p = TmpPolicy::new(
        "unanchored",
        "allow:\n  - rule_ids: [openai-api-key]\n    regexes: ['sk-test-']\n",
    );
    let out = kaban()
        .args(["--policy", p.arg(), "scan"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(4), "{out:?}");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("anchored") && !err.contains("sk-test"),
        "{err}"
    );
}

#[test]
fn scan_with_valid_policy_prints_verdict_without_the_value() {
    let p = TmpPolicy::new(
        "good",
        "words:\n  - { term: hunter2, action: redact, id: pw }\n",
    );
    let mut child = kaban()
        .args(["--policy", p.arg(), "scan"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"password=Qx9vLm2pRt7wZk and hunter2")
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["rejected"], serde_json::json!(true));
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(!s.contains("Qx9vLm2pRt7wZk"), "{s}");
}
