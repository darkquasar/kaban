//! OWN-84 (1): the F4 acceptance tier on the surfaces the ontology does not govern (engine-plan
//! E2.5g item 3; security or data path, the staged red kept as `e25g-red-human-tier.log`: with
//! the two `acceptance` keys parsed but not yet folded into the governed set, the agent is
//! accepted on both records). A kind declaring `acceptance: human` and a `discovered` block
//! declaring the same refuse an agent `human_only` on the EN-F13 `proposed` record and on the
//! EN-F5a `x:` candidate, never with a token; a human accepts both. Also here (the lane's one
//! new test file): default (c), an agent acceptance writes no EN-F12c record claim (red kept as
//! `e25g2-red-claim.log`), and OWN-84 (2) `auto_accept_when` (item 6; staged red kept as
//! `e25g2-red-auto.log`: the key parsed and folded into the governed set, placement not yet
//! reading it).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

#[path = "common/status.rs"]
mod status;

use kaban_api::{AcceptRequest, AcceptResponse, ApiError, Caller, KabanApi, RememberRequest};
use kaban_core::{
    Actor, Governance, GovernanceStore, MemoryStore, NamespaceSet, NodeId, OntologyFile,
    StoreError, resolve,
};
use kaban_store_sqlite::SqliteStore;
use status::{Scratch, api_over, fixture, human, submit};
use std::sync::Arc;

/// The EN-F13 `infra` ontology with both tiers at `human`: the declared `Hardware` kind by its
/// own key, undeclared labels by the `discovered` block's.
const HUMAN_TIER: &str = "ontology: infra
version: 1
rels: {}
kinds:
  Vendor: {description: vendor, min_confidence: 0.6}
  Team: {description: team, min_confidence: 0.6}
  Hardware: {description: hardware, min_confidence: 0.6, acceptance: human}
discovered: {policy: review, cap: 500, prune_after_days: 90, promote_when: {min_count: 20, min_sources: 3, min_mean_score: 0.7}, acceptance: human}
";

/// A store reconciling against the shipped files plus `yaml`, and the service whose registry
/// loaded the same file (the governed set every `accept` decides by).
fn service(s: &Scratch, yaml: &str) -> (Arc<SqliteStore>, KabanApi) {
    let mut files = kaban_core::shipped::files().unwrap();
    files.push(OntologyFile::from_yaml(yaml).unwrap());
    let ontology = resolve(&files, &NamespaceSet::new()).unwrap();
    let store = Arc::new(
        SqliteStore::open(&s.db(), &s.keys())
            .unwrap()
            .with_ontology(Arc::new(ontology))
            .unwrap(),
    );
    let registry = Arc::new(kaban_api::OntologyRegistry::new(Some(s.0.clone())).unwrap());
    let api = api_over(&store).with_registry(registry);
    let path = s.0.join("infra.yaml");
    std::fs::write(&path, yaml).unwrap();
    api.registry().load_file(&api, &path).unwrap();
    (store, api)
}

/// The proposed `infra:Hardware` record of the forward fixture (span 27..40, no target).
fn proposed_record(s: &Scratch) -> String {
    let conn = rusqlite::Connection::open(s.db()).unwrap();
    conn.query_row(
        "SELECT id FROM node WHERE kind = 'infra:Hardware' AND governance = 'proposed'",
        [],
        |r| r.get(0),
    )
    .unwrap()
}

fn request(id: &str) -> AcceptRequest {
    AcceptRequest {
        id: id.into(),
        reason: Some("reviewed".into()),
        path_scope: vec!["**".into()],
        ..Default::default()
    }
}

fn agent() -> Caller {
    Caller {
        kind: Actor::Agent,
        host: Some("mcp-client".into()),
        session: None,
    }
}

/// Both records of a fresh fixture: the reconciler's `proposed` `infra:Hardware` record and an
/// `x:Widget` candidate remembered under the `review` policy.
fn both_records(s: &Scratch) -> (Arc<SqliteStore>, KabanApi, String, String) {
    let fx = fixture();
    let (store, api) = service(s, HUMAN_TIER);
    let chunk = NodeId::new();
    submit(
        &store,
        &fx,
        &chunk,
        &["a1", "a2", "a3", "a4", "a5", "a6", "a7"],
        vec![],
    );
    let proposed = proposed_record(s);
    let candidate = api
        .remember(RememberRequest {
            kind: "Widget".into(),
            text: "a widget nobody declared".into(),
            ..Default::default()
        })
        .unwrap()
        .id;
    assert!(
        store
            .is_candidate(&NodeId::parse(&candidate).unwrap())
            .unwrap(),
        "the undeclared kind is a review candidate"
    );
    (store, api, proposed, candidate)
}

