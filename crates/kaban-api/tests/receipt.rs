//! Spec 05 SS-10 write receipts through the public entry points: a write that rolls back never
//! yields `committed`, a committed write carries the store's identities and its `ChangeLog`
//! position, a failure after the commit never turns the receipt into a rejection (the
//! identities are read once, the position inside the transaction), no local record reference
//! leaves in a receipt-bearing response (SS-27), the receipt's own fields are control under
//! every write scope, and a hook's refused write reports `rejected` while its stored one
//! reports `committed`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;
#[path = "common/status.rs"]
mod status;

use common::{Scratch, fact};
use kaban_api::{
    ApiError, DeprecateRequest, ForgetRequest, HookEvent, HookHost, KabanApi, LinkRequest,
    LinkSpec, PruningOperation, PruningProtectionRequest, PruningTargetKind, RememberRequest,
    WriteOutcome, WriteReceipt,
};
use kaban_core::{
    Actor, ChangeLogStore, GovernanceStore, IndexMeta, IngestStore, LeaseStore, MemoryStore,
    NodeId, PendingFailuresQuery, PendingScope,
};
use kaban_policy::Policy;
use kaban_store_sqlite::SqliteStore;
use serde_json::{Value, json};
use std::sync::Arc;

/// Hits the builtin credential rule `aws-access-key-id` (always reject); assembled at run time
/// so no key-shaped literal sits in the source.
fn canary() -> String {
    format!("AKIA{}", "QWERTYUIOPASDFGH")
}

fn policy(yaml: &str) -> Policy {
    Policy::from_file(Policy::parse(yaml.as_bytes()).unwrap(), "test".into()).unwrap()
}

fn store_api() -> (Arc<SqliteStore>, KabanApi) {
    let store = Arc::new(SqliteStore::open_memory().unwrap());
    let signer = store.signer();
    (
        store.clone(),
        KabanApi::new(store, policy(""), signer, None),
    )
}

/// A file-backed store (a second connection can inject a failure into one step of a call).
fn file_store_api(s: &Scratch) -> (Arc<SqliteStore>, KabanApi) {
    let store = Arc::new(SqliteStore::open(&s.db(), &s.keys()).unwrap());
    let signer = store.signer();
    (
        store.clone(),
        KabanApi::new(store, policy(""), signer, None),
    )
}

fn raw(s: &Scratch) -> rusqlite::Connection {
    rusqlite::Connection::open(s.db()).unwrap()
}

/// The receipt's fields against the store that answered: UUID identities and the head.
fn assert_identities(store: &SqliteStore, receipt: &WriteReceipt) {
    assert_eq!(receipt.instance_id, store.db_id());
    let meta = store.index_meta().unwrap();
    assert_eq!(
        Some(receipt.pack_lineage_id.as_str()),
        meta.get(IndexMeta::PACK_LINEAGE_ID)
    );
    NodeId::parse(&receipt.instance_id).expect("instance_id is a UUID");
    NodeId::parse(&receipt.pack_lineage_id).expect("pack_lineage_id is a UUID");
}

#[test]
fn a_rejected_remember_never_yields_committed_and_moves_no_sequence() {
    let (store, api) = store_api();
    let before = store.changes_head().unwrap();
    let r = api.remember(fact(&format!("token {} in the text", canary())));
    assert!(
        matches!(r, Err(ApiError::Rejected { .. })),
        "expected rejection, got {r:?}"
    );
    assert_ne!(
        r.as_ref().ok().map(|resp| resp.receipt.outcome),
        Some(WriteOutcome::Committed),
        "a pre-commit rollback never yields committed"
    );
    assert_eq!(
        store.changes_head().unwrap(),
        before,
        "a rollback consumes no sequence"
    );
    // the next committed write is the first position after the untouched head
    let ok = api.remember(fact("a clean note")).unwrap();
    assert_eq!(ok.receipt.outcome, WriteOutcome::Committed);
    assert!(ok.receipt.seq.unwrap() > before, "{:?}", ok.receipt);
    assert_eq!(ok.receipt.seq, Some(store.changes_head().unwrap()));
    assert_identities(&store, &ok.receipt);
}

