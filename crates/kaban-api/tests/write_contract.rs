//! REC-08: the write contract after governance. A `remember` write stores an active record at
//! commit, and a second process's `recall` returns it with no proposed or candidate state and no
//! policy stamp; `kaban-policy` still redacts secrets on write and on read.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::Scratch;
use kaban_api::{KabanApi, RecallRequest, RememberRequest, RememberStatus};
use kaban_core::{Lifecycle, MemoryStore, NodeId};
use kaban_policy::Policy;
use kaban_store_sqlite::SqliteStore;
use serde_json::json;
use std::sync::Arc;

/// A denylisted word the test policy redacts.
const CANARY: &str = "hunter2";
const POLICY: &[u8] = b"words:\n  - { term: hunter2, action: redact, id: pw }\n";

/// One process's view of the project database: its own store connection and API.
fn open(s: &Scratch) -> (Arc<SqliteStore>, KabanApi) {
    let policy = Policy::from_file(Policy::parse(POLICY).unwrap(), "test".into()).unwrap();
    let store = Arc::new(SqliteStore::open(&s.db(), &s.keys()).unwrap());
    let signer = store.signer();
    (store.clone(), KabanApi::new(store, policy, signer, None))
}

fn recall_one(api: &KabanApi, query: &str) -> serde_json::Value {
    let found = api
        .recall(RecallRequest {
            query: query.into(),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(found.count, 1, "{query}: {found:?}");
    serde_json::to_value(&found.results[0]).unwrap()
}

#[test]
fn a_remembered_decision_is_active_for_a_second_process_with_no_governance_state() {
    let s = Scratch::new("kaban-write-contract", "active");
    let (_, writer) = open(&s);
    let put = writer
        .remember(RememberRequest {
            kind: "Decision".into(),
            text: "Use SQLite for the write contract store".into(),
            props: Some(json!({"decision": "Use SQLite", "rationale": "one file"})),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(put.status, RememberStatus::Created);
    let (_, reader) = open(&s);
    let card = recall_one(&reader, "write contract store");
    assert_eq!(card["id"], json!(put.id), "{card}");
    assert_eq!(
        card["lifecycle"],
        json!(Lifecycle::Active.as_str()),
        "{card}"
    );
    for field in ["governance", "policy_version", "candidate", "proposed"] {
        assert!(card.get(field).is_none(), "card.{field}: {card}");
    }
}

#[test]
fn a_secret_bearing_body_is_redacted_on_write_and_on_read() {
    let s = Scratch::new("kaban-write-contract", "redact");
    let (_, writer) = open(&s);
    let put = writer
        .remember(RememberRequest {
            kind: "Fact".into(),
            text: format!("deploy note key {CANARY} rotated"),
            ..Default::default()
        })
        .unwrap();
    assert!(!put.redactions.is_empty(), "{put:?}");
    let (store, reader) = open(&s);
    let stored = store
        .get(&NodeId::parse(&put.id).unwrap(), true)
        .unwrap()
        .unwrap();
    assert!(!stored.text.contains(CANARY), "stored: {}", stored.text);
    let card = recall_one(&reader, "deploy");
    assert!(!card.to_string().contains(CANARY), "{card}");
}
