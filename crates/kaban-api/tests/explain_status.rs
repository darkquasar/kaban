//! EN-F14a explanation coverage (engine-plan E2.5 acceptance (a)): for every `Status` variant
//! a fixture item whose `explain` output carries the status, the sentence with its reason,
//! the EN-F14 explanation where one exists and the lineages with their offsets (never
//! surface text); every pending item has next actions, every settled one has none and says
//! why nothing is pending. The promotion half of EN-F13 (the red kept) is `accept_proposed.rs`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

#[path = "common/cg_fixture.rs"]
mod cg_fixture;
#[path = "common/status.rs"]
mod status;

use std::sync::Arc;

use kaban_api::{
    Addressable, ApiConfig, ApiError, ExplainRequest, ExplainStatusRequest, KabanApi, LinkRequest,
    PendingStatus, RememberRequest, SettledStatus, Status, StatusExplanation,
};
use kaban_core::ActorId;
use kaban_core::{CandidateReason, GraphStore, MemoryStore, NodeId, UnresolvedReason, WorkspaceId};
use kaban_policy::Policy;
use status::{Items, Scratch, api_over, claim, fixture, items, key_of, rows, submit};

fn explain(it: &Items, address: &str) -> StatusExplanation {
    it.api
        .explain_status(ExplainStatusRequest {
            address: address.into(),
        })
        .unwrap()
}

/// The sentence starts with the status word and names the reason right after it.
fn assert_sentence(x: &StatusExplanation, status_word: &str, reason: &str) {
    assert!(
        x.sentence.starts_with(&format!("{status_word}: {reason}")),
        "{status_word}/{reason}: {}",
        x.sentence
    );
}

#[test]
fn every_pending_status_has_its_sentence_its_reason_and_a_next_action() {
    let s = Scratch::new("pending");
    let it = items(&s);

    // candidate: below_margin on span 56..62 of c17, with the explanation and the lineages
    let x = explain(&it, it.chunk.as_ref());
    assert_eq!(
        x.status,
        Status::Pending(PendingStatus::Candidate(CandidateReason::BelowMargin))
    );
    assert_sentence(&x, "candidate", "below_margin");
    assert!(x.sentence.contains("span 56:6"), "{}", x.sentence);
    let explanation = x.explanation.as_ref().unwrap();
    assert_eq!(explanation.candidates.len(), 3, "{explanation:?}");
    assert!(
        x.lineages.iter().all(|l| l.spans == vec![(56, 6)]),
        "{:?}",
        x.lineages
    );
    assert!(
        !x.lineages.is_empty() && x.lineages.iter().all(|l| !l.lineage_id.contains("orange")),
        "offsets, never surface text: {:?}",
        x.lineages
    );
    assert_ne!(x.next_actions, vec![]);
    // the action names the group it decides (RW3-3), single-quoted canonical JSON
    assert_eq!(
        x.next_actions[0].command,
        format!(
            "kaban resolve {} --winner {} --group '{}'",
            it.chunk,
            explanation.candidates[0].value.as_str().unwrap(),
            explanation.group_key
        ),
        "{:?}",
        x.next_actions
    );
    assert!(
        x.next_actions[1].command.starts_with("kaban explain ")
            && x.next_actions[1].command.ends_with(" --deprecate reviewed"),
        "{:?}",
        x.next_actions
    );

    // unresolved: preferred_conflict, two human claims on one span
    let x = explain(&it, it.chunk_unresolved.as_ref());
    assert_eq!(
        x.status,
        Status::Pending(PendingStatus::Unresolved(
            UnresolvedReason::PreferredConflict
        )),
        "{x:?}"
    );
    assert_sentence(&x, "unresolved", "preferred_conflict");
    assert!(x.explanation.is_some());
    assert_ne!(x.next_actions, vec![]);

    // candidate: discovered_kind on the node, discovered on the kind
    let x = explain(&it, &it.discovered);
    assert_eq!(
        x.status,
        Status::Pending(PendingStatus::Candidate(CandidateReason::DiscoveredKind))
    );
    assert_sentence(&x, "candidate", "discovered_kind");
    assert_eq!(x.policy.discovered_policy.as_deref(), Some("review"));
    assert!(x.policy.cap.is_some());
    assert_eq!(
        x.next_actions[0].command,
        format!(
            "kaban accept {} --reason reviewed --path-scope '**'",
            it.discovered
        )
    );
    let x = explain(&it, "x:Widget");
    assert_eq!(
        x.status,
        Status::Pending(PendingStatus::Discovered { count: 1 })
    );
    assert_sentence(&x, "discovered", "x:Widget");
    assert_eq!(x.subject, Addressable::DiscoveredKind("x:Widget".into()));
    assert_eq!(
        x.next_actions[0].command,
        "kaban ontology promote x:Widget Entity"
    );

    // proposed: a governed record awaiting the handshake, `acceptance: human`
    let x = explain(&it, &it.proposed);
    assert_eq!(x.status, Status::Pending(PendingStatus::Proposed));
    assert_sentence(&x, "proposed", "the record awaits");
    assert_eq!(
        serde_json::to_value(&x.policy).unwrap()["acceptance"],
        serde_json::json!("human")
    );
    assert_eq!(
        x.next_actions[0].command,
        format!(
            "kaban accept {} --reason reviewed --path-scope '**'",
            it.proposed
        )
    );

    // conflict: the equal-rev row
    let x = explain(&it, &it.conflict);
    assert_eq!(x.status, Status::Pending(PendingStatus::Conflict));
    assert_sentence(&x, "conflict", "the record's file");
    assert_eq!(
        x.next_actions[0].command,
        format!("kaban resolve {} --take db", it.conflict)
    );
}

