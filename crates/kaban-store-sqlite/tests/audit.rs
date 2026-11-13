//! Audit chain in SQLite (core-spec S3; core-plan C1.4): one append-only HMAC chain per
//! database, written by whichever process holds SQLite's write lock, verifiable offline.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::Scratch;
use kaban_core::{AuditEntry, AuditOp, AuditStore, WorkspaceId};
use kaban_store_sqlite::SqliteStore;
use std::path::Path;

fn rejection(i: usize) -> AuditEntry {
    AuditEntry {
        op: AuditOp::Reject,
        tool: "remember".into(),
        field: "text".into(),
        workspace_id: WorkspaceId::new("ws-a"),
        policy_version: "pv1".into(),
        input_hmac: format!("{i:064x}"),
        rules: vec!["aws-access-key-id".into()],
        node_id: None,
    }
}

fn open(db: &Path, keys: &Path) -> SqliteStore {
    SqliteStore::open(db, keys).unwrap()
}

#[test]
fn two_writers_interleave_into_one_unforked_chain() {
    let s = Scratch::new("kaban-audit", "interleave");
    let first = open(&s.db(), &s.keys());
    let second = open(&s.db(), &s.keys());
    assert_eq!(first.db_id(), second.db_id(), "db_id lives in the database");
    let key_files: Vec<_> = std::fs::read_dir(s.keys()).unwrap().collect();
    assert_eq!(key_files.len(), 1, "both openers share one key file");
    std::thread::scope(|sc| {
        sc.spawn(|| {
            for i in 0..200 {
                first.audit_append(&[rejection(i)]).unwrap();
            }
        });
        sc.spawn(|| {
            for i in 200..400 {
                second.audit_append(&[rejection(i)]).unwrap();
            }
        });
    });
    let v = first.audit_verify().unwrap();
    assert_eq!(v.entries, 400);
    assert!(v.ok, "{v:?}");
    assert_eq!(v.first_bad_seq, None);
    // a third opener (fresh process) verifies the same chain with the key from disk
    let third = open(&s.db(), &s.keys());
    assert!(third.audit_verify().unwrap().ok);
}

#[test]
fn tampering_one_row_breaks_verification_at_that_seq() {
    let s = Scratch::new("kaban-audit", "tamper");
    let a = open(&s.db(), &s.keys());
    for i in 0..5 {
        a.audit_append(&[rejection(i)]).unwrap();
    }
    drop(a);
    // the append-only triggers refuse UPDATE/DELETE through SQL ...
    let raw = rusqlite::Connection::open(s.db()).unwrap();
    let err = raw
        .execute("UPDATE audit SET rules = '[]' WHERE seq = 3", [])
        .unwrap_err();
    assert!(err.to_string().contains("append-only"), "{err}");
    assert!(raw.execute("DELETE FROM audit WHERE seq = 3", []).is_err());
    // ... so an attacker with file access must drop them first; the chain still tells
    raw.execute_batch(
        "DROP TRIGGER audit_no_update; UPDATE audit SET rules = '[\"edited\"]' WHERE seq = 3;",
    )
    .unwrap();
    drop(raw);
    let b = open(&s.db(), &s.keys());
    let v = b.audit_verify().unwrap();
    assert!(!v.ok);
    assert_eq!(v.first_bad_seq, Some(3));
    assert_eq!(v.entries, 3, "verification stops at the first bad row");
}

#[test]
fn zero_length_key_file_is_rejected_at_startup() {
    let s = Scratch::new("kaban-audit", "zero-key");
    let db_id = open(&s.db(), &s.keys()).db_id().to_string();
    let key = s.keys().join(format!("{db_id}.key"));
    assert_eq!(std::fs::metadata(&key).unwrap().len(), 32);
    std::fs::write(&key, b"").unwrap();
    let err = SqliteStore::open(&s.db(), &s.keys()).unwrap_err();
    assert!(matches!(err, kaban_core::StoreError::Audit(_)), "{err:?}");
    let chain = format!("{err}: {}", std::error::Error::source(&err).unwrap());
    assert!(chain.contains("0 bytes") && chain.contains("32"), "{chain}");
    // a key of the wrong (non-zero) length is refused too
    std::fs::write(&key, [7u8; 16]).unwrap();
    assert!(SqliteStore::open(&s.db(), &s.keys()).is_err());
}

/// Review R1-7: the chain input is a length-prefixed encoding, so moving bytes between two
/// adjacent fields (here `workspace_id` -> `policy_version`) changes the HMAC even when the
/// bytes contain the old field separator; the verifier catches the redistribution at that seq.
#[test]
fn redistributing_separator_bytes_between_adjacent_fields_breaks_verification() {
    let s = Scratch::new("kaban-audit", "redistribute");
    let a = open(&s.db(), &s.keys());
    a.audit_append(&[rejection(0)]).unwrap();
    let mut moved = rejection(1);
    moved.workspace_id = WorkspaceId::new("ws\u{1f}x");
    moved.policy_version = "pv".into();
    a.audit_append(&[moved]).unwrap();
    a.audit_append(&[rejection(2)]).unwrap();
    assert!(a.audit_verify().unwrap().ok);
    drop(a);
    // same byte stream under a separator-joined encoding: "ws" + SEP + "x" + SEP + "pv" + SEP
    let raw = rusqlite::Connection::open(s.db()).unwrap();
    raw.execute_batch("DROP TRIGGER audit_no_update;").unwrap();
    raw.execute(
        "UPDATE audit SET workspace_id = 'ws', policy_version = ? WHERE seq = 2",
        ["x\u{1f}pv"],
    )
    .unwrap();
    drop(raw);
    let b = open(&s.db(), &s.keys());
    let v = b.audit_verify().unwrap();
    assert!(!v.ok, "field boundaries moved but the chain still verified");
    assert_eq!(v.first_bad_seq, Some(2));
    assert_eq!(v.entries, 2, "verification stops at the redistributed row");
}
