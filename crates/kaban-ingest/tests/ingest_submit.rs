//! Structured submit's public atomic placement and fixed-refusal workflows.
#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;
use kaban_api::KabanApi;
use kaban_core::{IngestStore, JobId, MemoryStore, NodeId};
use kaban_ingest::jobs::{brief::brief, submit::submit, types::IngestOpts};
use serde_json::{Value, json};

fn api(rig: &common::real::Rig) -> KabanApi {
    KabanApi::new(
        rig.store.clone(),
        kaban_policy::Policy::empty(),
        rig.store.signer(),
        None,
    )
}

#[tokio::test]
async fn public_submit_places_repeats_without_changes_and_accumulates_actual_outcomes() {
    let rig = common::real::rig("kaban-e39-submit", "repeat");
    std::fs::write(
        rig.root.join("a.md"),
        "# Source\n\nA harmless source body.\n",
    )
    .unwrap();
    let api = api(&rig);
    let prepared = brief(&api, &rig.root, "a.md", &IngestOpts::default())
        .await
        .unwrap();
    let body = json!({"job":prepared.job,"chunk":0,"records":[{"local_id":"fact","kind":"Fact","text":"A harmless source body.","evidence":[{"chunk_id":prepared.chunk_plan.chunks[0].id,"quote":"harmless   source"}]}]});
    let first = submit(&api, &rig.root, body.clone()).await.unwrap();
    assert_eq!(first["created"], 1);
    assert_eq!(first["skipped"], json!([]));
    assert_eq!(first["evidence_dropped"], json!([]));
    assert!(first["assertions"].as_u64().unwrap() >= 3);
    let id = NodeId::parse(first["records"][0]["id"].as_str().unwrap()).unwrap();
    let stored = rig.store.get(&id, false).unwrap().unwrap();
    assert_eq!(stored.text, "A harmless source body.");
    assert_eq!(stored.props["evidence"][0]["quote"], "harmless   source");
    let (_, edges) = rig.hop(&id, &["DERIVED_FROM", "EVIDENCE_FOR"], 1);
    assert!(
        edges.is_empty(),
        "default Script missing prior rejects projections, never gains implicit authority"
    );
    assert!(
        rig.store
            .rescan_assertions("test-negative", &mut |_, _| None)
            .unwrap()
            .scanned
            > 0
    );
    let last = rig.last_seq();
    let second = submit(&api, &rig.root, body).await.unwrap();
    assert_eq!(second["created"], 0);
    assert_eq!(second["unchanged"], 1);
    assert_eq!(second["assertions"], 0);
    assert_eq!(
        rig.last_seq(),
        last,
        "identical submit publishes no graph changes"
    );
    let row = rig
        .store
        .job_get(&JobId::parse(&prepared.job).unwrap(), api.workspace())
        .unwrap()
        .unwrap();
    assert_eq!((row.submits, row.created, row.unchanged), (2, 1, 1));
    assert!(
        row.named_edges
            .iter()
            .any(|(src, dst, rel)| src == id.as_str()
                && dst == &prepared.document.id
                && rel == "DERIVED_FROM")
    );
    let plan: Value = serde_json::from_str(&row.plan_json).unwrap();
    assert_eq!(plan["submit_state"]["named_records"], json!([id]));
    assert!(
        !row.plan_json.contains("harmless"),
        "accumulator stores identities, never contents"
    );
    let mut snapshot = first;
    snapshot["job"] = json!("<JOB>");
    snapshot["records"][0]["id"] = json!("<RECORD>");
    snapshot["changes"]["first_seq"] = json!("<FIRST_SEQ>");
    snapshot["changes"]["last_seq"] = json!("<LAST_SEQ>");
    insta::assert_json_snapshot!("report", snapshot);
}

