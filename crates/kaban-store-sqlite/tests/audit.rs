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

/// The packs of these tests include no code-graph snapshot, so its admission is never run.
fn no_snapshot(_: &Path) -> Result<String, String> {
    Err("no snapshot is included in this test".into())
}

fn no_index(_: &Path) -> Result<serde_json::Value, String> {
    Err("no index component is included in this test".into())
}

/// The adapter's admission of the pack's index component (what the binary hands the store).
fn admitted_index(path: &Path) -> Result<serde_json::Value, String> {
    kaban_index_tantivy::TantivyIndex::admit_captured(path).map_err(|e| e.to_string())
}

const NOTHING_ADMITTED: kaban_store_sqlite::pack::Admission = kaban_store_sqlite::pack::Admission {
    snapshot: no_snapshot,
    index: no_index,
};

/// The admission of a pack built with the text index component (every real pack).
const INDEX_ADMITTED: kaban_store_sqlite::pack::Admission = kaban_store_sqlite::pack::Admission {
    snapshot: no_snapshot,
    index: admitted_index,
};

/// A held text-index component whose every drain lets a writer slip in before the builder's
/// next lock: what a continuously written database looks like to `pack build` (T4 review
/// t4-8). Each drain records the bounds it was given.
struct SlippingWriter<'a> {
    store: &'a SqliteStore,
    /// Per drain: the target given, the head at the call, the deadline given.
    drains: std::sync::Mutex<Vec<(u64, u64, std::time::Instant)>>,
}

impl kaban_store_sqlite::pack::TextIndexComponent for SlippingWriter<'_> {
    fn with_held(
        &self,
        body: &mut dyn FnMut(
            &mut dyn kaban_store_sqlite::pack::HeldTextIndex,
        ) -> Result<
            kaban_store_sqlite::pack::Manifest,
            kaban_store_sqlite::pack::PackError,
        >,
    ) -> Result<Option<kaban_store_sqlite::pack::Manifest>, kaban_store_sqlite::pack::PackError>
    {
        body(&mut SlippingHeld(self)).map(Some)
    }
}

struct SlippingHeld<'c, 'a>(&'c SlippingWriter<'a>);

impl kaban_store_sqlite::pack::HeldTextIndex for SlippingHeld<'_, '_> {
    fn drain(
        &mut self,
        target: u64,
        deadline: std::time::Instant,
    ) -> Result<Option<u64>, kaban_core::StoreError> {
        let head = kaban_core::ChangeLogStore::changes_head(self.0.store)?;
        self.0.drains.lock().unwrap().push((target, head, deadline));
        // published through the target; then a writer lands before the builder locks again
        self.0
            .store
            .audit_append(&[rejection(900 + self.0.drains.lock().unwrap().len())])?;
        kaban_core::MemoryStore::remember(
            self.0.store,
            kaban_core::NewNode {
                kind: "Fact".into(),
                text: format!("slipped in {}", self.0.drains.lock().unwrap().len()),
                ..Default::default()
            },
            &[],
            &kaban_core::WriteCtx::new("v1", WorkspaceId::new("ws-a"), kaban_core::Actor::Agent),
        )?;
        Ok(Some(target))
    }

    fn captured(
        &self,
    ) -> Result<Option<(std::path::PathBuf, serde_json::Value)>, kaban_core::StoreError> {
        panic!("a build that never quiesced captures nothing")
    }
}

