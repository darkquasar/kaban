//! PC-5: a `config.yaml` that still spells a removed section (`[er]`, `[reconcile]`, `[why]`,
//! `[briefing]`) starts every verb; the section is ignored with one warning naming it.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::Scratch;
use std::process::Command;

#[test]
fn a_removed_er_section_starts_the_process_with_one_warning_naming_it() {
    let s = Scratch::new("kaban-config-removed", "er");
    std::fs::create_dir_all(s.home()).unwrap();
    // the section that used to fail startup on an unknown field now loads and is ignored
    std::fs::write(
        s.home().join("config.yaml"),
        "er:\n  match_threshold: 0.0\n  m:\n    colour: 0.5\n",
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_kaban"))
        .env("KABAN_HOME", s.home())
        .env("RUST_LOG", "warn")
        .args([
            "--json",
            "--db",
            s.db().to_str().unwrap(),
            "recall",
            "anything",
        ])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "the process starts: {stderr}");
    assert!(s.db().exists(), "the started process created the database");
    assert_eq!(
        stderr
            .matches("config section [er] is no longer used and is ignored")
            .count(),
        1,
        "one warning naming the section: {stderr}"
    );
}
