//! The rescan job (core-spec S4 "rescan job after policy change"; core-plan C2.7; MD-18,
//! MD-19): nodes stored under an older policy are re-judged by the loaded one, reject hits are
//! tombstoned with a `policy-reject` event, redact hits are masked in place, and
//! `index_meta.policy_hash` records the policy the whole database has been checked against.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::{Scratch, fact};
use kaban_api::{KabanApi, RESCAN_BATCH, RecallRequest, RememberRequest, RescanStatus};
use kaban_core::ActorId;
use kaban_core::{
    GovernanceStore, GraphStore, IndexMeta, IngestStore, Lifecycle, MemoryStore, NodeId,
    RecallQuery,
};
use kaban_policy::Policy;
use kaban_store_sqlite::SqliteStore;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// Rescan-specific view of the scratch directory: the store under test.
trait ScratchExt {
    fn store(&self) -> Arc<SqliteStore>;
}
impl ScratchExt for Scratch {
    fn store(&self) -> Arc<SqliteStore> {
        Arc::new(SqliteStore::open(&self.db(), &self.keys()).unwrap())
    }
}

/// Policy v1: nothing user-defined (builtin rules only).
const V1: &str = "";
/// Policy v2: one reject term and one redact term.
const MASK: &str = "[REDACTED:old-password]";
const V2: &str = "words:\n  - { term: nightjar, action: reject, id: codename }\n  - { term: hunter2, action: redact, id: old-password }\n";

fn api_with(store: &Arc<SqliteStore>, policy: &str) -> KabanApi {
    let policy = Policy::from_file(Policy::parse(policy.as_bytes()).unwrap(), "t".into()).unwrap();
    KabanApi::new(
        Arc::clone(store) as Arc<dyn GraphStore>,
        policy,
        store.signer(),
        None,
    )
}

fn nid(id: &str) -> NodeId {
    NodeId::parse(id).unwrap()
}

/// Whether `node.text` of `id` is stored as a `BLOB` (an encoded body, spec 05 SS-17).
fn encoded(s: &Scratch, id: &str) -> bool {
    let kind: String = rusqlite::Connection::open(s.db())
        .unwrap()
        .query_row("SELECT typeof(text) FROM node WHERE id = ?1", [id], |r| {
            r.get(0)
        })
        .unwrap();
    kind == "blob"
}

/// `(op, tool, field, rules)` of every audit row, oldest first (the chain of record).
fn audit_rows(s: &Scratch) -> Vec<(String, String, String, String)> {
    // the store exposes no row reader; the table is the record (core-spec S3)
    let conn = rusqlite::Connection::open(s.db()).unwrap();
    let mut st = conn
        .prepare("SELECT op, tool, field, rules FROM audit ORDER BY seq")
        .unwrap();
    st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
}

