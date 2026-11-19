//! Structured resubmission through the public submit surface: a changed record updates in place.
#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;
use kaban_api::KabanApi;
use kaban_core::{MemoryStore, NodeId};
use kaban_ingest::jobs::{brief::brief, submit::submit, types::IngestOpts};
use serde_json::json;

#[tokio::test]
async fn a_changed_record_updates_in_place_and_repeats_are_unchanged() {
    let rig = common::real::rig("kaban-e39-revision", "in-place");
    std::fs::write(
        rig.root.join("a.md"),
        "# Decision\n\nUse SQLite for a measured reason.\n",
    )
    .unwrap();
    let api = KabanApi::new(
        rig.store.clone(),
        kaban_policy::Policy::empty(),
        rig.store.signer(),
    );
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
    let body = json!({"job":prepared.job,"records":[{"local_id":"stable","kind":"Decision","text":"Use SQLite","props":{"rationale":"measured reason"}}]});
    let first = submit(&api, &rig.root, body.clone()).await.unwrap();
    assert_eq!(first["created"], 1);
    let id = NodeId::parse(first["records"][0]["id"].as_str().unwrap()).unwrap();
    let unchanged = submit(&api, &rig.root, body.clone()).await.unwrap();
    assert_eq!(unchanged["unchanged"], 1);
    let mut changed = body;
    changed["records"][0]["props"]["rationale"] = json!("different measured reason");
    let update = submit(&api, &rig.root, changed.clone()).await.unwrap();
    assert_eq!(update["updated"], 1, "{update}");
    assert_eq!(update["records"][0]["outcome"], "updated");
    assert_eq!(update["records"][0]["id"], json!(id));
    let node = rig.store.get(&id, false).unwrap().unwrap();
    assert_eq!(node.lifecycle, kaban_core::Lifecycle::Active);
    assert_eq!(node.props["rationale"], "different measured reason");
    let last = rig.last_seq();
    let repeat = submit(&api, &rig.root, changed).await.unwrap();
    assert_eq!(repeat["unchanged"], 1);
    assert_eq!(rig.last_seq(), last);
}