/// T4 review t4-8 (SS-18): the builder's quiescence is bounded. Every drain is asked for the
/// head the builder read under its lock, with a deadline ahead of it, and a writer that slips
/// in between every drain and the next lock exhausts the tries: the build is busy after
/// exactly three bounded drains (`QUIESCE_TRIES`), with rising targets, and leaves no staging
/// behind.
#[test]
fn a_writer_that_keeps_the_log_moving_makes_the_build_busy_after_bounded_drains() {
    use kaban_core::ChangeLogStore;
    use kaban_store_sqlite::pack::{self, BuildRequest, PackError};
    let s = Scratch::new("kaban-audit", "quiesce-bounded");
    let store = open(&s.db(), &s.keys());
    store.audit_append(&[rejection(1)]).unwrap();
    let head = store.changes_head().unwrap();
    let component = SlippingWriter {
        store: &store,
        drains: std::sync::Mutex::new(Vec::new()),
    };
    let started = std::time::Instant::now();
    let pack_dir = s.0.join("pack");
    let outcome = pack::build(&BuildRequest {
        db: &s.db(),
        dest: &pack_dir,
        base_commit: "d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4",
        source_commit: None,
        policy_version: "pv1".into(),
        snapshots: &[],
        admit: NOTHING_ADMITTED,
        index: Some(&component),
    });
    assert!(matches!(outcome, Err(PackError::Busy)), "{outcome:?}");
    let drains = component.drains.lock().unwrap();
    assert_eq!(drains.len(), 3, "one bounded drain per try: {drains:?}");
    assert!(
        drains.iter().all(|(target, at_call, _)| target == at_call),
        "each drain is bounded by the head read under the lock: {drains:?}"
    );
    let targets: Vec<u64> = drains.iter().map(|(target, _, _)| *target).collect();
    assert_eq!(targets[0], head);
    assert!(
        targets.windows(2).all(|pair| pair[0] < pair[1]),
        "the slipped writes move the target: {targets:?}"
    );
    assert!(
        drains.iter().all(|(_, _, deadline)| *deadline > started),
        "each drain has a deadline ahead of the build"
    );
    assert!(!pack_dir.exists());
    let leftovers: Vec<_> = std::fs::read_dir(&s.0)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains("staging"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

/// Review e-2 (SS-20): forks race to one destination name. Every attempt passes the absence
/// check before any publishes; exactly one publishes, the others are refused as
/// `Destination` once the name is taken, the published database is the winner's (never
/// overwritten by a later attempt), and no attempt removes another's staging copy or key.
#[test]
fn concurrent_forks_to_one_destination_leave_exactly_one_fork() {
    use kaban_store_sqlite::pack::{self, BuildRequest, PackError};
    const ATTEMPTS: usize = 6;
    let s = Scratch::new("kaban-audit", "fork-race");
    let parent = open(&s.db(), &s.keys());
    parent.audit_append(&[rejection(1)]).unwrap();
    let pack_dir = s.0.join("pack");
    pack::build(&BuildRequest {
        db: &s.db(),
        dest: &pack_dir,
        base_commit: "d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4",
        source_commit: None,
        policy_version: "pv1".into(),
        snapshots: &[],
        admit: INDEX_ADMITTED,
        index: Some(&parent),
    })
    .unwrap();
    let keys_before = std::fs::read_dir(s.keys()).unwrap().count();
    let child_db = s.0.join("child.db");
    let barrier = std::sync::Barrier::new(ATTEMPTS);
    let outcomes: Vec<_> = std::thread::scope(|sc| {
        let handles: Vec<_> = (0..ATTEMPTS)
            .map(|_| {
                sc.spawn(|| {
                    barrier.wait();
                    pack::fork(&pack_dir, &child_db, &s.keys(), INDEX_ADMITTED)
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let (won, lost): (Vec<_>, Vec<_>) = outcomes.into_iter().partition(Result::is_ok);
    assert_eq!(won.len(), 1, "exactly one fork publishes: {won:?} {lost:?}");
    for lost in &lost {
        assert!(
            matches!(lost, Err(PackError::Destination)),
            "a loser is refused by the taken name: {lost:?}"
        );
    }
    let winner = won.into_iter().next().unwrap().unwrap();
    let child = open(&child_db, &s.keys());
    assert_eq!(
        child.db_id(),
        winner.instance_id,
        "the published database is the winner's"
    );
    assert!(
        child.audit_verify().unwrap().ok,
        "the winner's key is in place"
    );
    assert_eq!(
        std::fs::read_dir(s.keys()).unwrap().count(),
        keys_before + 1,
        "one key for the one published identity"
    );
    let leftovers: Vec<_> = std::fs::read_dir(&s.0)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains("forking"))
        .collect();
    assert_eq!(leftovers, Vec::<String>::new(), "no staging copy is left");
}

/// Spec 05 SS-20 at the store seam: a fork's chain starts at a genesis that binds the parent
/// pack and chains onto the parent's historical head; the child verifies from there under its
/// own key, the parent history under the parent's, and an altered binding fails the child.
#[test]
fn a_fork_genesis_binds_the_parent_and_verifies_from_it() {
    use kaban_store_sqlite::pack::{self, BuildRequest};
    let s = Scratch::new("kaban-audit", "fork-genesis");
    let parent = open(&s.db(), &s.keys());
    parent.audit_append(&[rejection(1), rejection(2)]).unwrap();
    let pack_dir = s.0.join("pack");
    let manifest = pack::build(&BuildRequest {
        db: &s.db(),
        dest: &pack_dir,
        base_commit: "d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4",
        source_commit: None,
        policy_version: "pv1".into(),
        snapshots: &[],
        admit: INDEX_ADMITTED,
        index: Some(&parent),
    })
    .unwrap();
    assert_eq!(manifest.audit.entries, 2);
    let child_db = s.0.join("child.db");
    let report = pack::fork(&pack_dir, &child_db, &s.keys(), INDEX_ADMITTED).unwrap();
    assert_eq!(report.genesis_seq, 3);
    assert_ne!(report.instance_id, manifest.instance_id);
    assert_eq!(report.pack_lineage_id, manifest.pack_lineage_id);

    let child = open(&child_db, &s.keys());
    assert_eq!(child.db_id(), report.instance_id);
    let own = child.audit_verify().unwrap();
    assert!(own.ok, "{own:?}");
    assert_eq!(own.entries, 1, "the child chain is its genesis: {own:?}");
    let provenance = child.audit_fork().unwrap().expect("a fork has a genesis");
    assert_eq!(provenance.genesis_seq, 3);
    let binding = provenance.binding.expect("the binding parses");
    assert_eq!(binding["parent_pack_id"], manifest.pack_id);
    assert_eq!(binding["parent_instance_id"], manifest.instance_id);
    assert_eq!(binding["parent_audit_head"], manifest.audit.head);
    let parent_key =
        kaban_policy::KeySigner::open_existing(&s.keys(), &manifest.instance_id).unwrap();
    let history = child
        .audit_verify_parent_history(&parent_key)
        .unwrap()
        .expect("a fork has parent history");
    assert!(history.ok, "{history:?}");
    assert_eq!(history.entries, 2);
    // a write after the fork extends the child chain, never the parent's history
    child.audit_append(&[rejection(3)]).unwrap();
    assert_eq!(child.audit_verify().unwrap().entries, 2);
    drop(child);

    // the binding altered behind the append-only triggers: the child chain fails at the genesis
    let raw = rusqlite::Connection::open(&child_db).unwrap();
    raw.execute_batch(
        "DROP TRIGGER audit_no_update;
         UPDATE audit SET rules = replace(rules, 'parent_audit_head', 'parent_audit_HEAD') WHERE op = 'fork';",
    )
    .unwrap();
    drop(raw);
    let child = open(&child_db, &s.keys());
    let broken = child.audit_verify().unwrap();
    assert!(!broken.ok, "{broken:?}");
    assert_eq!(broken.first_bad_seq, Some(3), "{broken:?}");
    // the parent history is a separate verdict and still holds
    assert!(
        child
            .audit_verify_parent_history(&parent_key)
            .unwrap()
            .unwrap()
            .ok
    );
}
