//! Public-behaviour tests of the denylist gate through `KabanApi` (core-plan C1.2, S1).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

use kaban_api::{ApiError, KabanApi, RememberRequest};
use kaban_policy::Policy;
use kaban_store_sqlite::SqliteStore;
use serde_json::{Value, json};
use std::sync::Arc;

fn api_with(policy_yaml: &str) -> KabanApi {
    let file = Policy::parse(policy_yaml.as_bytes()).unwrap();
    let policy = Policy::from_file(file, "test".into()).unwrap();
    let store = Arc::new(SqliteStore::open_memory().unwrap());
    let signer = store.signer();
    KabanApi::new(store, policy, signer, None)
}

fn remember_props(api: &KabanApi, props: Value) -> Result<(), ApiError> {
    api.remember(RememberRequest {
        kind: "Fact".into(),
        text: "harmless note".into(),
        props: Some(props),
        ..Default::default()
    })
    .map(|_| ())
}

fn reject_rules(r: Result<(), ApiError>) -> Vec<String> {
    match r {
        Err(ApiError::Rejected { rules, .. }) => rules,
        other => panic!("expected a denylist rejection, got {other:?}"),
    }
}

#[test]
fn numeric_password_leaf_in_props_is_rejected() {
    let api = api_with("");
    let rules = reject_rules(remember_props(&api, json!({"password": 123_456_789})));
    assert!(
        rules.iter().any(|r| r == "generic-credential-assignment"),
        "rules: {rules:?}"
    );
}

#[test]
fn array_of_short_values_under_a_credential_key_is_rejected() {
    let api = api_with("");
    let rules = reject_rules(remember_props(&api, json!({"secret": ["a", "b"]})));
    assert!(
        rules.iter().any(|r| r == "credential-key"),
        "rules: {rules:?}"
    );
    // a credential-named key with a placeholder value is not a credential
    assert!(remember_props(&api, json!({"secret": "<your-secret>"})).is_ok());
    assert!(remember_props(&api, json!({"token": "none"})).is_ok());
    // credential-shaped *suffix* keys are prose, not assignments
    assert!(remember_props(&api, json!({"password_required": true})).is_ok());
}

#[test]
fn field_cap_applies_per_field_not_to_the_joined_view() {
    let api = api_with("");
    let word = "lesson ";
    let text: String = word.repeat(15_000 / word.len() + 1); // ~15k chars
    let summary: String = word.repeat(6_000 / word.len() + 1); // ~6k chars; joined > 20k
    let r = api.remember(RememberRequest {
        kind: "Fact".into(),
        text: text.clone(),
        summary: Some(summary),
        ..Default::default()
    });
    assert!(
        r.is_ok(),
        "joined view must not trip the per-field cap: {r:?}"
    );
    // a single field over the cap is still rejected
    let too_long = word.repeat(20_001 / word.len() + 1);
    let rules = reject_rules(
        api.remember(RememberRequest {
            kind: "Fact".into(),
            text: too_long,
            ..Default::default()
        })
        .map(|_| ()),
    );
    assert_eq!(rules, vec!["input-too-long".to_string()]);
    // and a secret split across the joined window boundary is still caught
    let mut t = word.repeat(19_990 / word.len());
    t.push_str("password=");
    let rules = reject_rules(
        api.remember(RememberRequest {
            kind: "Fact".into(),
            text: t,
            summary: Some("Tr0ub4dor&3".into()),
            ..Default::default()
        })
        .map(|_| ()),
    );
    assert!(!rules.is_empty(), "{rules:?}");
}

#[test]
fn props_keys_that_redact_to_the_same_name_are_rejected() {
    let api = api_with("words:\n  - { term: hunter2, action: redact, id: pw }\n");
    // one redacted key is fine (and the value view is used)
    let api2 = api_with("words:\n  - { term: hunter2, action: redact, id: pw }\n");
    assert!(remember_props(&api2, json!({"hunter2": "x"})).is_ok());
    let rules = reject_rules(remember_props(
        &api,
        json!({"hunter2": "a", "HUNTER2": "b"}),
    ));
    assert_eq!(rules, vec!["props-key-collision".to_string()]);
}

#[test]
fn redacted_leaf_value_is_what_gets_stored() {
    let api = api_with("words:\n  - { term: hunter2, action: redact, id: pw }\n");
    let r = api
        .remember(RememberRequest {
            kind: "Fact".into(),
            text: "harmless".into(),
            props: Some(json!({"note": "old pw hunter2", "n": 7, "list": ["hunter2", 3]})),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(
        r.redactions,
        vec!["props:pw".to_string(), "props:pw".to_string()]
    );
    let got = api
        .recall(kaban_api::RecallRequest {
            query: "harmless".into(),
            full_text: true,
            ..Default::default()
        })
        .unwrap();
    let props = got.results[0].props.clone().unwrap();
    assert_eq!(props["note"], json!("old pw [REDACTED:pw]"));
    assert_eq!(props["n"], json!(7));
    assert_eq!(props["list"], json!(["[REDACTED:pw]", 3]));
}

/// Review R1-2: the credential key context survives nested containers, and keys are classified
/// on the same normalized views as literals (fullwidth, confusables, casefold, separators).
#[test]
fn nested_containers_and_normalized_spellings_of_a_credential_key_are_rejected() {
    let api = api_with("");
    let cases: [(&str, Value); 6] = [
        ("nested array", json!({"password": [["a"]]})),
        ("nested object", json!({"password": {"value": "a"}})),
        (
            "fullwidth key",
            json!({"\u{ff53}\u{ff45}\u{ff43}\u{ff52}\u{ff45}\u{ff54}": "a"}),
        ),
        ("cyrillic er in password", json!({"\u{0440}assword": "a"})),
        (
            "object under an array under the key",
            json!({"api-key": [{"v": "a"}]}),
        ),
        (
            "mixed case with a dot path",
            json!({"db": {"Smtp.Password": "a"}}),
        ),
    ];
    for (name, props) in cases {
        let rules = reject_rules(remember_props(&api, props));
        assert!(
            rules.iter().any(|r| r == "credential-key"),
            "{name}: rules {rules:?}"
        );
    }
    // negatives stay clean: suffix keys, placeholders, and nested non-credential keys
    assert!(remember_props(&api, json!({"password_required": true})).is_ok());
    assert!(remember_props(&api, json!({"secret": "<your-secret>"})).is_ok());
    assert!(remember_props(&api, json!({"token": "none"})).is_ok());
    assert!(remember_props(&api, json!({"db": {"host": ["h1", "h2"]}})).is_ok());
    assert!(remember_props(&api, json!({"password_policy": {"min_length": 12}})).is_ok());
}
