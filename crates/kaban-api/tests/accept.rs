//! Public-behaviour tests of `accept` (core-spec F4; core-plan C3.2): the tiered gate, the
//! confirmation handshake, `supersede`, and the token's absence from every place it must not
//! reach (review RC-5).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::Scratch;
use kaban_api::{
    AcceptRequest, AcceptResponse, ApiError, Caller, ConfirmationToken, KabanApi, RememberRequest,
};
use kaban_core::{Actor, GovernanceStore, MemoryStore, NodeId, SignerKind};
use kaban_policy::{AuditExport, Policy};
use kaban_store_sqlite::SqliteStore;
use serde_json::{Value, json};
use std::sync::Arc;

/// Hits the builtin credential rule `aws-access-key-id` (always reject) in every view.
const CANARY: &str = "AKIAQWERTYUIOPASDFGH";

fn agent() -> Caller {
    Caller {
        kind: Actor::Agent,
        host: Some("test-host".into()),
        session: Some("sess-1".into()),
    }
}

fn human() -> Caller {
    Caller {
        kind: Actor::HumanCli,
        ..Default::default()
    }
}

/// A file-backed service with an audit export, so bytes on disk can be inspected.
fn api_on(s: &Scratch) -> (KabanApi, Arc<SqliteStore>) {
    let store = Arc::new(SqliteStore::open(&s.db(), &s.keys()).unwrap());
    let export = AuditExport::new(s.0.join("audit.jsonl"), store.signer());
    let api = KabanApi::new(store.clone(), Policy::empty(), store.signer(), Some(export));
    (api, store)
}

/// Every byte under the scratch directory except the key file: main db, WAL, SHM, export.
fn disk_bytes(s: &Scratch) -> Vec<u8> {
    let mut out = Vec::new();
    for f in std::fs::read_dir(&s.0).unwrap() {
        let p = f.unwrap().path();
        if p.is_file() {
            out.extend(std::fs::read(&p).unwrap());
        }
    }
    out
}

fn remember(api: &KabanApi, kind: &str, text: &str) -> String {
    let props = (kind == "Decision").then(|| json!({"rationale": "because"}));
    api.remember(RememberRequest {
        kind: kind.into(),
        text: text.into(),
        props,
        ..Default::default()
    })
    .unwrap()
    .id
}

fn request(id: &str) -> AcceptRequest {
    AcceptRequest {
        id: id.into(),
        reason: Some("reviewed in standup".into()),
        path_scope: vec!["src/api/**".into()],
        ..Default::default()
    }
}

/// The issued token as the wire sees it (the only place it is serialized).
fn issued(resp: &AcceptResponse) -> (String, String) {
    match resp {
        AcceptResponse::ConfirmationRequired { prompt, .. } => {
            let v = serde_json::to_value(resp).unwrap();
            (
                v["confirmation_token"].as_str().unwrap().to_owned(),
                prompt.clone(),
            )
        }
        other @ AcceptResponse::Accepted { .. } => {
            panic!("expected confirmation_required, got {other:?}")
        }
    }
}

fn redeem(id: &str, token: &str) -> AcceptRequest {
    AcceptRequest {
        id: id.into(),
        confirmation_token: Some(ConfirmationToken::parse(token).unwrap()),
        confirmed_by_user: true,
        ..Default::default()
    }
}

fn contains(hay: &[u8], needle: &str) -> bool {
    hay.windows(needle.len()).any(|w| w == needle.as_bytes())
}

/// The record's lifecycle events. A record created outside any repository carries an OWN-65
/// `unstamped` reason row first (engine-plan E2.0; `repo_root` is unset in these tests), which
/// these assertions are not about.
fn lifecycle_events(store: &SqliteStore, id: &str) -> Vec<kaban_core::Event> {
    store
        .events(&NodeId::parse(id).unwrap())
        .unwrap()
        .into_iter()
        .filter(|e| e.kind != "unstamped")
        .collect()
}