#[tokio::test]
async fn bounded_union_preserves_names_and_overflow_does_not_refuse_later_submit() {
    let rig = common::real::rig("kaban-e39-submit", "cap");
    std::fs::write(rig.root.join("a.md"), "# Source\n\nHarmless source.\n").unwrap();
    let mut engine = kaban_core::config::EngineConfig::default();
    engine.ingest.finish_cap = 1;
    let api = KabanApi::with_config(
        rig.store.clone(),
        kaban_policy::Policy::empty(),
        rig.store.signer(),
        None,
        kaban_api::ApiConfig {
            engine: std::sync::Arc::new(engine),
            ..Default::default()
        },
    );
    let prepared = brief(&api, &rig.root, "a.md", &IngestOpts::default())
        .await
        .unwrap();
    let mut prior = None;
    for local_id in ["first", "second", "third"] {
        let result = submit(
            &api,
            &rig.root,
            json!({"job":prepared.job,"records":[
                {"local_id":local_id,"kind":"Fact","text":"Harmless fact"}
            ]}),
        )
        .await
        .unwrap();
        assert_eq!(result["created"], 1);
        let row = rig
            .store
            .job_get(&JobId::parse(&prepared.job).unwrap(), api.workspace())
            .unwrap()
            .unwrap();
        let plan: Value = serde_json::from_str(&row.plan_json).unwrap();
        assert_eq!(
            plan["submit_state"]["named_records"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(row.named_edges.len(), 1);
        if let Some((names, edges)) = &prior {
            assert_eq!(&plan["submit_state"]["named_records"], names);
            assert_eq!(&row.named_edges, edges);
            assert_eq!(plan["submit_state"]["overflow"], true);
        }
        prior = Some((
            plan["submit_state"]["named_records"].clone(),
            row.named_edges,
        ));
    }
}

#[tokio::test]
async fn duplicates_internal_props_bad_quotes_and_forbidden_edges_are_fixed_skips() {
    let rig = common::real::rig("kaban-e39-submit", "negative");
    std::fs::write(rig.root.join("a.md"), "# Source\n\nHarmless source.\n").unwrap();
    let api = api(&rig);
    let prepared = brief(&api, &rig.root, "a.md", &IngestOpts::default())
        .await
        .unwrap();
    let result = submit(&api, &rig.root, json!({"job":prepared.job,"records":[
        {"local_id":"dup","kind":"Fact","text":"first"},
        {"local_id":" dup ","kind":"Fact","text":"second"},
        {"local_id":"internal","kind":"Fact","text":"third","props":{"accepted_commit":"forged"}},
        {"local_id":"valid","kind":"Fact","text":"fourth","evidence":[{"chunk_id":prepared.chunk_plan.chunks[0].id,"quote":"absent quote"},{"chunk_id":NodeId::new(),"quote":"Harmless"}]}
    ],"edges":[{"from_local_id":"valid","rel":"SUPERSEDES","to_local_id":"valid"}]})).await.unwrap();
    assert_eq!(result["created"], 1);
    let reasons: Vec<_> = result["skipped"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["reason"].as_str().unwrap())
        .collect();
    assert_eq!(
        reasons,
        [
            "duplicate_local_id",
            "duplicate_local_id",
            "invalid_props",
            "edge_not_allowed"
        ]
    );
    assert_eq!(result["evidence_dropped"][0]["reason"], "quote_not_found");
    assert_eq!(result["evidence_dropped"][1]["reason"], "chunk_not_in_job");
}

#[tokio::test]
async fn malformed_body_foreign_handle_and_source_edit_publish_nothing() {
    let rig = common::real::rig("kaban-e39-submit", "stale");
    std::fs::write(rig.root.join("a.md"), "# Source\n\nOriginal body.\n").unwrap();
    let api = api(&rig);
    let prepared = brief(&api, &rig.root, "a.md", &IngestOpts::default())
        .await
        .unwrap();
    let before = rig.last_seq();
    let unknown = submit(&api, &rig.root, json!({"job":"a".repeat(64),"records":[]}))
        .await
        .unwrap_err();
    assert_eq!(unknown.exit_code(), 3);
    let malformed = submit(
        &api,
        &rig.root,
        json!({"job":prepared.job,"records":[],"accepted":true}),
    )
    .await
    .unwrap_err();
    assert_eq!(malformed.to_string(), "invalid_body");
    std::fs::write(rig.root.join("a.md"), "# Source\n\nChanged body.\n").unwrap();
    let stale = submit(&api, &rig.root, json!({"job":prepared.job,"records":[{"local_id":"x","kind":"Fact","text":"must not appear"}]})).await.unwrap_err();
    assert_eq!(stale.to_string(), "stale_job_contract");
    assert_eq!(rig.last_seq(), before);
}

#[tokio::test]
async fn ontology_reload_marks_job_stale_without_submit_graph_writes() {
    let rig = common::real::rig("kaban-e39-submit", "ontology");
    std::fs::write(rig.root.join("a.md"), "# Source\n\nHarmless source.\n").unwrap();
    let registry = std::sync::Arc::new(
        kaban_api::OntologyRegistry::new(Some(rig.root.to_path_buf())).unwrap(),
    );
    let api = api(&rig).with_registry(registry.clone());
    let prepared = brief(&api, &rig.root, "a.md", &IngestOpts::default())
        .await
        .unwrap();
    let file = rig.root.join("incidents.yaml");
    std::fs::write(&file, include_str!("fixtures/structured/incidents.yaml")).unwrap();
    registry.load_file(&api, &file).unwrap();
    let last = rig.last_seq();
    let error = submit(&api,&rig.root,json!({"job":prepared.job,"records":[{"local_id":"r","kind":"Fact","text":"must not publish"}]})).await.unwrap_err();
    assert_eq!(error.to_string(), "stale_job_contract");
    assert_eq!(rig.last_seq(), last);
    let row = rig
        .store
        .job_get(&JobId::parse(&prepared.job).unwrap(), api.workspace())
        .unwrap()
        .unwrap();
    assert_eq!(row.status, kaban_core::JobStatus::Stale);
    assert_eq!(row.submits, 0);
}