/// Spec 05 OWN-M (constraint 2): the store projects its own commit after the transaction,
/// and a projection that cannot publish (another holder owns the `text-index` lease, as a
/// serving process would) never turns the committed write into a rejection. The receipt is
/// `committed_not_yet_searchable` with its position, the record is stored, and once the lease
/// is back the next write publishes and reports `committed`.
#[test]
fn a_write_whose_projection_cannot_publish_reports_committed_not_yet_searchable() {
    let s = Scratch::new("ss10-receipt", "lease-held");
    let (store, api) = file_store_api(&s);
    let lease = store
        .lease_acquire("text-index", "another:holder", 60)
        .unwrap()
        .unwrap();

    let pending = api
        .remember(fact("quokka committed, not yet searchable"))
        .unwrap();

    assert_eq!(
        pending.receipt.outcome,
        WriteOutcome::CommittedNotYetSearchable
    );
    assert_eq!(pending.receipt.seq, Some(store.changes_head().unwrap()));
    assert_identities(&store, &pending.receipt);
    assert!(
        store
            .get(&NodeId::parse(&pending.id).unwrap(), false)
            .unwrap()
            .is_some(),
        "the write stands"
    );
    store.lease_release(&lease).unwrap();
    let published = api.remember(fact("quokka searchable")).unwrap();
    assert_eq!(published.receipt.outcome, WriteOutcome::Committed);
}

/// D3 review d3-10: the rollback happens inside the mutation transaction after partial work.
/// `remember` inserts the node first and its inline links after it; a `GOVERNS` edge from a
/// `Fact` breaks the endpoint rule inside the same transaction, so the whole call rolls back:
/// no `committed`, no sequence consumed, no node stored, and the typed rejection carries the
/// identities and no position.
#[test]
fn a_remember_that_fails_after_its_node_insert_rolls_back_whole_and_is_rejected() {
    let (store, api) = store_api();
    let target = api.remember(fact("a target to govern")).unwrap().id;
    let before = store.changes_head().unwrap();
    let nodes_before = store
        .list_nodes("Fact", api.workspace().as_str(), 0, 100)
        .unwrap()
        .len();
    let r = api.remember(RememberRequest {
        links: vec![LinkSpec {
            rel: "GOVERNS".into(),
            to: Some(target),
            from: None,
            confidence: None,
        }],
        ..fact("a fact may not govern")
    });
    assert!(
        matches!(r, Err(ApiError::Store(_))),
        "the second write of the batch is refused: {r:?}"
    );
    assert_eq!(
        store.changes_head().unwrap(),
        before,
        "nothing of the call is in the log"
    );
    assert_eq!(
        store
            .list_nodes("Fact", api.workspace().as_str(), 0, 100)
            .unwrap()
            .len(),
        nodes_before,
        "the node inserted before the refused link is gone with it"
    );
    let rejected = api.rejected_receipt();
    assert_eq!(rejected.outcome, WriteOutcome::Rejected);
    assert_eq!(rejected.seq, None, "a rejection holds no position");
    assert_identities(&store, &rejected);
}

#[test]
fn a_committed_forget_carries_its_transaction_sequence_and_the_store_identities() {
    let (store, api) = store_api();
    let id = api.remember(fact("to be forgotten")).unwrap().id;
    let r = api.forget(ForgetRequest { id, mode: None }).unwrap();
    assert_eq!(r.receipt.outcome, WriteOutcome::Committed);
    assert_eq!(
        r.receipt.seq,
        Some(r.outbox_seq),
        "the seq read inside the transaction"
    );
    assert_eq!(r.receipt.seq, Some(store.changes_head().unwrap()));
    assert_identities(&store, &r.receipt);
}

/// D3 review d3-2: the identities are per database and read once when the service is built,
/// so a committed write never fails on reading them afterwards; the rows gone from
/// `index_meta` after construction change nothing in the receipt's identities or position.
/// The store's lineage-bound index (spec 05 OWN-M, amendment 2) cannot serve a database whose
/// lineage key is gone, so the barrier reports the committed write as not yet searchable,
/// never as a failure.
#[test]
fn a_committed_write_keeps_its_receipt_when_the_identities_cannot_be_read_after_the_commit() {
    let s = Scratch::new("kaban-receipt", "identities");
    let (store, api) = file_store_api(&s);
    let lineage = store
        .index_meta()
        .unwrap()
        .get(IndexMeta::PACK_LINEAGE_ID)
        .unwrap()
        .to_owned();
    raw(&s)
        .execute(
            "DELETE FROM index_meta WHERE key IN (?1, ?2)",
            [IndexMeta::DB_ID, IndexMeta::PACK_LINEAGE_ID],
        )
        .unwrap();
    assert_eq!(store.index_meta().unwrap().get(IndexMeta::DB_ID), None);
    let r = api
        .remember(fact("durable whatever the receipt read"))
        .expect("a write that committed is never an error");
    assert!(
        matches!(
            r.receipt.outcome,
            WriteOutcome::Committed | WriteOutcome::CommittedNotYetSearchable
        ),
        "{:?}",
        r.receipt
    );
    assert_eq!(r.receipt.instance_id, store.db_id());
    assert_eq!(r.receipt.pack_lineage_id, lineage);
    assert_eq!(r.receipt.seq, Some(store.changes_head().unwrap()));
}

