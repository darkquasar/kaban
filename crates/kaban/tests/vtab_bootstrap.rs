//! The standalone CLI installs kaban-vtab's SQLite allocator adapter before `main` (spec 11 T12,
//! QRY-07): black-box through the real binary. The observable is the one line kaban-vtab's
//! startup constructor writes to stderr when `KABAN_VTAB_BOOTSTRAP_REPORT=1`; `main` cannot
//! write it, so its presence as the first stderr line proves the constructor ran and installed
//! the adapter, and its absence without the variable proves the CLI stays silent by default.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

/// How long `kaban --version` may run before it is killed and the test fails.
const LIMIT: Duration = Duration::from_secs(30);

/// The variable kaban-vtab's bootstrap reads, and the line it writes (kaban-vtab's
/// `bootstrap::REPORT_ENV` and `bootstrap::REPORT_LINE`).
const REPORT_ENV: &str = "KABAN_VTAB_BOOTSTRAP_REPORT";
const REPORT_LINE: &str = "kaban-vtab: sqlite allocator adapter installed before main";

fn version(report: bool) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_kaban"));
    cmd.arg("--version")
        .env_remove(REPORT_ENV)
        .env("RUST_LOG", "warn")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if report {
        cmd.env(REPORT_ENV, "1");
    }
    let mut child = cmd.spawn().unwrap();
    let started = Instant::now();
    while child.try_wait().unwrap().is_none() {
        if started.elapsed() > LIMIT {
            child.kill().unwrap();
            panic!("kaban --version ran past {LIMIT:?} and was killed");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    child.wait_with_output().unwrap()
}

#[test]
fn the_cli_installs_the_allocator_adapter_before_main() {
    let output = version(true);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{}: {stderr}", output.status);
    assert_eq!(
        stderr.lines().next(),
        Some(REPORT_LINE),
        "the bootstrap line must be the first stderr line:\n{stderr}"
    );
    assert!(String::from_utf8_lossy(&output.stdout).starts_with("kaban "));
}

#[test]
fn the_cli_reports_nothing_without_the_variable() {
    let output = version(false);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{}: {stderr}", output.status);
    assert!(!stderr.contains(REPORT_LINE), "{stderr}");
}