/// The property: at the `human` tier an agent is refused `human_only` on the proposed record
/// (the kind's own key) and on the `x:` candidate (the `discovered` block's key); neither
/// record moves and no token is issued. Red at the staged base: both are accepted.
#[test]
fn an_agent_is_refused_human_only_where_the_ontology_declares_the_human_tier() {
    let s = Scratch::new("human-tier-agent");
    let (store, api, proposed, candidate) = both_records(&s);
    for (id, what) in [(&proposed, "proposed record"), (&candidate, "x: candidate")] {
        let refused = api.accept(request(id), &agent());
        assert!(
            matches!(refused, Err(ApiError::Store(StoreError::HumanOnly(_)))),
            "an agent acceptance of the {what} at the human tier: {refused:?}"
        );
    }
    let node = store
        .get(&NodeId::parse(&proposed).unwrap(), true)
        .unwrap()
        .unwrap();
    assert_eq!(
        node.governance,
        Governance::Proposed,
        "the record did not move"
    );
    assert!(
        store
            .is_candidate(&NodeId::parse(&candidate).unwrap())
            .unwrap(),
        "the candidate row stands"
    );
    let conn = rusqlite::Connection::open(s.db()).unwrap();
    let tokens: i64 = conn
        .query_row("SELECT count(*) FROM confirmations", [], |r| r.get(0))
        .unwrap();
    assert_eq!(tokens, 0, "no handshake token for a human-tier candidate");
}

/// The negative of the tier: a human accepts both records under the same ontology.
#[test]
fn a_human_accepts_both_records_at_the_human_tier() {
    let s = Scratch::new("human-tier-human");
    let (store, api, proposed, candidate) = both_records(&s);
    for id in [&proposed, &candidate] {
        let accepted = api.accept(request(id), &human());
        assert!(
            matches!(accepted, Ok(AcceptResponse::Accepted { .. })),
            "{accepted:?}"
        );
    }
    let node = store
        .get(&NodeId::parse(&proposed).unwrap(), false)
        .unwrap()
        .unwrap();
    assert_eq!(node.governance, Governance::Accepted);
    assert!(
        !store
            .is_candidate(&NodeId::parse(&candidate).unwrap())
            .unwrap(),
        "the candidate row is released"
    );
}

/// The EN-F13 `infra` ontology as the fixture ships it (no tier declared: `agent`).
fn agent_tier_yaml() -> String {
    fixture()["ontology"].as_str().unwrap().to_owned()
}

/// The fixture ontology plus a `discovered` block carrying `auto_accept_when`.
fn auto_accept_yaml(min_count: u32, min_mean_score: f64) -> String {
    format!(
        "{}discovered: {{policy: review, cap: 500, prune_after_days: 90, promote_when: {{min_count: 20, min_sources: 3, min_mean_score: 0.7}}, auto_accept_when: {{min_count: {min_count}, min_sources: 1, min_mean_score: {min_mean_score}}}}}\n",
        agent_tier_yaml()
    )
}

/// The `kind_of_record` claims in the store (the EN-F12c acceptance claim is one).
fn record_claims(s: &Scratch) -> i64 {
    let conn = rusqlite::Connection::open(s.db()).unwrap();
    conn.query_row(
        "SELECT count(*) FROM assertion WHERE claim_class = 'kind_of_record'",
        [],
        |r| r.get(0),
    )
    .unwrap()
}

fn submit_forward(store: &Arc<SqliteStore>) {
    let fx = fixture();
    submit(
        store,
        &fx,
        &NodeId::new(),
        &["a1", "a2", "a3", "a4", "a5", "a6", "a7"],
        vec![],
    );
}

/// Default (c) (OWN-84 (1), EN-F12c): an agent's acceptance at the agent tier writes the event
/// and no `preferred` human record claim, so protection stays human-only and `kaban forget
/// <id>` is the human's revert; a human's acceptance writes the one claim. Red at the base:
/// the agent's acceptance writes the claim too (`human:agent`).
#[test]
fn an_agent_acceptance_writes_no_record_claim_and_a_human_acceptance_writes_one() {
    let s = Scratch::new("claim-agent");
    let (store, api) = service(&s, &agent_tier_yaml());
    submit_forward(&store);
    let id = proposed_record(&s);
    let before = record_claims(&s);
    let accepted = api.accept(request(&id), &agent());
    assert!(
        matches!(accepted, Ok(AcceptResponse::Accepted { .. })),
        "{accepted:?}"
    );
    assert_eq!(
        record_claims(&s),
        before,
        "an agent acceptance leaves no kind_of_record claim"
    );
    let s2 = Scratch::new("claim-human");
    let (store2, api2) = service(&s2, &agent_tier_yaml());
    submit_forward(&store2);
    let id2 = proposed_record(&s2);
    let before2 = record_claims(&s2);
    api2.accept(request(&id2), &human()).unwrap();
    assert_eq!(record_claims(&s2), before2 + 1, "the human's claim");
}