/// D3 review d3-5: the store records the call's terminal sequence in the write context from
/// inside its transaction (BEGIN IMMEDIATE serializes writers, so nothing commits between
/// that read and the commit), and the service reports it: no second query after the commit,
/// which another writer could have moved past the call's own rows.
#[test]
fn the_store_records_the_terminal_sequence_of_a_write_in_its_context() {
    let (store, api) = store_api();
    let a = api.remember(fact("left")).unwrap();
    let b = api.remember(fact("right")).unwrap();
    let (a, b) = (NodeId::parse(&a.id).unwrap(), NodeId::parse(&b.id).unwrap());
    let ctx =
        || kaban_core::WriteCtx::new(api.policy_version(), api.workspace().clone(), Actor::Agent);

    let linking = ctx();
    assert_eq!(
        linking.terminal_seq(),
        None,
        "nothing recorded before a write"
    );
    store
        .link(&a, &b, "DEPENDS_ON", None, None, &linking)
        .unwrap();
    assert_eq!(
        linking.terminal_seq(),
        Some(store.changes_head().unwrap()),
        "the link's transaction recorded its terminal sequence"
    );

    // `forget` reports its position on its own outcome (read inside the transaction already)
    let outcome = store
        .forget(&a, kaban_core::ForgetMode::Tombstone, &ctx())
        .unwrap();
    assert_eq!(outcome.outbox_seq, store.changes_head().unwrap());

    // through the service: a no-op `remember` (the duplicate) still names the position it saw
    let again = api.remember(fact("right")).unwrap();
    assert_eq!(again.status, kaban_api::RememberStatus::Existing);
    assert_eq!(again.receipt.seq, Some(store.changes_head().unwrap()));
}

/// Every key of `value`, recursively.
fn keys(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for (k, v) in map {
                out.push(k.clone());
                keys(v, out);
            }
        }
        Value::Array(items) => items.iter().for_each(|v| keys(v, out)),
        _ => {}
    }
}

/// SS-27: no local record reference anywhere in a response, keys and JSON encoded inside
/// strings included (a canonical group key is JSON in a string).
fn assert_no_local_reference(path: &str, value: &Value) {
    const LOCAL: [&str; 5] = ["rid", "rowid", "chunk_rid", "src", "dst"];
    match value {
        Value::Object(map) => {
            for (k, v) in map {
                let here = format!("{path}.{k}");
                assert!(
                    !(LOCAL.contains(&k.as_str()) && (v.is_number() || v.is_array())),
                    "a local record reference left the database at {here}: {v}"
                );
                assert_no_local_reference(&here, v);
            }
        }
        Value::Array(items) => {
            for (i, v) in items.iter().enumerate() {
                assert_no_local_reference(&format!("{path}[{i}]"), v);
            }
        }
        Value::String(s) => {
            if let Ok(inner @ (Value::Object(_) | Value::Array(_))) =
                serde_json::from_str::<Value>(s)
            {
                assert_no_local_reference(&format!("{path}<json>"), &inner);
            }
        }
        _ => {}
    }
}

/// The receipt object itself: exactly the four fields, UUID identities, one number.
fn assert_receipt_shape(response: &Value) {
    let receipt = response["receipt"].as_object().unwrap();
    let mut names: Vec<&str> = receipt.keys().map(String::as_str).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        ["instance_id", "outcome", "pack_lineage_id", "seq"],
        "{response}"
    );
    for (key, value) in receipt {
        match value {
            Value::String(s) if key.ends_with("_id") => {
                NodeId::parse(s).unwrap_or_else(|_| panic!("{key} is not a UUID: {s}"));
            }
            Value::Number(_) | Value::Null => {
                assert_eq!(key, "seq", "the only number is the position");
            }
            Value::String(_) => assert_eq!(key, "outcome"),
            other => panic!("unexpected receipt leaf {key}: {other}"),
        }
    }
    let mut all = Vec::new();
    keys(response, &mut all);
    assert!(
        all.iter().all(|k| k != "rid" && k != "rowid"),
        "a local row id left the database: {response}"
    );
}

