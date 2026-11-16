//! A startup hook that uses SQLite before kaban's bootstrap constructor makes the process refuse
//! startup by name, through an abort before `main` (spec 11 T12, QRY-07). ELF only: the test's
//! own hook relies on `.init_array` priorities to run first.
#![cfg(all(feature = "process-bootstrap", target_os = "linux"))]
#![allow(clippy::unwrap_used, clippy::panic)]

mod fresh;

use std::os::unix::process::ExitStatusExt;

use rusqlite::ffi;

const CASE: &str = "a_hook_using_sqlite_before_the_bootstrap_aborts_startup_by_name";

/// In the child of [`CASE`] only, initialize SQLite before kaban's constructor runs.
extern "C" fn early_sqlite_user() {
    if fresh::is_child(CASE) {
        // SAFETY: plain FFI call with no arguments, on the only thread of a starting process.
        let _ = unsafe { ffi::sqlite3_initialize() };
    }
}

// SAFETY: one function pointer in the init array, at a priority before kaban's (00101); the
// function takes no arguments that matter and never unwinds.
#[used]
#[unsafe(link_section = ".init_array.00100")]
static EARLY_SQLITE_USER: extern "C" fn() = early_sqlite_user;

#[test]
fn a_hook_using_sqlite_before_the_bootstrap_aborts_startup_by_name() {
    assert!(
        !fresh::is_child(CASE),
        "startup was not refused: the test body ran"
    );
    let output = fresh::spawn_child(CASE);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.signal(),
        Some(6),
        "the child must abort (SIGABRT); status {}, stderr:\n{stderr}",
        output.status
    );
    assert!(
        stderr.contains("startup refused") && stderr.contains("initialized by the host"),
        "the refusal must name the error:\n{stderr}"
    );
    assert!(!String::from_utf8_lossy(&output.stdout).contains("running"));
}
