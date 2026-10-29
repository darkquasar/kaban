//! v3 lifecycle semantics in the store (core-spec §4, F5; core-plan C2.2): the text index holds
//! active rows only, so tombstoned and archived notes leave FTS and come back on reactivation.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::{Scratch, created, ctx, nid};
use kaban_core::{
    ForgetMode, Governance, GovernanceStore, InvalidTransition, Lifecycle, MemoryStore, NewEvent,
    NewNode, NodeId, RecallQuery, SignerKind, TraverseQuery, TraverseResult,
};
use kaban_store_sqlite::SqliteStore;

fn found(store: &SqliteStore, query: &str, id: &str) -> bool {
    store
        .recall(&RecallQuery {
            query: query.into(),
            limit: 5,
            ..Default::default()
        })
        .unwrap()
        .iter()
        .any(|h| h.node.id == id)
}

#[test]
fn fts_holds_active_rows_only_after_tombstone_and_archive() {
    let store = SqliteStore::open_memory().unwrap();
    let fact = created(&store, "Fact", "quokkas live on rottnest island");
    let decision = created(&store, "Decision", "we deploy quokka builds on fridays");
    assert_eq!(fact.lifecycle, Lifecycle::Active);
    assert_eq!(fact.governance, Governance::None);
    assert_eq!(
        decision.governance,
        Governance::Proposed,
        "governed kinds start proposed"
    );
    assert_eq!(fact.workspace_id, "default");
    assert!(found(&store, "quokkas rottnest", &fact.id));
    assert!(found(&store, "quokka fridays", &decision.id));

    store
        .forget(&node_id(&fact.id), ForgetMode::Invalidate, &ctx())
        .unwrap();
    let gone = store.get(&node_id(&fact.id), true).unwrap().unwrap();
    assert_eq!(gone.lifecycle, Lifecycle::Tombstoned, "OD-4 default");
    assert_eq!(
        gone.governance,
        Governance::None,
        "Facts gain no governance"
    );
    assert!(gone.valid_to.is_some());
    assert!(
        !found(&store, "quokkas rottnest", &fact.id),
        "tombstoned rows leave the index"
    );

    store
        .forget(&node_id(&decision.id), ForgetMode::Archive, &ctx())
        .unwrap();
    let archived = store.get(&node_id(&decision.id), false).unwrap().unwrap();
    assert_eq!(archived.lifecycle, Lifecycle::Archived);
    assert_eq!(
        archived.governance,
        Governance::Proposed,
        "archive is lifecycle only"
    );
    assert!(
        !found(&store, "quokka fridays", &decision.id),
        "archived rows leave the index"
    );
    // a new identical note after a tombstone is `created` (OD-16: never re-proposed)
    let again = created(&store, "Fact", "quokkas live on rottnest island");
    assert_ne!(again.id, fact.id);
    assert!(found(&store, "quokkas rottnest", &again.id));
}

#[test]
fn invalidate_on_a_governed_record_sets_governance_invalidated_too() {
    let store = SqliteStore::open_memory().unwrap();
    let decision = created(&store, "Convention", "always pin toolchains");
    let outcome = store
        .forget(&node_id(&decision.id), ForgetMode::Invalidate, &ctx())
        .unwrap();
    assert_eq!(
        (outcome.lifecycle, outcome.governance),
        (Lifecycle::Tombstoned, Governance::Invalidated)
    );
    let n = store.get(&node_id(&decision.id), true).unwrap().unwrap();
    assert_eq!(
        (n.lifecycle, n.governance),
        (Lifecycle::Tombstoned, Governance::Invalidated)
    );
    assert_eq!(n.rev, 2);
    // the invalidation is a governance transition, so it is on the event log (C2.3)
    let events = store.events(&node_id(&decision.id)).unwrap();
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0].kind, "invalidated");
    assert_eq!(events[0].signer_kind, SignerKind::Agent);
    // invalidating it again: tombstoned is terminal
    assert!(matches!(
        store.forget(&node_id(&decision.id), ForgetMode::Invalidate, &ctx()),
        Err(kaban_core::StoreError::InvalidTransition(
            InvalidTransition::Lifecycle { .. }
        ))
    ));
    // a superseded record cannot be invalidated (governance table), even though its
    // lifecycle (archived) could still move to tombstoned
    let old = created(&store, "Decision", "ship on mondays");
    store
        .transition(
            &node_id(&old.id),
            Governance::Accepted,
            accept_event("ok"),
            &ctx(),
        )
        .unwrap();
    let new = created(&store, "Decision", "ship on wednesdays");
    store
        .link(
            &node_id(&new.id),
            &node_id(&old.id),
            "SUPERSEDES",
            None,
            None,
            &ctx(),
        )
        .unwrap();
    assert!(matches!(
        store.forget(&node_id(&old.id), ForgetMode::Invalidate, &ctx()),
        Err(kaban_core::StoreError::InvalidTransition(
            InvalidTransition::Governance { .. }
        ))
    ));
    assert_eq!(store.events(&node_id(&old.id)).unwrap().len(), 2);
}