/// D3 review d3-3 (SS-27): every receipt-bearing response type this suite can produce,
/// serialized as the adapters serialize it, carries no local record reference (`rid`,
/// `rowid`, `chunk_rid`, numeric `src`/`dst`), not even as JSON inside a string: `remember`,
/// `link`, `pruning_protection`, `forget`, `explain --deprecate` and `resolve --winner` (the
/// one that used to echo the conflict group's canonical key, whose subject is a row id).
#[test]
fn no_local_record_reference_leaves_in_any_receipt_bearing_response() {
    let (store, api) = store_api();
    let remembered = api.remember(fact("scanned")).unwrap();
    let other = api.remember(fact("scanned too")).unwrap();
    let linked = api
        .link(LinkRequest {
            from: remembered.id.clone(),
            rel: "DEPENDS_ON".into(),
            to: other.id.clone(),
            confidence: None,
            props: None,
        })
        .unwrap();
    let protected = api
        .pruning_protection(
            PruningProtectionRequest {
                operation: PruningOperation::Set,
                target_kind: PruningTargetKind::Node,
                id: Some(other.id.clone()),
                key: None,
                src: None,
                dst: None,
                rel: None,
            },
            Actor::HumanCli,
        )
        .unwrap();
    let forgotten = api
        .forget(ForgetRequest {
            id: remembered.id.clone(),
            mode: None,
        })
        .unwrap();
    let mut responses = vec![
        ("remember", serde_json::to_value(&remembered).unwrap()),
        ("link", serde_json::to_value(&linked).unwrap()),
        (
            "pruning_protection",
            serde_json::to_value(&protected).unwrap(),
        ),
        ("forget", serde_json::to_value(&forgotten).unwrap()),
    ];
    // the position is a log position, not the node's row id (the one node row is 1)
    assert!(store.changes_head().unwrap() > 1);

    // the claim-bearing responses over the reconcile fixture (a chunk with a candidate group)
    let s = status::Scratch::new("receipt-scan");
    let it = status::items(&s);
    let winner = it
        .api
        .explain_status(kaban_api::ExplainStatusRequest {
            address: it.chunk.to_string(),
        })
        .unwrap()
        .explanation
        .unwrap()
        .candidates[0]
        .value
        .as_str()
        .unwrap()
        .to_owned();
    let resolved = it
        .api
        .resolve_winner(&it.chunk, &winner, None, Actor::HumanCli)
        .unwrap();
    let deprecated = it
        .api
        .deprecate(
            DeprecateRequest {
                key: status::key_of(&s, "a1"),
                reason: "reviewed".into(),
            },
            Actor::HumanCli,
        )
        .unwrap();
    responses.push(("resolve", serde_json::to_value(&resolved).unwrap()));
    responses.push(("explain", serde_json::to_value(&deprecated).unwrap()));

    for (scope, response) in &responses {
        assert_receipt_shape(response);
        assert_no_local_reference(scope, response);
    }
}

/// D3 review d3-4: the receipt's outcome and identities are control under every scope whose
/// response carries one (`remember`, `link`, `forget`, `unmerge`, `pruning_protection`, the
/// CLI's `explain --deprecate` and `resolve`, `sync`'s and `import`'s import report,
/// `ingest_finish`), so a policy that rejects the word `committed` can never withhold or
/// alter a committed mutation's own outcome at the response gate.
#[test]
fn a_policy_rejecting_the_word_committed_never_withholds_a_receipt_on_any_write_scope() {
    let store = Arc::new(SqliteStore::open_memory().unwrap());
    let signer = store.signer();
    let api = KabanApi::new(
        store.clone(),
        policy("words:\n  - { term: committed, action: reject, id: no-commit }\n"),
        signer,
        None,
    );
    let receipt = serde_json::to_value(api.committed_receipt(Some(7))).unwrap();
    assert_eq!(receipt["outcome"], json!("committed"));
    let plain = json!({"receipt": receipt});
    let nested = json!({"import": {"receipt": receipt}});
    for (scope, value) in [
        ("remember", &plain),
        ("link", &plain),
        ("forget", &plain),
        ("unmerge", &plain),
        ("pruning_protection", &plain),
        ("explain", &plain),
        ("resolve", &plain),
        ("import", &plain),
        ("ingest_finish", &plain),
        ("sync", &nested),
    ] {
        let gated = api
            .gate_response(scope, value)
            .unwrap_or_else(|e| panic!("{scope}: the receipt was withheld: {e}"));
        assert_eq!(&gated, value, "{scope}: the receipt was altered");
    }
    // the same policy still gates content under those scopes
    let err = api
        .gate_response(
            "resolve",
            &json!({"value": "committed", "receipt": receipt}),
        )
        .unwrap_err();
    assert!(matches!(err, ApiError::Rejected { .. }), "{err:?}");
}

