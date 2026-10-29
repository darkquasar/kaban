//! RW3-1 (wave-3 astra review; core-spec S1): a typed-argument failure on the CLI side of the
//! seam (`--stdin-json`) is a fixed text. serde quotes a wrong-type value in double quotes and
//! an unknown variant in backticks; the CLI's answer carries the expected shape and no byte of
//! the value, whatever the quoting (escaped quotes, mixed delimiters, an ellipsis). The exit is
//! 4 (invalid request) and stdout stays empty. Failure diagnostics print no byte of the output.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::Scratch;
use std::io::Write;
use std::process::{Command, Output, Stdio};

const EXIT_INVALID: i32 = 4;

fn with_stdin(s: &Scratch, args: &[&str], body: &[u8]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_kaban"))
        .env("KABAN_HOME", s.home())
        .env("RUST_LOG", "warn")
        .args(["--db", s.db().to_str().unwrap()])
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let _ = child.stdin.take().unwrap().write_all(body);
    child.wait_with_output().unwrap()
}

/// The three quotings of a wrong scalar type on `recall.full_text` (a boolean field) and an
/// unknown variant; each marker is absent from stderr and stdout, the shape is present.
#[test]
fn a_wrong_scalar_type_on_stdin_json_is_refused_with_a_fixed_text() {
    let s = Scratch::new("kaban-cli-typed-args", "wrong-type");
    let cases: [(&str, &str, &str, &str); 4] = [
        (
            "recall",
            r#"{"query": "q", "full_text": "yes-ZXQV9"}"#,
            "ZXQV9",
            "expected a boolean",
        ),
        (
            "recall",
            r#"{"query": "q", "full_text": "yes\"QWJ7X-tail\" please"}"#,
            "QWJ7X",
            "expected a boolean",
        ),
        (
            "recall",
            r#"{"query": "q", "full_text": "`mixed\" …K4M2P` \"and` more"}"#,
            "K4M2P",
            "expected a boolean",
        ),
        (
            "recall",
            r#"{"query": "q", "hops": "side`ways\"-R8TY3"}"#,
            "R8TY3",
            "expected",
        ),
    ];
    for (case, (cmd, body, marker, shape)) in cases.into_iter().enumerate() {
        let out = with_stdin(&s, &[cmd, "--stdin-json"], body.as_bytes());
        let err = String::from_utf8_lossy(&out.stderr).into_owned();
        assert_eq!(
            out.status.code(),
            Some(EXIT_INVALID),
            "case {case}: a mistyped field is an invalid request (exit 4)"
        );
        assert!(
            out.stdout.is_empty(),
            "case {case}: stdout stays empty on error"
        );
        // the property (RW3-1): no caller byte reaches the terminal, whatever the quoting
        assert!(
            !err.contains(marker),
            "case {case}: the caller's value is echoed on stderr"
        );
        // positive: the shape of the error survives as fixed text
        assert!(
            err.contains(shape),
            "case {case}: the error lost its shape ({shape} absent)"
        );
    }
}
