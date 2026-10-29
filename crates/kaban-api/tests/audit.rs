//! Audit behaviour through the service layer (core-spec S3; core-plan C1.4).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::{Scratch, fact};
use kaban_api::{ApiError, ForgetRequest, KabanApi, RecallRequest, RememberRequest};
use kaban_core::{AuditStore, ForgetMode, GovernanceStore, GraphStore, MemoryStore, NodeId};
use kaban_policy::{AuditExport, Policy};
use kaban_store_sqlite::SqliteStore;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;

const POLICY: &str = "words:\n  - { term: hunter2, action: redact, id: old-password }\n";

/// Audit-specific view of the scratch directory.
trait ScratchExt {
    /// Every byte SQLite may have written (main file, WAL, SHM).
    fn db_bytes(&self) -> Vec<u8>;
}
impl ScratchExt for Scratch {
    fn db_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for f in std::fs::read_dir(&self.0).unwrap() {
            let p = f.unwrap().path();
            if p.is_file()
                && p.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("kaban.db")
            {
                out.extend(std::fs::read(&p).unwrap());
            }
        }
        out
    }
}

fn api_on(s: &Scratch) -> (KabanApi, Arc<SqliteStore>) {
    let store = Arc::new(SqliteStore::open(&s.db(), &s.keys()).unwrap());
    let policy = Policy::from_file(Policy::parse(POLICY.as_bytes()).unwrap(), "t".into()).unwrap();
    let api = KabanApi::new(store.clone(), policy, store.signer(), None);
    (api, store)
}

fn node_count(store: &SqliteStore) -> i64 {
    store.stats(None).unwrap()["nodes"].as_i64().unwrap()
}

#[test]
fn audit_failure_rejects_the_write_and_stores_nothing() {
    let s = Scratch::new("kaban-api-audit", "fail-closed");
    let (api, store) = api_on(&s);
    // sabotage the chain of record after open (a read-only or truncated database behaves the
    // same way: the audit insert fails inside the write transaction)
    let raw = rusqlite::Connection::open(s.db()).unwrap();
    raw.execute_batch("DROP TRIGGER audit_no_delete; DROP TABLE audit;")
        .unwrap();
    drop(raw);
    // a rejection cannot be recorded -> fail closed with the audit marker, not the rule id
    match api.remember(fact("deploy password = Sup3rS3cretValue!")) {
        Err(ApiError::Rejected { rules, .. }) => {
            assert_eq!(rules, vec!["audit-unavailable".to_string()]);
        }
        other => panic!("expected fail-closed rejection, got {other:?}"),
    }
    // a redaction rides on the write transaction -> the whole write fails, nothing is stored
    assert!(api.remember(fact("staging note, old pw hunter2")).is_err());
    assert_eq!(node_count(&store), 0);
    // and a clean write without any audit entry still fails only if it needs the table: it
    // does not, so the store stays usable for clean content
    assert!(api.remember(fact("harmless")).is_ok());
}

#[test]
fn read_only_database_file_cannot_take_writes() {
    let s = Scratch::new("kaban-api-audit", "read-only");
    {
        let (api, _) = api_on(&s);
        api.remember(fact("seed")).unwrap();
    }
    for f in std::fs::read_dir(&s.0).unwrap() {
        let p = f.unwrap().path();
        if p.is_file() {
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o444)).unwrap();
        }
    }
    let opened = SqliteStore::open(&s.db(), &s.keys());
    let rejected = match opened {
        Err(_) => true, // refused at open: nothing can be written
        Ok(store) => {
            let store = Arc::new(store);
            let policy =
                Policy::from_file(Policy::parse(POLICY.as_bytes()).unwrap(), "t".into()).unwrap();
            let api = KabanApi::new(store.clone(), policy, store.signer(), None);
            let r = api.remember(fact("deploy password = Sup3rS3cretValue!"));
            matches!(r, Err(ApiError::Rejected { .. })) && node_count(&store) == 1
        }
    };
    assert!(rejected);
    for f in std::fs::read_dir(&s.0).unwrap() {
        let p = f.unwrap().path();
        if p.is_file() {
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
    }
}

fn audit_ops(s: &Scratch) -> Vec<(String, String, String)> {
    let raw = rusqlite::Connection::open(s.db()).unwrap();
    let mut st = raw
        .prepare("SELECT op, field, input_hmac FROM audit ORDER BY seq")
        .unwrap();
    st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
}

