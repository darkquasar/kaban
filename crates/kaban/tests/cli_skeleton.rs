//! The C4.1a CLI skeleton (core-spec F7, §6; core-plan C4.1 as amended by M4-1, M4-2, M4-4),
//! black-box through the binary: a usage error prints a fixed text (no argument value) and
//! exits 4 while help and version exit 0; the unshipped flags exit 4 with "not available in
//! this build"; `--json` prints one compact JSON document; `--stdin-json` is bounded at 1 MiB
//! and exclusive with argument flags.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::Scratch;
use std::io::Write;
use std::process::{Command, Output, Stdio};

/// A string no policy rule matches: its absence proves clap never formatted the value.
const CANARY: &str = "CANARY-argv-value-5b1e";

fn kaban(s: &Scratch) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_kaban"));
    cmd.env("KABAN_HOME", s.home())
        .env("RUST_LOG", "warn")
        .args(["--db", s.db().to_str().unwrap()])
        .stdin(Stdio::null());
    cmd
}

fn with_stdin(mut cmd: Command, body: &[u8]) -> Output {
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    {
        let mut stdin = child.stdin.take().unwrap();
        // the binary may stop reading at its cap; a closed pipe is not a test failure
        let _ = stdin.write_all(body);
    }
    child.wait_with_output().unwrap()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn a_usage_error_exits_4_with_a_fixed_text_and_never_echoes_the_argument() {
    let s = Scratch::new("kaban-cli-skel", "clap");
    // an unknown argument carrying the value, and a value that fails its option's parse
    let unknown = kaban(&s).arg(format!("--{CANARY}")).output().unwrap();
    let bad_value = kaban(&s).args(["bench", "--n", CANARY]).output().unwrap();
    let no_sub = kaban(&s).arg(CANARY).output().unwrap();
    for (what, out) in [
        ("unknown argument", &unknown),
        ("invalid value", &bad_value),
        ("unknown subcommand", &no_sub),
    ] {
        let err = stderr(out);
        assert_eq!(out.status.code(), Some(4), "{what}: {err}");
        assert!(!err.contains(CANARY), "{what}: the value was echoed: {err}");
        assert!(
            err.starts_with("error: ") && err.contains("--help"),
            "{what}: fixed text with the help pointer: {err}"
        );
        assert!(out.stdout.is_empty(), "{what}: stdout stays empty");
    }
    // help and version are not errors
    let help = kaban(&s).arg("--help").output().unwrap();
    assert_eq!(help.status.code(), Some(0), "{}", stderr(&help));
    assert!(String::from_utf8_lossy(&help.stdout).contains("recall"));
    let version = kaban(&s).arg("--version").output().unwrap();
    assert_eq!(version.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&version.stdout).starts_with("kaban "));
}

#[test]
fn unshipped_flags_exit_4_without_silent_success() {
    let s = Scratch::new("kaban-cli-skel", "stubs");
    let stubs: [&[&str]; 2] = [&["serve", "--http"], &["--global-db", "global.db", "serve"]];
    for args in stubs {
        let out = kaban(&s).args(args).output().unwrap();
        let err = stderr(&out);
        assert_eq!(out.status.code(), Some(4), "{args:?}: {err}");
        assert!(
            err.contains("not available in this build"),
            "{args:?}: {err}"
        );
        assert!(out.stdout.is_empty(), "{args:?}: stdout stays empty");
    }
    assert!(!s.db().exists(), "a stub never creates the database");
}

#[test]
fn json_prints_one_compact_document_and_the_default_one_pretty_document() {
    let s = Scratch::new("kaban-cli-skel", "json");
    // `scan` needs no database; `recall` goes through the store
    drop(kaban_store_sqlite::SqliteStore::open(&s.db(), &s.home().join("keys")).unwrap());
    for args in [&["scan"][..], &["recall", "anything"][..]] {
        let compact = with_stdin(
            {
                let mut c = kaban(&s);
                c.arg("--json").args(args);
                c
            },
            b"a plain sentence\n",
        );
        assert_eq!(compact.status.code(), Some(0), "{}", stderr(&compact));
        let text = String::from_utf8(compact.stdout).unwrap();
        assert_eq!(text.lines().count(), 1, "{args:?}: one line: {text}");
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert!(value.is_object(), "{args:?}: one JSON document: {text}");

        let pretty = with_stdin(
            {
                let mut c = kaban(&s);
                c.args(args);
                c
            },
            b"a plain sentence\n",
        );
        assert_eq!(pretty.status.code(), Some(0), "{}", stderr(&pretty));
        let text = String::from_utf8(pretty.stdout).unwrap();
        assert!(text.lines().count() > 1, "{args:?}: pretty: {text}");
        let again: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(again, value, "{args:?}: the same document in both layouts");
    }
}

#[test]
fn stdin_json_is_bounded_at_one_mebibyte_and_exclusive_with_arguments() {
    let s = Scratch::new("kaban-cli-skel", "stdin");
    // one byte over the cap: refused before the database is touched
    let mut body = Vec::with_capacity(1024 * 1024 + 64);
    body.extend_from_slice(br#"{"id": ""#);
    body.resize(1024 * 1024 + 1, b'a');
    let over = with_stdin(
        {
            let mut c = kaban(&s);
            c.args(["--stdin-json", "forget"]);
            c
        },
        &body,
    );
    let err = stderr(&over);
    assert_eq!(over.status.code(), Some(4), "{err}");
    assert!(err.contains("exceeds 1 MiB"), "{err}");
    assert!(over.stdout.is_empty(), "stdout stays empty on error");
    assert!(
        !s.db().exists(),
        "an oversized body never opens the database"
    );

    // the body and the argument flags are exclusive
    let both = with_stdin(
        {
            let mut c = kaban(&s);
            c.args(["--stdin-json", "forget", "--mode", "archive"]);
            c
        },
        b"{}",
    );
    assert_eq!(both.status.code(), Some(4), "{}", stderr(&both));
    assert!(stderr(&both).contains("not both"));

    // a subcommand without a request body refuses the flag instead of ignoring it
    let refused = kaban(&s).args(["--stdin-json", "scan"]).output().unwrap();
    assert_eq!(refused.status.code(), Some(4), "{}", stderr(&refused));

    // a body that is not one JSON object is refused with a fixed text (never echoed)
    let not_object = with_stdin(
        {
            let mut c = kaban(&s);
            c.args(["--stdin-json", "forget"]);
            c
        },
        format!("[\"{CANARY}\"]").as_bytes(),
    );
    let err = stderr(&not_object);
    assert_eq!(not_object.status.code(), Some(4), "{err}");
    assert!(!err.contains(CANARY), "{err}");

    // a well-formed body reaches the seam like the MCP adapter would: an unknown id is 3
    let body = r#"{"id": "01a17a03-5cbc-7000-8000-000000000000", "mode": "archive"}"#;
    let not_found = with_stdin(
        {
            let mut c = kaban(&s);
            c.args(["--stdin-json", "forget"]);
            c
        },
        body.as_bytes(),
    );
    assert_eq!(not_found.status.code(), Some(3), "{}", stderr(&not_found));
}
