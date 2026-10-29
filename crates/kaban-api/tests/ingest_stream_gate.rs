//! Ingestion stream leaves retain policy scanning without the unrelated scalar field cap.
#![allow(clippy::unwrap_used)] // test assertions

use kaban_api::KabanApi;
use kaban_policy::Policy;
use kaban_store_sqlite::SqliteStore;
use serde_json::json;
use std::sync::Arc;

fn api(yaml: &str) -> KabanApi {
    let policy = Policy::from_file(Policy::parse(yaml.as_bytes()).unwrap(), "test".into()).unwrap();
    let store = Arc::new(SqliteStore::open_memory().unwrap());
    KabanApi::new(store.clone(), policy, store.signer(), None)
}

#[test]
fn normative_thirty_two_kib_ingest_slice_is_not_rejected_by_scalar_cap() {
    let api = api("{}");
    let text = "x".repeat(32 * 1024);
    let value = json!({"chunk": {"text": text}});
    assert_eq!(
        api.gate_response("ingest_next", &value).unwrap(),
        value,
        "a normative ingestion slice was refused by the ordinary scalar field limit"
    );
}

#[test]
fn stream_windows_reject_and_redact_spanning_boundary_canaries() {
    let text = format!("{} spansecret {}", "x".repeat(19_995), "x".repeat(20_000));
    let redact = api("words:\n  - { term: spansecret, action: redact, id: span-mask }\n");
    let masked = redact
        .gate_response("ingest_next", &json!({"chunks": [{"text": text}]}))
        .unwrap();
    let output = masked["chunks"][0]["text"].as_str().unwrap();
    assert!(!output.contains("spansecret"));
    assert!(output.contains("[REDACTED:span-mask]"));
    assert!(
        output.len() > 20_000,
        "masking dropped the rest of the stream"
    );
    let reject = api("words:\n  - { term: spansecret, action: reject, id: span-reject }\n");
    let error = reject
        .gate_response("ingest_next", &json!({"chunk": {"text": text}}))
        .unwrap_err();
    assert!(error.to_string().contains("span-reject"));
    assert!(!error.to_string().contains("spansecret"));
}

#[test]
fn requests_neighboring_metadata_other_tools_and_keys_keep_scalar_cap() {
    let api = api("{}");
    let long = "x".repeat(32 * 1024);
    for value in [
        json!({"chunk": {"context_prefix": long}}),
        json!({"text": long}),
        json!({"other": {"chunk": {"text": long}}}),
        json!({"chunk": {long.clone(): "a value"}}),
    ] {
        assert!(
            api.gate_response("ingest_next", &value)
                .unwrap_err()
                .to_string()
                .contains("input-too-long")
        );
    }
    assert!(
        api.gate_response("recall", &json!({"chunk": {"text": long}}))
            .unwrap_err()
            .to_string()
            .contains("input-too-long")
    );
    assert!(
        api.prepare_args("ingest_next", &json!({"chunk": {"text": long}}))
            .unwrap_err()
            .to_string()
            .contains("input-too-long")
    );
}