#[test]
fn a_node_a_newer_policy_rejects_is_hidden_after_startup_with_an_audit_row_and_an_event() {
    // Arrange: a node stored under v1 whose text v2 rejects, and a clean node
    let s = Scratch::new("kaban-api-rescan", "reject");
    let store = s.store();
    let v1 = api_with(&store, V1);
    let secret = v1
        .remember(fact("the nightjar codename must not leak"))
        .unwrap();
    let clean = v1.remember(fact("a plain note about cargo")).unwrap();
    let before = audit_rows(&s).len();
    assert!(store.get(&nid(&secret.id), false).unwrap().is_some());

    // Act: the server starts with v2 and runs the rescan job
    let v2 = api_with(&store, V2);
    assert_ne!(v1.policy_version(), v2.policy_version());
    let outcome = v2
        .rescan_if_policy_changed(&CancellationToken::new())
        .unwrap();

    // Assert: hidden from every default read, the audit row exists, the event names the reason
    assert_eq!(outcome.status, RescanStatus::Completed, "{outcome:?}");
    assert_eq!((outcome.rejected, outcome.kept), (1, 1), "{outcome:?}");
    assert!(
        store.get(&nid(&secret.id), false).unwrap().is_none(),
        "rejected node still visible by id"
    );
    let hidden = store.get(&nid(&secret.id), true).unwrap().unwrap();
    assert_eq!(hidden.lifecycle, Lifecycle::Tombstoned);
    assert_eq!(hidden.policy_version, v2.policy_version());
    let found = v2
        .recall(RecallRequest {
            query: "codename".into(),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(found.count, 0, "{found:?}");
    let rows = audit_rows(&s);
    let new_rows = &rows[before..];
    assert!(
        new_rows.iter().any(|(op, tool, _, rules)| op == "reject"
            && tool == "rescan"
            && rules.contains("codename")),
        "no rescan reject audit row: {new_rows:?}"
    );
    // the OWN-65 `unstamped` reason row of a record created outside any repository is not a
    // lifecycle event (engine-plan E2.0)
    let events: Vec<_> = store
        .events(&nid(&secret.id))
        .unwrap()
        .into_iter()
        .filter(|e| e.kind != "unstamped")
        .collect();
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0].reason, "policy-reject");
    assert_eq!(events[0].kind, "tombstoned");
    // the clean node is untouched apart from its policy stamp
    let kept = store.get(&nid(&clean.id), false).unwrap().unwrap();
    assert_eq!(kept.policy_version, v2.policy_version());
    assert_eq!(kept.rev, 1);
    assert!(
        store
            .events(&nid(&clean.id))
            .unwrap()
            .iter()
            .all(|e| e.kind == "unstamped")
    );
    // the database is now checked against v2: a second start does nothing
    assert_eq!(
        store.index_meta().unwrap().get(IndexMeta::POLICY_HASH),
        Some(v2.policy_version().as_str())
    );
    let again = v2
        .rescan_if_policy_changed(&CancellationToken::new())
        .unwrap();
    assert_eq!(again.status, RescanStatus::UpToDate, "{again:?}");
    assert_eq!(audit_rows(&s).len(), rows.len());
}

#[test]
fn a_redactable_term_is_masked_in_place_with_a_new_rev_and_the_policy_version_updated() {
    redactable_term_is_masked_in_place(false);
}

/// Spec 05 SS-17 (T7 review t7-5): the rescan over an encoded body is the rescan over a plain
/// one. The node is written encoded under v1, the rescan decodes it, masks it and stores the
/// masked body encoded again: the same outcome, revision, stamp and audit row, and the body
/// reads back masked.
#[test]
fn a_redactable_term_is_masked_in_place_over_an_encoded_body() {
    redactable_term_is_masked_in_place(true);
}

