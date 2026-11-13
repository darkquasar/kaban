//! Review RM-1 (core-spec S1): a storage backend failure never renders the driver's text on a
//! CLI boundary. The driver message of the real SQLite backend may quote SQL text or bound
//! values, so the open paths (`serve`, `rescan`, `remember`, `bench`) print only
//! the safe outer message, never a chained `{:#}` rendering, and never hand a store-backed
//! `anyhow::Error` to Rust's top-level reporter (which prints the whole chain).
//!
//! The canary is a string no policy rule matches, so its absence proves the rendering itself
//! is content-free rather than that a denylist gate caught it.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::Scratch;
use kaban_store_sqlite::SqliteStore;
use std::process::{Command, Output, Stdio};

const CANARY: &str = "CANARY-driver-text-7f3a9c";

/// A database whose open-time seed fails inside the driver with a message carrying the canary:
/// `SqliteStore::open` runs `INSERT OR IGNORE INTO schema_type` in its seed transaction, and
/// the trigger raises before the conflict check.
fn poisoned_db(s: &Scratch) {
    drop(SqliteStore::open(&s.db(), &s.home().join("keys")).unwrap());
    let conn = rusqlite::Connection::open(s.db()).unwrap();
    conn.execute_batch(&format!(
        "CREATE TRIGGER poison BEFORE INSERT ON schema_type BEGIN SELECT RAISE(ABORT, '{CANARY}'); END;"
    ))
    .unwrap();
}

fn kaban(s: &Scratch, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_kaban"))
        .env("KABAN_HOME", s.home())
        .env("RUST_LOG", "trace")
        .args(["--db", s.db().to_str().unwrap()])
        .args(args)
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn assert_safe_failure(out: &Output, what: &str) {
    let err = String::from_utf8_lossy(&out.stderr);
    assert_ne!(out.status.code(), Some(0), "{what}: must fail: {err}");
    assert!(
        !err.contains(CANARY),
        "{what}: the driver text reached stderr: {err}"
    );
    assert!(
        err.contains("storage backend error"),
        "{what}: the safe outer message is printed: {err}"
    );
    assert!(out.stdout.is_empty(), "{what}: stdout stays empty on error");
}

#[test]
fn a_driver_error_on_open_never_reaches_stderr_on_any_cli_path() {
    let s = Scratch::new("kaban-cli-errors", "driver-canary");
    poisoned_db(&s);
    // the driver really does carry the canary: this is what the CLI must not print
    let driver = SqliteStore::open(&s.db(), &s.home().join("keys"))
        .expect_err("the poisoned seed fails the open");
    let held = std::error::Error::source(&driver)
        .and_then(|src| src.downcast_ref::<kaban_core::BackendSource>())
        .map(|holder| holder.driver().to_string())
        .expect("a backend failure with its driver error held");
    assert!(held.contains(CANARY), "fixture: {held}");

    assert_safe_failure(&kaban(&s, &["rescan"]), "rescan");
    assert_safe_failure(
        &kaban(&s, &["remember", "Fact", "a plain note"]),
        "remember",
    );
    assert_safe_failure(&kaban(&s, &["serve"]), "serve");
    // `bench` recreates its database, so the poisoned file cannot reach its open path; its
    // boundary is covered by the structural rule (no chained rendering, no top-level `anyhow`)
}

/// The chained `anyhow` renderings a binary reaches for by habit (`{:#}` and the top-level
/// reporter's `{:?}`) are content-free too: the holder ends the chain before the driver text.
#[test]
fn anyhow_chain_renderings_of_a_store_error_carry_no_driver_text() {
    #[derive(Debug)]
    struct Driver;
    impl std::fmt::Display for Driver {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "near \"{CANARY}\": syntax error")
        }
    }
    impl std::error::Error for Driver {}
    let store_err = kaban_core::StoreError::Backend {
        source: kaban_core::BackendSource::new(Driver),
    };
    let e = anyhow::Error::from(store_err).context("opening database");
    for rendered in [format!("{e}"), format!("{e:#}"), format!("{e:?}")] {
        assert!(
            !rendered.contains(CANARY),
            "an anyhow rendering carries the driver text: {rendered}"
        );
    }
    assert!(format!("{e:#}").contains("storage backend error"));
}