/// `archived -> active` does not exist and `archived -> archived` is not a transition: a second
/// archive is `InvalidTransition`; a tombstone of an archived record is allowed.
#[test]
fn an_archived_record_cannot_be_archived_again_but_can_be_tombstoned() {
    let store = SqliteStore::open_memory().unwrap();
    let fact = created(&store, "Fact", "bilbies have long ears");
    let first = store
        .forget(&node_id(&fact.id), ForgetMode::Archive, &ctx())
        .unwrap();
    assert_eq!(first.lifecycle, Lifecycle::Archived);
    assert!(
        store.get(&node_id(&fact.id), false).unwrap().is_some(),
        "archived stays visible by id"
    );
    assert!(matches!(
        store.forget(&node_id(&fact.id), ForgetMode::Archive, &ctx()),
        Err(kaban_core::StoreError::InvalidTransition(
            InvalidTransition::Lifecycle {
                from: Lifecycle::Archived,
                to: Lifecycle::Archived
            }
        ))
    ));
    let second = store
        .forget(&node_id(&fact.id), ForgetMode::Tombstone, &ctx())
        .unwrap();
    assert_eq!(second.lifecycle, Lifecycle::Tombstoned);
    assert!(second.outbox_seq > first.outbox_seq);
    assert_eq!(store.events(&node_id(&fact.id)).unwrap(), vec![]);
}

fn node_id(id: &str) -> NodeId {
    NodeId::parse(id).unwrap()
}

fn traverse_from(store: &SqliteStore, id: &str) -> kaban_core::Result<TraverseResult> {
    store.traverse(&TraverseQuery {
        start_ids: vec![node_id(id)],
        rels: vec![],
        direction: kaban_core::TraverseDirection::Both,
        max_depth: 2,
        node_kinds: vec![],
        governance: vec![],
        limit: 50,
        workspace_id: None,
        ..Default::default()
    })
}

/// C2.3 slice 1 (core-spec F5, OD-16): `tombstone` is the default mode; it is lifecycle only
/// (governance untouched, no event) and the row disappears from every default read, while a
/// new identical note is `created`, never answered with the tombstone as `existing`.
#[test]
fn tombstone_is_the_default_and_hides_the_node_from_every_default_read() {
    let store = SqliteStore::open_memory().unwrap();
    let rule = created(&store, "Convention", "wombat builds run on tuesdays");
    let fact = created(&store, "Fact", "wombats dig square burrows");
    store
        .link(
            &node_id(&fact.id),
            &node_id(&rule.id),
            "RELATES_TO",
            None,
            None,
            &ctx(),
        )
        .unwrap();

    let outcome = store
        .forget(&node_id(&rule.id), ForgetMode::default(), &ctx())
        .unwrap();
    assert_eq!(outcome.lifecycle, Lifecycle::Tombstoned);
    assert_eq!(
        outcome.governance,
        Governance::Proposed,
        "tombstone is lifecycle only"
    );
    assert!(outcome.outbox_seq > 0, "the write produced an changes row");

    assert!(
        store.get(&node_id(&rule.id), false).unwrap().is_none(),
        "hidden from get by default"
    );
    let hidden = store.get(&node_id(&rule.id), true).unwrap().unwrap();
    assert_eq!(
        (hidden.lifecycle, hidden.governance, hidden.rev),
        (Lifecycle::Tombstoned, Governance::Proposed, 2)
    );
    assert!(
        store.events(&node_id(&rule.id)).unwrap().is_empty(),
        "a plain tombstone records no governance event (D1)"
    );
    assert!(!found(&store, "wombat tuesdays", &rule.id), "not in recall");
    // graph expansion from its neighbour must not resurface it either
    let via_graph = store
        .recall(&RecallQuery {
            query: "wombats burrows".into(),
            hops: 1,
            limit: 5,
            ..Default::default()
        })
        .unwrap();
    assert!(
        via_graph.iter().all(|h| h.node.id != rule.id),
        "{via_graph:?}"
    );
    let t = traverse_from(&store, &fact.id).unwrap();
    assert!(t.nodes.iter().all(|n| n.id != rule.id), "{t:?}");
    assert!(t.edges.is_empty(), "edges to hidden nodes are not returned");
    assert!(
        matches!(
            traverse_from(&store, &rule.id),
            Err(kaban_core::StoreError::NotFound(_))
        ),
        "a tombstoned start id is as good as absent"
    );
    // OD-16: a new identical note is created, the tombstone is never `existing`
    let again = created(&store, "Convention", "wombat builds run on tuesdays");
    assert_ne!(again.id, rule.id);
    assert!(found(&store, "wombat tuesdays", &again.id));
    // tombstoned is terminal except for purge
    assert!(matches!(
        store.forget(&node_id(&rule.id), ForgetMode::Archive, &ctx()),
        Err(kaban_core::StoreError::InvalidTransition(_))
    ));
}

fn accept_event(reason: &str) -> NewEvent {
    NewEvent {
        reason: reason.into(),
        path_scope: vec!["crates/**".into()],
        signer_kind: SignerKind::HumanCli,
        commit_sha: Some("abc1234".into()),
        ..Default::default()
    }
}

