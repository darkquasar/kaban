//! The rescan job (core-spec S4 "rescan job after policy change"; core-plan C2.7; MD-18,
//! MD-19): nodes stored under an older policy are re-judged by the loaded one, reject hits are
//! tombstoned, redact hits are masked in place, and
//! `index_meta.policy_hash` records the policy the whole database has been checked against.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::{Scratch, fact};
use kaban_api::{KabanApi, RESCAN_BATCH, RecallRequest, RememberRequest, RescanStatus};
use kaban_core::ActorId;
use kaban_core::{GraphStore, IndexMeta, IngestStore, Lifecycle, MemoryStore, NodeId, RecallQuery};
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

#[test]
fn a_node_a_newer_policy_rejects_is_hidden_after_startup() {
    // Arrange: a node stored under v1 whose text v2 rejects, and a clean node
    let s = Scratch::new("kaban-api-rescan", "reject");
    let store = s.store();
    let v1 = api_with(&store, V1);
    let secret = v1
        .remember(fact("the nightjar codename must not leak"))
        .unwrap();
    let clean = v1.remember(fact("a plain note about cargo")).unwrap();
    assert!(store.get(&nid(&secret.id), false).unwrap().is_some());

    // Act: the server starts with v2 and runs the rescan job
    let v2 = api_with(&store, V2);
    assert_ne!(v1.policy_version(), v2.policy_version());
    let outcome = v2
        .rescan_if_policy_changed(&CancellationToken::new())
        .unwrap();

    // Assert: hidden from every default read
    assert_eq!(outcome.status, RescanStatus::Completed, "{outcome:?}");
    assert_eq!((outcome.rejected, outcome.kept), (1, 1), "{outcome:?}");
    assert!(
        store.get(&nid(&secret.id), false).unwrap().is_none(),
        "rejected node still visible by id"
    );
    let hidden = store.get(&nid(&secret.id), true).unwrap().unwrap();
    assert_eq!(hidden.lifecycle, Lifecycle::Tombstoned);
    let found = v2
        .recall(RecallRequest {
            query: "codename".into(),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(found.count, 0, "{found:?}");
    // the clean node is untouched
    let kept = store.get(&nid(&clean.id), false).unwrap().unwrap();
    assert_eq!(kept.rev, 1);
    // the database is now checked against v2: a second start does nothing
    assert_eq!(
        store.index_meta().unwrap().get(IndexMeta::POLICY_HASH),
        Some(v2.policy_version().as_str())
    );
    let again = v2
        .rescan_if_policy_changed(&CancellationToken::new())
        .unwrap();
    assert_eq!(again.status, RescanStatus::UpToDate, "{again:?}");
}

#[test]
fn a_redactable_term_is_masked_in_place_with_a_new_rev() {
    redactable_term_is_masked_in_place(false);
}

/// Spec 05 SS-17 (T7 review t7-5): the rescan over an encoded body is the rescan over a plain
/// one. The node is written encoded under v1, the rescan decodes it, masks it and stores the
/// masked body encoded again: the same outcome and revision, and the body
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

    // Act
    let v2 = api_with(&store, V2);
    let outcome = v2
        .rescan_if_policy_changed(&CancellationToken::new())
        .unwrap();

    // Assert: masked in place, still active, new rev, stamped
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
    assert!(
        !masked.text.contains("hunter2"),
        "the stored text still carries the term"
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

#[test]
fn cancellation_between_batches_leaves_earlier_batches_committed_and_the_rest_pending() {
    // Arrange: 2 batches and a half of nodes under v1, a reject term in the first and the
    // third batch
    let s = Scratch::new("kaban-api-rescan", "cancel");
    let store = s.store();
    let total = 2 * RESCAN_BATCH + RESCAN_BATCH / 2;
    // seeded in one batch transaction: rows carry no policy stamp (REC-08), so how they were
    // written does not matter to the rescan, and one transaction keeps the fixture fast
    let ops = (0..total)
        .map(|i| {
            let text = if i == 10 || i == total - 10 {
                format!("note {i} mentions the nightjar codename")
            } else {
                format!("note {i} about cargo and tokio")
            };
            kaban_core::Op::UpsertNode {
                id: NodeId::new(),
                node: kaban_core::NewNode {
                    kind: "Fact".into(),
                    text,
                    ..Default::default()
                },
            }
        })
        .collect();
    store
        .apply_batch(
            kaban_core::WriteBatch::new(kaban_core::WorkspaceId::default(), ops),
            &kaban_core::WriteCtx::new(
                kaban_core::WorkspaceId::default(),
                kaban_core::Actor::Agent,
            ),
        )
        .unwrap();
    let v2 = api_with(&store, V2);
    let version = v2.policy_version();

    // Act: the token is already cancelled, so the job checks it after its first batch
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    let first = v2.rescan_if_policy_changed(&cancelled).unwrap();

    // Assert: exactly one batch committed, the hash not recorded
    assert_eq!(first.status, RescanStatus::Cancelled, "{first:?}");
    assert_eq!(
        (first.batches, first.scanned),
        (1, RESCAN_BATCH as u64),
        "{first:?}"
    );
    assert_eq!(first.rejected, 1, "{first:?}");
    assert_ne!(
        store.index_meta().unwrap().get(IndexMeta::POLICY_HASH),
        Some(version.as_str()),
        "policy_hash must not claim a complete rescan"
    );

    // Act again: nodes carry no stamp (REC-08), so the re-run walks the table again and
    // finishes
    let second = v2
        .rescan_if_policy_changed(&CancellationToken::new())
        .unwrap();

    // Assert: every node judged, the first reject still tombstoned, the second found
    assert_eq!(second.status, RescanStatus::Completed, "{second:?}");
    assert_eq!(second.scanned, total as u64, "{second:?}");
    assert_eq!(
        second.batches, 3,
        "two full and one partial batch; the empty terminating read is not a batch: {second:?}"
    );
    assert_eq!(second.rejected, 2, "{second:?}");
    assert_eq!(
        store.index_meta().unwrap().get(IndexMeta::POLICY_HASH),
        Some(version.as_str())
    );
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
    let names: Vec<String> = st.column_names().map(ToString::to_string).collect();
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
    // not rescanned yet: the read projection hides it on its own
    assert!(store.get(&nid(&node.id), false).unwrap().is_some());
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

/// Every TEXT value of the `changes` table (what the bookkeeping keeps).
fn bookkeeping_text(s: &Scratch) -> Vec<String> {
    let conn = rusqlite::Connection::open(s.db()).unwrap();
    let mut out = Vec::new();
    for table in ["changes"] {
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
/// node column, from FTS and from the bookkeeping tables.
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
            path_scope: vec!["src/nightjar/**".into()],
            ..fact("the third clean note, a decision")
        })
        .unwrap();
    let scoped = store.get(&nid(&by_scope.id), false).unwrap().unwrap();
    assert_eq!(scoped.path_scope, vec!["src/nightjar/**".to_string()]);
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
    // nothing the rescan wrote (the change rows) carries a raw term, and no bookkeeping row
    // carries one at all (the acceptance event that kept `path_scope` went with governance)
    let bookkeeping_after = bookkeeping_text(&s);
    let written: Vec<&String> = bookkeeping_after
        .iter()
        .filter(|v| !bookkeeping_before.contains(v))
        .collect();
    for value in written {
        assert!(
            !value.contains("hunter2") && !value.contains("nightjar"),
            "the rescan wrote a raw term into the bookkeeping: {value}"
        );
    }
    assert!(
        !bookkeeping_after
            .iter()
            .any(|v| v.contains("hunter2") || v.contains("nightjar")),
        "no bookkeeping row keeps a term"
    );
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

#[test]
fn a_stale_node_written_after_the_marker_was_set_is_rescanned_at_the_next_start() {
    let s = Scratch::new("kaban-api-rescan", "stale-after-marker");
    let store = s.store();
    let v2 = api_with(&store, V2);
    v2.remember(fact("a clean note under v2")).unwrap();
    let first = v2
        .rescan_if_policy_changed(&CancellationToken::new())
        .unwrap();
    assert_eq!(first.status, RescanStatus::Completed, "{first:?}");
    assert_eq!(
        store.index_meta().unwrap().get(IndexMeta::POLICY_HASH),
        Some(v2.policy_version().as_str()),
        "{first:?}"
    );
    // another process, its own connection, still running the older policy writes a node the
    // old policy lets through
    let other = s.store();
    let v1 = api_with(&other, V1);
    let stale = v1
        .remember(fact("the nightjar codename written by an old process"))
        .unwrap();
    drop(v1);
    drop(other);

    // the next start with v2 (marker already equal) must still find and judge it
    let next = v2
        .rescan_if_policy_changed(&CancellationToken::new())
        .unwrap();
    assert_eq!((next.scanned, next.rejected), (1, 1), "{next:?}");
    assert_eq!(next.status, RescanStatus::Completed, "{next:?}");
    assert!(store.get(&nid(&stale.id), false).unwrap().is_none());
    // and a start with nothing new scans nothing
    let idle = v2
        .rescan_if_policy_changed(&CancellationToken::new())
        .unwrap();
    assert_eq!(
        (idle.status, idle.scanned),
        (RescanStatus::UpToDate, 0),
        "{idle:?}"
    );
}

/// The stored `props` of the `rel` edge from `src` to `dst`, if it exists.
fn edge_props(s: &Scratch, src: &str, dst: &str) -> Option<String> {
    rusqlite::Connection::open(s.db())
        .unwrap()
        .query_row(
            "SELECT e.props FROM edge e JOIN node a ON a.rid = e.src JOIN node b ON b.rid = e.dst WHERE a.id = ?1 AND b.id = ?2",
            [src, dst],
            |r| r.get(0),
        )
        .ok()
}

#[test]
fn link_properties_a_newer_policy_forbids_are_masked_or_dropped_by_the_rescan() {
    // Arrange: two links whose properties v1 accepts, one v2 redacts and one v2 rejects
    let s = Scratch::new("kaban-api-rescan", "edge-props");
    let store = s.store();
    let v1 = api_with(&store, V1);
    let a = v1.remember(fact("a note about cargo")).unwrap().id;
    let b = v1.remember(fact("a note about tokio")).unwrap().id;
    let c = v1.remember(fact("a note about serde")).unwrap().id;
    for (to, note) in [(&b, "the old password hunter2"), (&c, "the nightjar plan")] {
        v1.link(kaban_api::LinkRequest {
            from: a.clone(),
            rel: "RELATES_TO".into(),
            to: to.clone(),
            confidence: None,
            props: Some(serde_json::json!({ "note": note })),
        })
        .unwrap();
    }
    assert!(edge_props(&s, &a, &b).unwrap().contains("hunter2"));

    // Act: the server starts with v2
    let v2 = api_with(&store, V2);
    let outcome = v2
        .rescan_if_policy_changed(&CancellationToken::new())
        .unwrap();

    // Assert: the redacted value is gone from the stored edge, the rejected edge is deleted
    assert_eq!(outcome.status, RescanStatus::Completed, "{outcome:?}");
    assert_eq!((outcome.redacted, outcome.rejected), (1, 1), "{outcome:?}");
    let masked = edge_props(&s, &a, &b).unwrap();
    assert!(!masked.contains("hunter2"), "{masked}");
    assert!(masked.contains(MASK), "{masked}");
    let producer: String = rusqlite::Connection::open(s.db())
        .unwrap()
        .query_row(
            "SELECT e.producer FROM edge e JOIN node d ON d.rid = e.dst WHERE d.id = ?1",
            [&b],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(producer, "authored", "the masked link keeps its producer");
    assert_eq!(edge_props(&s, &a, &c), None, "the rejected edge is dropped");
    let again = v2
        .rescan_if_policy_changed(&CancellationToken::new())
        .unwrap();
    assert_eq!(again.status, RescanStatus::UpToDate, "{again:?}");
}

/// Policy v3: a different reject term (a policy other than v2).
const V3: &str = "words:\n  - { term: albatross, action: reject, id: bird }\n";

/// END review: an interrupted rescan under another policy never certifies freshness for the
/// policy that completed before it. v2 completes; another process writes a row v2 rejects,
/// past the first batch; a v3 run is cancelled after one batch; v2 starts again and must judge
/// that row.
#[test]
fn an_interrupted_rescan_under_another_policy_leaves_the_completed_policy_unfresh() {
    let s = Scratch::new("kaban-api-rescan", "interrupted-other-policy");
    let store = s.store();
    let ops = (0..RESCAN_BATCH + 10)
        .map(|i| kaban_core::Op::UpsertNode {
            id: NodeId::new(),
            node: kaban_core::NewNode {
                kind: "Fact".into(),
                text: format!("note {i} about cargo and tokio"),
                ..Default::default()
            },
        })
        .collect();
    store
        .apply_batch(
            kaban_core::WriteBatch::new(kaban_core::WorkspaceId::default(), ops),
            &kaban_core::WriteCtx::new(
                kaban_core::WorkspaceId::default(),
                kaban_core::Actor::Agent,
            ),
        )
        .unwrap();
    let v2 = api_with(&store, V2);
    let done = v2
        .rescan_if_policy_changed(&CancellationToken::new())
        .unwrap();
    assert_eq!(done.status, RescanStatus::Completed, "{done:?}");

    // another process under v1 writes a row v2 rejects; its row id is past the first batch
    let other = s.store();
    let stale = api_with(&other, V1)
        .remember(fact("the nightjar codename written by an old process"))
        .unwrap();
    drop(other);

    // a v3 run judges one batch and is cancelled before it reaches that row
    let v3 = api_with(&store, V3);
    assert_ne!(v3.policy_version(), v2.policy_version());
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    let partial = v3.rescan_if_policy_changed(&cancelled).unwrap();
    assert_eq!(partial.status, RescanStatus::Cancelled, "{partial:?}");
    assert_eq!(partial.scanned, RESCAN_BATCH as u64, "{partial:?}");

    // v2 starts again: the row was never judged under v2
    let again = v2
        .rescan_if_policy_changed(&CancellationToken::new())
        .unwrap();
    assert_eq!(again.status, RescanStatus::Completed, "{again:?}");
    assert!(again.rejected >= 1, "{again:?}");
    assert!(
        store.get(&nid(&stale.id), false).unwrap().is_none(),
        "the row v2 rejects is hidden"
    );
    let idle = v2
        .rescan_if_policy_changed(&CancellationToken::new())
        .unwrap();
    assert_eq!(idle.status, RescanStatus::UpToDate, "{idle:?}");
}