/// A Claude Code `PostToolUseFailure` envelope for a shell command.
fn failure_envelope(command: &str) -> Value {
    json!({
        "hook_event_name": "PostToolUseFailure",
        "session_id": "s-receipt",
        "tool_name": "Bash",
        "tool_input": {"command": command},
        "error": "exit status 1",
    })
}

/// A Claude Code `PostToolUse` envelope for a shell command that succeeded.
fn success_envelope(command: &str) -> Value {
    json!({
        "hook_event_name": "PostToolUse",
        "session_id": "s-receipt",
        "tool_name": "Bash",
        "tool_input": {"command": command},
        "tool_response": {"stdout": "ok", "stderr": ""},
    })
}

/// The session's pending Failures, newest first.
fn pending(api: &KabanApi, store: &SqliteStore) -> Vec<kaban_core::Node> {
    store
        .pending_failures(&PendingFailuresQuery {
            workspace_id: api.workspace().as_str().to_owned(),
            session_id: "s-receipt".into(),
            scope: PendingScope::Session,
            limit: 16,
        })
        .unwrap()
}

#[test]
fn a_hook_reports_committed_for_its_stored_write_and_rejected_for_a_refused_one() {
    let (store, api) = store_api();
    let stored = api.hook(
        HookHost::ClaudeCode,
        HookEvent::PostToolUseFailure,
        &failure_envelope("make test"),
    );
    assert_eq!(stored.output, json!({}), "{stored:?}");
    let receipt = stored.receipt.expect("the failure was stored");
    assert_eq!(receipt.outcome, WriteOutcome::Committed);
    assert_eq!(receipt.seq, Some(store.changes_head().unwrap()));
    assert_identities(&store, &receipt);

    let head = store.changes_head().unwrap();
    let refused = api.hook(
        HookHost::ClaudeCode,
        HookEvent::PostToolUseFailure,
        &failure_envelope(&format!("curl -H 'x: {}'", canary())),
    );
    assert_eq!(refused.output, json!({}), "{refused:?}");
    let receipt = refused
        .receipt
        .expect("a refused write still has an outcome");
    assert_eq!(receipt.outcome, WriteOutcome::Rejected);
    assert_eq!(receipt.seq, None, "a rejection holds no position");
    assert_eq!(store.changes_head().unwrap(), head);
    assert_identities(&store, &receipt);
}

/// D3 review d3-8: a successful command is evidence for the session's pending Failure with
/// the same fingerprint; that evidence write commits, so the `PostToolUse` outcome carries
/// one `committed` receipt covering it.
#[test]
fn a_post_tool_use_that_records_evidence_reports_committed() {
    let (store, api) = store_api();
    let _ = api.hook(
        HookHost::ClaudeCode,
        HookEvent::PostToolUseFailure,
        &failure_envelope("make test"),
    );
    let head = store.changes_head().unwrap();
    let evidence = api.hook(
        HookHost::ClaudeCode,
        HookEvent::PostToolUse,
        &success_envelope("make test"),
    );
    assert_eq!(evidence.output, json!({}), "{evidence:?}");
    let failure = &pending(&api, &store)[0];
    assert!(
        failure.props["evidence_runs"].is_array(),
        "the evidence was written: {}",
        failure.props
    );
    let receipt = evidence.receipt.expect("the evidence write committed");
    assert_eq!(receipt.outcome, WriteOutcome::Committed);
    assert!(receipt.seq.unwrap() >= head, "{receipt:?}");
    assert_eq!(receipt.seq, Some(store.changes_head().unwrap()));

    // negative: a success with no pending Failure of its fingerprint writes nothing
    let nothing = api.hook(
        HookHost::ClaudeCode,
        HookEvent::PostToolUse,
        &success_envelope("make other"),
    );
    assert_eq!(nothing.receipt, None, "{nothing:?}");
}