/// C2.3 slice 2 (core-spec §4 governance table): `proposed -> accepted` through `transition`
/// records `accepted_commit`, the path scope and one `accepted` event listed by `events`;
/// `accepted -> proposed` and every other pair outside the table are `InvalidTransition`, as
/// is any governance change on a non-active or non-governed record.
#[test]
fn accepting_a_proposal_writes_an_event_and_the_reverse_is_rejected() {
    let store = SqliteStore::open_memory().unwrap();
    let decision = created(&store, "Decision", "numbats are the mascot of the build");
    let id = node_id(&decision.id);
    let (accepted, seq) = store
        .transition(
            &id,
            Governance::Accepted,
            accept_event("reviewed in standup"),
            &ctx(),
        )
        .unwrap();
    assert!(seq > 0, "transition reports the event seq (M3-18)");
    assert_eq!(accepted.governance, Governance::Accepted);
    assert_eq!(accepted.lifecycle, Lifecycle::Active);
    assert_eq!(accepted.accepted_commit.as_deref(), Some("abc1234"));
    assert_eq!(accepted.path_scope, vec!["crates/**".to_string()]);
    assert_eq!(accepted.rev, 2, "every store update bumps rev (D4)");
    let events = store.events(&id).unwrap();
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0].kind, "accepted");
    assert_eq!(events[0].reason, "reviewed in standup");
    assert_eq!(events[0].signer_kind, SignerKind::HumanCli);
    assert_eq!(events[0].commit_sha.as_deref(), Some("abc1234"));
    assert_eq!(events[0].node_id, decision.id);
    assert_eq!(events[0].workspace_id, "default");

    // accepted -> proposed is not in the table: rejected, nothing written
    let err = store
        .transition(
            &id,
            Governance::Proposed,
            accept_event("take it back"),
            &ctx(),
        )
        .unwrap_err();
    assert!(
        matches!(
            err,
            kaban_core::StoreError::InvalidTransition(InvalidTransition::Governance {
                from: Governance::Accepted,
                to: Governance::Proposed
            })
        ),
        "{err:?}"
    );
    assert_eq!(
        store.events(&id).unwrap().len(),
        1,
        "a rejection writes no event"
    );
    assert_eq!(
        store
            .get(&node_id(&decision.id), false)
            .unwrap()
            .unwrap()
            .rev,
        2
    );

    // a non-governed kind has no governance to move
    let fact = created(&store, "Fact", "numbats eat termites");
    assert!(matches!(
        store.transition(
            &node_id(&fact.id),
            Governance::Accepted,
            accept_event("no"),
            &ctx()
        ),
        Err(kaban_core::StoreError::InvalidTransition(
            InvalidTransition::NotGoverned(_)
        ))
    ));
    // governance changes only on active records
    let rule = created(&store, "Convention", "numbat names are lowercase");
    store
        .forget(&node_id(&rule.id), ForgetMode::Archive, &ctx())
        .unwrap();
    assert!(matches!(
        store.transition(
            &node_id(&rule.id),
            Governance::Accepted,
            accept_event("late"),
            &ctx()
        ),
        Err(kaban_core::StoreError::InvalidTransition(
            InvalidTransition::NotActive(Lifecycle::Archived)
        ))
    ));
    // an unknown id is NotFound
    assert!(matches!(
        store.transition(
            &NodeId::new(),
            Governance::Accepted,
            accept_event("ghost"),
            &ctx()
        ),
        Err(kaban_core::StoreError::NotFound(_))
    ));
}

fn edge_count(store: &SqliteStore, id: &str) -> usize {
    traverse_from(store, id).unwrap().edges.len()
}