#[test]
fn forget_and_purge_write_audit_rows_in_the_same_transaction() {
    let s = Scratch::new("kaban-api-audit", "purge");
    let (api, store) = api_on(&s);
    let a = api.remember(fact("first note")).unwrap();
    let b = api.remember(fact("second note")).unwrap();
    assert_eq!(audit_ops(&s).len(), 0, "clean writes leave no audit rows");
    api.forget(ForgetRequest {
        id: a.id.clone(),
        mode: Some(ForgetMode::Invalidate),
    })
    .unwrap();
    api.forget(ForgetRequest {
        id: b.id.clone(),
        mode: Some(ForgetMode::Purge),
    })
    .unwrap();
    let ops = audit_ops(&s);
    assert_eq!(ops.len(), 2, "{ops:?}");
    assert_eq!((ops[0].0.as_str(), ops[0].1.as_str()), ("forget", "node"));
    assert_eq!((ops[1].0.as_str(), ops[1].1.as_str()), ("purge", "node"));
    assert_ne!(ops[1].2, b.id, "the row carries HMAC(id), not the id");
    assert!(
        store
            .get(&NodeId::parse(&b.id).unwrap(), false)
            .unwrap()
            .is_none()
    );
    assert!(store.audit_verify().unwrap().ok);
    // an unknown id is a NotFound and leaves no audit row
    assert!(matches!(
        api.forget(ForgetRequest {
            id: "nope".into(),
            mode: Some(ForgetMode::Purge)
        }),
        Err(ApiError::NotFound(_))
    ));
    assert_eq!(audit_ops(&s).len(), 2);
}