/// D3 review d3-1 (SS-10 "post-commit failure never becomes rejection"): `hook fix` commits
/// the Improvement with its `FIXED_BY` edge through `remember`, then patches the Failure in a
/// second transaction. A failure of that patch (injected: a trigger aborts the props update)
/// leaves the Improvement committed, so the outcome is `committed` at the Improvement's
/// position, with the note saying the patch failed and the Failure still pending.
#[test]
fn a_fix_whose_failure_patch_fails_after_the_improvement_committed_reports_committed() {
    let s = Scratch::new("kaban-receipt", "fix-post-commit");
    let (store, api) = file_store_api(&s);
    let _ = api.hook(
        HookHost::ClaudeCode,
        HookEvent::PostToolUseFailure,
        &failure_envelope("make test"),
    );
    let failure_id = pending(&api, &store)[0].id.clone();
    raw(&s)
        .execute_batch(
            "CREATE TRIGGER abort_patch BEFORE UPDATE OF props ON node
               WHEN NEW.kind = 'Failure' AND json_extract(NEW.props, '$.pending_fix') = 0
             BEGIN SELECT RAISE(ABORT, 'injected failure'); END;",
        )
        .unwrap();
    let head = store.changes_head().unwrap();
    let out = api.hook(
        HookHost::ClaudeCode,
        HookEvent::Fix,
        &json!({"failure_id": failure_id, "fix": "renamed open_v2 back to open"}),
    );
    let receipt = out
        .receipt
        .clone()
        .expect("the Improvement committed: {out:?}");
    assert_eq!(receipt.outcome, WriteOutcome::Committed, "{out:?}");
    assert!(
        receipt.seq.unwrap() > head,
        "the Improvement and its edge are in the log: {out:?}"
    );
    assert_identities(&store, &receipt);
    assert!(out.note.is_some(), "the patch failure is said: {out:?}");
    assert_eq!(out.output, json!({}), "{out:?}");
    // the edge alone resolves the Failure (M4-14): it left the pending list although the
    // patch never applied
    let failure = store
        .get(&NodeId::parse(&failure_id).unwrap(), true)
        .unwrap()
        .unwrap();
    assert_eq!(
        failure.props["pending_fix"],
        json!(true),
        "the patch did not apply: {}",
        failure.props
    );
    assert!(
        pending(&api, &store).is_empty(),
        "the FIXED_BY edge is stored"
    );
}

/// D3 review d3-1: `post-tool-use-failure` commits the Failure through `remember`, then pairs
/// it with the session in a second write. A failure of the pairing (injected: a trigger
/// aborts the `session_edits` insert) leaves the Failure committed, so the outcome is
/// `committed` at its position, with the note saying the pairing failed.
#[test]
fn a_failure_whose_session_pairing_fails_after_the_record_committed_reports_committed() {
    let s = Scratch::new("kaban-receipt", "pairing-post-commit");
    let (store, api) = file_store_api(&s);
    raw(&s)
        .execute_batch(
            "CREATE TRIGGER abort_pairing BEFORE INSERT ON session_edits
             BEGIN SELECT RAISE(ABORT, 'injected failure'); END;",
        )
        .unwrap();
    let head = store.changes_head().unwrap();
    let out = api.hook(
        HookHost::ClaudeCode,
        HookEvent::PostToolUseFailure,
        &failure_envelope("make test"),
    );
    let receipt = out.receipt.clone().expect("the Failure committed: {out:?}");
    assert_eq!(receipt.outcome, WriteOutcome::Committed, "{out:?}");
    assert!(receipt.seq.unwrap() > head, "{out:?}");
    assert_eq!(receipt.seq, Some(store.changes_head().unwrap()));
    assert_identities(&store, &receipt);
    assert!(out.note.is_some(), "the pairing failure is said: {out:?}");
    // stored, though not paired with the session (the pairing is what failed)
    assert_eq!(
        store
            .list_nodes("Failure", api.workspace().as_str(), 0, 10)
            .unwrap()
            .len(),
        1,
        "the Failure is stored"
    );
    assert!(pending(&api, &store).is_empty(), "no session pairing");
}