/// C2.3 slice 3 (MD-7 as decided: wave A §6 option B). `link SUPERSEDES` on a *proposed*
/// Decision is the strict §4 rejection: the whole link fails with `InvalidTransition`, no edge
/// and no event are written. On an *accepted* Decision it archives the target with
/// `governance=superseded` and a `superseded` event. `INVALIDATES` on a non-governed target
/// tombstones it with an `invalidated` event (OD-4). A link to a tombstoned node never brings
/// it back into traverse or recall (D3).
#[test]
fn supersedes_rejects_a_proposed_decision_and_archives_an_accepted_one() {
    let store = SqliteStore::open_memory().unwrap();
    let old = created(&store, "Decision", "platypus builds use the old cache key");
    let new = created(&store, "Decision", "platypus builds use the new cache key");

    let err = store
        .link(
            &node_id(&new.id),
            &node_id(&old.id),
            "SUPERSEDES",
            None,
            None,
            &ctx(),
        )
        .unwrap_err();
    assert!(
        matches!(
            err,
            kaban_core::StoreError::InvalidTransition(InvalidTransition::Governance {
                from: Governance::Proposed,
                to: Governance::Superseded
            })
        ),
        "{err:?}"
    );
    let msg = err.to_string();
    assert!(
        msg.contains("proposed") && msg.contains("superseded"),
        "the error names the states: {msg}"
    );
    assert_eq!(edge_count(&store, &new.id), 0, "no edge was written");
    assert_eq!(store.events(&node_id(&old.id)).unwrap(), vec![]);
    let untouched = store.get(&node_id(&old.id), false).unwrap().unwrap();
    assert_eq!(
        (untouched.lifecycle, untouched.governance, untouched.rev),
        (Lifecycle::Active, Governance::Proposed, 1)
    );
    // the same request inside `remember` rolls the new node back too
    assert!(
        store
            .remember(
                NewNode {
                    kind: "Decision".into(),
                    text: "platypus builds use a third cache key".into(),
                    ..Default::default()
                },
                &[kaban_core::LinkReq {
                    rel: "SUPERSEDES".into(),
                    to: Some(node_id(&old.id)),
                    ..Default::default()
                }],
                &ctx(),
            )
            .is_err()
    );
    let hits = store
        .recall(&RecallQuery {
            query: "third cache key".into(),
            limit: 5,
            ..Default::default()
        })
        .unwrap();
    assert!(
        hits.iter().all(|h| !h.node.text.contains("third")),
        "the rolled-back note must not be stored: {hits:?}"
    );

    // accept, then supersede: archived + superseded + event
    store
        .transition(
            &node_id(&old.id),
            Governance::Accepted,
            NewEvent {
                reason: "seeded".into(),
                signer_kind: SignerKind::System,
                ..Default::default()
            },
            &ctx(),
        )
        .unwrap();
    store
        .link(
            &node_id(&new.id),
            &node_id(&old.id),
            "SUPERSEDES",
            None,
            None,
            &ctx(),
        )
        .unwrap();
    let superseded = store.get(&node_id(&old.id), false).unwrap().unwrap();
    assert_eq!(
        (superseded.lifecycle, superseded.governance, superseded.rev),
        (Lifecycle::Archived, Governance::Superseded, 3)
    );
    assert!(superseded.valid_to.is_some());
    let events = store.events(&node_id(&old.id)).unwrap();
    assert_eq!(
        events.iter().map(|e| e.kind.as_str()).collect::<Vec<_>>(),
        vec!["accepted", "superseded"]
    );
    assert_eq!(
        events[1].signer_kind,
        SignerKind::Agent,
        "from the write context"
    );
    assert!(events[1].reason.contains(&new.id), "{events:?}");
    assert_eq!(
        edge_count(&store, &new.id),
        0,
        "archived targets are not returned by traverse"
    );
    // a second SUPERSEDES edge to the now-superseded target is refused as well
    let third = created(&store, "Decision", "platypus builds use yet another key");
    assert!(matches!(
        store.link(
            &node_id(&third.id),
            &node_id(&old.id),
            "SUPERSEDES",
            None,
            None,
            &ctx()
        ),
        Err(kaban_core::StoreError::InvalidTransition(_))
    ));

    // OD-4: INVALIDATES on a Fact tombstones it with an event, governance stays none
    let fact = created(&store, "Fact", "platypuses lay eggs in burrows");
    store
        .link(
            &node_id(&new.id),
            &node_id(&fact.id),
            "INVALIDATES",
            None,
            None,
            &ctx(),
        )
        .unwrap();
    assert!(store.get(&node_id(&fact.id), false).unwrap().is_none());
    let gone = store.get(&node_id(&fact.id), true).unwrap().unwrap();
    assert_eq!(
        (gone.lifecycle, gone.governance),
        (Lifecycle::Tombstoned, Governance::None)
    );
    let ev = store.events(&node_id(&fact.id)).unwrap();
    assert_eq!(ev.len(), 1);
    assert_eq!(ev[0].kind, "invalidated");

    // D3: a later link to the tombstoned node is allowed and never resurfaces it
    let other = created(&store, "Fact", "platypus venom comes from a spur");
    store
        .link(
            &node_id(&other.id),
            &node_id(&fact.id),
            "RELATES_TO",
            None,
            None,
            &ctx(),
        )
        .unwrap();
    let t = traverse_from(&store, &other.id).unwrap();
    assert!(t.nodes.iter().all(|n| n.id != fact.id), "{t:?}");
    assert!(t.edges.is_empty());
    let hits = store
        .recall(&RecallQuery {
            query: "platypus venom spur".into(),
            hops: 2,
            limit: 10,
            ..Default::default()
        })
        .unwrap();
    assert!(hits.iter().all(|h| h.node.id != fact.id), "{hits:?}");
    assert!(matches!(
        traverse_from(&store, &fact.id),
        Err(kaban_core::StoreError::NotFound(_))
    ));
}

