//! Identifier vocabulary diagnostics are read-only and never disclose malformed markers.
#![allow(clippy::unwrap_used)]

mod common;

use common::Scratch;
use kaban_api::{DoctorContext, DoctorReport, KabanApi};
use kaban_core::{
    Actor, DatabaseFileModes, IngestStore, MemoryStore, NewNode, NodeId, RememberOutcome,
    WorkspaceId, WriteCtx, ident::IDENT_TERMS_VERSION,
};
use kaban_policy::Policy;
use kaban_store_sqlite::{LATEST_SCHEMA_VERSION, SqliteStore};
use std::sync::Arc;

fn context(scratch: &Scratch) -> DoctorContext {
    DoctorContext {
        files: DatabaseFileModes {
            db: DatabaseFileModes::OWNER_ONLY,
            wal: None,
            shm: None,
        },
        schema_version: LATEST_SCHEMA_VERSION,
        keys_dir: scratch.keys(),
        vacuum: false,
        priors: false,
    }
}

fn ident_check(report: &DoctorReport) -> &kaban_api::DoctorCheck {
    report
        .checks
        .iter()
        .find(|row| row.name == "ident_terms_version")
        .unwrap()
}

#[test]
fn current_and_noncurrent_markers_are_diagnosed_without_mutation_or_disclosure() {
    let scratch = Scratch::new("doctor-ident", "markers");
    let store = Arc::new(SqliteStore::open(&scratch.db(), &scratch.keys()).unwrap());
    let outcome = store
        .remember(
            NewNode {
                kind: "Fact".into(),
                text: "CanonicalReader".into(),
                ..Default::default()
            },
            &[],
            &WriteCtx::new("v1", WorkspaceId::default(), Actor::Agent),
        )
        .unwrap();
    let RememberOutcome::Created(node) = outcome else {
        unreachable!()
    };
    let id = NodeId::parse(&node.id).unwrap();
    let api = KabanApi::new(store.clone(), Policy::empty(), store.signer(), None);
    let canary = "RAW_IDENT_METADATA_CANARY";
    for marker in [
        Some(IDENT_TERMS_VERSION),
        Some("0"),
        None,
        Some(canary),
        Some("999"),
    ] {
        if let Some(marker) = marker {
            store.set_index_meta("ident_terms_version", marker).unwrap();
        } else {
            // The public seam sets entries; deletion models a legacy absent marker.
            rusqlite::Connection::open(scratch.db())
                .unwrap()
                .execute("DELETE FROM index_meta WHERE key='ident_terms_version'", [])
                .unwrap();
        }
        let metadata = store.index_meta().unwrap().entries;
        let canonical = serde_json::to_value(store.get(&id, false).unwrap()).unwrap();
        let report = api.doctor(&context(&scratch));
        let row = ident_check(&report);
        assert_eq!(row.ok, marker == Some(IDENT_TERMS_VERSION));
        if row.ok {
            assert_eq!(row.detail, IDENT_TERMS_VERSION);
            assert!(report.ok, "{report:?}");
        } else {
            assert!(!report.ok);
            assert!(row.detail.contains("kaban reindex text"));
        }
        assert!(!serde_json::to_string(&report).unwrap().contains(canary));
        assert!(
            report
                .checks
                .iter()
                .any(|row| row.name == "audit" && row.ok)
        );
        assert_eq!(store.index_meta().unwrap().entries, metadata);
        assert_eq!(
            serde_json::to_value(store.get(&id, false).unwrap()).unwrap(),
            canonical
        );
    }
}

#[test]
fn metadata_read_failure_is_named_and_does_not_hide_siblings() {
    let scratch = Scratch::new("doctor-ident", "read-failure");
    let store = Arc::new(SqliteStore::open(&scratch.db(), &scratch.keys()).unwrap());
    let api = KabanApi::new(store.clone(), Policy::empty(), store.signer(), None);
    rusqlite::Connection::open(scratch.db())
        .unwrap()
        .execute("DROP TABLE index_meta", [])
        .unwrap();
    let report = api.doctor(&context(&scratch));
    let row = ident_check(&report);
    assert!(!row.ok);
    assert_eq!(row.detail, "not checked: backend read failed");
    for name in [
        "db_modes",
        "integrity",
        "fts_integrity",
        "layers",
        "audit",
        "unresolved",
    ] {
        assert!(
            report.checks.iter().any(|row| row.name == name),
            "{report:?}"
        );
    }
}
