//! EN-F13 promotion out of `proposed` (engine-plan E2.5b, then E2.5g; security or data path,
//! the reds kept as `e25b-red-promotion.log` and `e25g-red-agent.log`): the record the
//! reconciler materialised for a span winner without a target is promoted by `kaban accept
//! <id>` from a human, and (OWN-84 (1), E2.5g) by an agent too when the kind's acceptance tier
//! is `agent` (the default: `infra:Hardware` declares none), the agent's acceptance recorded
//! with `signer_kind: agent` and its signer. Compiles against the base: at the E2.5b base the
//! §4 table refused the human with `NotGoverned`; at the E2.5g base the agent is refused
//! `human_only` (the behavioural reds).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

#[path = "common/status.rs"]
mod status;

use kaban_api::{AcceptRequest, AcceptResponse, Caller};
use kaban_core::{Actor, Governance, GovernanceStore, MemoryStore, NodeId, SignerKind};
use status::{Scratch, human, items};

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

/// EN-F13 promotion (E2.5b, the stop E2.5 carried): the `proposed` record the reconciler
/// materialised (`props.proposed_by = kaban-reconcile`, kind `infra:Hardware`, ungoverned)
/// is accepted by a human `kaban accept <id>` with its event. Red at the E2.5b base: the §4
/// table refuses the human with `NotGoverned`.
#[test]
fn a_human_promotes_the_proposed_record_of_a_span_winner() {
    let s = Scratch::new("promotion");
    let it = items(&s);
    let id = proposed_record(&s);
    let accepted = it.api.accept(request(&id), &human());
    let node = match accepted {
        Ok(AcceptResponse::Accepted { node, .. }) => node,
        other => panic!("a human acceptance of the proposed record: {other:?}"),
    };
    assert_eq!(node.governance, Governance::Accepted);
    let stored = it
        .store
        .get(&NodeId::parse(&id).unwrap(), false)
        .unwrap()
        .unwrap();
    assert_eq!(stored.governance, Governance::Accepted);
    let events = GovernanceStore::events(&*it.store, &NodeId::parse(&id).unwrap()).unwrap();
    assert!(events.iter().any(|e| e.kind == "accepted"), "{events:?}");
}

/// OWN-84 (1) (E2.5g): the same `proposed` record of a kind at the `agent` tier (the default
/// when no `acceptance` is declared) is promoted by an agent through the `accept` tool: the
/// record moves to `accepted`, its row is released, and the one `events` row carries the
/// tier (`signer_kind: agent`), the request's `signer` and the client name as `host`. Red at
/// the E2.5g base: the agent is refused `human_only`.
#[test]
fn an_agent_promotes_the_proposed_record_of_a_span_winner_at_the_agent_tier() {
    let s = Scratch::new("promotion-agent");
    let it = items(&s);
    let id = proposed_record(&s);
    let node_id = NodeId::parse(&id).unwrap();
    let agent = Caller {
        kind: Actor::Agent,
        host: Some("mcp-client".into()),
        session: Some("sess-9".into()),
    };
    let mut req = request(&id);
    req.signer = Some("agent-7".into());
    let accepted = it.api.accept(req, &agent);
    let (node, signer_kind) = match accepted {
        Ok(AcceptResponse::Accepted {
            node, signer_kind, ..
        }) => (node, signer_kind),
        other => panic!("an agent acceptance of an agent-tier proposed record: {other:?}"),
    };
    assert_eq!(signer_kind, SignerKind::Agent);
    assert_eq!(node.governance, Governance::Accepted);
    let stored = it.store.get(&node_id, false).unwrap().unwrap();
    assert_eq!(stored.governance, Governance::Accepted);
    assert!(
        !it.store.is_candidate(&node_id).unwrap(),
        "the proposed row is released by the acceptance"
    );
    let events = GovernanceStore::events(&*it.store, &node_id).unwrap();
    let accepted: Vec<_> = events.iter().filter(|e| e.kind == "accepted").collect();
    assert_eq!(accepted.len(), 1, "{events:?}");
    assert_eq!(accepted[0].signer_kind, SignerKind::Agent);
    assert_eq!(accepted[0].signer.as_deref(), Some("agent-7"));
    assert_eq!(accepted[0].host.as_deref(), Some("mcp-client"));
    assert_eq!(accepted[0].reason, "reviewed");
}