/// `(entity, op, id)` of every changes row with `seq > after`, in order.
fn changes_after(db: &std::path::Path, after: i64) -> Vec<(String, String, String)> {
    let raw = rusqlite::Connection::open(db).unwrap();
    let mut st = raw
        .prepare("SELECT entity, op, id FROM changes WHERE seq > ? ORDER BY seq")
        .unwrap();
    st.query_map([after], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
}

/// C2.3 slice 5 (core-spec F5 purge, S5, MD-24): purge removes text, the FTS entry and the
/// edges, writes exactly one explicit `op='purge'` changes row (plus one `delete` row per removed
/// edge, no node `update`/`delete` row) and keeps the id as a `purged` ghost row so it never
/// reappears: the id is `NotFound` for links and forget, hidden from `get`, and the same text
/// is `created` again.
#[test]
fn purge_keeps_a_ghost_row_and_emits_one_purge_changes_row() {
    let s = Scratch::new("kaban-lifecycle", "purge");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let victim = created(&store, "Fact", "echidna spines are modified hairs");
    let left = created(&store, "Fact", "echidnas are monotremes");
    let right = created(&store, "Idea", "add an echidna mascot");
    store
        .link(
            &node_id(&left.id),
            &node_id(&victim.id),
            "RELATES_TO",
            None,
            None,
            &ctx(),
        )
        .unwrap();
    store
        .link(
            &node_id(&victim.id),
            &node_id(&right.id),
            "ABOUT",
            None,
            None,
            &ctx(),
        )
        .unwrap();
    let before = i64::try_from(
        store
            .forget(&node_id(&right.id), ForgetMode::Archive, &ctx())
            .unwrap()
            .outbox_seq,
    )
    .unwrap();

    let outcome = store
        .forget(&node_id(&victim.id), ForgetMode::Purge, &ctx())
        .unwrap();
    assert_eq!(outcome.lifecycle, Lifecycle::Purged);
    assert!(i64::try_from(outcome.outbox_seq).unwrap() > before);

    let rows = changes_after(&s.db(), before);
    let purge_rows: Vec<_> = rows.iter().filter(|r| r.1 == "purge").collect();
    assert_eq!(purge_rows.len(), 1, "{rows:?}");
    assert_eq!(purge_rows[0].0, "node");
    assert_eq!(purge_rows[0].2, victim.id);
    assert_eq!(
        rows.iter()
            .filter(|r| r.0 == "edge" && r.1 == "delete")
            .count(),
        2,
        "one delete row per removed edge: {rows:?}"
    );
    assert!(
        rows.iter()
            .all(|r| !(r.0 == "node" && r.2 == victim.id && r.1 != "purge")),
        "no node update/delete row for a purge (MD-24): {rows:?}"
    );
    assert_eq!(
        i64::try_from(outcome.outbox_seq).unwrap(),
        before + i64::try_from(rows.len()).unwrap(),
        "outbox_seq is the last row of this write"
    );

    assert!(store.get(&node_id(&victim.id), false).unwrap().is_none());
    let ghost = store.get(&node_id(&victim.id), true).unwrap().unwrap();
    assert_eq!(ghost.lifecycle, Lifecycle::Purged);
    assert_eq!(ghost.text, "");
    assert_eq!(ghost.content_hash, "");
    assert!(ghost.summary.is_none() && ghost.tags.is_empty());
    assert_eq!(ghost.props, serde_json::json!({}));
    assert!(
        !found(&store, "echidna spines", &victim.id),
        "FTS entry removed"
    );
    let t = traverse_from(&store, &left.id).unwrap();
    assert!(t.edges.is_empty() && t.nodes.len() == 1, "{t:?}");
    for attempt in [
        store
            .link(
                &node_id(&left.id),
                &node_id(&victim.id),
                "RELATES_TO",
                None,
                None,
                &ctx(),
            )
            .map(drop),
        store
            .forget(&node_id(&victim.id), ForgetMode::Purge, &ctx())
            .map(drop),
        traverse_from(&store, &victim.id).map(drop),
    ] {
        assert!(
            matches!(attempt, Err(kaban_core::StoreError::NotFound(_))),
            "{attempt:?}"
        );
    }
    let again = created(&store, "Fact", "echidna spines are modified hairs");
    assert_ne!(again.id, victim.id);
    // purge of a tombstoned record is allowed (any -> purged)
    store
        .forget(&node_id(&left.id), ForgetMode::Tombstone, &ctx())
        .unwrap();
    assert_eq!(
        store
            .forget(&node_id(&left.id), ForgetMode::Purge, &ctx())
            .unwrap()
            .lifecycle,
        Lifecycle::Purged
    );
}

// ---- M2 review R2-4 (C2.10): lifecycle eligibility while every frontier is built ----

/// `A(active) -> B -> C(active)` with `B` hidden by `mode`; returns `(a, b, c)` ids.
fn bridge(store: &SqliteStore, mode: ForgetMode) -> (String, String, String) {
    let a = created(store, "Fact", "quokka alpha anchors the chain");
    let b = created(store, "Fact", "quokka bravo is the hidden bridge");
    let c = created(store, "Fact", "quokka charlie sits behind the bridge");
    // an unconstrained causal rel (`DERIVED_FROM`, core, `[]`): the archived frontier of
    // recall crosses causal rels only, and `CAUSED_BY` is `from: [Failure]`, enforced as data
    // since E1.8 (E1.7 R1)
    for (src, dst) in [(&a, &b), (&b, &c)] {
        store
            .link(
                &node_id(&src.id),
                &node_id(&dst.id),
                "DERIVED_FROM",
                None,
                None,
                &ctx(),
            )
            .unwrap();
    }
    store.forget(&node_id(&b.id), mode, &ctx()).unwrap();
    (a.id, b.id, c.id)
}

/// R2-4: traverse expands through active nodes only. An archived or tombstoned `B` must not
/// bridge `A` to `C` at depth 2, and no edge touching `B` is returned.
#[test]
fn traverse_does_not_expand_through_an_archived_or_tombstoned_bridge() {
    for mode in [ForgetMode::Archive, ForgetMode::Tombstone] {
        let store = SqliteStore::open_memory().unwrap();
        let (a, b, c) = bridge(&store, mode);
        let t = traverse_from(&store, &a).unwrap();
        let ids: Vec<&str> = t.nodes.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![a.as_str()],
            "{mode:?}: only the start is reachable: {t:?}"
        );
        assert!(
            !ids.contains(&b.as_str()) && !ids.contains(&c.as_str()),
            "{mode:?}: the hidden node bridged to C"
        );
        assert!(t.edges.is_empty(), "{mode:?}: {:?}", t.edges);
    }
}