#[test]
fn agent_accept_on_a_decision_returns_a_token_and_a_row_holding_only_its_hash() {
    let s = Scratch::new("kaban-accept", "issue");
    let (api, store) = api_on(&s);
    let id = remember(&api, "Decision", "ship on mondays");
    let resp = api.accept(request(&id), &agent()).unwrap();
    let (token, prompt) = issued(&resp);
    assert_eq!(token.len(), 64);
    assert!(token.bytes().all(|b| b.is_ascii_hexdigit()));
    assert!(
        !prompt.contains(&token),
        "the prompt must not carry the token"
    );
    assert!(prompt.contains("ship on mondays") && prompt.contains("reviewed in standup"));
    // the row: hash only
    let conn = rusqlite::Connection::open(s.db()).unwrap();
    let (hash, node_id, reason): (String, String, String) = conn
        .query_row(
            "SELECT token_hash, node_id, reason FROM confirmations",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_ne!(hash, token);
    assert_eq!(hash.len(), 64);
    assert_eq!(node_id, id);
    assert_eq!(reason, "reviewed in standup");
    drop(conn);
    assert!(
        !contains(&disk_bytes(&s), &token),
        "token found in db, wal, shm or export bytes"
    );
    // `Debug` of the response never shows it
    assert!(!format!("{resp:?}").contains(&token));
    // the Decision is still proposed
    let node = store
        .get(&NodeId::parse(&id).unwrap(), false)
        .unwrap()
        .unwrap();
    assert_eq!(node.governance, kaban_core::Governance::Proposed);
}

#[test]
fn the_same_token_redeems_once_and_the_replay_is_invalid_without_echoing_it() {
    let s = Scratch::new("kaban-accept", "redeem");
    let (api, store) = api_on(&s);
    let id = remember(&api, "Decision", "ship on mondays");
    let (token, _) = issued(&api.accept(request(&id), &agent()).unwrap());
    // redeemed by a second caller (the CLI), with no reason or scope of its own
    let resp = api.accept(redeem(&id, &token), &human()).unwrap();
    let AcceptResponse::Accepted {
        node,
        event_seq,
        signer_kind,
        ..
    } = &resp
    else {
        panic!("expected accepted, got {resp:?}");
    };
    assert_eq!(*signer_kind, SignerKind::HumanViaChat);
    assert_eq!(node.governance, kaban_core::Governance::Accepted);
    assert!(*event_seq > 0);
    let events = lifecycle_events(&store, &id);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].seq, i64::try_from(*event_seq).unwrap());
    assert_eq!(events[0].signer_kind, SignerKind::HumanViaChat);
    assert_eq!(events[0].reason, "reviewed in standup");
    assert_eq!(events[0].path_scope, vec!["src/api/**".to_string()]);
    assert_eq!(events[0].host.as_deref(), Some("test-host"));
    // the accepted record carries the confirmed scope
    let n = store
        .get(&NodeId::parse(&id).unwrap(), false)
        .unwrap()
        .unwrap();
    assert_eq!(n.path_scope, vec!["src/api/**".to_string()]);
    // the non-issuing response never carries the token
    let wire = serde_json::to_string(&resp).unwrap();
    assert!(!wire.contains(&token));
    // replay
    let err = api.accept(redeem(&id, &token), &human()).unwrap_err();
    assert!(
        matches!(&err, ApiError::Invalid(m) if m == "confirmation_token"),
        "{err:?}"
    );
    assert!(!err.to_string().contains(&token));
    assert_eq!(lifecycle_events(&store, &id).len(), 1);
    assert!(!contains(&disk_bytes(&s), &token), "token on disk");
}

#[test]
fn an_expired_token_and_a_token_for_another_node_are_invalid() {
    let s = Scratch::new("kaban-accept", "expired");
    let (api, store) = api_on(&s);
    let a = remember(&api, "Decision", "decision a");
    let b = remember(&api, "Decision", "decision b");
    let (token_a, _) = issued(&api.accept(request(&a), &agent()).unwrap());
    let (token_b, _) = issued(&api.accept(request(&b), &agent()).unwrap());
    // token for A cannot accept B (and is not consumed by the attempt)
    let err = api.accept(redeem(&b, &token_a), &human()).unwrap_err();
    assert!(matches!(&err, ApiError::Invalid(m) if m == "confirmation_token"));
    assert!(!err.to_string().contains(&token_a));
    // created 601 s ago: the row's `expires_at` is moved into the past directly
    let conn = rusqlite::Connection::open(s.db()).unwrap();
    conn.execute(
        "UPDATE confirmations SET expires_at = unixepoch() - 1 WHERE node_id = ?",
        [&b],
    )
    .unwrap();
    drop(conn);
    let err = api.accept(redeem(&b, &token_b), &human()).unwrap_err();
    assert!(matches!(&err, ApiError::Invalid(m) if m == "confirmation_token"));
    assert!(!err.to_string().contains(&token_b));
    // A's token is still live and redeems A
    let resp = api.accept(redeem(&a, &token_a), &human()).unwrap();
    assert!(matches!(resp, AcceptResponse::Accepted { .. }));
    assert_eq!(lifecycle_events(&store, &b).len(), 0);
    let bytes = disk_bytes(&s);
    assert!(!contains(&bytes, &token_a) && !contains(&bytes, &token_b));
}