#[test]
fn every_settled_status_says_nothing_is_pending_and_has_no_next_action() {
    let s = Scratch::new("settled");
    let it = items(&s);
    let cases = [
        (it.active.clone(), SettledStatus::Active, "active"),
        (it.accepted.clone(), SettledStatus::Accepted, "accepted"),
        (
            it.superseded.clone(),
            SettledStatus::Superseded,
            "superseded",
        ),
        (it.forgotten.clone(), SettledStatus::Rejected, "rejected"),
    ];
    for (id, want, word) in cases {
        let x = explain(&it, &id);
        assert_eq!(x.status, Status::Settled(want), "{word}: {x:?}");
        assert_eq!(x.sentence, format!("{word}: nothing pending"));
        assert!(x.next_actions.is_empty(), "{word}: {:?}", x.next_actions);
    }
    // the accepted record's history carries its acceptance
    let x = explain(&it, &it.accepted);
    assert!(
        x.history.iter().any(|e| e.kind == "accepted"),
        "{:?}",
        x.history
    );

    // claims: the winner's supporter is active, the loser is dissent, a reviewed one is a
    // candidate; the claim key is an address of its own
    let a1 = explain(&it, &key_of(&s, "a1"));
    assert_eq!(a1.status, Status::Settled(SettledStatus::Active), "{a1:?}");
    assert!(a1.explanation.is_some());
    let a2 = explain(&it, &key_of(&s, "a2"));
    assert_eq!(a2.status, Status::Settled(SettledStatus::Dissent), "{a2:?}");
    assert_eq!(
        a2.sentence,
        "dissent: the claim lost its vote, nothing pending"
    );
    let a5 = explain(&it, &key_of(&s, "a5"));
    assert_eq!(
        a5.status,
        Status::Pending(PendingStatus::Candidate(CandidateReason::BelowMargin))
    );
}

/// Core-spec S6 (the wave-2 review's RW2-1, the red kept): a claim key of another workspace
/// is `NotFound` through `explain_status` and through the E2.4 `explain`, with the same
/// output as a key nothing holds; nothing of the foreign group is rendered. Positive: the
/// service bound to that workspace explains the same key.
#[test]
fn a_claim_key_of_another_workspace_is_not_found_like_an_unknown_key() {
    let s = Scratch::new("foreign-claim");
    let it = items(&s);
    // a human `link` in workspace `other` over the same store: one human edge claim there
    let other = KabanApi::with_config(
        Arc::clone(&it.store) as Arc<dyn GraphStore>,
        Policy::empty(),
        it.store.signer(),
        None,
        ApiConfig {
            workspace: WorkspaceId::new("other"),
            ..Default::default()
        },
    );
    let remember = |text: &str| -> String {
        other
            .remember(RememberRequest {
                kind: "Fact".into(),
                text: text.into(),
                ..Default::default()
            })
            .unwrap()
            .id
    };
    let (from, to) = (
        remember("a fact of the other workspace"),
        remember("its neighbour"),
    );
    other
        .link(LinkRequest {
            from,
            rel: "RELATES_TO".into(),
            to,
            confidence: None,
            props: None,
        })
        .unwrap();
    let foreign_key: String = rusqlite::Connection::open(s.db())
        .unwrap()
        .query_row(
            "SELECT key FROM assertion WHERE workspace_id = 'other'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let x = other
        .explain_status(ExplainStatusRequest {
            address: foreign_key.clone(),
        })
        .unwrap();
    assert_eq!(
        x.status,
        Status::Settled(SettledStatus::Active),
        "the bound workspace explains its own claim: {x:?}"
    );

    // the property: from the default workspace the key names nothing, exactly as an
    // unknown key does (the key is echoed as asked, nothing else differs)
    let unknown = "0".repeat(64);
    let foreign = it
        .api
        .explain_status(ExplainStatusRequest {
            address: foreign_key.clone(),
        })
        .expect_err("a foreign claim key is not found");
    assert!(
        matches!(&foreign, ApiError::NotFound(k) if *k == foreign_key),
        "{foreign:?}"
    );
    let missing = it
        .api
        .explain_status(ExplainStatusRequest {
            address: unknown.clone(),
        })
        .expect_err("an unknown key is not found");
    assert_eq!(
        format!("{foreign:?}").replace(&foreign_key, "<key>"),
        format!("{missing:?}").replace(&unknown, "<key>"),
        "indistinguishable from an unknown key"
    );
    let foreign = it
        .api
        .explain(ExplainRequest {
            key: foreign_key.clone(),
        })
        .expect_err("the E2.4 entry refuses the foreign key too");
    assert!(
        matches!(&foreign, ApiError::NotFound(k) if *k == foreign_key),
        "{foreign:?}"
    );
}

/// Negative: an address that matches nothing is `NotFound` (exit 3 on the CLI); one no
/// grammar branch accepts is `Invalid`; a next-action command never embeds free text from
/// the index (ids, keys and fixed literals only).
#[test]
fn unknown_and_malformed_addresses_and_the_closed_command_table() {
    let s = Scratch::new("negative");
    let it = items(&s);
    let unknown = NodeId::new().to_string();
    assert!(matches!(
        it.api
            .explain_status(ExplainStatusRequest { address: unknown }),
        Err(ApiError::NotFound(_))
    ));
    assert!(matches!(
        it.api.explain_status(ExplainStatusRequest {
            address: "x:Nope".into()
        }),
        Err(ApiError::NotFound(_))
    ));
    assert!(matches!(
        it.api.explain_status(ExplainStatusRequest {
            address: "not an address".into()
        }),
        Err(ApiError::Invalid(_))
    ));
    for address in [
        it.chunk.to_string(),
        it.discovered.clone(),
        it.proposed.clone(),
        it.conflict.clone(),
        "x:Widget".to_owned(),
    ] {
        let x = explain(&it, &address);
        for a in &x.next_actions {
            for word in a.command.split_whitespace() {
                // a single-quoted canonical group key (`--group '[...]'`, RW3-3) is structure;
                // the glob and the placeholder are quoted for the shell (RW4-1)
                let fixed = word.starts_with("kaban")
                    || word.starts_with("--")
                    || (word.starts_with("'[") && word.ends_with("]'"))
                    || matches!(
                        word,
                        "reviewed"
                            | "'**'"
                            | "'<key>'"
                            | "db"
                            | "file"
                            | "Entity"
                            | "accept"
                            | "explain"
                            | "forget"
                            | "ontology"
                            | "promote"
                            | "prune"
                            | "resolve"
                    )
                    || NodeId::parse(word).is_ok()
                    || (word.len() == 64 && word.bytes().all(|b| b.is_ascii_hexdigit()))
                    || word.starts_with("x:")
                    // a qualified kind key (`infra:Team`): the `--winner` value is a key
                    || (word.contains(':')
                        && word
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b':' | b'_')));
                assert!(fixed, "free text in a command: {word:?} in {}", a.command);
            }
        }
    }
}

