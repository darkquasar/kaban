//! Reversible merges (engine-spec EN-F51 (3)-(5), (7); engine-plan E2.10 items 2-3): a `Match`
//! identity claim wins its group and writes the `node_identity` row, `recall` folds the merged
//! node onto its canonical once, a `Possible` is a `merge` candidate with no row, `unmerge`
//! restores reads and leaves both nodes, a human `preferred` distinction is
//! `Unresolved{identity_conflict}`, and a merge across workspaces is impossible by construction.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::{Scratch, created, ctx};
use kaban_core::{
    BatchAssertion, BatchSubject, ClaimClass, Fixed, IngestStore, Layer, MemoryStore, Method, Node,
    Rank, RecallQuery, WorkspaceId, WriteBatch,
};
use kaban_store_sqlite::SqliteStore;
use rusqlite::Connection;
use serde_json::json;

fn open(s: &Scratch) -> SqliteStore {
    SqliteStore::open(&s.db(), &s.keys()).unwrap()
}

fn raw(s: &Scratch) -> Connection {
    Connection::open(s.db()).unwrap()
}

fn rid(conn: &Connection, node: &Node) -> i64 {
    conn.query_row("SELECT rid FROM node WHERE id = ?1", [&node.id], |r| {
        r.get(0)
    })
    .unwrap()
}

/// An ER identity claim as the store writes it (`subject` the lesser rid's node, `object` the
/// greater's), submitted through the public batch surface.
fn identity_claim(a: &Node, b: &Node, is_match: bool, w: i64, raw_score: u32) -> BatchAssertion {
    BatchAssertion {
        subject: BatchSubject::Node(kaban_core::NodeId::parse(&a.id).unwrap()),
        claim_class: ClaimClass::Identity,
        predicate: None,
        object: Some(kaban_core::NodeId::parse(&b.id).unwrap()),
        value: json!({"match": is_match, "W": w, "contributions": [
            {"field": "title", "level": "agree", "w": 64919},
            {"field": "kind", "level": "agree", "w": 16630}
        ]}),
        layer: Layer::Reconcile,
        method: Method::Er,
        extractor_id: None,
        extractor_version: None,
        source_id: None,
        lineage_id: "reconcile:er".into(),
        raw_score: Fixed(raw_score),
        rank: Rank::Normal,
        rank_reason: None,
        calibration_version: 0,
        supersedes: None,
    }
}

fn submit(store: &SqliteStore, claims: Vec<BatchAssertion>) {
    let mut b = WriteBatch::new(WorkspaceId::default(), vec![]);
    b.assertions = claims;
    store.apply_batch(b, &ctx()).unwrap();
}

fn disposition(conn: &Connection, lineage: &str) -> Vec<String> {
    conn.prepare(
        "SELECT s.disposition FROM assertion a JOIN assertion_status s ON s.assertion_id = a.id
         WHERE a.lineage_id = ?1 ORDER BY a.id",
    )
    .unwrap()
    .query_map([lineage], |r| r.get(0))
    .unwrap()
    .collect::<rusqlite::Result<Vec<String>>>()
    .unwrap()
}

fn recall_ids(store: &SqliteStore, query: &str) -> Vec<String> {
    store
        .recall(&RecallQuery {
            query: query.into(),
            limit: 10,
            ..Default::default()
        })
        .unwrap()
        .into_iter()
        .map(|h| h.node.id)
        .collect()
}

