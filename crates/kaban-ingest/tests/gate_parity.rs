//! Remember/submit masks and rule identities agree; successful audit accompanies publication.
#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;
use kaban_api::{KabanApi, RememberRequest};
use kaban_core::{AuditStore, MemoryStore, NodeId};
use kaban_ingest::jobs::{brief::brief, submit::submit, types::IngestOpts};
use kaban_policy::Policy;
use serde_json::json;

#[tokio::test]
async fn summary_and_props_masks_match_remember_and_commit_no_raw_canary() {
    let policy = Policy::from_file(
        Policy::parse(b"words:\n  - {term: privatecanary7781, action: redact, id: masked}\n")
            .unwrap(),
        "parity".into(),
    )
    .unwrap();
    let rig = common::real::rig_on_disk("kaban-e39-parity", "mask", Policy::empty());
    std::fs::write(rig.root.join("a.md"), "# Source\n\nHarmless source.\n").unwrap();
    let api = KabanApi::new(rig.store.clone(), policy, rig.store.signer(), None);
    let remembered = api
        .remember(RememberRequest {
            kind: "Fact".into(),
            text: "Remembered fact".into(),
            summary: Some("privatecanary7781".into()),
            props: Some(json!({"note":"privatecanary7781"})),
            ..Default::default()
        })
        .unwrap();
    assert!(remembered.redactions.contains(&"summary:masked".into()));
    assert!(remembered.redactions.contains(&"props:masked".into()));
    let original = rig
        .store
        .get(&NodeId::parse(&remembered.id).unwrap(), true)
        .unwrap()
        .unwrap();
    let prepared = brief(&api, &rig.root, "a.md", &IngestOpts::default())
        .await
        .unwrap();
    let audits = rig.store.audit_verify().unwrap().entries;
    let report = submit(&api,&rig.root,json!({"job":prepared.job,"records":[{"local_id":"r","kind":"Fact","text":"Submitted fact","summary":"privatecanary7781","props":{"note":"privatecanary7781"}}]})).await.unwrap();
    assert_eq!(report["redactions"], 2);
    assert_eq!(rig.store.audit_verify().unwrap().entries, audits + 2);
    let stored = rig
        .store
        .get(
            &NodeId::parse(report["records"][0]["id"].as_str().unwrap()).unwrap(),
            true,
        )
        .unwrap()
        .unwrap();
    assert_eq!(stored.summary, original.summary);
    assert_eq!(stored.props, original.props);
    assert!(
        !rig.disk_bytes()
            .windows(17)
            .any(|part| part == b"privatecanary7781")
    );
    assert!(!report.to_string().contains("privatecanary7781"));
    let (_, rules) = api
        .gate_field("remember", "summary", "privatecanary7781")
        .unwrap();
    assert_eq!(rules, vec!["summary:masked"]);
}