/// R2-4 (core-spec §4, F6: traversal defaults to active nodes): an archived start id is as
/// good as absent, like a tombstoned one.
#[test]
fn an_archived_traverse_start_is_not_found() {
    let store = SqliteStore::open_memory().unwrap();
    let (_, b, _) = bridge(&store, ForgetMode::Archive);
    assert!(
        matches!(
            traverse_from(&store, &b),
            Err(kaban_core::StoreError::NotFound(ref id)) if *id == b
        ),
        "an archived start id must be NotFound"
    );
}

fn recall_ids(store: &SqliteStore, query: &str, include_superseded: bool) -> Vec<String> {
    store
        .recall(&RecallQuery {
            query: query.into(),
            hops: 2,
            limit: 10,
            include_superseded,
            ..Default::default()
        })
        .unwrap()
        .into_iter()
        .map(|h| h.node.id)
        .collect()
}

/// R2-4: recall's hop expansion never runs through a tombstoned node, and through an archived
/// one only when `include_superseded` opts in. The query matches `A` alone; `C` is reachable
/// only across `B`.
#[test]
fn recall_does_not_expand_through_a_hidden_bridge_unless_archived_and_opted_in() {
    let store = SqliteStore::open_memory().unwrap();
    let (a, b, c) = bridge(&store, ForgetMode::Archive);
    let ids = recall_ids(&store, "alpha anchors", false);
    assert_eq!(
        ids,
        vec![a.clone()],
        "archived B must not bridge to C: {ids:?}"
    );
    let ids = recall_ids(&store, "alpha anchors", true);
    assert!(
        ids.contains(&b) && ids.contains(&c),
        "with include_superseded the archived B and C behind it are reachable: {ids:?}"
    );

    let store = SqliteStore::open_memory().unwrap();
    let (a, _, _) = bridge(&store, ForgetMode::Tombstone);
    for opt in [false, true] {
        let ids = recall_ids(&store, "alpha anchors", opt);
        assert_eq!(
            ids,
            vec![a.clone()],
            "tombstoned B must never bridge (include_superseded={opt}): {ids:?}"
        );
    }
}

// --- EN-F4 (engine-plan E1.4): governance decided by the write context's `GovernedSet` ---

/// The shipped files plus `fixtures/ontology/incidents.yaml` resolved as one, as the registry
/// would hold them after a user load: `incidents:Incident{acceptance: human}`,
/// `incidents:Outage{serialize: none}`, `incidents:Decision` without the mixin,
/// `incidents:REPLACES{on_link: archive_target}`, `incidents:MENTIONED_IN`.
fn incidents_set() -> std::sync::Arc<kaban_core::GovernedSet> {
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../kaban-core/fixtures/ontology/incidents.yaml");
    let text = std::fs::read_to_string(&fixture).unwrap();
    let mut files = kaban_core::shipped::files().unwrap();
    files.push(kaban_core::OntologyFile::from_yaml(&text).unwrap());
    let ontology = kaban_core::resolve(&files, &kaban_core::NamespaceSet::new()).unwrap();
    std::sync::Arc::new(kaban_core::GovernedSet::from_ontology(&ontology))
}

/// A store that knows the `incidents` kinds and rels (the `schema_type` rows the registry
/// writes through `schema_extend`) and a context carrying the `incidents` governed set (the
/// one place a write decides governance by; the store holds no set of its own since E1.8).
fn incidents_store() -> (SqliteStore, kaban_core::WriteCtx) {
    use kaban_core::{SchemaStore, TypeDef};
    let store = SqliteStore::open_memory().unwrap();
    for (kind, name) in [
        ("label", "incidents:Incident"),
        ("label", "incidents:Outage"),
        ("label", "incidents:Decision"),
        ("rel", "incidents:REPLACES"),
        ("rel", "incidents:MENTIONED_IN"),
    ] {
        store
            .schema_extend(TypeDef {
                kind: kind.into(),
                name: name.into(),
                ns: None,
                description: name.into(),
                from_kinds: vec![],
                to_kinds: vec![],
                directed: true,
                props_schema: None,
                builtin: false,
                declaration: kaban_core::TypeDeclaration::default(),
            })
            .unwrap();
    }
    let ctx = ctx().with_governed(incidents_set());
    (store, ctx)
}

fn remember_with(
    store: &SqliteStore,
    ctx: &kaban_core::WriteCtx,
    kind: &str,
    text: &str,
) -> kaban_core::Node {
    match store
        .remember(
            NewNode {
                kind: kind.into(),
                text: text.into(),
                ..Default::default()
            },
            &[],
            ctx,
        )
        .unwrap()
    {
        kaban_core::RememberOutcome::Created(n) => n,
        kaban_core::RememberOutcome::Existing { node, .. } => panic!("fresh store: {node:?}"),
    }
}