/// Behavioural red at base (`v1.rs:220`: an `identity` claim is always `Candidate{merge}`, and
/// before that `missing_prior`): a `{match: true}` ER claim at full confidence wins its group
/// and leaves no `merge` review row on the pair.
#[test]
fn a_matching_identity_claim_wins_its_group_instead_of_queueing_a_merge() {
    let s = Scratch::new("e210-merge", "winner");
    let store = open(&s);
    let a = created(&store, "Fact", "quantum kettle warranty terms");
    let b = created(&store, "Fact", "quantum kettle warranty terms, second copy");
    submit(&store, vec![identity_claim(&a, &b, true, 81549, 10_000)]);
    let conn = raw(&s);
    assert_eq!(
        disposition(&conn, "reconcile:er"),
        vec!["winner".to_owned()]
    );
    let merges: i64 = conn
        .query_row(
            "SELECT count(*) FROM node_candidate WHERE reason = 'merge'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(merges, 0);
}

/// Behavioural red at base (no reader touches `node_identity`): with a row redirecting `b` to
/// `a`, a recall matching both returns the canonical node once and never `b`.
#[test]
fn recall_folds_a_redirected_node_onto_its_canonical_once() {
    let s = Scratch::new("e210-merge", "recall");
    let store = open(&s);
    let a = created(&store, "Fact", "velvet antenna calibration notes");
    let b = created(&store, "Fact", "velvet antenna calibration notes again");
    let conn = raw(&s);
    let (ra, rb) = (rid(&conn, &a), rid(&conn, &b));
    assert_eq!(
        recall_ids(&store, "velvet antenna").len(),
        2,
        "both nodes recall before the redirect"
    );
    conn.execute(
        "INSERT INTO node_identity(rid, canonical_rid, assertion_key, since) VALUES (?1, ?2, 'k', 0)",
        [rb, ra],
    )
    .unwrap();
    assert_eq!(recall_ids(&store, "velvet antenna"), vec![a.id.clone()]);
    // a purged canonical leaves `canonical_rid NULL` (persist.rs purge): no redirect, `b` is itself
    conn.execute(
        "UPDATE node_identity SET canonical_rid = NULL WHERE rid = ?1",
        [rb],
    )
    .unwrap();
    let mut ids = recall_ids(&store, "velvet antenna");
    ids.sort();
    let mut both = vec![a.id.clone(), b.id.clone()];
    both.sort();
    assert_eq!(ids, both);
}

fn identity_rows(conn: &Connection) -> Vec<(i64, Option<i64>)> {
    conn.prepare("SELECT rid, canonical_rid FROM node_identity ORDER BY rid")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
}

fn review_rows(conn: &Connection) -> Vec<(i64, String)> {
    conn.prepare("SELECT rid, reason FROM node_candidate ORDER BY rid")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
}

/// Two nodes of one kind whose normalised titles are equal block together; `Match` needs
/// `W >= 80000`: equal title (`64919`) and kind (`16630`) is `81549`.
fn twins(store: &SqliteStore, title: &str) -> (Node, Node) {
    let a = created(store, "Fact", &format!("{title}\nfirst body line"));
    let b = created(store, "Fact", &format!("{title}\nsecond body line"));
    (a, b)
}

/// Slice: `Match` (title and kind agree, `W = 81549`) writes the identity claim as the group's
/// winner, the `node_identity` row from the greater rid to the lesser in the same pass, and
/// `recall` returns the canonical once; the explanation carries the signed waterfall.
#[test]
fn a_match_writes_the_identity_row_and_recall_returns_the_canonical_once() {
    let s = Scratch::new("e210-merge", "match");
    let store = open(&s);
    let (a, b) = twins(&store, "Saffron ledger reconciliation");
    let report = store.er_resolve("default").unwrap();
    assert_eq!(
        (report.compared, report.matched, report.possible),
        (1, 1, 0)
    );
    let conn = raw(&s);
    let (ra, rb) = (rid(&conn, &a), rid(&conn, &b));
    assert_eq!(identity_rows(&conn), vec![(rb, Some(ra))]);
    assert_eq!(
        disposition(&conn, "reconcile:er"),
        vec!["winner".to_owned()]
    );
    assert_eq!(review_rows(&conn), vec![]);
    assert_eq!(recall_ids(&store, "saffron ledger"), vec![a.id.clone()]);
    // both nodes stay (EN-F51 (5): a merge never deletes a node)
    assert!(
        store
            .get(&kaban_core::NodeId::parse(&b.id).unwrap(), false)
            .unwrap()
            .is_some()
    );
    let key: String = conn
        .query_row(
            "SELECT key FROM assertion WHERE lineage_id = 'reconcile:er'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let explanation = store
        .explain_group(&store.claim_group(&key).unwrap())
        .unwrap();
    let waterfall: Vec<(String, String, String)> = explanation
        .er_contributions
        .unwrap()
        .into_iter()
        .map(|c| (c.field, c.level, c.w))
        .collect();
    assert_eq!(
        waterfall,
        vec![
            ("title".into(), "agree".into(), "6.4919".into()),
            ("aliases".into(), "missing".into(), "0.0000".into()),
            ("kind".into(), "agree".into(), "1.6630".into()),
            ("tags".into(), "missing".into(), "0.0000".into()),
            ("path_scope".into(), "missing".into(), "0.0000".into()),
            ("anchors".into(), "missing".into(), "0.0000".into()),
            ("summary".into(), "missing".into(), "0.0000".into()),
        ]
    );
    // a second pass compares nothing: the merged node is represented by its canonical
    let again = store.er_resolve("default").unwrap();
    assert_eq!(again.compared, 0);
}

/// Slice: `Possible` (title agrees, kind disagrees: `64919 - 38074 = 26845` is below `30000`,
/// so make the tags agree too: `52695`) is a `merge` review row on the lesser node and no
/// identity row; recall still returns both.
#[test]
fn a_possible_is_a_merge_candidate_with_no_identity_row() {
    let s = Scratch::new("e210-merge", "possible");
    let store = open(&s);
    let title = "Teal compass bearings";
    let a = store
        .remember(
            kaban_core::NewNode {
                kind: "Fact".into(),
                text: format!("{title}\none"),
                tags: vec!["nav".into()],
                ..Default::default()
            },
            &[],
            &ctx(),
        )
        .unwrap();
    let b = store
        .remember(
            kaban_core::NewNode {
                kind: "Decision".into(),
                text: format!("{title}\ntwo"),
                tags: vec!["nav".into()],
                ..Default::default()
            },
            &[],
            &ctx(),
        )
        .unwrap();
    let (kaban_core::RememberOutcome::Created(a), kaban_core::RememberOutcome::Created(b)) = (a, b)
    else {
        panic!("fresh store");
    };
    let report = store.er_resolve("default").unwrap();
    assert_eq!(
        (report.compared, report.matched, report.possible),
        (1, 0, 1)
    );
    let conn = raw(&s);
    let (ra, rb) = (rid(&conn, &a), rid(&conn, &b));
    assert_eq!(identity_rows(&conn), vec![]);
    assert_eq!(
        disposition(&conn, "reconcile:er"),
        vec!["candidate".to_owned()]
    );
    assert_eq!(review_rows(&conn), vec![(ra.min(rb), "merge".to_owned())]);
    let w: i64 = conn
        .query_row(
            "SELECT json_extract(value, '$.W') FROM assertion WHERE lineage_id = 'reconcile:er'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(w, 64919 - 38074 + 25850);
    // EN-F49: the node under review (the `merge` row's subject) never seeds a recall unless
    // asked; the other node of the pair is not redirected and returns as itself
    let other = if ra < rb { &b } else { &a };
    assert_eq!(recall_ids(&store, "teal compass"), vec![other.id.clone()]);
}

/// Slice: `unmerge <id>` of the merged node deletes the row, deprecates the identity claim
/// under the reason, re-decides the group and restores both reads; a second `unmerge` is
/// `NotFound`; a foreign workspace never sees the node.
#[test]
fn unmerge_restores_reads_and_leaves_both_nodes() {
    let s = Scratch::new("e210-merge", "unmerge");
    let store = open(&s);
    let (a, b) = twins(&store, "Umber tide tables");
    store.er_resolve("default").unwrap();
    assert_eq!(recall_ids(&store, "umber tide"), vec![a.id.clone()]);
    let b_id = kaban_core::NodeId::parse(&b.id).unwrap();
    assert!(matches!(
        store.unmerge("other", &b_id, "reviewed", &ctx()),
        Err(kaban_core::StoreError::NotFound(_))
    ));
    let outcome = store.unmerge("default", &b_id, "reviewed", &ctx()).unwrap();
    assert_eq!(outcome.canonical_id.as_deref(), Some(a.id.as_str()));
    assert_eq!(outcome.restored, vec![b.id.clone()]);
    assert_eq!(outcome.deprecated.len(), 1);
    let conn = raw(&s);
    assert_eq!(identity_rows(&conn), vec![]);
    let (rank, reason): (String, String) = conn
        .query_row(
            "SELECT rank, rank_reason FROM assertion WHERE key = ?1",
            [&outcome.deprecated[0]],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!((rank.as_str(), reason.as_str()), ("deprecated", "reviewed"));
    assert_ne!(
        disposition(&conn, "reconcile:er"),
        vec!["winner".to_owned()]
    );
    let mut ids = recall_ids(&store, "umber tide");
    ids.sort();
    let mut both = vec![a.id.clone(), b.id.clone()];
    both.sort();
    assert_eq!(ids, both);
    assert!(matches!(
        store.unmerge("default", &b_id, "again", &ctx()),
        Err(kaban_core::StoreError::NotFound(_))
    ));
}

/// Slice (EN-F51 (7)): a pair a human `preferred` claim keeps distinct is
/// `Unresolved{identity_conflict}` when the scorer says `Match`: no identity row, both recall.
#[test]
fn a_human_preferred_distinction_makes_a_match_an_identity_conflict() {
    let s = Scratch::new("e210-merge", "conflict");
    let store = open(&s);
    let (a, b) = twins(&store, "Cobalt harbour pilotage");
    let conn = raw(&s);
    let (ra, rb) = (rid(&conn, &a), rid(&conn, &b));
    let (lo, hi) = if ra < rb { (&a, &b) } else { (&b, &a) };
    let mut human = identity_claim(lo, hi, false, 0, 10_000);
    human.layer = Layer::Human;
    human.method = Method::Human;
    human.rank = Rank::Preferred;
    human.lineage_id = "human:cli".into();
    human.value = json!({"match": false});
    submit(&store, vec![human]);
    let report = store.er_resolve("default").unwrap();
    assert_eq!((report.matched, report.conflicts), (0, 1));
    assert_eq!(identity_rows(&conn), vec![]);
    let key: String = conn
        .query_row(
            "SELECT key FROM assertion WHERE lineage_id = 'reconcile:er'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let explanation = store
        .explain_group(&store.claim_group(&key).unwrap())
        .unwrap();
    assert_eq!(explanation.verdict, kaban_core::AssertionStatus::Unresolved);
    assert_eq!(
        explanation.unresolved_reason,
        Some(kaban_core::UnresolvedReason::IdentityConflict)
    );
    assert_eq!(recall_ids(&store, "cobalt harbour").len(), 2);
}

/// Negative (plan): blocks are per workspace, so two identical nodes in two workspaces are
/// never compared and no merge can join them.
#[test]
fn a_merge_across_workspaces_is_impossible_by_construction() {
    let s = Scratch::new("e210-merge", "workspaces");
    let store = open(&s);
    let title = "Ochre granary inventory";
    created(&store, "Fact", &format!("{title}\nhere"));
    store
        .remember(
            kaban_core::NewNode {
                kind: "Fact".into(),
                text: format!("{title}\nthere"),
                workspace_id: Some("other".into()),
                ..Default::default()
            },
            &[],
            &kaban_core::WriteCtx::new("pv", WorkspaceId::new("other"), kaban_core::Actor::Agent),
        )
        .unwrap();
    let here = store.er_resolve("default").unwrap();
    let there = store.er_resolve("other").unwrap();
    assert_eq!((here.compared, there.compared), (0, 0));
    assert_eq!(identity_rows(&raw(&s)), vec![]);
}

/// The group keys of every stored identity claim status row, distinct.
fn identity_groups(conn: &Connection) -> i64 {
    conn.query_row(
        "SELECT count(DISTINCT s.group_key) FROM assertion a JOIN assertion_status s ON s.assertion_id = a.id
         WHERE a.claim_class = 'identity'",
        [],
        |r| r.get(0),
    )
    .unwrap()
}

/// `(subject_rid, object_rid)` of every stored identity claim, in key order.
fn identity_endpoints(conn: &Connection) -> Vec<(i64, i64)> {
    conn.prepare(
        "SELECT subject_rid, object_rid FROM assertion WHERE claim_class = 'identity' ORDER BY key",
    )
    .unwrap()
    .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
    .unwrap()
    .collect::<rusqlite::Result<Vec<_>>>()
    .unwrap()
}

/// A human `preferred` "these are distinct" claim on the pair.
fn human_distinction(a: &Node, b: &Node) -> BatchAssertion {
    let mut human = identity_claim(a, b, false, 0, 10_000);
    human.layer = Layer::Human;
    human.method = Method::Human;
    human.rank = Rank::Preferred;
    human.lineage_id = "human:cli".into();
    human.value = json!({"match": false});
    human
}

/// BR-2 (E2.10b): an identity group is an unordered pair. Two claims about the same two nodes
/// submitted in opposite orders land in one group, and both are stored with the lesser rid as
/// the subject and the greater as the object, whatever the submission order.
#[test]
fn reversed_identity_submissions_land_in_one_normalised_group() {
    let s = Scratch::new("e210b-pairs", "reversed");
    let store = open(&s);
    let a = created(&store, "Fact", "amber sextant readings");
    let b = created(&store, "Fact", "amber sextant readings, copy");
    let mut reversed = identity_claim(&b, &a, true, 81549, 10_000);
    reversed.lineage_id = "reconcile:er-second-pass".into();
    submit(
        &store,
        vec![identity_claim(&a, &b, true, 81549, 10_000), reversed],
    );
    let conn = raw(&s);
    let (ra, rb) = (rid(&conn, &a), rid(&conn, &b));
    assert_eq!(
        identity_groups(&conn),
        1,
        "the pair is one group whatever the submission order"
    );
    assert_eq!(
        identity_endpoints(&conn),
        vec![(ra.min(rb), ra.max(rb)), (ra.min(rb), ra.max(rb))]
    );
}

/// BR-2 (E2.10b): A-B and A-C are two groups: a human `preferred` distinction about A-B is no
/// vote on A-C, whose `Match` wins its own group.
#[test]
fn a_distinction_about_one_pair_never_blocks_another_pair_sharing_an_endpoint() {
    let s = Scratch::new("e210b-pairs", "shared-endpoint");
    let store = open(&s);
    let a = created(&store, "Fact", "basalt kiln firing log");
    let b = created(&store, "Fact", "basalt kiln firing log, b");
    let c = created(&store, "Fact", "basalt kiln firing log, c");
    submit(&store, vec![human_distinction(&a, &b)]);
    submit(&store, vec![identity_claim(&a, &c, true, 81549, 10_000)]);
    let conn = raw(&s);
    assert_eq!(
        disposition(&conn, "reconcile:er"),
        vec!["winner".to_owned()],
        "the A-C match is decided in its own group"
    );
    assert_eq!(identity_groups(&conn), 2);
}

/// BR-3 (E2.10b): four twins are six `Match` pairs; the projection is one flat cluster: every
/// member points at the one canonical (the least rid), no chain, and `recall` returns one
/// representative.
#[test]
fn matching_pairs_form_one_flat_cluster_with_one_canonical() {
    let scratch = Scratch::new("e210b-cluster", "flat");
    let store = open(&scratch);
    let title = "Cerulean dockyard manifest";
    let a = created(&store, "Fact", &format!("{title}\none"));
    let b = created(&store, "Fact", &format!("{title}\ntwo"));
    let c = created(&store, "Fact", &format!("{title}\nthree"));
    let d = created(&store, "Fact", &format!("{title}\nfour"));
    let report = store.er_resolve("default").unwrap();
    assert_eq!((report.compared, report.matched), (6, 6));
    let conn = raw(&scratch);
    let (ra, rb, rc, rd) = (
        rid(&conn, &a),
        rid(&conn, &b),
        rid(&conn, &c),
        rid(&conn, &d),
    );
    assert_eq!(
        identity_rows(&conn),
        vec![(rb, Some(ra)), (rc, Some(ra)), (rd, Some(ra))],
        "every member points at the one canonical"
    );
    assert_eq!(recall_ids(&store, "cerulean dockyard"), vec![a.id.clone()]);
}

/// BR-3 (E2.10b, the bridge): a human `preferred` distinction between B and C blocks the join of
/// the components holding them, so A-B joins and A-C does not: B and C are never joined through
/// A. Both survivors recall.
#[test]
fn a_human_distinction_blocks_the_bridge_between_components() {
    let s = Scratch::new("e210b-cluster", "bridge");
    let store = open(&s);
    let title = "Damson orchard survey";
    let a = created(&store, "Fact", &format!("{title}\none"));
    let b = created(&store, "Fact", &format!("{title}\ntwo"));
    let c = created(&store, "Fact", &format!("{title}\nthree"));
    submit(&store, vec![human_distinction(&b, &c)]);
    let report = store.er_resolve("default").unwrap();
    let conn = raw(&s);
    let (ra, rb) = (rid(&conn, &a), rid(&conn, &b));
    assert_eq!(
        identity_rows(&conn),
        vec![(rb, Some(ra))],
        "the distinct pair is never joined through a third node"
    );
    assert_eq!(
        (report.compared, report.matched, report.conflicts),
        (3, 1, 2)
    );
    let mut ids = recall_ids(&store, "damson orchard");
    ids.sort();
    let mut survivors = vec![a.id.clone(), c.id.clone()];
    survivors.sort();
    assert_eq!(ids, survivors);
}

/// BR-4 (E2.10b): `unmerge` is durable: the next unchanged ER pass finds the same deprecated
/// claim, its group holds no eligible match, and no redirect is written.
#[test]
fn unmerge_is_durable_against_the_next_unchanged_pass() {
    let s = Scratch::new("e210b-verdict", "durable-unmerge");
    let store = open(&s);
    let (a, b) = twins(&store, "Ebony signal lamp");
    store.er_resolve("default").unwrap();
    let b_id = kaban_core::NodeId::parse(&b.id).unwrap();
    store.unmerge("default", &b_id, "reviewed", &ctx()).unwrap();
    let again = store.er_resolve("default").unwrap();
    let conn = raw(&s);
    assert_eq!(
        identity_rows(&conn),
        vec![],
        "a deprecated identity claim holds no redirect"
    );
    assert_eq!((again.compared, again.matched), (1, 0));
    let mut ids = recall_ids(&store, "ebony signal");
    ids.sort();
    let mut both = vec![a.id.clone(), b.id.clone()];
    both.sort();
    assert_eq!(ids, both);
}

/// BR-4 (E2.10b): the redirect is a consequence of the final verdict through the shared
/// projection, so an identity claim submitted through the batch surface reproduces ER's
/// physical effect, and `unmerge` reverses it the same way.
#[test]
fn a_batch_submitted_identity_claim_reproduces_the_physical_merge() {
    let s = Scratch::new("e210b-verdict", "batch");
    let store = open(&s);
    let a = created(&store, "Fact", "fennel brass gauge");
    let b = created(&store, "Fact", "fennel brass gauge, again");
    submit(&store, vec![identity_claim(&a, &b, true, 81549, 10_000)]);
    let conn = raw(&s);
    let (ra, rb) = (rid(&conn, &a), rid(&conn, &b));
    assert_eq!(
        identity_rows(&conn),
        vec![(rb, Some(ra))],
        "a batch match writes the redirect the ER pass writes"
    );
    assert_eq!(recall_ids(&store, "fennel brass"), vec![a.id.clone()]);
    let b_id = kaban_core::NodeId::parse(&b.id).unwrap();
    let outcome = store.unmerge("default", &b_id, "reviewed", &ctx()).unwrap();
    assert_eq!(outcome.canonical_id.as_deref(), Some(a.id.as_str()));
    assert_eq!(identity_rows(&conn), vec![]);
}

/// BR-5 (E2.10b): a visible alias never exposes a canonical that is still a candidate
/// (EN-F49): the exclusion is re-applied to the resolved canonical; `include_candidates`
/// shows it, once.
#[test]
fn a_visible_alias_never_exposes_a_candidate_canonical() {
    let s = Scratch::new("e210b-recall", "candidate-canonical");
    let store = open(&s);
    let a = created(&store, "Fact", "garnet tiller notes");
    let b = created(&store, "Fact", "garnet tiller notes, alias");
    let conn = raw(&s);
    let (ra, rb) = (rid(&conn, &a), rid(&conn, &b));
    conn.execute(
        "INSERT INTO node_identity(rid, canonical_rid, assertion_key, since) VALUES (?1, ?2, 'k', 0)",
        [rb, ra],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO node_candidate(rid, workspace_id, reason, group_key, since) VALUES (?1, 'default', 'below_margin', 'g', 0)",
        [ra],
    )
    .unwrap();
    assert_eq!(
        recall_ids(&store, "garnet tiller"),
        Vec::<String>::new(),
        "the alias does not expose its candidate canonical"
    );
    let shown: Vec<String> = store
        .recall(&RecallQuery {
            query: "garnet tiller".into(),
            limit: 10,
            filters: kaban_core::ReadFilters {
                include_candidates: true,
            },
            ..Default::default()
        })
        .unwrap()
        .into_iter()
        .map(|h| h.node.id)
        .collect();
    assert_eq!(shown, vec![a.id.clone()]);
}