/// The `HUMAN_TIER` ontology (both tiers `human`) plus `auto_accept_when` on its `discovered`
/// block: the human half of the RW3-2 cross-product.
fn human_tier_auto_accept_yaml(min_count: u32, min_mean_score: f64) -> String {
    HUMAN_TIER.replace(
        "min_mean_score: 0.7}, acceptance: human}\n",
        &format!(
            "min_mean_score: 0.7}}, acceptance: human, auto_accept_when: {{min_count: {min_count}, min_sources: 1, min_mean_score: {min_mean_score}}}}}\n"
        ),
    )
}

/// Remember one `x:Widget` record through the service (the `review` policy's candidate).
fn remember(api: &KabanApi, text: &str) -> NodeId {
    NodeId::parse(
        &api.remember(RememberRequest {
            kind: "Widget".into(),
            text: text.into(),
            ..Default::default()
        })
        .unwrap()
        .id,
    )
    .unwrap()
}

/// Set `x:Widget`'s `kind_stats.mean_score` as the E1.8 bridge would (an `x:` label with no
/// reconciled edges never gets one on its own).
fn bridge(s: &Scratch) {
    let conn = rusqlite::Connection::open(s.db()).unwrap();
    conn.execute(
        "UPDATE kind_stats SET mean_score = 0.9 WHERE ns = 'x' AND kind = 'Widget'",
        [],
    )
    .unwrap();
}

/// The declared kind's `kind_stats` row passing every `auto_accept_when` of these tests (a
/// declared kind has a row only after a promotion: `touch_stats` refreshes, never creates).
fn stats(s: &Scratch) {
    let conn = rusqlite::Connection::open(s.db()).unwrap();
    conn.execute(
        "INSERT INTO kind_stats(workspace_id, ns, kind, n_nodes, n_sources, mean_score, first_seen, last_seen)
         VALUES ('default', 'infra', 'Hardware', 5, 3, 0.9, unixepoch(), unixepoch())",
        [],
    )
    .unwrap();
}