#[test]
fn convention_accepted_by_an_agent_appends_an_agent_event_and_human_cli_accepts_a_decision() {
    let s = Scratch::new("kaban-accept", "direct");
    let (api, store) = api_on(&s);
    let rule = remember(&api, "Convention", "numbats review every PR");
    let resp = api.accept(request(&rule), &agent()).unwrap();
    let AcceptResponse::Accepted { signer_kind, .. } = resp else {
        panic!("{resp:?}");
    };
    assert_eq!(signer_kind, SignerKind::Agent);
    let events = lifecycle_events(&store, &rule);
    assert_eq!(events[0].signer_kind, SignerKind::Agent);
    assert_eq!(events[0].kind, "accepted");
    // a human on the CLI accepts a Decision directly
    let d = remember(&api, "Decision", "ship on mondays");
    let resp = api.accept(request(&d), &human()).unwrap();
    let AcceptResponse::Accepted { signer_kind, .. } = resp else {
        panic!("{resp:?}");
    };
    assert_eq!(signer_kind, SignerKind::HumanCli);
    // the rules: no reason, no scope, a bad scope
    for req in [
        AcceptRequest {
            reason: None,
            ..request(&rule)
        },
        AcceptRequest {
            path_scope: vec![],
            ..request(&rule)
        },
        AcceptRequest {
            path_scope: vec!["../etc/**".into()],
            ..request(&rule)
        },
        AcceptRequest {
            path_scope: vec![":(top)src".into()],
            ..request(&rule)
        },
    ] {
        let other = remember(&api, "Convention", "another rule");
        let err = api
            .accept(AcceptRequest { id: other, ..req }, &agent())
            .unwrap_err();
        assert!(matches!(err, ApiError::Invalid(_)), "{err:?}");
    }
}

#[test]
fn supersede_marks_the_old_decision_superseded_and_accepting_a_tombstoned_node_is_invalid() {
    let s = Scratch::new("kaban-accept", "supersede");
    let (api, store) = api_on(&s);
    let old = remember(&api, "Decision", "ship on mondays");
    api.accept(request(&old), &human()).unwrap();
    let new = remember(&api, "Decision", "ship on wednesdays");
    api.accept(request(&new), &human()).unwrap();
    let edge = api.supersede(&new, &old, &human()).unwrap();
    assert_eq!(edge.rel, "SUPERSEDES");
    let n = store
        .get(&NodeId::parse(&old).unwrap(), true)
        .unwrap()
        .unwrap();
    assert_eq!(n.governance, kaban_core::Governance::Superseded);
    assert_eq!(n.lifecycle, kaban_core::Lifecycle::Archived);
    let events = store.events(&NodeId::parse(&old).unwrap()).unwrap();
    assert_eq!(events.last().unwrap().kind, "superseded");
    assert_eq!(events.last().unwrap().signer_kind, SignerKind::HumanCli);
    // tombstoned: InvalidTransition, no token
    let gone = remember(&api, "Decision", "ship never");
    api.forget(kaban_api::ForgetRequest {
        id: gone.clone(),
        mode: None,
    })
    .unwrap();
    let err = api.accept(request(&gone), &agent()).unwrap_err();
    assert!(
        matches!(
            &err,
            ApiError::Store(kaban_core::StoreError::InvalidTransition(_))
        ),
        "{err:?}"
    );
    let conn = rusqlite::Connection::open(s.db()).unwrap();
    let rows: i64 = conn
        .query_row("SELECT count(*) FROM confirmations", [], |r| r.get(0))
        .unwrap();
    assert_eq!(rows, 0);
}

/// RC-5 negatives: a pasted credential or any non-token value under `confirmation_token` is
/// content and rejected before parsing (`credential-key`), never echoed; a valid-looking token
/// in a content field is still content.
#[test]
fn a_pasted_credential_under_confirmation_token_is_rejected_as_content() {
    let s = Scratch::new("kaban-accept", "pasted");
    let (api, _) = api_on(&s);
    let id = NodeId::new();
    for value in [CANARY, "hunter2", "abc", &"A".repeat(64)] {
        let err = api
            .prepare_args(
                "accept",
                &json!({"id": id.as_str(), "confirmation_token": value, "confirmed_by_user": true}),
            )
            .unwrap_err();
        let text = err.to_string();
        assert!(text.contains("credential-key"), "{value}: {text}");
        assert!(!text.contains(value), "echoed: {text}");
    }
    // a genuine token shape passes the pre-parse scan unscanned
    let token = "a".repeat(64);
    api.prepare_args(
        "accept",
        &json!({"id": id.as_str(), "confirmation_token": token, "confirmed_by_user": true}),
    )
    .unwrap();
    // the same 64-hex string in `reason` is content (scanned; clean here, so accepted)
    api.prepare_args("accept", &json!({"id": id.as_str(), "reason": token}))
        .unwrap();
    // the typed newtype refuses anything but the shape, without echoing the value
    let err = serde_json::from_value::<AcceptRequest>(
        json!({"id": id.as_str(), "confirmation_token": "not-a-token"}),
    )
    .unwrap_err()
    .to_string();
    assert!(
        err.contains("64 lowercase hex") && !err.contains("not-a-token"),
        "{err}"
    );
    let _: Value = json!(null);
}