fn redactable_term_is_masked_in_place(compressed: bool) {
    // Arrange: text, summary, a tag and a props leaf carry the term v2 redacts; the encoded
    // variant's text is long enough for the body codec to encode it (a short body stays plain)
    let s = Scratch::new(
        "kaban-api-rescan",
        if compressed {
            "redact-encoded"
        } else {
            "redact"
        },
    );
    let store = s.store();
    let tail = if compressed {
        ", written in the shore log beside the decision record so the whole team could read it"
    } else {
        ""
    };
    let v1 = api_with(&store, V1);
    let node = v1
        .remember(RememberRequest {
            summary: Some("summary with hunter2".into()),
            tags: vec!["hunter2".into(), "clean".into()],
            props: Some(serde_json::json!({"note": "props hunter2 leaf", "n": 1})),
            ..fact(&format!("the old door code was hunter2 for years{tail}"))
        })
        .unwrap();
    let stored = store.get(&nid(&node.id), false).unwrap().unwrap();
    assert_eq!(stored.rev, 1);
    assert!(
        encoded(&s, &node.id),
        "every body is stored encoded (OWN-M)"
    );
    let before = audit_rows(&s).len();

    // Act
    let v2 = api_with(&store, V2);
    let outcome = v2
        .rescan_if_policy_changed(&CancellationToken::new())
        .unwrap();

    // Assert: masked in place, still active, new rev, stamped; the verdict is in the chain
    assert_eq!((outcome.redacted, outcome.rejected), (1, 0), "{outcome:?}");
    let masked = store.get(&nid(&node.id), false).unwrap().unwrap();
    assert_eq!(masked.lifecycle, Lifecycle::Active);
    assert_eq!(
        masked.text,
        format!("the old door code was {MASK} for years{tail}")
    );
    assert!(encoded(&s, &node.id), "the masked body is stored encoded");
    assert_eq!(
        masked.summary.as_deref(),
        Some(&*format!("summary with {MASK}"))
    );
    assert_eq!(masked.tags, vec![MASK.to_string(), "clean".to_string()]);
    assert_eq!(
        masked.props["note"],
        serde_json::json!(format!("props {MASK} leaf"))
    );
    assert_eq!(masked.props["n"], serde_json::json!(1));
    assert_eq!(masked.rev, 2);
    assert_eq!(masked.policy_version, v2.policy_version());
    assert!(
        !masked.text.contains("hunter2"),
        "the stored text still carries the term"
    );
    let rows = audit_rows(&s);
    assert!(
        rows[before..]
            .iter()
            .any(|(op, tool, field, rules)| op == "redact"
                && tool == "rescan"
                && field == "text"
                && rules.contains("old-password")),
        "no rescan redact audit row: {:?}",
        &rows[before..]
    );
    // the text index follows the masked text: the raw term no longer matches (asked at the
    // store, since the API masks the query itself before FTS sees it), the rest still does;
    // a store past the retirement serves recall through an attached index only (SS-16)
    if !compressed {
        let by_term = store
            .recall(&RecallQuery {
                query: "hunter2".into(),
                ..Default::default()
            })
            .unwrap();
        assert!(by_term.is_empty(), "{by_term:?}");
        let by_rest = store
            .recall(&RecallQuery {
                query: "door years".into(),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(by_rest.len(), 1, "{by_rest:?}");
    }
    assert_eq!(
        store.index_meta().unwrap().get(IndexMeta::POLICY_HASH),
        Some(v2.policy_version().as_str())
    );
}

/// Nodes still stamped with another policy version than `version`.
fn pending(s: &Scratch, version: &str) -> usize {
    let conn = rusqlite::Connection::open(s.db()).unwrap();
    conn.query_row(
        "SELECT count(*) FROM node WHERE policy_version <> ?1 AND lifecycle <> 'purged'",
        [version],
        |r| r.get::<_, i64>(0),
    )
    .map(|n| usize::try_from(n).unwrap())
    .unwrap()
}

#[test]
fn cancellation_between_batches_leaves_earlier_batches_committed_and_the_rest_pending() {
    // Arrange: 2 batches and a half of nodes under v1, a reject term in the first and the
    // third batch
    let s = Scratch::new("kaban-api-rescan", "cancel");
    let store = s.store();
    let v1 = api_with(&store, V1);
    let total = 2 * RESCAN_BATCH + RESCAN_BATCH / 2;
    for i in 0..total {
        let text = if i == 10 || i == total - 10 {
            format!("note {i} mentions the nightjar codename")
        } else {
            format!("note {i} about cargo and tokio")
        };
        v1.remember(fact(&text)).unwrap();
    }
    let v2 = api_with(&store, V2);
    let version = v2.policy_version();
    assert_eq!(pending(&s, &version), total);

    // Act: the token is already cancelled, so the job checks it after its first batch
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    let first = v2.rescan_if_policy_changed(&cancelled).unwrap();

    // Assert: exactly one batch committed, the rest pending, the hash not recorded
    assert_eq!(first.status, RescanStatus::Cancelled, "{first:?}");
    assert_eq!(
        (first.batches, first.scanned),
        (1, RESCAN_BATCH as u64),
        "{first:?}"
    );
    assert_eq!(first.rejected, 1, "{first:?}");
    assert_eq!(pending(&s, &version), total - RESCAN_BATCH);
    assert_ne!(
        store.index_meta().unwrap().get(IndexMeta::POLICY_HASH),
        Some(version.as_str()),
        "policy_hash must not claim a complete rescan"
    );
    let audit_after_first = audit_rows(&s).len();

    // Act again: the re-run resumes with what is pending and finishes
    let second = v2
        .rescan_if_policy_changed(&CancellationToken::new())
        .unwrap();

    // Assert: the remaining nodes only, the second reject found, nothing judged twice
    assert_eq!(second.status, RescanStatus::Completed, "{second:?}");
    assert_eq!(second.scanned, (total - RESCAN_BATCH) as u64, "{second:?}");
    assert_eq!(
        second.batches, 2,
        "one full and one partial batch; the empty terminating read is not a batch: {second:?}"
    );
    assert_eq!(second.rejected, 1, "{second:?}");
    assert_eq!(pending(&s, &version), 0);
    assert_eq!(
        store.index_meta().unwrap().get(IndexMeta::POLICY_HASH),
        Some(version.as_str())
    );
    // one reject audit row per rejected node, none for the kept ones
    let rows = audit_rows(&s);
    let rescan_rows: Vec<_> = rows
        .iter()
        .filter(|(_, tool, _, _)| tool == "rescan")
        .collect();
    assert_eq!(rescan_rows.len(), 2, "{rescan_rows:?}");
    assert!(rows.len() > audit_after_first);
    // a third start has nothing to do
    let third = v2
        .rescan_if_policy_changed(&CancellationToken::new())
        .unwrap();
    assert_eq!(third.status, RescanStatus::UpToDate, "{third:?}");
}

#[test]
fn policy_handle_shares_the_apis_policy_instance_with_ingestion() {
    // Arrange
    let store = Arc::new(SqliteStore::open_memory().unwrap());
    let api = api_with(&store, V2);

    // Act: what docs-ingest receives (D-3), and what a clone of the API holds
    let handle = api.policy_handle();
    let through_clone = api.clone().policy_handle();

    // Assert: one instance, and it is the policy the API judges with
    assert!(Arc::ptr_eq(&handle, &through_clone));
    let guard = handle.read().unwrap();
    assert_eq!(guard.effective_hash(), api.policy_version());
    assert!(guard.scan("the nightjar codename").rejected);
}

// ---- M2 review R2-2 and R2-3 (C2.11): one inventory of persisted node strings ----

/// Every TEXT column of one node row as stored (the raw bytes the record keeps).
fn node_columns(s: &Scratch, id: &str) -> Vec<(String, String)> {
    let conn = rusqlite::Connection::open(s.db()).unwrap();
    let mut st = conn.prepare("SELECT * FROM node WHERE id = ?1").unwrap();
    let names: Vec<String> = st.column_names().iter().map(ToString::to_string).collect();
    st.query_row([id], |r| {
        let mut out = Vec::new();
        for (i, name) in names.iter().enumerate() {
            if let Ok(Some(v)) = r.get::<_, Option<String>>(i) {
                out.push((name.clone(), v));
            }
        }
        Ok(out)
    })
    .unwrap()
}

/// R2-2 (read path): the projection judges the same inventory of persisted strings as the
/// rescan. A reject hit confined to `session_id` hides the node from reads under the new
/// policy before the rescan job has reached it.
#[test]
fn a_reject_hit_confined_to_session_id_hides_the_node_on_the_read_path() {
    let s = Scratch::new("kaban-api-rescan", "project-session");
    let store = s.store();
    let v1 = api_with(&store, V1);
    let node = v1
        .remember(RememberRequest {
            session_id: Some("session-nightjar-7".into()),
            ..fact("a plain note about the build cache")
        })
        .unwrap();
    let v2 = api_with(&store, V2);
    // not rescanned yet: the stored row still carries v1's stamp
    assert_eq!(
        store
            .get(&nid(&node.id), false)
            .unwrap()
            .unwrap()
            .policy_version,
        v1.policy_version()
    );
    let found = v2
        .recall(RecallRequest {
            query: "build cache".into(),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(
        found.count, 0,
        "a node whose session_id hits a reject rule leaked through the projection: {found:?}"
    );
}

/// Every TEXT value of the `events`, `changes` and `audit` tables (what the bookkeeping keeps).
fn bookkeeping_text(s: &Scratch) -> Vec<String> {
    let conn = rusqlite::Connection::open(s.db()).unwrap();
    let mut out = Vec::new();
    for table in ["events", "changes", "audit"] {
        let mut st = conn.prepare(&format!("SELECT * FROM {table}")).unwrap();
        let n = st.column_count();
        let rows = st
            .query_map([], |r| {
                let mut row = Vec::new();
                for i in 0..n {
                    if let Ok(Some(v)) = r.get::<_, Option<String>>(i) {
                        row.push(v);
                    }
                }
                Ok(row)
            })
            .unwrap();
        for row in rows {
            out.extend(row.unwrap());
        }
    }
    out
}

/// Active rows whose decoded body or summary carries `term`: the columns the text index is
/// derived from (spec 05 OWN-M; the in-database FTS5 table is gone).
fn body_matches(s: &Scratch, term: &str) -> i64 {
    let conn = rusqlite::Connection::open(s.db()).unwrap();
    kaban_store_sqlite::register_body_reader(&conn).unwrap();
    conn.query_row(
        "SELECT count(*) FROM node WHERE lifecycle = 'active' \
         AND (instr(kaban_body(text), ?1) > 0 OR instr(coalesce(summary, ''), ?1) > 0)",
        [term],
        |r| r.get(0),
    )
    .unwrap()
}

/// R2-2 (rescan): the inventory covers every persisted node string, not only the content
/// fields. A redact hit confined to `session_id` or `repo` is rewritten in place; a reject hit
/// confined to `path_scope` tombstones the node. Afterwards the raw terms are absent from every
/// node column, from FTS and from the events, changes and audit tables.
#[test]
fn hits_confined_to_session_id_repo_and_path_scope_are_rewritten_or_hidden_by_the_rescan() {
    // Arrange under v1: three nodes whose text is clean
    let s = Scratch::new("kaban-api-rescan", "inventory");
    let store = s.store();
    let v1 = api_with(&store, V1);
    let by_session = v1
        .remember(RememberRequest {
            session_id: Some("pair-session-hunter2".into()),
            ..fact("the first clean note")
        })
        .unwrap();
    let by_repo = v1.remember(fact("the second clean note")).unwrap();
    {
        // `repo` has no writer yet (M3+): the column is filled the way a migration would
        let conn = rusqlite::Connection::open(s.db()).unwrap();
        SqliteStore::bind_writer(&conn, &ActorId::default()).unwrap();
        conn.execute(
            "UPDATE node SET repo = 'git@forge:acme/hunter2-tools.git' WHERE id = ?1",
            [by_repo.id.as_str()],
        )
        .unwrap();
    }
    let by_scope = v1
        .remember(RememberRequest {
            kind: "Decision".into(),
            props: Some(serde_json::json!({"rationale": "a Decision needs one (C3.1)"})),
            ..fact("the third clean note, a decision")
        })
        .unwrap();
    store
        .transition(
            &nid(&by_scope.id),
            kaban_core::Governance::Accepted,
            kaban_core::NewEvent {
                reason: "accepted in review".into(),
                path_scope: vec!["src/nightjar/**".into()],
                signer_kind: kaban_core::SignerKind::HumanCli,
                commit_sha: Some("abc1234".into()),
                ..Default::default()
            },
            &kaban_core::WriteCtx::new(
                v1.policy_version(),
                kaban_core::WorkspaceId::default(),
                kaban_core::Actor::Agent,
            ),
        )
        .unwrap();
    let scoped = store.get(&nid(&by_scope.id), false).unwrap().unwrap();
    assert_eq!(scoped.path_scope, vec!["src/nightjar/**".to_string()]);
    assert_eq!(
        scoped.policy_version,
        v1.policy_version(),
        "stamped with v1 by the transition"
    );
    let bookkeeping_before = bookkeeping_text(&s);

    // Act: start with v2
    let v2 = api_with(&store, V2);
    let outcome = v2
        .rescan_if_policy_changed(&CancellationToken::new())
        .unwrap();

    // Assert: two rewritten, one hidden
    assert_eq!(
        (outcome.redacted, outcome.rejected, outcome.kept),
        (2, 1, 0),
        "{outcome:?}"
    );
    let n = store.get(&nid(&by_session.id), false).unwrap().unwrap();
    assert_eq!(
        n.session_id.as_deref(),
        Some(&*format!("pair-session-{MASK}"))
    );
    assert_eq!((n.rev, n.lifecycle), (2, Lifecycle::Active));
    let n = store.get(&nid(&by_repo.id), false).unwrap().unwrap();
    assert_eq!(
        n.repo.as_deref(),
        Some(&*format!("git@forge:acme/{MASK}-tools.git"))
    );
    assert_eq!((n.rev, n.lifecycle), (2, Lifecycle::Active));
    assert!(
        store.get(&nid(&by_scope.id), false).unwrap().is_none(),
        "a reject hit in path_scope must hide the node"
    );
    assert_eq!(
        store
            .get(&nid(&by_scope.id), true)
            .unwrap()
            .unwrap()
            .lifecycle,
        Lifecycle::Tombstoned
    );
    for id in [&by_session.id, &by_repo.id] {
        for (col, value) in node_columns(&s, id) {
            assert!(
                !value.contains("hunter2"),
                "{id}: column {col} still carries the raw term: {value}"
            );
        }
    }
    assert_eq!(body_matches(&s, "hunter2"), 0);
    assert_eq!(body_matches(&s, "nightjar"), 0);
    // nothing the rescan wrote (events, changes, audit) carries a raw term; the one row that
    // does predates it: the acceptance event's `path_scope`, written under v1 and append-only
    // by design (F17), so a policy change cannot rewrite it (read-time gating of events is the
    // `events` surface's job)
    let bookkeeping_after = bookkeeping_text(&s);
    let written: Vec<&String> = bookkeeping_after
        .iter()
        .filter(|v| !bookkeeping_before.contains(v))
        .collect();
    assert!(
        written.len() >= 3,
        "the rescan wrote audit, event and change rows: {written:?}"
    );
    for value in written {
        assert!(
            !value.contains("hunter2") && !value.contains("nightjar"),
            "the rescan wrote a raw term into the bookkeeping: {value}"
        );
    }
    let carried: Vec<&String> = bookkeeping_after
        .iter()
        .filter(|v| v.contains("hunter2") || v.contains("nightjar"))
        .collect();
    assert_eq!(
        carried,
        vec![&"[\"src/nightjar/**\"]".to_string()],
        "only the pre-existing acceptance event keeps the term"
    );
    // the verdicts name the fields they were found in
    let rows = audit_rows(&s);
    let fields: Vec<&str> = rows
        .iter()
        .filter(|(_, tool, ..)| tool == "rescan")
        .map(|(_, _, field, _)| field.as_str())
        .collect();
    assert!(fields.contains(&"session_id"), "{fields:?}");
    assert!(fields.contains(&"repo"), "{fields:?}");
    assert!(fields.contains(&"path_scope"), "{fields:?}");
}

/// R2-2 (`content_hash` after an in-place redact): the stored hash is replaced by one derived
/// from the node id and the masked fields: it differs from the pre-redaction hash and from a
/// plain sha256 of the original text (no guess oracle on the masked secret), and from the
/// masked text's `remember` hash (so a later `remember` of the masked text is a new node, and
/// two notes masked alike never collide on `node_hash_active`).
#[test]
fn content_hash_after_redact_is_neither_the_original_hash_nor_the_masked_remember_hash() {
    let s = Scratch::new("kaban-api-rescan", "hash");
    let store = s.store();
    let v1 = api_with(&store, V1);
    let text = "the old door code was hunter2 for years";
    let a = v1.remember(fact(text)).unwrap();
    let b = v1
        .remember(fact("the old door code was hunter3 for years"))
        .unwrap();
    let before = store.get(&nid(&a.id), false).unwrap().unwrap().content_hash;

    let v2 = api_with(
        &store,
        "words:\n  - { term: hunter2, action: redact, id: old-password }\n  - { term: hunter3, action: redact, id: old-password }\n",
    );
    let outcome = v2
        .rescan_if_policy_changed(&CancellationToken::new())
        .unwrap();
    assert_eq!(outcome.redacted, 2, "{outcome:?}");

    let masked_a = store.get(&nid(&a.id), false).unwrap().unwrap();
    let masked_b = store.get(&nid(&b.id), false).unwrap().unwrap();
    assert_eq!(masked_a.text, masked_b.text, "both mask to the same text");
    assert_ne!(
        masked_a.content_hash, before,
        "the pre-redaction hash is gone"
    );
    assert_ne!(
        masked_a.content_hash,
        kaban_policy::sha256_hex(text.as_bytes()),
        "no plain digest of the original"
    );
    assert_ne!(
        masked_a.content_hash, masked_b.content_hash,
        "two notes masked alike keep distinct identities"
    );
    // the masked text's own `remember` hash differs: a new note is created, not deduplicated
    let again = v2.remember(fact(&masked_a.text)).unwrap();
    assert_eq!(
        again.status,
        kaban_api::RememberStatus::Created,
        "{again:?}"
    );
    assert!(![a.id.as_str(), b.id.as_str()].contains(&again.id.as_str()));
}

/// R2-3: the startup decision does not trust the database-wide marker alone. A node stamped
/// with an older policy by another process after `policy_hash` was recorded is found by the
/// stale-row query at the next start and rescanned (here: a reject term, so it is hidden).
#[test]
fn a_stale_node_written_after_the_marker_was_set_is_rescanned_at_the_next_start() {
    let s = Scratch::new("kaban-api-rescan", "stale-after-marker");
    let store = s.store();
    let v2 = api_with(&store, V2);
    v2.remember(fact("a clean note under v2")).unwrap();
    let first = v2
        .rescan_if_policy_changed(&CancellationToken::new())
        .unwrap();
    assert_eq!(
        store.index_meta().unwrap().get(IndexMeta::POLICY_HASH),
        Some(v2.policy_version().as_str()),
        "{first:?}"
    );
    // another process still running an older policy writes a node the old policy let through
    let stale_ctx = kaban_core::WriteCtx::new(
        "an-older-policy-version",
        kaban_core::WorkspaceId::default(),
        kaban_core::Actor::Agent,
    );
    let stale = match store
        .remember(
            kaban_core::NewNode {
                kind: "Fact".into(),
                text: "the nightjar codename written by an old process".into(),
                ..Default::default()
            },
            &[],
            &stale_ctx,
        )
        .unwrap()
    {
        kaban_core::RememberOutcome::Created(n) => n,
        other @ kaban_core::RememberOutcome::Existing { .. } => panic!("{other:?}"),
    };
    assert_eq!(pending(&s, &v2.policy_version()), 1);

    // the next start with v2 (marker already equal) must still find and judge it
    let next = v2
        .rescan_if_policy_changed(&CancellationToken::new())
        .unwrap();
    assert_eq!((next.scanned, next.rejected), (1, 1), "{next:?}");
    assert_eq!(next.status, RescanStatus::Completed, "{next:?}");
    assert!(store.get(&nid(&stale.id), false).unwrap().is_none());
    assert_eq!(pending(&s, &v2.policy_version()), 0);
    // and a start with nothing stale scans nothing
    let idle = v2
        .rescan_if_policy_changed(&CancellationToken::new())
        .unwrap();
    assert_eq!(
        (idle.status, idle.scanned),
        (RescanStatus::UpToDate, 0),
        "{idle:?}"
    );
}
