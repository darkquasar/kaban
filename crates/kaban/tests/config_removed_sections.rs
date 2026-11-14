//! A `config.yaml` that still spells the section of a removed feature (`er`, `reconcile`,
//! `why`, `briefing`) is refused at startup like any unknown key: exit 4 naming the section,
//! never a value, and nothing is created.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::Scratch;
use std::process::Command;

#[test]
fn a_removed_section_exits_4_naming_the_section() {
    for section in ["er", "reconcile", "why", "briefing"] {
        let s = Scratch::new("kaban-config-removed", section);
        std::fs::create_dir_all(s.home()).unwrap();
        std::fs::write(
            s.home().join("config.yaml"),
            format!("{section}:\n  match_threshold: 0.4242\n"),
        )
        .unwrap();
        let out = Command::new(env!("CARGO_BIN_EXE_kaban"))
            .env("KABAN_HOME", s.home())
            .env_remove("RUST_LOG")
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
        assert_eq!(out.status.code(), Some(4), "{section}: {stderr}");
        assert!(
            stderr.contains(&format!("config key invalid: {section}")),
            "{section}: {stderr}"
        );
        assert!(!stderr.contains("0.4242"), "{section}: {stderr}");
        assert!(!s.db().exists(), "{section}: no database is created");
    }
}