#[test]
fn a_thousand_recalls_of_one_redactable_node_produce_one_read_audit_row() {
    let s = Scratch::new("kaban-api-audit", "read-dedup");
    let (api, store) = api_on(&s);
    // store under a policy that does not know the term, so the stored text still carries it
    let store_dyn: Arc<dyn GraphStore> = store.clone();
    let plain = KabanApi::new(
        store_dyn,
        Policy::from_file(Policy::parse(b"").unwrap(), "plain".into()).unwrap(),
        store.signer(),
        None,
    );
    let r = plain
        .remember(fact("the old pw was hunter2 back then"))
        .unwrap();
    assert_eq!(r.redactions, Vec::<String>::new());
    assert_eq!(audit_ops(&s).len(), 0);
    // now read it 1000 times under the policy that redacts hunter2
    for _ in 0..1000 {
        let got = api
            .recall(RecallRequest {
                query: "old pw".into(),
                full_text: true,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(got.count, 1);
        let text = got.results[0].text.clone().unwrap();
        assert!(!text.contains("hunter2"), "{text}");
        assert!(text.contains("[REDACTED:old-password]"));
        assert_eq!(
            got.results[0].redactions,
            vec!["text:old-password".to_string()]
        );
    }
    let ops = audit_ops(&s);
    assert_eq!(ops.len(), 1, "{ops:?}");
    assert_eq!(ops[0].0, "redact");
    assert!(store.audit_verify().unwrap().ok);
    // a read audit failure masks, never hides
    let raw = rusqlite::Connection::open(s.db()).unwrap();
    raw.execute_batch(
        "DROP TRIGGER audit_no_delete; DROP TABLE audit; DROP TABLE read_audit_seen;",
    )
    .unwrap();
    drop(raw);
    let got = api
        .recall(RecallRequest {
            query: "old pw".into(),
            full_text: true,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(got.count, 1);
    assert!(
        got.results[0]
            .text
            .clone()
            .unwrap()
            .contains("[REDACTED:old-password]")
    );
}

const CANARY: &str = "Sup3rS3cretValue!";
const REDACTED: &str = "hunter2";

#[test]
fn audit_rows_never_contain_the_input() {
    let s = Scratch::new("kaban-api-audit", "canary");
    let (api, store) = api_on(&s);
    assert!(
        api.remember(fact(&format!("deploy password = {CANARY}")))
            .is_err()
    );
    let r = api
        .remember(fact(&format!("old pw {REDACTED} rotated")))
        .unwrap();
    assert_eq!(r.redactions, vec!["text:old-password".to_string()]);
    api.forget(ForgetRequest {
        id: r.id,
        mode: Some(ForgetMode::Purge),
    })
    .unwrap();
    assert_eq!(audit_ops(&s).len(), 3);
    assert!(store.audit_verify().unwrap().ok);
    // checkpoint so the WAL content is in the main file too, then scan every byte
    drop(api);
    drop(store);
    let bytes = s.db_bytes();
    assert_ne!(bytes.len(), 0);
    let find = |needle: &str| bytes.windows(needle.len()).any(|w| w == needle.as_bytes());
    assert!(!find(CANARY), "rejected value leaked into the database");
    assert!(!find(REDACTED), "redacted value leaked into the database");
}

/// Review R1-1: the workspace is gated before the write context exists, so a redact-class
/// workspace never reaches the audit rows, the JSONL export or any database byte raw.
#[test]
fn a_redact_class_workspace_is_masked_on_the_node_in_audit_rows_and_in_the_export() {
    let s = Scratch::new("kaban-api-audit", "ws-canary");
    let store = Arc::new(SqliteStore::open(&s.db(), &s.keys()).unwrap());
    let policy = Policy::from_file(Policy::parse(POLICY.as_bytes()).unwrap(), "t".into()).unwrap();
    let export_path = s.0.join("audit-export.jsonl");
    let api = KabanApi::new(
        store.clone(),
        policy,
        store.signer(),
        Some(AuditExport::new(export_path.clone(), store.signer())),
    );
    let r = api
        .remember(RememberRequest {
            kind: "Fact".into(),
            text: "release notes draft for the autumn cut".into(),
            workspace_id: Some(REDACTED.into()),
            ..Default::default()
        })
        .unwrap();
    assert!(
        r.redactions
            .contains(&"workspace_id:old-password".to_string()),
        "{:?}",
        r.redactions
    );
    let node = store
        .get(&NodeId::parse(&r.id).unwrap(), false)
        .unwrap()
        .unwrap();
    assert_eq!(node.workspace_id, "[REDACTED:old-password]");
    // the audit rows written with that remember carry the masked workspace only
    let raw = rusqlite::Connection::open(s.db()).unwrap();
    let workspaces: Vec<String> = raw
        .prepare("SELECT workspace_id FROM audit ORDER BY seq")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    drop(raw);
    assert!(!workspaces.is_empty(), "the redaction must be audited");
    assert!(
        workspaces.iter().all(|w| w == "[REDACTED:old-password]"),
        "raw workspace in audit rows: {workspaces:?}"
    );
    let export = std::fs::read_to_string(&export_path).unwrap();
    assert!(export.lines().count() >= 1);
    assert!(
        !export.contains(REDACTED),
        "raw workspace in export: {export}"
    );
    assert!(store.audit_verify().unwrap().ok);
    drop(api);
    drop(store);
    let bytes = s.db_bytes();
    assert!(
        !bytes
            .windows(REDACTED.len())
            .any(|w| w == REDACTED.as_bytes()),
        "raw workspace in db/-wal/-shm bytes"
    );
}

/// C2.3 (core-spec F5): the service default is `tombstone`, which is lifecycle only (a
/// Convention keeps `proposed`, no event), the response reports the resulting states and the
/// `ChangeLog` position, and the audit row is a `forget`. A string that is not a node id is
/// `NotFound` with the gated form echoed (D2), and leaves no audit row.
#[test]
fn forget_defaults_to_tombstone_and_reports_the_resulting_states() {
    let s = Scratch::new("kaban-api-audit", "tombstone-default");
    let (api, store) = api_on(&s);
    let rule = api
        .remember(RememberRequest {
            kind: "Convention".into(),
            text: "quolls are nocturnal".into(),
            ..Default::default()
        })
        .unwrap();
    let r = api
        .forget(ForgetRequest {
            id: rule.id.clone(),
            mode: None,
        })
        .unwrap();
    assert_eq!(r.mode, ForgetMode::Tombstone);
    assert_eq!(r.lifecycle, kaban_core::Lifecycle::Tombstoned);
    assert_eq!(r.governance, kaban_core::Governance::Proposed);
    assert!(r.outbox_seq > 0);
    assert!(
        store
            .get(&NodeId::parse(&rule.id).unwrap(), false)
            .unwrap()
            .is_none()
    );
    // no lifecycle event; the one row is the OWN-65 `unstamped` reason of a record created
    // outside any repository (engine-plan E2.0)
    assert!(
        store
            .events(&kaban_core::NodeId::parse(&rule.id).unwrap())
            .unwrap()
            .iter()
            .all(|e| e.kind == "unstamped")
    );
    let ops = audit_ops(&s);
    assert_eq!(ops.len(), 1, "{ops:?}");
    assert_eq!((ops[0].0.as_str(), ops[0].1.as_str()), ("forget", "node"));
    // not a node id: NotFound carrying the (clean) input, no audit row (D2)
    match api.forget(ForgetRequest {
        id: "not-a-uuid".into(),
        mode: None,
    }) {
        Err(ApiError::NotFound(echoed)) => assert_eq!(echoed, "not-a-uuid"),
        other => panic!("{other:?}"),
    }
    assert_eq!(audit_ops(&s).len(), 1);
    // a governance rejection surfaces as a store error, not as NotFound
    let d1 = api
        .remember(RememberRequest {
            kind: "Decision".into(),
            text: "quoll builds run nightly".into(),
            props: Some(serde_json::json!({"rationale": "a Decision needs one (C3.1)"})),
            ..Default::default()
        })
        .unwrap();
    let d2 = api
        .remember(RememberRequest {
            kind: "Decision".into(),
            text: "quoll builds run hourly".into(),
            props: Some(serde_json::json!({"rationale": "a Decision needs one (C3.1)"})),
            ..Default::default()
        })
        .unwrap();
    let err = api
        .link(kaban_api::LinkRequest {
            from: d2.id,
            to: d1.id,
            rel: "SUPERSEDES".into(),
            confidence: None,
            props: None,
        })
        .unwrap_err();
    assert!(
        matches!(
            err,
            ApiError::Store(kaban_core::StoreError::InvalidTransition(_))
        ),
        "{err:?}"
    );
    assert!(err.to_string().contains("proposed -> superseded"), "{err}");
}

/// C2.6 (D2 for the converted signatures): `link` endpoints, `remember` link targets and
/// `traverse` start ids are parsed at the API edge; a string that is not a node id is
/// `NotFound` echoing the gated form, nothing is written and no audit row is left.
#[test]
fn link_remember_links_and_traverse_map_an_unparsable_id_to_not_found() {
    let s = Scratch::new("kaban-api-audit", "d2-ids");
    let (api, store) = api_on(&s);
    let anchor = api
        .remember(RememberRequest {
            kind: "Fact".into(),
            text: "an anchor note".into(),
            ..Default::default()
        })
        .unwrap();
    let before = store.audit_verify().unwrap().entries;
    let err = api
        .link(kaban_api::LinkRequest {
            from: anchor.id.clone(),
            rel: "RELATES_TO".into(),
            to: "not-a-uuid".into(),
            confidence: None,
            props: None,
        })
        .expect_err("link to a non-id");
    assert!(
        matches!(err, ApiError::NotFound(ref e) if e == "not-a-uuid"),
        "{err:?}"
    );
    let err = api
        .remember(RememberRequest {
            kind: "Fact".into(),
            text: "a note linking to a non-id".into(),
            links: vec![kaban_api::LinkSpec {
                rel: "RELATES_TO".into(),
                from: None,
                to: Some("not-a-uuid".into()),
                confidence: None,
            }],
            ..Default::default()
        })
        .expect_err("remember with a link to a non-id");
    assert!(
        matches!(err, ApiError::NotFound(ref e) if e == "not-a-uuid"),
        "{err:?}"
    );
    let err = api
        .traverse(kaban_api::TraverseRequest {
            start_ids: vec!["not-a-uuid".into()],
            ..Default::default()
        })
        .expect_err("traverse from a non-id");
    assert!(
        matches!(err, ApiError::NotFound(ref e) if e == "not-a-uuid"),
        "{err:?}"
    );
    assert_eq!(
        store.audit_verify().unwrap().entries,
        before,
        "no audit row for a NotFound"
    );
    assert!(
        store
            .recall(&kaban_core::RecallQuery {
                query: "linking".into(),
                limit: 5,
                ..Default::default()
            })
            .unwrap()
            .is_empty(),
        "the remember with the bad link stored nothing"
    );
}
