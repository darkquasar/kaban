//! Explicit full import spelling and its absence-pass contract through the binary.
#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;

use common::Scratch;
use std::process::{Command, Stdio};

#[test]
fn import_without_record_directory_is_read_only_and_rebuild_is_retired() {
    let scratch = Scratch::new("cli-import", "empty-repository");
    let repository = scratch.0.join("repo");
    std::fs::create_dir_all(&repository).unwrap();
    assert!(
        Command::new("git")
            .args(["init", "-q"])
            .arg(&repository)
            .status()
            .unwrap()
            .success()
    );
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_kaban"))
            .env("KABAN_HOME", scratch.home())
            .args(["--db", scratch.db().to_str().unwrap(), "--json"])
            .args(args)
            .current_dir(&repository)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    };
    let imported = run(&["import"]);
    assert_eq!(
        imported.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&imported.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&imported.stdout).unwrap();
    assert_eq!(report["imported"], 0);
    assert!(
        !repository.join(".kaban").exists(),
        "DB-only import must not create repository records"
    );
    let retired = run(&["rebuild"]);
    assert_eq!(retired.status.code(), Some(4));
    assert_eq!(retired.stdout, [] as [u8; 0]);
}

#[test]
fn import_help_explains_guarded_absence_pass() {
    let output = Command::new(env!("CARGO_BIN_EXE_kaban"))
        .args(["import", "--help"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let help = String::from_utf8(output.stdout).unwrap();
    assert!(help.contains("absence pass"));
    assert!(help.contains("serialized revision"));
    assert!(help.contains("Does not write repository files"));
}
