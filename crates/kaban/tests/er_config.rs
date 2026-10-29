//! BR-7 (E2.10 boundary review; E2.11b): the `[er]` section of `<KABAN_HOME>/config.yaml`
//! reaches the store's startup seam. A valid strict section starts the process; an invalid one
//! is refused at startup as a config error (exit 4) naming the loader's category and the field
//! path under `er.`, never the value. The verdict change the frozen thresholds make is pinned at
//! the seam (`kaban-store-sqlite/tests/er_priors_seam.rs`): the CLI has no ER trigger.
//!
//! RW5-3 (`obs-no-sensitive-data`): no assertion here formats stdout or stderr. A failure
//! message carries the exit code, the byte counts of both channels and the name of the
//! property that failed; a regression of the no-echo rule must not print the planted value
//! into the test log.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::Scratch;
use std::process::{Command, Output};

/// `kaban --json --db <db> recall anything` with the scratch home: a read that opens (and on a
/// fresh scratch creates) the store through the startup path.
fn recall(s: &Scratch) -> Output {
    Command::new(env!("CARGO_BIN_EXE_kaban"))
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
        .unwrap()
}

fn with_config(s: &Scratch, yaml: &str) {
    std::fs::create_dir_all(s.home()).unwrap();
    std::fs::write(s.home().join("config.yaml"), yaml).unwrap();
}

/// The payload-free facts of one run, the only things an assertion message may carry: the
/// exit code and the byte count of each channel.
fn shape(out: &Output) -> String {
    format!(
        "exit {:?}, stdout {} bytes, stderr {} bytes",
        out.status.code(),
        out.stdout.len(),
        out.stderr.len()
    )
}

/// Whether `needle` occurs in the channel's bytes (lossy UTF-8), as a boolean only.
fn holds(channel: &[u8], needle: &str) -> bool {
    String::from_utf8_lossy(channel).contains(needle)
}

/// Whether `needle` occurs on stdout or stderr.
fn on_either(out: &Output, needle: &str) -> bool {
    holds(&out.stdout, needle) || holds(&out.stderr, needle)
}

/// A refused config: exit 4, the category and the field path on stderr, `hidden` on neither
/// channel, and no database file created.
fn assert_refused(s: &Scratch, out: &Output, path: &str, category: &str, hidden: &[&str]) {
    let facts = shape(out);
    assert_eq!(
        out.status.code(),
        Some(4),
        "refused at startup as a config error: {facts}"
    );
    assert!(
        holds(&out.stderr, &format!("config key invalid: {path}")),
        "stderr names the field path `{path}`: {facts}"
    );
    assert!(
        holds(&out.stderr, category),
        "stderr names the loader category `{category}`: {facts}"
    );
    for (i, h) in hidden.iter().enumerate() {
        assert!(
            !on_either(out, h),
            "planted value {i} is echoed on stdout or stderr: {facts}"
        );
    }
    assert!(
        !s.db().exists(),
        "the refused config created no database file: {facts}"
    );
}

#[test]
fn a_valid_strict_er_section_starts_the_process() {
    let s = Scratch::new("kaban-er-config", "valid");
    with_config(
        &s,
        "er:\n  match_threshold: 9.0\n  possible_threshold: 3.0\n  m:\n    title: 0.95\n",
    );
    let out = recall(&s);
    assert_eq!(
        out.status.code(),
        Some(0),
        "a valid section starts: {}",
        shape(&out)
    );
    assert!(
        s.db().exists(),
        "the started process created the database: {}",
        shape(&out)
    );
}

#[test]
fn an_invalid_er_section_exits_4_with_the_category_and_the_field_path_and_never_the_value() {
    const CANARY: &str = "0.123456";
    // RW4-5 (W4C): the unknown key never crosses the error boundary; the table names the hole
    let s = Scratch::new("kaban-er-config", "unknown-field");
    with_config(&s, &format!("er:\n  m:\n    colour: {CANARY}\n"));
    let out = recall(&s);
    assert_refused(
        &s,
        &out,
        "er.m.<unknown>",
        "er_unknown_field",
        &["colour", CANARY],
    );

    let s = Scratch::new("kaban-er-config", "threshold");
    with_config(&s, "er:\n  match_threshold: 0.0\n");
    let out = recall(&s);
    assert_refused(&s, &out, "er.match_threshold", "er_threshold_invalid", &[]);

    let s = Scratch::new("kaban-er-config", "range");
    with_config(&s, "er:\n  u:\n    title: 1.0\n");
    let out = recall(&s);
    assert_refused(&s, &out, "er.u.title", "er_probability_out_of_range", &[]);
}

/// The negative of [`assert_refused`]'s no-echo check: the boolean holds the planted value
/// when a channel does carry it, so an echo would fail the refusal test.
#[test]
fn the_no_echo_check_sees_a_value_planted_on_either_channel() {
    use std::os::unix::process::ExitStatusExt;
    let planted = |stdout: &[u8], stderr: &[u8]| Output {
        status: std::process::ExitStatus::from_raw(4 << 8),
        stdout: stdout.to_vec(),
        stderr: stderr.to_vec(),
    };
    assert!(
        on_either(&planted(b"", b"key colour: 0.123456"), "0.123456"),
        "stderr echo seen"
    );
    assert!(
        on_either(&planted(b"{\"v\":\"0.123456\"}", b""), "0.123456"),
        "stdout echo seen"
    );
    assert!(
        !on_either(&planted(b"{}", b"er_unknown_field"), "0.123456"),
        "no echo, none seen"
    );
}