#[test]
fn a_fixture_kind_with_the_mixin_is_proposed_on_write_and_needs_the_handshake() {
    // Arrange
    let (store, ctx) = incidents_store();

    // Act
    let incident = remember_with(
        &store,
        &ctx,
        "incidents:Incident",
        "the wombat cache fell over",
    );
    let outage = remember_with(
        &store,
        &ctx,
        "incidents:Outage",
        "wombat cache outage, 12 minutes",
    );
    let plain = remember_with(
        &store,
        &ctx,
        "incidents:Decision",
        "we restart the wombat cache nightly",
    );

    // Assert: governed kinds start proposed, `serialize: none` is still governed, the
    // mixin-less `Decision` of another namespace is not governed
    assert_eq!(incident.governance, Governance::Proposed);
    assert_eq!(
        outage.governance,
        Governance::Proposed,
        "serialize: none is still governed"
    );
    assert_eq!(
        plain.governance,
        Governance::None,
        "no mixin, no governance"
    );
    // the handshake: proposed -> accepted through the strict table, under the same set
    let (accepted, _) = store
        .transition(
            &node_id(&incident.id),
            Governance::Accepted,
            accept_event("incident review"),
            &ctx,
        )
        .unwrap();
    assert_eq!(accepted.governance, Governance::Accepted);
    // the mixin-less kind has no governance to move
    let err = store
        .transition(
            &node_id(&plain.id),
            Governance::Accepted,
            accept_event("not governed"),
            &ctx,
        )
        .unwrap_err();
    assert!(
        matches!(
            err,
            kaban_core::StoreError::InvalidTransition(InvalidTransition::NotGoverned(ref k)) if k == "incidents:Decision"
        ),
        "{err:?}"
    );
    // the same writes under the shipped set (no `incidents` loaded) are not governed
    let shipped = remember_with(
        &store,
        &common::ctx(),
        "incidents:Incident",
        "a second wombat incident",
    );
    assert_eq!(shipped.governance, Governance::None);
}

#[test]
fn a_user_rel_with_on_link_archive_target_archives_its_target_and_one_without_does_not() {
    // Arrange: two plain Facts (non-governed targets take the lifecycle path) and an accepted
    // Incident (a governed target takes the governance path)
    let (store, ctx) = incidents_store();
    let old_fact = remember_with(&store, &ctx, "Fact", "the wombat cache holds 2 GiB");
    let new_fact = remember_with(&store, &ctx, "Fact", "the wombat cache holds 4 GiB");
    let mentioned = remember_with(&store, &ctx, "Fact", "the wombat cache is in eu-west");
    let note = remember_with(&store, &ctx, "Fact", "cache sizing note");
    let old_incident = remember_with(
        &store,
        &ctx,
        "incidents:Incident",
        "cache fell over on monday",
    );
    store
        .transition(
            &node_id(&old_incident.id),
            Governance::Accepted,
            accept_event("reviewed"),
            &ctx,
        )
        .unwrap();
    let new_incident = remember_with(
        &store,
        &ctx,
        "incidents:Incident",
        "cache fell over on tuesday",
    );

    // Act
    store
        .link(
            &node_id(&new_fact.id),
            &node_id(&old_fact.id),
            "incidents:REPLACES",
            None,
            None,
            &ctx,
        )
        .unwrap();
    store
        .link(
            &node_id(&note.id),
            &node_id(&mentioned.id),
            "incidents:MENTIONED_IN",
            None,
            None,
            &ctx,
        )
        .unwrap();
    store
        .link(
            &node_id(&new_incident.id),
            &node_id(&old_incident.id),
            "incidents:REPLACES",
            None,
            None,
            &ctx,
        )
        .unwrap();

    // Assert: `on_link: archive_target` behaves like `SUPERSEDES`, a rel without it is inert
    let replaced = store.get(&node_id(&old_fact.id), true).unwrap().unwrap();
    assert_eq!(replaced.lifecycle, Lifecycle::Archived);
    assert_eq!(replaced.governance, Governance::None);
    let events = store.events(&node_id(&old_fact.id)).unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind, "superseded");
    assert_eq!(
        events[0].reason,
        format!("incidents:REPLACES edge from {}", new_fact.id)
    );
    let untouched = store.get(&node_id(&mentioned.id), true).unwrap().unwrap();
    assert_eq!(untouched.lifecycle, Lifecycle::Active);
    assert_eq!(store.events(&node_id(&mentioned.id)).unwrap().len(), 0);
    let superseded = store
        .get(&node_id(&old_incident.id), true)
        .unwrap()
        .unwrap();
    assert_eq!(superseded.lifecycle, Lifecycle::Archived);
    assert_eq!(superseded.governance, Governance::Superseded);
    // under the shipped set `incidents:REPLACES` is just an edge
    let a = remember_with(&store, &ctx, "Fact", "plain a");
    let b = remember_with(&store, &ctx, "Fact", "plain b");
    store
        .link(
            &node_id(&a.id),
            &node_id(&b.id),
            "incidents:REPLACES",
            None,
            None,
            &common::ctx(),
        )
        .unwrap();
    assert_eq!(
        store.get(&node_id(&b.id), true).unwrap().unwrap().lifecycle,
        Lifecycle::Active
    );
}

