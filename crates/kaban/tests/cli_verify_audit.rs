//! `kaban verify-audit` recomputes the audit chain (core-spec S3; core-plan C1.4). The key is
//! `$KABAN_HOME/keys/<db-id>.key`; a tampered row makes the command exit 5 and name the seq.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::Scratch;
use kaban_core::{AuditEntry, AuditOp, AuditStore, WorkspaceId};
use kaban_store_sqlite::SqliteStore;
use std::path::PathBuf;
use std::process::Command;

fn kaban(s: &Scratch) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_kaban"));
    c.env("KABAN_HOME", s.home())
        .args(["--db", s.db().to_str().unwrap()]);
    c
}

#[test]
fn verify_audit_reports_ok_then_names_the_tampered_seq() {
    let s = Scratch::new("kaban-verify", "ok-then-tamper");
    {
        let store = SqliteStore::open(&s.db(), &s.home().join("keys")).unwrap();
        for i in 0..3 {
            store
                .audit_append(&[AuditEntry {
                    op: AuditOp::Reject,
                    tool: "remember".into(),
                    field: "text".into(),
                    workspace_id: WorkspaceId::default(),
                    policy_version: "pv".into(),
                    input_hmac: format!("{i:064x}"),
                    rules: vec!["jwt".into()],
                    node_id: None,
                }])
                .unwrap();
        }
    }
    let out = kaban(&s).arg("verify-audit").output().unwrap();
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["ok"], serde_json::json!(true));
    assert_eq!(v["entries"], serde_json::json!(3));

    let raw = rusqlite::Connection::open(s.db()).unwrap();
    raw.execute_batch("DROP TRIGGER audit_no_update; UPDATE audit SET tool = 'x' WHERE seq = 2;")
        .unwrap();
    drop(raw);
    let out = kaban(&s).arg("verify-audit").output().unwrap();
    assert_eq!(out.status.code(), Some(5), "{out:?}");
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["ok"], serde_json::json!(false));
    assert_eq!(v["first_bad_seq"], serde_json::json!(2));
}

#[test]
fn verify_audit_with_a_missing_key_fails_closed() {
    let s = Scratch::new("kaban-verify", "missing-key");
    {
        let _ = SqliteStore::open(&s.db(), &s.home().join("keys")).unwrap();
    }
    // the key lives outside the data directory: point the binary at a different home
    let other = s.0.join("other-home");
    let out = Command::new(env!("CARGO_BIN_EXE_kaban"))
        .env("KABAN_HOME", &other)
        .args(["--db", s.db().to_str().unwrap(), "verify-audit"])
        .output()
        .unwrap();
    // a fresh home has no key for this db: creating one would silently fork the chain, so the
    // verifier must not create keys and must fail
    assert_ne!(out.status.code(), Some(0), "{out:?}");
    assert!(
        !other.join("keys").exists() || std::fs::read_dir(other.join("keys")).unwrap().count() == 0
    );
}

fn write_chain(s: &Scratch, rows: usize) {
    let store = SqliteStore::open(&s.db(), &s.home().join("keys")).unwrap();
    for i in 0..rows {
        store
            .audit_append(&[AuditEntry {
                op: AuditOp::Reject,
                tool: "remember".into(),
                field: "text".into(),
                workspace_id: WorkspaceId::default(),
                policy_version: "pv".into(),
                input_hmac: format!("{i:064x}"),
                rules: vec!["jwt".into()],
                node_id: None,
            }])
            .unwrap();
    }
}

fn schema_version(s: &Scratch) -> Option<String> {
    let conn = rusqlite::Connection::open(s.db()).unwrap();
    conn.query_row(
        "SELECT value FROM index_meta WHERE key = 'schema_version'",
        [],
        |r| r.get(0),
    )
    .ok()
}

fn backups(s: &Scratch) -> Vec<PathBuf> {
    std::fs::read_dir(&s.0)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("kaban.db.bak-"))
        })
        .collect()
}

/// OWN-P: a database at another schema version is refused by the verifier with the way out
/// (exit 4), never migrated or backed up: an unversioned M1-era file here.
#[test]
fn verify_audit_refuses_an_older_database_without_migrating_or_backing_it_up() {
    let s = Scratch::new("kaban-verify", "no-migrate");
    write_chain(&s, 3);
    // an M1-era file: index_meta(db_id) and the chain, but no schema_version key (MD-1)
    let raw = rusqlite::Connection::open(s.db()).unwrap();
    raw.execute("DELETE FROM index_meta WHERE key = 'schema_version'", [])
        .unwrap();
    drop(raw);
    let out = kaban(&s).arg("verify-audit").output().unwrap();
    assert_eq!(out.status.code(), Some(4), "{out:?}");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("schema version 1 ") && err.contains("re-ingest"),
        "the refusal names the version and the way out: {err}"
    );
    assert_eq!(
        schema_version(&s),
        None,
        "the verifier must not migrate the database (MD-28, OWN-P)"
    );
    assert!(
        backups(&s).is_empty(),
        "and takes no backup: {:?}",
        backups(&s)
    );
}

#[test]
fn verify_audit_refuses_a_database_from_a_newer_kaban() {
    let s = Scratch::new("kaban-verify", "newer");
    write_chain(&s, 1);
    let raw = rusqlite::Connection::open(s.db()).unwrap();
    raw.execute(
        "UPDATE index_meta SET value = '999' WHERE key = 'schema_version'",
        [],
    )
    .unwrap();
    drop(raw);
    let out = kaban(&s).arg("verify-audit").output().unwrap();
    assert_eq!(out.status.code(), Some(4), "{out:?}");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("999") && err.contains("re-ingest"),
        "a clear refusal with the way out: {err}"
    );
    assert_eq!(schema_version(&s).as_deref(), Some("999"), "untouched");
}
