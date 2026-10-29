//! Trusted adapter actors, tenant binding, closed inputs and policy precedence.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use kaban_api::{KabanApi, PruningProtectionRequest, RememberRequest};
use kaban_core::Actor;
use kaban_policy::Policy;
use kaban_store_sqlite::SqliteStore;
use serde_json::{Value, json};
use std::sync::Arc;

fn fixture(policy: Policy) -> (KabanApi, String) {
    let store = Arc::new(SqliteStore::open_memory().unwrap());
    let api = KabanApi::new(store.clone(), policy, store.signer(), None);
    let response = api
        .remember(
            serde_json::from_value::<RememberRequest>(json!({"kind":"Fact","text":"invented"}))
                .unwrap(),
        )
        .unwrap();
    let value = serde_json::to_value(response).unwrap();
    (api, value["id"].as_str().unwrap().into())
}
fn request(op: &str, id: &str) -> PruningProtectionRequest {
    serde_json::from_value(json!({"operation":op,"target_kind":"node","id":id})).unwrap()
}

#[test]
fn human_and_agent_can_get_set_clear_but_injected_authority_is_refused() {
    let (api, id) = fixture(Policy::empty());
    for actor in [Actor::HumanCli, Actor::Agent] {
        assert!(
            !api.pruning_protection(request("get", &id), actor.clone())
                .unwrap()
                .explicit
        );
        assert!(
            api.pruning_protection(request("set", &id), actor.clone())
                .unwrap()
                .explicit
        );
        assert!(
            !api.pruning_protection(request("clear", &id), actor)
                .unwrap()
                .effective
        );
    }
    for field in ["actor", "workspace_id"] {
        let mut raw = json!({"operation":"set","target_kind":"node","id":id});
        raw[field] = Value::String("foreign".into());
        assert!(serde_json::from_value::<PruningProtectionRequest>(raw).is_err());
    }
    assert!(
        !api.pruning_protection(request("get", &id), Actor::Agent)
            .unwrap()
            .explicit
    );
    assert!(
        api.pruning_protection(
            request("set", kaban_core::NodeId::new().as_str()),
            Actor::Agent
        )
        .is_err()
    );
}

#[test]
fn relationship_content_and_closed_values_obey_policy_gate() {
    let (api, id) = fixture(Policy::empty());
    let raw = json!({"operation":"set","target_kind":"relationship","src":id,"dst":id,"rel":"AKIAQWERTYUIOPASDFGH"});
    assert!(api.prepare_args("pruning_protection", &raw).is_err());
    assert!(
        api.pruning_protection(serde_json::from_value(raw).unwrap(), Actor::Agent)
            .is_err()
    );
    assert!(
        api.prepare_args(
            "pruning_protection",
            &json!({"operation":"AKIAQWERTYUIOPASDFGH"})
        )
        .is_err()
    );
    let response = json!({"operation":"set","target_kind":"node","explicit":true,"effective":true});
    assert_eq!(
        api.gate_response("pruning_protection", &response).unwrap(),
        response
    );
}

#[test]
fn foreign_api_and_redacted_relationship_identity_cannot_mutate_marks() {
    use kaban_api::{ApiConfig, ApiError};
    use kaban_core::{
        GovernanceStore, MemoryStore, NewNode, NodeId, PruningTarget, WorkspaceId, WriteCtx,
    };
    let store = Arc::new(SqliteStore::open_memory().unwrap());
    let ctx = WriteCtx::new("v1", WorkspaceId::default(), Actor::Agent);
    let outcome = store
        .remember(
            NewNode {
                kind: "Fact".into(),
                text: "invented foreign target".into(),
                ..Default::default()
            },
            &[],
            &ctx,
        )
        .unwrap();
    assert!(matches!(outcome, kaban_core::RememberOutcome::Created(_)));
    let node = match outcome {
        kaban_core::RememberOutcome::Created(n)
        | kaban_core::RememberOutcome::Existing { node: n, .. } => n,
    };
    let id = NodeId::parse(&node.id).unwrap();
    let target = PruningTarget::Node(id.clone());
    store.pruning_protection_set(&target, &ctx).unwrap();
    let foreign = KabanApi::with_config(
        store.clone(),
        Policy::empty(),
        store.signer(),
        None,
        ApiConfig {
            workspace: WorkspaceId::new("foreign"),
            ..Default::default()
        },
    );
    for operation in ["get", "set", "clear"] {
        assert!(matches!(
            foreign.pruning_protection(request(operation, id.as_str()), Actor::Agent),
            Err(ApiError::NotFound(_))
        ));
    }
    let policy = Policy::from_file(
        Policy::parse(b"words:\n  - { term: hunter2, action: redact, id: protected-word }\n")
            .unwrap(),
        "redact".into(),
    )
    .unwrap();
    let api = KabanApi::new(store.clone(), policy, store.signer(), None);
    let raw =
        json!({"operation":"set","target_kind":"relationship","src":id,"dst":id,"rel":"hunter2"});
    assert!(
        matches!(api.pruning_protection(serde_json::from_value(raw).unwrap(),Actor::Agent),Err(ApiError::Invalid(reason)) if reason=="pruning_identity_redacted")
    );
    assert!(
        store
            .pruning_protection_get(&target, &ctx)
            .unwrap()
            .explicit
    );
}