/// The one `infra:Hardware` record: its id and governance column.
fn hardware(s: &Scratch) -> (String, String) {
    let conn = rusqlite::Connection::open(s.db()).unwrap();
    conn.query_row(
        "SELECT id, governance FROM node WHERE kind = 'infra:Hardware'",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .unwrap()
}

/// RW3-2 (OWN-84 (1) over (2), the human half of the tier x thresholds cross-product): a
/// `human` tier is never cleared by `auto_accept_when`. Under the `discovered` block declaring
/// `acceptance: human`, the second `x:Widget` whose row passes every threshold still takes a
/// `node_candidate` row (and so does one whose row falls short); under `Hardware: {acceptance:
/// human}` the reconciler's record stays `proposed` with no event although its row and the
/// span verdict pass. The agent half is the two tests above. Red at the base: the passing
/// label is placed without a row and the passing record is written `accepted` by a count.
#[test]
fn auto_accept_when_never_clears_a_human_tier() {
    // the candidate half, thresholds passing
    let s = Scratch::new("auto-human-candidate");
    let (store, api) = service(&s, &human_tier_auto_accept_yaml(2, 0.5));
    let first = remember(&api, "a widget nobody declared");
    assert!(store.is_candidate(&first).unwrap(), "count 1: a candidate");
    bridge(&s);
    let second = remember(&api, "another widget nobody declared");
    assert!(
        store.is_candidate(&second).unwrap(),
        "human tier: a passing row never clears the queue item"
    );
    // the candidate half, thresholds short: a candidate as before
    let s2 = Scratch::new("auto-human-candidate-short");
    let (store2, api2) = service(&s2, &human_tier_auto_accept_yaml(3, 0.5));
    let _ = remember(&api2, "a widget nobody declared");
    bridge(&s2);
    let second2 = remember(&api2, "another widget nobody declared");
    assert!(
        store2.is_candidate(&second2).unwrap(),
        "human tier, short: a candidate"
    );
    // the proposed half, thresholds passing: `proposed`, no event, no claim
    let s3 = Scratch::new("auto-human-proposed");
    let (store3, _api3) = service(&s3, &human_tier_auto_accept_yaml(2, 0.5));
    stats(&s3);
    submit_forward(&store3);
    let (id, governance) = hardware(&s3);
    assert_eq!(
        governance, "proposed",
        "human tier: a passing row never writes the record accepted"
    );
    assert_eq!(
        GovernanceStore::events(&*store3, &NodeId::parse(&id).unwrap()).unwrap(),
        Vec::<kaban_core::Event>::new(),
        "no system event at the human tier"
    );
    assert_eq!(record_claims(&s3), 0, "no EN-F12c claim");
    // the proposed half, thresholds short
    let s4 = Scratch::new("auto-human-proposed-short");
    let (store4, _api4) = service(&s4, &human_tier_auto_accept_yaml(2, 0.95));
    stats(&s4);
    submit_forward(&store4);
    assert_eq!(hardware(&s4).1, "proposed", "human tier, short: proposed");
}

/// OWN-84 (2), the candidate half: under `auto_accept_when {min_count: 2, min_sources: 1,
/// min_mean_score: 0.5}` the second `x:Widget` record arrives with a `kind_stats` row that
/// passes all three (`mean_score` set as the reconciler's bridge would) and is placed with no
/// `node_candidate` row, governance `none`; the first (count 1) was a candidate. Negative:
/// `min_count: 3` keeps the second a candidate. Staged red: the second is a candidate too.
#[test]
fn a_discovered_label_passing_auto_accept_when_gets_no_queue_item() {
    // the property
    let s = Scratch::new("auto-candidate");
    let (store, api) = service(&s, &auto_accept_yaml(2, 0.5));
    let first = remember(&api, "a widget nobody declared");
    assert!(store.is_candidate(&first).unwrap(), "count 1: a candidate");
    bridge(&s);
    let second = remember(&api, "another widget nobody declared");
    assert!(
        !store.is_candidate(&second).unwrap(),
        "count 2, one source, mean 0.9: accepted without a queue item"
    );
    let node = store.get(&second, false).unwrap().unwrap();
    assert_eq!(node.governance, Governance::None, "a released candidate");
    // the negative: one threshold short
    let s2 = Scratch::new("auto-candidate-short");
    let (store2, api2) = service(&s2, &auto_accept_yaml(3, 0.5));
    let _ = remember(&api2, "a widget nobody declared");
    bridge(&s2);
    let second2 = remember(&api2, "another widget nobody declared");
    assert!(
        store2.is_candidate(&second2).unwrap(),
        "min_count 3 short: today's row"
    );
}

/// OWN-84 (2), the proposed half: when the kind's `kind_stats` row (`mean_score` 0.9) passes
/// `auto_accept_when` and the span verdict's confidence (`P = 1` for the single-candidate
/// Hardware span, EN-F13) is at or above `min_mean_score`, the reconciler writes the EN-F13
/// record `accepted` with one `system` event (`kaban-auto-accept`, `auto_accept_when`,
/// `["**"]`) and no EN-F12c claim. Negative: `min_mean_score: 0.95` is above the row's
/// mean, so today's `proposed` record and no event. Staged red: the record is `proposed`.
#[test]
fn a_proposed_record_passing_auto_accept_when_is_written_accepted_with_a_system_event() {
    // the property
    let s = Scratch::new("auto-proposed");
    let (store, _api) = service(&s, &auto_accept_yaml(2, 0.5));
    stats(&s);
    submit_forward(&store);
    let (id, governance) = hardware(&s);
    assert_eq!(governance, "accepted", "the record is written accepted");
    let events = GovernanceStore::events(&*store, &NodeId::parse(&id).unwrap()).unwrap();
    assert_eq!(events.len(), 1, "{events:?}");
    let ev = &events[0];
    assert_eq!(ev.kind, "accepted");
    assert_eq!(ev.signer_kind, kaban_core::SignerKind::System);
    assert_eq!(ev.signer.as_deref(), Some("kaban-auto-accept"));
    assert_eq!(ev.reason, "auto_accept_when");
    assert_eq!(ev.path_scope, vec!["**".to_owned()]);
    assert_eq!(record_claims(&s), 0, "no EN-F12c claim");
    // the negative: the row's mean_score (0.9) is under min_mean_score 0.95
    let s2 = Scratch::new("auto-proposed-short");
    let (store2, _api2) = service(&s2, &auto_accept_yaml(2, 0.95));
    stats(&s2);
    submit_forward(&store2);
    let (id2, governance2) = hardware(&s2);
    assert_eq!(governance2, "proposed", "today's record");
    assert_eq!(
        GovernanceStore::events(&*store2, &NodeId::parse(&id2).unwrap()).unwrap(),
        Vec::<kaban_core::Event>::new(),
        "no event for today's record"
    );
}
