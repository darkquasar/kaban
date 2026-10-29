//! The SQL error boundary (`GM2a` RG2-1; G1-S8 "no echo"; core-spec S1): a driver failure on the
//! read or the write side is typed (`ReadError::Sql`, `WriteError::Sql`) but renders no driver
//! text through `Display`, `Debug`, alternate `Debug` or a chain walker (`anyhow`), while the
//! driver error itself stays downcastable behind the `BackendSource` holder for a test or an
//! operator that asks for it explicitly.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

#[path = "common/builder.rs"]
mod builder;

use std::error::Error;

use builder::{Snapshot, commit, scratch};
use kaban_codegraph::{CodeGraphReader, ReadError, SqliteSnapshot, WriteError};

/// An invented diagnostic marker planted where SQLite will quote it in its own message.
const MARKER: &str = "diag_marker_7f3a9c1e";

/// Every rendering a consumer could reach: `Display`, `{:?}`, `{:#?}`, and the `anyhow` chain in
/// its `{:#}` (inline causes) and `{:?}` (`Caused by:` list) forms.
fn renderings(err: impl Error + Send + Sync + 'static) -> Vec<String> {
    let display = err.to_string();
    let debug = format!("{err:?}");
    let alternate = format!("{err:#?}");
    let chain = anyhow::Error::new(err);
    vec![
        display,
        debug,
        alternate,
        format!("{chain:#}"),
        format!("{chain:?}"),
    ]
}

/// The holder the chain ends at, and the driver error behind it.
fn driver_behind<'a>(err: &'a (dyn Error + 'static)) -> &'a rusqlite::Error {
    let holder = err
        .source()
        .expect("the SQL variant keeps its source")
        .downcast_ref::<kaban_core::BackendSource>()
        .expect("the source is the redacting holder");
    assert!(
        holder.source().is_none(),
        "the chain ends at the holder, never at the driver"
    );
    holder
        .driver()
        .downcast_ref::<rusqlite::Error>()
        .expect("the held error is the rusqlite error itself")
}

#[test]
fn a_read_failure_renders_no_driver_text_and_keeps_the_driver_downcastable() {
    let dir = scratch();
    let commit = commit(0xe0);
    let path = Snapshot::new(&commit).write_final(dir.path(), &commit);
    // a view over `files` calling an unknown function named after the marker: `quick_check`
    // passes (views are schema text), every query touching `files` fails with "no such
    // function: <marker>" (the reviewer's probe)
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(&format!(
        "ALTER TABLE files RENAME TO files_real; \
         CREATE VIEW files AS SELECT id, path, language, is_test, lines FROM files_real \
         WHERE {MARKER}(path);"
    ))
    .unwrap();
    drop(conn);

    let snap = SqliteSnapshot::open_ro(&path).expect("admitted: quick_check passes");
    let err = snap
        .node("file:x")
        .expect_err("the view's function is unknown");
    assert!(matches!(err, ReadError::Sql(_)), "{err:?}");
    assert_eq!(err.to_string(), "sqlite failed");
    for text in renderings(snap.node("file:x").unwrap_err()) {
        assert!(!text.contains(MARKER), "driver text leaked: {text}");
        assert!(
            !text.contains("no such function"),
            "driver text leaked: {text}"
        );
    }
    let driver = driver_behind(&err);
    assert!(
        driver.to_string().contains(MARKER),
        "the driver's diagnosis is intact for an explicit downcast: {driver}"
    );
}

#[test]
fn a_write_failure_renders_no_driver_text_and_keeps_the_driver_downcastable() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    kaban_codegraph::write::begin(&conn).unwrap();
    // a trigger that aborts every `meta` insert with the marker as SQLite's own message
    conn.execute_batch(&format!(
        "CREATE TRIGGER refuse_meta BEFORE INSERT ON meta BEGIN SELECT RAISE(ABORT, '{MARKER}'); END;"
    ))
    .unwrap();
    let fail = || kaban_codegraph::write::insert_meta(&conn, &[("commit", "x")]).unwrap_err();

    let err = fail();
    assert!(matches!(err, WriteError::Sql(_)), "{err:?}");
    assert_eq!(err.to_string(), "sqlite failed");
    for text in renderings(fail()) {
        assert!(!text.contains(MARKER), "driver text leaked: {text}");
    }
    let driver = driver_behind(&err);
    assert!(
        driver.to_string().contains(MARKER),
        "the driver's diagnosis is intact for an explicit downcast: {driver}"
    );
}