/// Engine-plan E2.5 `resolve --winner` (EN-F5a, the red kept): a human's winner on the
/// `below_margin` span candidate writes the `preferred` claim and decides the group in one
/// transaction. The span vote names no object, so the chunk never holds a `node_candidate`
/// row (OWN116: a span claim's candidate state is never its source chunk's); the review item
/// comes from the claims' `candidate` statuses. Positive: the item leaves `review` and
/// `explain` says the chunk is settled. Atomicity: with an injected failure on the group's
/// decision (a schema trigger on the `winner` status update, visible to every connection)
/// the call fails, the group stays open and no human claim exists. Negative: an agent is
/// refused with nothing written; a node with no candidate group is `NotFound`.
#[test]
fn resolve_winner_writes_the_claim_and_decides_the_group_in_one_transaction_or_not_at_all() {
    use kaban_api::ReviewQuery;
    use kaban_core::Actor;
    let s = Scratch::new("winner");
    let it = items(&s);
    let before = explain(&it, it.chunk.as_ref());
    let value = before.explanation.as_ref().unwrap().candidates[0]
        .value
        .as_str()
        .unwrap()
        .to_owned();
    let listed = |api: &kaban_api::KabanApi| -> usize {
        api.review(&ReviewQuery::default())
            .unwrap()
            .items
            .iter()
            .filter(|i| i.subject == Addressable::Node(it.chunk.clone()))
            .count()
    };
    assert_eq!(listed(&it.api), 1);
    let conn = rusqlite::Connection::open(s.db()).unwrap();
    let human_claims = || -> i64 {
        conn.query_row(
            "SELECT count(*) FROM assertion WHERE layer = 'human' AND chunk_rid = (SELECT rid FROM node WHERE id = ?1)",
            [it.chunk.as_str()],
            |r| r.get(0),
        )
        .unwrap()
    };
    let candidate_rows = || -> i64 {
        conn.query_row(
            "SELECT count(*) FROM node_candidate WHERE rid = (SELECT rid FROM node WHERE id = ?1)",
            [it.chunk.as_str()],
            |r| r.get(0),
        )
        .unwrap()
    };
    // the chunk's open `candidate` groups (the review item's source)
    let open_groups = || -> i64 {
        conn.query_row(
            "SELECT count(DISTINCT s.group_key) FROM assertion_status s JOIN assertion a ON a.id = s.assertion_id
             WHERE s.disposition = 'candidate' AND a.chunk_rid = (SELECT rid FROM node WHERE id = ?1)",
            [it.chunk.as_str()],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert_eq!(candidate_rows(), 0, "the source chunk is never a candidate");
    assert_eq!((human_claims(), open_groups()), (0, 1));

    // an agent is refused, nothing written
    let err = it
        .api
        .resolve_winner(&it.chunk, &value, None, Actor::Agent)
        .unwrap_err();
    assert!(matches!(err, ApiError::Invalid(_)), "{err:?}");
    assert_eq!((human_claims(), open_groups()), (0, 1));

    // the injected failure: the decision aborts, so the whole call rolls back
    conn.execute_batch(
        "CREATE TRIGGER abort_decision BEFORE UPDATE OF disposition ON assertion_status
           WHEN NEW.disposition = 'winner' BEGIN
           SELECT RAISE(ABORT, 'injected failure');
         END;",
    )
    .unwrap();
    let err = it
        .api
        .resolve_winner(&it.chunk, &value, None, Actor::HumanCli)
        .unwrap_err();
    assert!(matches!(err, ApiError::Store(_)), "{err:?}");
    assert_eq!(
        (human_claims(), open_groups()),
        (0, 1),
        "the claim and the decision commit together or not at all"
    );
    conn.execute_batch("DROP TRIGGER abort_decision").unwrap();

    // the atomic path: one call, the claim written, the group decided, the item out of review
    let out = it
        .api
        .resolve_winner(&it.chunk, &value, None, Actor::HumanCli)
        .unwrap();
    assert_eq!(out.value, value);
    assert_eq!((human_claims(), open_groups(), candidate_rows()), (1, 0, 0));
    assert_eq!(
        listed(&it.api),
        0,
        "the item left review: {:?}",
        it.api.review(&ReviewQuery::default()).unwrap().items
    );
    let after = explain(&it, it.chunk.as_ref());
    assert!(
        matches!(after.status, Status::Settled(_)),
        "{:?}",
        after.status
    );
    assert_eq!(after.next_actions, vec![]);

    // a node with no candidate group is not found
    let active = NodeId::parse(&it.active).unwrap();
    let err = it
        .api
        .resolve_winner(&active, &value, None, Actor::HumanCli)
        .unwrap_err();
    assert!(matches!(err, ApiError::NotFound(_)), "{err:?}");
}

/// RW3-3 at the API (EN-F5a, EN-F14a): a chunk with two `candidate` groups (the fixture's
/// `a5`, `a6`, `a7` at 56..62 and again at 10..16). No group named is refused
/// `ambiguous_group` with nothing written; another node's group key is `NotFound` with nothing
/// written; the second group named is decided and the first is still listed.
#[test]
fn resolve_winner_needs_the_group_when_a_node_has_two_and_refuses_a_foreign_one() {
    use kaban_api::ReviewQuery;
    use kaban_core::{Actor, BatchSubject, StoreError};
    let s = Scratch::new("winner-groups");
    let it = items(&s);
    let fx = fixture();
    let chunk = NodeId::new();
    let at_10: Vec<_> = rows(&fx, &["a5", "a6", "a7"])
        .into_iter()
        .map(|r| {
            let mut c = claim(&fx, r, &chunk);
            c.subject = BatchSubject::Span {
                chunk: chunk.clone(),
                offset: 10,
                len: 6,
            };
            c
        })
        .collect();
    submit(&it.store, &fx, &chunk, &["a5", "a6", "a7"], at_10);
    let groups_of = |node: &NodeId| -> Vec<String> {
        it.api
            .review(&ReviewQuery::default())
            .unwrap()
            .items
            .iter()
            .filter(|i| i.subject == Addressable::Node(node.clone()))
            .filter_map(|i| i.group_key.clone())
            .collect()
    };
    let before = groups_of(&chunk);
    assert_eq!(before.len(), 2, "{before:?}");

    // no group named: ambiguous, nothing written
    let err = it
        .api
        .resolve_winner(&chunk, "infra:Team", None, Actor::HumanCli)
        .unwrap_err();
    assert!(
        matches!(
            &err,
            ApiError::Store(StoreError::InvalidName(field, reason))
                if field == "group" && *reason == "ambiguous_group"
        ),
        "{err:?}"
    );
    assert_eq!(groups_of(&chunk), before, "nothing written");

    // another node's group: not this node's, nothing written
    let foreign = groups_of(&it.chunk).remove(0);
    let err = it
        .api
        .resolve_winner(&chunk, "infra:Team", Some(&foreign), Actor::HumanCli)
        .unwrap_err();
    assert!(matches!(err, ApiError::NotFound(_)), "{err:?}");
    assert_eq!(groups_of(&chunk), before, "nothing written");
    assert_eq!(
        groups_of(&it.chunk),
        vec![foreign],
        "the other node is untouched"
    );

    // the second group named: decided; the first still a candidate
    let out = it
        .api
        .resolve_winner(&chunk, "infra:Team", Some(&before[1]), Actor::HumanCli)
        .unwrap();
    assert_eq!(out.id, chunk.to_string());
    assert_eq!(groups_of(&chunk), vec![before[0].clone()]);
}

/// A `Chunk` of `path` covering `byte_start..byte_end` of the document, with the fixture
/// text, written through one producer batch with `claims` about it.
fn chunk_with_id(
    it: &Items,
    id: NodeId,
    path: &str,
    byte_start: u64,
    byte_end: u64,
    claims: Vec<kaban_core::BatchAssertion>,
) -> NodeId {
    use kaban_core::{Actor, EdgeDiff, IngestStore, NewNode, Op, Owner, WriteBatch, WriteCtx};
    let fx = status::fixture();
    let mut b = WriteBatch::new(
        WorkspaceId::default(),
        vec![Op::UpsertNode {
            id: id.clone(),
            node: NewNode {
                kind: "Chunk".into(),
                text: fx["chunk"]["text"].as_str().unwrap().into(),
                props: Some(serde_json::json!({
                    "path": path, "byte_start": byte_start, "byte_end": byte_end
                })),
                ..Default::default()
            },
        }],
    );
    b.edge_diff = EdgeDiff::Owned {
        owner: Owner::KabanIngest,
    };
    b.assertions = claims;
    it.store
        .apply_batch(
            b,
            &WriteCtx::new("v1", WorkspaceId::default(), Actor::Agent),
        )
        .unwrap();
    id
}

/// A human `preferred` claim on `offset..offset+6` of `chunk` (the fixture's `human_claim`
/// at a chosen offset).
fn human_claim_at(
    chunk: &NodeId,
    offset: u32,
    value: &str,
    lineage: &str,
) -> kaban_core::BatchAssertion {
    use kaban_core::{BatchSubject, ClaimClass, Fixed, Layer, Method, Rank};
    kaban_core::BatchAssertion {
        subject: BatchSubject::Span {
            chunk: chunk.clone(),
            offset,
            len: 6,
        },
        claim_class: ClaimClass::Type,
        predicate: None,
        object: None,
        value: serde_json::json!(value),
        layer: Layer::Human,
        method: Method::Link,
        extractor_id: None,
        extractor_version: None,
        source_id: Some("c17".into()),
        lineage_id: lineage.to_owned(),
        raw_score: Fixed::ONE,
        rank: Rank::Preferred,
        rank_reason: None,
        calibration_version: 1,
        supersedes: None,
    }
}

/// RW2-6 (the red kept, `e25c3-red-spanref-bound.log`): a `<path>:<offset>` address names
/// no document, and the store holds more chunks than one listing page. The answer is
/// `NotFound` without the whole chunk population being read: one chunk row past the first
/// page is made undecodable (its `tags` column is not a string list), so a lookup that pages
/// through every chunk fails on it, and a bounded lookup of the path never touches it.
#[test]
fn a_missing_path_is_answered_without_reading_the_whole_chunk_population() {
    use kaban_core::{Actor, EdgeDiff, IngestStore, NewNode, Op, Owner, WriteBatch, WriteCtx};
    let s = Scratch::new("span-bound");
    let it = items(&s);
    // the id that sorts last: the poisoned row lands on the last page of any id-ordered scan
    let poison = NodeId::parse("ffffffff-ffff-7fff-bfff-ffffffffffff").unwrap();
    let mut ops: Vec<Op> = (0..501u64)
        .map(|i| Op::UpsertNode {
            id: NodeId::new(),
            node: NewNode {
                kind: "Chunk".into(),
                text: format!("chunk {i} of a long document"),
                props: Some(serde_json::json!({
                    "path": "big.md", "byte_start": i * 10, "byte_end": i * 10 + 10
                })),
                ..Default::default()
            },
        })
        .collect();
    ops.push(Op::UpsertNode {
        id: poison.clone(),
        node: NewNode {
            kind: "Chunk".into(),
            text: "the last chunk".into(),
            props: Some(serde_json::json!({
                "path": "big.md", "byte_start": 5010, "byte_end": 5020
            })),
            ..Default::default()
        },
    });
    let mut b = WriteBatch::new(WorkspaceId::default(), ops);
    b.edge_diff = EdgeDiff::Owned {
        owner: Owner::KabanIngest,
    };
    it.store
        .apply_batch(
            b,
            &WriteCtx::new("v1", WorkspaceId::default(), Actor::Agent),
        )
        .unwrap();
    let conn = rusqlite::Connection::open(s.db()).unwrap();
    kaban_store_sqlite::SqliteStore::bind_writer(&conn, &ActorId::default()).unwrap();
    conn.execute(
        "UPDATE node SET tags = '{}' WHERE id = ?1",
        [poison.as_str()],
    )
    .unwrap();
    // reading the poisoned row fails: the whole-population scan cannot answer
    let err = it
        .store
        .list_nodes("Chunk", "default", 500, 500)
        .unwrap_err();
    assert!(
        matches!(err, kaban_core::StoreError::Backend { .. }),
        "{err:?}"
    );

    let err = it
        .api
        .explain_status(ExplainStatusRequest {
            address: "nowhere.md:5".into(),
        })
        .unwrap_err();
    assert!(
        matches!(err, ApiError::NotFound(_)),
        "a missing path is answered without reading the whole chunk population: {err:?}"
    );
    // the positive: a covered offset of a healthy document is found past the first page too
    let x = it
        .api
        .explain_status(ExplainStatusRequest {
            address: "big.md:4995".into(),
        })
        .unwrap();
    assert_eq!(
        x.subject,
        Addressable::SpanRef {
            path: "big.md".into(),
            offset: 4995
        }
    );
}

/// RW2-6: two pending spans inside one chunk each get their own status: the `candidate`
/// vote at 56..62 and the `unresolved` pair at 10..16 of a chunk covering `doc.md`
/// 100..200; an offset the document does not reach is `NotFound`.
#[test]
fn two_pending_spans_in_one_chunk_each_get_their_own_status() {
    let s = Scratch::new("span-two");
    let it = items(&s);
    let fx = status::fixture();
    let id = NodeId::new();
    let mut claims: Vec<kaban_core::BatchAssertion> = status::rows(&fx, &["a5", "a6", "a7"])
        .into_iter()
        .map(|r| status::claim(&fx, r, &id))
        .collect();
    claims.push(human_claim_at(&id, 10, "infra:Team", "human:cli:1"));
    claims.push(human_claim_at(&id, 10, "infra:Vendor", "human:cli:2"));
    let chunk = chunk_with_id(&it, id, "doc.md", 100, 200, claims);
    let at = |offset: u64| explain(&it, &format!("doc.md:{offset}"));
    let x = at(156);
    assert_eq!(
        x.status,
        Status::Pending(PendingStatus::Candidate(CandidateReason::BelowMargin)),
        "{x:?}"
    );
    assert!(x.sentence.contains("span 56:6"), "{}", x.sentence);
    assert_eq!(
        x.subject,
        Addressable::SpanRef {
            path: "doc.md".into(),
            offset: 156
        }
    );
    let y = at(110);
    assert_eq!(
        y.status,
        Status::Pending(PendingStatus::Unresolved(
            UnresolvedReason::PreferredConflict
        )),
        "{y:?}"
    );
    assert!(y.sentence.contains("span 10:6"), "{}", y.sentence);
    assert_eq!(
        y.subject,
        Addressable::SpanRef {
            path: "doc.md".into(),
            offset: 110
        }
    );
    // the node itself is one address with one status (the first pending group)
    let n = explain(&it, chunk.as_ref());
    assert!(matches!(n.status, Status::Pending(_)), "{n:?}");
    // beyond the document: nothing
    let err = it
        .api
        .explain_status(ExplainStatusRequest {
            address: "doc.md:250".into(),
        })
        .unwrap_err();
    assert!(matches!(err, ApiError::NotFound(_)), "{err:?}");
}

/// A machine claim parked `unresolved` under a human winner (`resolve --winner`, EN-F12c)
/// explains as `dissent`: the group is decided, nothing is pending, no `preferred_conflict`
/// is reported for it (the explain dissent rendering). The negative: before the human spoke
/// the same claim is a `candidate`, and a true conflict of two human claims stays
/// `unresolved: preferred_conflict`.
#[test]
fn a_parked_machine_claim_under_a_human_winner_is_dissent_not_a_conflict() {
    use kaban_core::Actor;
    let s = Scratch::new("dissent");
    let it = items(&s);
    let a5 = key_of(&s, "a5");
    let before = explain(&it, &a5);
    assert_eq!(
        before.status,
        Status::Pending(PendingStatus::Candidate(CandidateReason::BelowMargin)),
        "{before:?}"
    );
    it.api
        .resolve_winner(&it.chunk, "infra:Team", None, Actor::HumanCli)
        .unwrap();
    // a5 (`x:Fruit`) lost to the human `infra:Team`: parked, settled, nothing pending
    let after = explain(&it, &a5);
    assert_eq!(
        after.status,
        Status::Settled(SettledStatus::Dissent),
        "{after:?}"
    );
    assert_eq!(
        after.sentence,
        "dissent: the claim lost its vote, nothing pending"
    );
    assert_eq!(after.next_actions, vec![]);
    // the chunk's own status is settled too, and it is no review item
    let chunk = explain(&it, it.chunk.as_ref());
    assert_eq!(
        chunk.status,
        Status::Settled(SettledStatus::Active),
        "{chunk:?}"
    );
    assert!(
        !it.api
            .review(&kaban_api::ReviewQuery::default())
            .unwrap()
            .items
            .iter()
            .any(|i| i.subject == Addressable::Node(it.chunk.clone())),
    );
    // two human claims disagreeing are still a conflict
    let conflict = explain(&it, it.chunk_unresolved.as_ref());
    assert_eq!(
        conflict.status,
        Status::Pending(PendingStatus::Unresolved(
            UnresolvedReason::PreferredConflict
        )),
        "{conflict:?}"
    );
}

/// A one-commit scripted repository at `<dir>/repo` (the head `resolve_for_card` rev-parses;
/// the snapshot itself is the `FixedSnapshot` below).
fn scripted_repo(dir: &std::path::Path) -> std::path::PathBuf {
    let root = dir.join("repo");
    std::fs::create_dir_all(&root).unwrap();
    for args in [
        vec!["init", "-q", "-b", "main"],
        vec!["add", "a.txt"],
        vec!["commit", "-q", "-m", "c0"],
    ] {
        if args[0] == "add" {
            std::fs::write(root.join("a.txt"), "0").unwrap();
        }
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(&args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
    }
    root
}

/// `kaban resolve <coderef-id> --symbol <key>` over the `cg_fixture` stale item (engine-plan
/// E2.5, SO-6; the `why_symbol_snapshot_mismatch` harness: `AFERO_KEY` anchored, the resolved
/// snapshot is `afero-renamed`). Positive: the item is `stale` with `resolve --symbol` as its
/// first action and `forget` second, and is listed by `review`; the re-target to
/// `AFERO_RENAMED_KEY` makes `explain` settle (`active`, no action), removes the item from
/// `review`, writes one human `prop_value` claim with predicate `symbol` and rewrites the
/// `symbol` anchor row (EN-F10, read at rest: the join key is the property). Negatives: an
/// agent is refused and the versioned form of the new key is `symbol_invalid`, with nothing
/// written either time; a node that is no `CodeRef` is not found.
#[test]
fn a_stale_coderef_is_retargeted_by_resolve_symbol_and_leaves_review() {
    use cg_fixture::{AFERO_COMMIT, AFERO_KEY, AFERO_PATH, AFERO_RENAMED_KEY, REPO_ID, Variant};
    use kaban_api::{RESOLVE_SYMBOL_INVALID, ReviewQuery};
    use kaban_codegraph::SnapshotProvider;
    use kaban_codegraph::testing::FixedSnapshot;
    use kaban_core::Actor;
    use kaban_store_sqlite::SqliteStore;
    use serde_json::json;

    const REPO: &str = "kaban";
    const VERSIONED: &str = "scip-go gomod github.com/spf13/afero v1.11.0 MemMapFs#CreateFile().";

    let s = Scratch::new("stale-symbol");
    let root = scripted_repo(&s.0);
    let built = cg_fixture::build_with_policy(
        &s.0,
        Variant::AferoRenamed,
        AFERO_COMMIT,
        cg_fixture::POLICY_VERSION,
    );
    let provider: Arc<dyn SnapshotProvider> = Arc::new(FixedSnapshot::new(&built.path));
    let store = Arc::new(SqliteStore::open(&s.db(), &s.keys()).unwrap());
    let api = KabanApi::with_config(
        Arc::clone(&store) as Arc<dyn GraphStore>,
        Policy::from_file(Policy::parse(b"").unwrap(), "test".into()).unwrap(),
        store.signer(),
        None,
        ApiConfig {
            repo: Some(REPO.into()),
            repo_root: Some(root),
            ..Default::default()
        },
    )
    .with_snapshot_provider(s.0.join("codegraph").join(REPO_ID), provider);
    let remember = |kind: &str, props: serde_json::Value| -> String {
        api.remember(RememberRequest {
            kind: kind.into(),
            text: format!("{REPO} {AFERO_PATH} {kind}"),
            props: Some(props),
            ..Default::default()
        })
        .unwrap()
        .id
    };
    let coderef = remember(
        "CodeRef",
        json!({
            "repo": REPO, "commit": AFERO_COMMIT, "tool": "fixture", "relative_path": AFERO_PATH,
            "name_path": "MemMapFs/Create", "kind": "Method", "line": 7, "symbol": AFERO_KEY,
            "snapshot_commit": AFERO_COMMIT,
        }),
    );
    let fact = remember("Fact", json!({}));
    let id = NodeId::parse(&coderef).unwrap();
    let explain = |address: &str| -> StatusExplanation {
        api.explain_status(ExplainStatusRequest {
            address: address.into(),
        })
        .unwrap()
    };
    let listed = || -> Vec<kaban_api::ReviewItem> {
        api.review(&ReviewQuery::default())
            .unwrap()
            .items
            .into_iter()
            .filter(|i| i.subject == Addressable::Node(id.clone()))
            .collect()
    };
    let conn = rusqlite::Connection::open(s.db()).unwrap();
    let at_rest = || -> (String, Vec<String>, i64) {
        let props: String = conn
            .query_row("SELECT props FROM node WHERE id = ?1", [&coderef], |r| {
                r.get(0)
            })
            .unwrap();
        let props: serde_json::Value = serde_json::from_str(&props).unwrap();
        let mut st = conn
            .prepare(
                "SELECT key FROM node_anchor WHERE anchor_kind = 'symbol'
                   AND rid = (SELECT rid FROM node WHERE id = ?1) ORDER BY key",
            )
            .unwrap();
        let anchors: Vec<String> = st
            .query_map([&coderef], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        let claims: i64 = conn
            .query_row(
                "SELECT count(*) FROM assertion WHERE layer = 'human' AND claim_class = 'prop_value'
                   AND predicate = 'symbol' AND subject_rid = (SELECT rid FROM node WHERE id = ?1)",
                [&coderef],
                |r| r.get(0),
            )
            .unwrap();
        (
            props["symbol"].as_str().unwrap().to_owned(),
            anchors,
            claims,
        )
    };
    let before = (AFERO_KEY.to_owned(), vec![AFERO_KEY.to_owned()], 0);
    assert_eq!(at_rest(), before);

    // stale, with the re-target first and `forget` second; listed by review
    let x = explain(&coderef);
    assert_eq!(x.status, Status::Pending(PendingStatus::Stale), "{x:?}");
    assert_sentence(&x, "stale", "the resolved snapshot");
    assert_eq!(
        x.next_actions
            .iter()
            .map(|a| a.command.as_str())
            .collect::<Vec<_>>(),
        vec![
            format!("kaban resolve {coderef} --symbol '<key>'").as_str(),
            format!("kaban forget {coderef}").as_str(),
        ],
        "{:?}",
        x.next_actions
    );
    let items = listed();
    assert_eq!(items.len(), 1, "{items:?}");
    assert_eq!(items[0].status, PendingStatus::Stale);
    assert_eq!(items[0].first_action.command, x.next_actions[0].command);

    // an agent is refused, nothing written
    let err = api
        .resolve_symbol(&id, AFERO_RENAMED_KEY, Actor::Agent)
        .unwrap_err();
    assert!(matches!(err, ApiError::Invalid(_)), "{err:?}");
    assert_eq!(at_rest(), before);

    // the versioned form of the new key is `symbol_invalid`, nothing written, still stale
    let err = api
        .resolve_symbol(&id, VERSIONED, Actor::HumanCli)
        .unwrap_err();
    assert!(
        matches!(&err, ApiError::Invalid(m) if m == RESOLVE_SYMBOL_INVALID),
        "{err:?}"
    );
    assert!(
        !err.to_string().contains("afero"),
        "the key is never echoed: {err}"
    );
    assert_eq!(at_rest(), before);
    assert_eq!(
        explain(&coderef).status,
        Status::Pending(PendingStatus::Stale)
    );

    // a node that is no CodeRef has no symbol to re-target
    let err = api
        .resolve_symbol(
            &NodeId::parse(&fact).unwrap(),
            AFERO_RENAMED_KEY,
            Actor::HumanCli,
        )
        .unwrap_err();
    assert!(matches!(err, ApiError::NotFound(_)), "{err:?}");

    // the re-target: one call, the claim, the prop and the anchor row move together
    let out = api
        .resolve_symbol(&id, AFERO_RENAMED_KEY, Actor::HumanCli)
        .unwrap();
    assert_eq!(out.id, coderef);
    assert_eq!(out.symbol, AFERO_RENAMED_KEY);
    assert_eq!(
        at_rest(),
        (
            AFERO_RENAMED_KEY.to_owned(),
            vec![AFERO_RENAMED_KEY.to_owned()],
            1
        )
    );
    let after = explain(&coderef);
    assert_eq!(
        after.status,
        Status::Settled(SettledStatus::Active),
        "{after:?}"
    );
    assert_eq!(after.next_actions, vec![]);
    assert_eq!(listed(), vec![], "the re-targeted CodeRef left review");
}

/// BR-6 (E2.10 boundary review; E2.11b), the logical-read contract: `canonical_of` is the
/// store read of a node's identity redirect. Positive: after the ER pass matches two twins,
/// the merged node (the later rid) reads as the canonical (the earlier). Negative: the canonical
/// itself, a node no identity row names and a foreign workspace all read `None`.
#[test]
fn canonical_of_reads_the_redirect_of_a_merged_node_and_none_otherwise() {
    use kaban_store_sqlite::SqliteStore;
    let s = Scratch::new("canonical-of");
    let store = Arc::new(SqliteStore::open(&s.db(), &s.keys()).unwrap());
    let api = api_over(&store);
    let mut ids = Vec::new();
    for body in ["first body line", "second body line"] {
        ids.push(
            api.remember(RememberRequest {
                kind: "Fact".into(),
                text: format!("Saffron ledger reconciliation\n{body}"),
                ..Default::default()
            })
            .unwrap()
            .id,
        );
    }
    let alone = api
        .remember(RememberRequest {
            kind: "Fact".into(),
            text: "velvet antenna calibration notes".into(),
            ..Default::default()
        })
        .unwrap()
        .id;
    let (canonical, merged) = (
        NodeId::parse(&ids[0]).unwrap(),
        NodeId::parse(&ids[1]).unwrap(),
    );
    // before the pass: no identity row names either twin
    assert_eq!(store.canonical_of("default", &merged).unwrap(), None);
    let report = store.er_resolve("default").unwrap();
    assert_eq!((report.compared, report.matched), (1, 1), "{report:?}");
    assert_eq!(
        store.canonical_of("default", &merged).unwrap(),
        Some(canonical.clone())
    );
    assert_eq!(store.canonical_of("default", &canonical).unwrap(), None);
    assert_eq!(
        store
            .canonical_of("default", &NodeId::parse(&alone).unwrap())
            .unwrap(),
        None
    );
    assert_eq!(store.canonical_of("other", &merged).unwrap(), None);
    // the reversal removes the redirect
    api.unmerge(&ids[1], None, kaban_core::Actor::HumanCli)
        .unwrap();
    assert_eq!(store.canonical_of("default", &merged).unwrap(), None);
}

/// BR-6 renderer (E2.11b): `explain` of the merged twin is `settled: merged` with the `merged`
/// template naming the pair as node ids and the one runnable reversal; the canonical renders as
/// before (`active`, no action), and so does the pair after `unmerge`.
#[test]
fn an_applied_merge_renders_settled_merged_with_the_unmerge_action_and_the_canonical_stays_active()
{
    use kaban_store_sqlite::SqliteStore;
    let s = Scratch::new("explain-merged");
    let store = Arc::new(SqliteStore::open(&s.db(), &s.keys()).unwrap());
    let api = api_over(&store);
    let mut ids = Vec::new();
    for body in ["first body line", "second body line"] {
        ids.push(
            api.remember(RememberRequest {
                kind: "Fact".into(),
                text: format!("Saffron ledger reconciliation\n{body}"),
                ..Default::default()
            })
            .unwrap()
            .id,
        );
    }
    let (canonical, merged) = (ids[0].as_str(), ids[1].as_str());
    let report = store.er_resolve("default").unwrap();
    assert_eq!((report.compared, report.matched), (1, 1), "{report:?}");
    let explain_of = |address: &str| {
        api.explain_status(ExplainStatusRequest {
            address: address.into(),
        })
        .unwrap()
    };
    let x = explain_of(merged);
    assert_eq!(x.status, Status::Settled(SettledStatus::Merged), "{x:?}");
    assert_eq!(
        x.sentence,
        format!(
            "merged: {merged} reads as {canonical}; a human reverses it with kaban unmerge {merged}"
        )
    );
    assert_eq!(x.next_actions.len(), 1, "{:?}", x.next_actions);
    assert_eq!(x.next_actions[0].command, format!("kaban unmerge {merged}"));
    assert!(x.explanation.is_none() && x.lineages.is_empty(), "{x:?}");
    // the canonical is itself: no redirect, no action
    let c = explain_of(canonical);
    assert_eq!(c.status, Status::Settled(SettledStatus::Active), "{c:?}");
    assert!(c.next_actions.is_empty(), "{:?}", c.next_actions);
    // after the reversal both read as before
    api.unmerge(merged, None, kaban_core::Actor::HumanCli)
        .unwrap();
    let x = explain_of(merged);
    assert_eq!(x.status, Status::Settled(SettledStatus::Active), "{x:?}");
    assert!(x.next_actions.is_empty(), "{:?}", x.next_actions);
}

/// RW4-2 (EN-F12c; E2.5 `resolve --symbol`; wave-4 review): a person's second re-target of
/// one `CodeRef` supersedes their first in the same transaction, so the winning `symbol` claim,
/// the stored `props.symbol` and the `symbol` anchor row agree after two re-targets in either
/// lexical order (`CreateFile` then `Open` on one node, `Open` then `CreateFile` on another).
/// Red at the base: two equal-weight claims of one person are ordered by value, so one order
/// leaves the earlier key the winning claim while the later key is stored.
#[test]
fn a_second_retarget_supersedes_the_first_so_the_winning_claim_is_the_stored_key() {
    use cg_fixture::{AFERO_COMMIT, AFERO_KEY, AFERO_PATH, AFERO_RENAMED_KEY, REPO_ID, Variant};
    use kaban_codegraph::SnapshotProvider;
    use kaban_codegraph::testing::FixedSnapshot;
    use kaban_core::Actor;
    use kaban_store_sqlite::SqliteStore;
    use serde_json::json;

    const REPO: &str = "kaban";
    const LOW: &str = AFERO_RENAMED_KEY;
    const HIGH: &str = "scip-go gomod github.com/spf13/afero . MemMapFs#Open().";
    assert!(LOW < HIGH);

    let s = Scratch::new("retarget-twice");
    let root = scripted_repo(&s.0);
    let built = cg_fixture::build_with_policy(
        &s.0,
        Variant::AferoRenamed,
        AFERO_COMMIT,
        cg_fixture::POLICY_VERSION,
    );
    let provider: Arc<dyn SnapshotProvider> = Arc::new(FixedSnapshot::new(&built.path));
    let store = Arc::new(SqliteStore::open(&s.db(), &s.keys()).unwrap());
    let api = KabanApi::with_config(
        Arc::clone(&store) as Arc<dyn GraphStore>,
        Policy::from_file(Policy::parse(b"").unwrap(), "test".into()).unwrap(),
        store.signer(),
        None,
        ApiConfig {
            repo: Some(REPO.into()),
            repo_root: Some(root),
            ..Default::default()
        },
    )
    .with_snapshot_provider(s.0.join("codegraph").join(REPO_ID), provider);
    let remember = |tag: &str| -> String {
        api.remember(RememberRequest {
            kind: "CodeRef".into(),
            text: format!("{REPO} {AFERO_PATH} CodeRef {tag}"),
            props: Some(json!({
                "repo": REPO, "commit": AFERO_COMMIT, "tool": "fixture", "relative_path": AFERO_PATH,
                "name_path": "MemMapFs/Create", "kind": "Method", "line": 7, "symbol": AFERO_KEY,
                "snapshot_commit": AFERO_COMMIT,
            })),
            ..Default::default()
        })
        .unwrap()
        .id
    };
    let conn = rusqlite::Connection::open(s.db()).unwrap();
    // the stored key, the anchor rows, the winning human `symbol` claims' values and how many
    // of the person's `symbol` claims supersede another
    let at_rest = |id: &str| -> (String, Vec<String>, Vec<String>, i64) {
        let props: String = conn
            .query_row("SELECT props FROM node WHERE id = ?1", [id], |r| r.get(0))
            .unwrap();
        let props: serde_json::Value = serde_json::from_str(&props).unwrap();
        let mut st = conn
            .prepare(
                "SELECT key FROM node_anchor WHERE anchor_kind = 'symbol'
                   AND rid = (SELECT rid FROM node WHERE id = ?1) ORDER BY key",
            )
            .unwrap();
        let anchors: Vec<String> = st
            .query_map([id], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        let mut st = conn
            .prepare(
                "SELECT a.value FROM assertion a JOIN assertion_status s ON s.assertion_id = a.id
                 WHERE s.disposition = 'winner' AND a.layer = 'human' AND a.claim_class = 'prop_value'
                   AND a.predicate = 'symbol' AND a.subject_rid = (SELECT rid FROM node WHERE id = ?1)
                 ORDER BY a.value",
            )
            .unwrap();
        let winners: Vec<String> = st
            .query_map([id], |r| r.get::<_, String>(0))
            .unwrap()
            .map(|v| {
                let v = v.unwrap();
                serde_json::from_str::<serde_json::Value>(&v)
                    .ok()
                    .and_then(|j| j.as_str().map(str::to_owned))
                    .unwrap_or(v)
            })
            .collect();
        let superseding: i64 = conn
            .query_row(
                "SELECT count(*) FROM assertion WHERE layer = 'human' AND claim_class = 'prop_value'
                   AND predicate = 'symbol' AND supersedes IS NOT NULL
                   AND subject_rid = (SELECT rid FROM node WHERE id = ?1)",
                [id],
                |r| r.get(0),
            )
            .unwrap();
        (
            props["symbol"].as_str().unwrap().to_owned(),
            anchors,
            winners,
            superseding,
        )
    };

    let rising = remember("rising");
    let falling = remember("falling");
    for (coderef, first, second) in [(&rising, LOW, HIGH), (&falling, HIGH, LOW)] {
        let id = NodeId::parse(coderef).unwrap();
        assert_eq!(
            at_rest(coderef),
            (AFERO_KEY.to_owned(), vec![AFERO_KEY.to_owned()], vec![], 0)
        );
        api.resolve_symbol(&id, first, Actor::HumanCli).unwrap();
        assert_eq!(
            at_rest(coderef),
            (
                first.to_owned(),
                vec![first.to_owned()],
                vec![first.to_owned()],
                0
            ),
            "after the first re-target"
        );
        api.resolve_symbol(&id, second, Actor::HumanCli).unwrap();
        let (stored, anchors, winners, superseding) = at_rest(coderef);
        assert_eq!(
            winners,
            vec![second.to_owned()],
            "the winning claim is the stored key after re-targeting twice ({} then {})",
            if first < second { "lower" } else { "higher" },
            if first < second { "higher" } else { "lower" },
        );
        assert_eq!(
            (stored, anchors),
            (second.to_owned(), vec![second.to_owned()])
        );
        assert_eq!(superseding, 1, "the second claim supersedes the first");
    }
}
