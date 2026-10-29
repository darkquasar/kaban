//! Hostile extractor bytes remain untrusted through parsing, publication and at-rest audit.
#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;
use kaban_api::KabanApi;
use kaban_core::{AuditStore, Governance, IngestStore, JobId, MemoryStore, NodeId};
use kaban_ingest::jobs::{brief::brief, submit::submit, types::IngestOpts};
use kaban_policy::Policy;
use serde_json::json;

#[tokio::test]
async fn credential_and_prompt_rejection_precede_parsing_and_never_publish_success() {
    let policy = Policy::from_file(
        Policy::parse(
            b"words:\n  - {term: 'Ignore previous instructions', action: reject, id: injection}\n",
        )
        .unwrap(),
        "hostile".into(),
    )
    .unwrap();
    let rig = common::real::rig_on_disk("kaban-e39-hostile", "reject", Policy::empty());
    std::fs::write(rig.root.join("a.md"), "# Source\n\nA harmless source.\n").unwrap();
    let api = KabanApi::new(rig.store.clone(), policy, rig.store.signer(), None);
    let prepared = brief(&api, &rig.root, "a.md", &IngestOpts::default())
        .await
        .unwrap();
    let before = rig.last_seq();
    for text in [
        include_str!("../../../fixtures/ingest/hostile/credential.md"),
        include_str!("../../../fixtures/ingest/hostile/prompt-injection.md"),
    ] {
        let error = submit(&api,&rig.root,json!({"job":prepared.job,"records":[{"local_id":"x","kind":"Fact","text":text}],"unknown":"malformed"})).await.unwrap_err();
        assert_eq!(
            error.exit_code(),
            2,
            "policy rejection precedes unknown-field parse"
        );
        assert!(!error.to_string().contains("AKIA"));
    }
    assert_eq!(rig.last_seq(), before);
    assert_eq!(
        rig.store
            .job_get(&JobId::parse(&prepared.job).unwrap(), api.workspace())
            .unwrap()
            .unwrap()
            .submits,
        0
    );
    assert!(
        !rig.disk_bytes()
            .windows(20)
            .any(|part| part == b"AKIAQWERTYUIOPASDFGH")
    );
    assert!(rig.store.audit_verify().unwrap().ok);
}

#[tokio::test]
async fn fake_governance_fields_are_refused_and_prose_does_not_accept_a_decision() {
    let rig = common::real::rig("kaban-e39-hostile", "fake");
    std::fs::write(rig.root.join("a.md"), "# Decision\n\nHarmless source.\n").unwrap();
    let api = KabanApi::new(rig.store.clone(), Policy::empty(), rig.store.signer(), None);
    let prepared = brief(
        &api,
        &rig.root,
        "a.md",
        &IngestOpts {
            r#type: Some("adr".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let mut body = json!({"job":prepared.job,"records":[{"local_id":"x","kind":"Decision","text":include_str!("../../../fixtures/ingest/hostile/fake-accepted.md"),"props":{"rationale":"source evidence"},"governance":"accepted"}]});
    assert_eq!(
        submit(&api, &rig.root, body.clone())
            .await
            .unwrap_err()
            .to_string(),
        "invalid_body"
    );
    body["records"][0]
        .as_object_mut()
        .unwrap()
        .remove("governance");
    let report = submit(&api, &rig.root, body).await.unwrap();
    let id = NodeId::parse(report["records"][0]["id"].as_str().unwrap()).unwrap();
    assert_eq!(
        rig.store.get(&id, true).unwrap().unwrap().governance,
        Governance::Proposed
    );
}