/// M3-23: `remember` stamps the process repository on the note, `accept` stamps
/// `accepted_commit` from `git rev-parse HEAD` in `repo_root` within the 200 ms deadline;
/// a runner whose git hangs is killed at the deadline and the acceptance proceeds unstamped.
#[test]
fn remember_stamps_the_repo_and_accept_stamps_the_head_commit_within_the_deadline() {
    let s = Scratch::new("kaban-accept", "repo");
    let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let head = std::process::Command::new("git")
        .args(["-C", root.to_str().unwrap(), "rev-parse", "HEAD"])
        .output()
        .unwrap();
    let head = String::from_utf8(head.stdout).unwrap().trim().to_owned();
    assert_eq!(head.len(), 40, "the test runs inside the kaban repository");
    let store = Arc::new(SqliteStore::open(&s.db(), &s.keys()).unwrap());
    let api = KabanApi::with_config(
        store.clone(),
        Policy::empty(),
        store.signer(),
        None,
        kaban_api::ApiConfig {
            repo_root: Some(root),
            repo: Some("kaban-test-repo".into()),
            ..Default::default()
        },
    );
    let id = remember(&api, "Convention", "numbats review every PR");
    let n = store
        .get(&NodeId::parse(&id).unwrap(), false)
        .unwrap()
        .unwrap();
    assert_eq!(n.repo.as_deref(), Some("kaban-test-repo"));
    let started = std::time::Instant::now();
    api.accept(request(&id), &agent()).unwrap();
    let n = store
        .get(&NodeId::parse(&id).unwrap(), false)
        .unwrap()
        .unwrap();
    assert_eq!(n.accepted_commit.as_deref(), Some(head.as_str()));
    let events = lifecycle_events(&store, &id);
    assert_eq!(events[0].commit_sha.as_deref(), Some(head.as_str()));
    assert!(started.elapsed() < std::time::Duration::from_secs(2));

    // a hanging git: killed at the deadline, no stamp, acceptance still recorded
    let wrapper = s.0.join("slow-git.sh");
    std::fs::write(&wrapper, "#!/bin/sh\nexec sleep 5\n").unwrap();
    std::fs::set_permissions(
        &wrapper,
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .unwrap();
    let slow = api.clone().with_git(kaban_api::GitRunner {
        program: wrapper,
        timeout: std::time::Duration::from_millis(200),
    });
    let other = remember(&slow, "Convention", "numbats sleep in hollow logs");
    let started = std::time::Instant::now();
    slow.accept(request(&other), &agent()).unwrap();
    let took = started.elapsed();
    assert!(
        took < std::time::Duration::from_secs(2),
        "git kill took {took:?}"
    );
    let n = store
        .get(&NodeId::parse(&other).unwrap(), false)
        .unwrap()
        .unwrap();
    assert_eq!(n.governance, kaban_core::Governance::Accepted);
    assert_eq!(n.accepted_commit, None);
}

/// M3 review R3-6: `path_scope` is persisted as its canonical set (`\` to `/`, duplicates
/// dropped) by `remember` and by `accept` (the record, the event and the confirmation prompt),
/// so the F14 denominator counts each effective entry once.
#[test]
fn a_duplicated_path_scope_is_persisted_once_by_remember_and_accept() {
    let s = Scratch::new("kaban-accept", "canonical-scope");
    let (api, store) = api_on(&s);
    let id = api
        .remember(RememberRequest {
            kind: "Decision".into(),
            text: "one pool per worker".into(),
            props: Some(json!({"rationale": "because"})),
            path_scope: vec![
                "src/pool.rs".into(),
                "src\\pool.rs".into(),
                "src/pool.rs".into(),
            ],
            ..Default::default()
        })
        .unwrap()
        .id;
    let n = store
        .get(&NodeId::parse(&id).unwrap(), false)
        .unwrap()
        .unwrap();
    assert_eq!(n.path_scope, vec!["src/pool.rs".to_string()], "remember");

    let resp = api
        .accept(
            AcceptRequest {
                id: id.clone(),
                reason: Some("reviewed".into()),
                path_scope: vec!["src/a.rs".into(), "src\\a.rs".into(), "src/a.rs".into()],
                ..Default::default()
            },
            &human(),
        )
        .unwrap();
    assert!(matches!(resp, AcceptResponse::Accepted { .. }), "{resp:?}");
    let n = store
        .get(&NodeId::parse(&id).unwrap(), false)
        .unwrap()
        .unwrap();
    assert_eq!(
        n.path_scope,
        vec!["src/a.rs".to_string()],
        "accepted record"
    );
    let events = lifecycle_events(&store, &id);
    assert_eq!(events[0].path_scope, vec!["src/a.rs".to_string()], "event");
}