#[test]
fn the_briefing_lists_accepted_records_of_the_kinds_that_govern_paths() {
    // Arrange: accepted records of a shipped governing kind, of the fixture's governing kind
    // (`Incident`), of its non-governing kind (`Outage`) and one still proposed
    let (store, ctx) = incidents_store();
    let decision = remember_with(&store, &ctx, "Decision", "quolls get the fast lane");
    let incident = remember_with(&store, &ctx, "incidents:Incident", "quoll lane outage");
    let outage = remember_with(
        &store,
        &ctx,
        "incidents:Outage",
        "quoll lane down 3 minutes",
    );
    let proposed = remember_with(&store, &ctx, "incidents:Incident", "quoll lane flapping");
    for id in [&decision.id, &incident.id, &outage.id] {
        store
            .transition(
                &node_id(id),
                Governance::Accepted,
                accept_event("reviewed"),
                &ctx,
            )
            .unwrap();
    }

    // Act
    // the caller (kaban-api) hands the governing kinds of its registry over (E1.6)
    let set = incidents_set();
    let governing: Vec<&str> = set.governing().collect();
    let briefing = store.briefing("default", &governing, 10).unwrap();
    let shipped_store = SqliteStore::open_memory().unwrap();
    let plain = remember_with(&shipped_store, &ctx, "Decision", "plain quoll decision");
    shipped_store
        .transition(
            &node_id(&plain.id),
            Governance::Accepted,
            accept_event("reviewed"),
            &ctx,
        )
        .unwrap();
    let shipped = kaban_core::GovernedSet::shipped();
    let shipped_governing: Vec<&str> = shipped.governing().collect();
    let shipped_briefing = shipped_store
        .briefing("default", &shipped_governing, 10)
        .unwrap();

    // Assert: `governs_paths: true` kinds only, accepted only; the shipped store still works
    let ids: Vec<&str> = briefing.iter().map(|n| n.id.as_str()).collect();
    assert!(ids.contains(&decision.id.as_str()), "{ids:?}");
    assert!(
        ids.contains(&incident.id.as_str()),
        "Incident governs paths: {ids:?}"
    );
    assert!(
        !ids.contains(&outage.id.as_str()),
        "Outage does not govern paths: {ids:?}"
    );
    assert!(
        !ids.contains(&proposed.id.as_str()),
        "proposed records are not briefed"
    );
    assert_eq!(briefing.len(), 2);
    assert_eq!(shipped_briefing.len(), 1);
    assert_eq!(shipped_briefing[0].id, plain.id);
}

/// E1.7a RE2-6 (Medium): the import update of an existing custom governed record decides the
/// governance move by the context's set, like the insert and `transition` do: a higher-rev
/// `proposed -> accepted` file of an `incidents:Incident` imports (no `NotGoverned`), while
/// a backwards `accepted -> proposed` file is still refused by the governance table.
#[test]
fn an_import_update_decides_the_governance_move_by_the_context_set() {
    use kaban_core::{Governance, SyncStore};
    // Arrange: a proposed Incident, then its file at rev 2 says accepted
    let (store, ctx) = incidents_store();
    let node = remember_with(&store, &ctx, "incidents:Incident", "the queue stalled");
    assert_eq!(node.governance, Governance::Proposed);
    let file = |rev: i64, governance: Governance| kaban_core::ImportedRecord {
        id: nid(&node.id),
        kind: "incidents:Incident".into(),
        text: "the queue stalled".into(),
        summary: None,
        tags: vec![],
        confidence: None,
        importance: None,
        lifecycle: kaban_core::Lifecycle::Active,
        governance,
        path_scope: vec![],
        props: serde_json::json!({}),
        rev,
        created_at: node.created_at.clone(),
        updated_at: node.updated_at.clone(),
        valid_from: None,
        valid_to: None,
        accepted_commit: None,
        agent_id: None,
        session_id: None,
        repo: None,
        commit_sha: None,
        dup_key: None,
        event: Some(kaban_core::NewEvent {
            reason: "reviewed".into(),
            signer_kind: kaban_core::SignerKind::HumanPr,
            ..Default::default()
        }),
    };

    // Act
    let accepted = store.import_record(&file(2, Governance::Accepted), &ctx);

    // Assert
    let accepted = accepted.expect("a custom governed kind follows the governance table");
    assert_eq!(accepted.governance, Governance::Accepted);
    assert_eq!(accepted.rev, 2);
    let events = store.events(&nid(&node.id)).unwrap();
    assert_eq!(events.last().map(|e| e.kind.as_str()), Some("accepted"));
    // Negative: backwards is the table's refusal, not a missing governance
    let err = store
        .import_record(&file(3, Governance::Proposed), &ctx)
        .unwrap_err();
    assert!(
        matches!(
            err,
            kaban_core::StoreError::InvalidTransition(
                kaban_core::InvalidTransition::Governance { .. }
            )
        ),
        "{err:?}"
    );
}
