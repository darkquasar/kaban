//! Public brief placement, preview, schema and authority workflows.
#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;
use kaban_api::KabanApi;
use kaban_core::ValidatorLimits;
use kaban_core::WorkspaceId;
use kaban_core::{IngestStore, JobId, MemoryStore, NodeId};
use kaban_ingest::jobs::next::PageBudget;
use kaban_ingest::jobs::types::{
    IngestDialect, JobEnvelope, PairJudgement, PairRef, PreparationOptions, StructuredReturn,
    job_id,
};
use kaban_ingest::jobs::{brief::brief, types::IngestOpts};
use serde_json::json;
use std::collections::BTreeSet;
use std::fmt::Write;

fn envelope() -> JobEnvelope {
    JobEnvelope {
        v: 1,
        lease: None,
        config_hash: String::new(),
        document_id: NodeId::new(),
        ontology_hash: "ontology".into(),
        policy_fingerprint: "policy".into(),
        source_bytes_hmac: "keyed-source".into(),
        target_schema: json!({"kinds":["Fact"]}),
        allowed_rels: vec!["RELATES_TO".into()],
        chunks: Vec::new(),
        options: PreparationOptions {
            link: "none".into(),
            symbols: false,
        },
        submit_state: json!({}),
        finished_report_json: None,
    }
}

#[test]
fn envelope_is_versioned_and_never_infers_legacy_validation_snapshot() {
    let original = envelope();
    let bytes = original.encode().unwrap();
    assert_eq!(JobEnvelope::decode(&bytes).unwrap(), original);
    assert!(JobEnvelope::decode("[]").is_err());
    let mut unknown = original.clone();
    unknown.v = 2;
    assert!(unknown.encode().is_err());
    let mut extra = serde_json::to_value(&original).unwrap();
    extra["chunk_text"] = "CANARY_UNSTORED_TEXT".into();
    assert!(JobEnvelope::decode(&extra.to_string()).is_err());
    let mut broken = original;
    broken.submit_state = json!([]);
    assert!(broken.encode().is_err());
}

#[test]
fn canonical_wire_alias_and_schema_preserve_folded_field_names() {
    let id = NodeId::new().to_string();
    let value = json!({"job":"a".repeat(64),"chunk":0,"records":[{
        "local_id":"stable","kind":"Fact","text":"Source-supported fact",
        "evidence":[{"chunk_id":id,"quote":"Source-supported"}]
    }],"links":[{"from_local_id":"stable","rel":"RELATES_TO","to_id":id}]});
    let domain: StructuredReturn = serde_json::from_value(value.clone()).unwrap();
    let wire: kaban_api::IngestSubmitRequest = domain.clone();
    assert_eq!(
        serde_json::to_value(&wire).unwrap(),
        serde_json::to_value(&domain).unwrap()
    );
    let mut unknown = value;
    unknown["records"][0]["title"] = "not-a-field".into();
    assert!(serde_json::from_value::<StructuredReturn>(unknown).is_err());
    let schema = kaban_api::ingest_jobs::structured_return_schema();
    assert_eq!(schema["additionalProperties"], false);
    assert!(
        schema["$defs"]["RecordDraft"]["properties"]
            .get("text")
            .is_some()
    );
    assert!(
        schema["$defs"]["RecordDraft"]["properties"]
            .get("title")
            .is_none()
    );
    assert!(
        schema["$defs"]["EvidenceRef"]["properties"]
            .get("chunk_id")
            .is_some()
    );
    assert_eq!(schema["$defs"]["LinkDraft"]["additionalProperties"], false);
}

#[test]
fn pair_conversion_preserves_exact_core_judgement_shape() {
    let a = NodeId::new().to_string();
    let b = NodeId::new().to_string();
    let wire = PairJudgement {
        pair: PairRef {
            a: a.clone(),
            b: b.clone(),
        },
        same: false,
    };
    let core: kaban_core::PairJudgement = wire.clone().into();
    assert_eq!(
        serde_json::to_value(wire).unwrap(),
        serde_json::to_value(core).unwrap()
    );
}

#[test]
fn v1_job_identity_is_repeatable_and_workspace_source_bound() {
    let workspace = WorkspaceId::new("a");
    let path = kaban_ingest::gate_path(&kaban_policy::Policy::empty(), "a.md").unwrap();
    let source = kaban_ingest::SourceId::new(&workspace, &path);
    let id = job_id(
        &workspace,
        &source,
        "hash",
        "1",
        "config",
        "structured",
        IngestDialect::None,
        "generic",
    );
    assert_eq!(
        id,
        job_id(
            &workspace,
            &source,
            "hash",
            "1",
            "config",
            "structured",
            IngestDialect::None,
            "generic"
        )
    );
    assert_ne!(
        id,
        job_id(
            &WorkspaceId::new("b"),
            &source,
            "hash",
            "1",
            "config",
            "structured",
            IngestDialect::None,
            "generic"
        )
    );
    assert_ne!(
        id,
        job_id(
            &workspace,
            &source,
            "changed",
            "1",
            "config",
            "structured",
            IngestDialect::None,
            "generic"
        )
    );
    assert_ne!(source.as_str(), kaban_ingest::document_id(&source).as_str());
}

fn api(rig: &common::real::Rig) -> KabanApi {
    KabanApi::new(
        rig.store.clone(),
        kaban_policy::Policy::empty(),
        rig.store.signer(),
        None,
    )
}

#[tokio::test]
async fn gazetteer_advice_and_about_use_admitted_hits_without_edges_or_fake_chunks() {
    use kaban_core::{Actor, NewNode, RememberOutcome, WriteCtx};
    let rig = common::real::rig("kaban-e38-tail", "about-signals");
    let api = api(&rig);
    let title_only = api
        .remember(
            serde_json::from_value(
                json!({"kind":"Fact","text":"Nightjar Metadata","summary":"Nightjar Metadata"}),
            )
            .unwrap(),
        )
        .unwrap()
        .id;
    let aggregate = api
        .remember(
            serde_json::from_value(
                json!({"kind":"Fact","text":"Heron Aggregate","summary":"Heron Aggregate"}),
            )
            .unwrap(),
        )
        .unwrap()
        .id;
    let once = api
        .remember(
            serde_json::from_value(
                json!({"kind":"Fact","text":"Egret Single","summary":"Egret Single"}),
            )
            .unwrap(),
        )
        .unwrap()
        .id;
    for text in [
        "duplicate one",
        "duplicate two",
        "duplicate three",
        "duplicate four",
        "duplicate five",
        "duplicate six",
    ] {
        rig.store
            .remember(
                NewNode {
                    kind: "Fact".into(),
                    text: text.into(),
                    props: Some(json!({"title":"Ambiguous Sparrow"})),
                    ..NewNode::default()
                },
                &[],
                &WriteCtx::new("v1", WorkspaceId::default(), Actor::Agent),
            )
            .unwrap();
    }
    let foreign = rig
        .store
        .remember(
            NewNode {
                kind: "Fact".into(),
                text: "Foreign Falcon".into(),
                ..NewNode::default()
            },
            &[],
            &WriteCtx::new("v1", WorkspaceId::new("foreign"), Actor::Agent),
        )
        .unwrap();
    let foreign = match foreign {
        RememberOutcome::Created(node) | RememberOutcome::Existing { node, .. } => node.id,
    };
    for path in ["src/alpha/", "src/beta/"] {
        api.remember(
            serde_json::from_value(
                json!({"kind":"Fact","text":format!("Target {path}"),"path_scope":[path]}),
            )
            .unwrap(),
        )
        .unwrap();
    }
    std::fs::write(rig.root.join("a.md"), "---\ntitle: Nightjar Metadata\n---\n# Requirements\n\nHeron Aggregate Egret Single Egret Single Ambiguous Sparrow Foreign Falcon src/alpha/ src/beta/\n\n# Other\n\nHeron Aggregate Ambiguous Sparrow\n").unwrap();
    let opts = IngestOpts {
        link: "gazetteer".into(),
        ..IngestOpts::default()
    };
    let result = brief(&api, &rig.root, "a.md", &opts).await.unwrap();
    assert!(
        result
            .about_candidates
            .iter()
            .any(|hit| hit.id == title_only)
    );
    assert!(
        !result.candidates.iter().any(|hit| hit.id == title_only),
        "metadata title is absent from chunk bodies"
    );
    assert!(
        result
            .about_candidates
            .iter()
            .any(|hit| hit.id == aggregate)
    );
    assert!(
        !result
            .about_candidates
            .iter()
            .any(|hit| hit.id == once || hit.id == foreign)
    );
    assert!(result.candidates.iter().any(|hit| hit.ambiguous));
    assert!(result.about_candidates.iter().all(|hit| {
        result
            .candidates
            .iter()
            .filter(|candidate| candidate.id == hit.id)
            .all(|candidate| !candidate.ambiguous)
    }));
    let ids: BTreeSet<_> = result.about_candidates.iter().map(|hit| &hit.id).collect();
    assert_eq!(ids.len(), result.about_candidates.len());
    for hit in &result.about_candidates {
        let row = serde_json::to_value(hit).unwrap();
        assert_eq!(
            row.as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["id", "kind", "score"]
        );
        assert!(
            serde_json::from_value::<kaban_api::ingest_jobs::AboutCandidate>(
                json!({"id":hit.id,"kind":hit.kind,"score":hit.score,"chunk":"fake"})
            )
            .is_err()
        );
    }
    let suggestion = result
        .suggestion
        .candidates
        .iter()
        .find(|candidate| candidate.r#type == "design_spec")
        .unwrap();
    assert_eq!(suggestion.confidence, 0.4);
    assert_eq!(suggestion.signals, ["gazetteer_symbol_path_targets"]);
    let seq = rig.last_seq();
    let repeat = brief(&api, &rig.root, "a.md", &opts).await.unwrap();
    assert_eq!(
        repeat
            .about_candidates
            .iter()
            .map(|hit| &hit.id)
            .collect::<BTreeSet<_>>(),
        ids
    );
    let stable = brief(&api, &rig.root, "a.md", &opts).await.unwrap();
    assert_eq!(stable.about_candidates, repeat.about_candidates);
    assert_eq!(repeat.job, result.job);
    assert_eq!(rig.last_seq(), seq);
    let edges = rig
        .store
        .traverse(&kaban_core::TraverseQuery {
            start_ids: vec![NodeId::parse(&result.document.id).unwrap()],
            max_depth: 3,
            workspace_id: Some(api.workspace().to_string()),
            ..kaban_core::TraverseQuery::default()
        })
        .unwrap();
    assert!(
        edges
            .edges
            .iter()
            .all(|edge| edge.rel != "ABOUT" && edge.rel != "MENTIONS")
    );
    let no_link = brief(
        &api,
        &rig.root,
        "a.md",
        &IngestOpts {
            dry_run: true,
            link: "none".into(),
            r#type: Some("design_spec".into()),
            ..IngestOpts::default()
        },
    )
    .await;
    assert!(
        no_link.is_err(),
        "changed options refuse before publication"
    );
    assert_eq!(rig.last_seq(), seq);
}

#[tokio::test]
async fn failure_title_advice_is_named_and_link_none_supplies_no_bonus() {
    let rig = common::real::rig("kaban-e38-tail", "failure-advice");
    let api = api(&rig);
    api.remember(
        serde_json::from_value(
            json!({"kind":"Failure","text":"Nightjar Outage","summary":"Nightjar Outage"}),
        )
        .unwrap(),
    )
    .unwrap();
    let body = "# Timeline\n\nNightjar Outage occurred during an invented test.\n";
    std::fs::write(rig.root.join("a.md"), body).unwrap();
    std::fs::write(rig.root.join("b.md"), body).unwrap();
    let linked = brief(
        &api,
        &rig.root,
        "a.md",
        &IngestOpts {
            link: "gazetteer".into(),
            dry_run: true,
            ..IngestOpts::default()
        },
    )
    .await
    .unwrap();
    let signal = linked
        .suggestion
        .candidates
        .iter()
        .find(|row| row.r#type == "postmortem")
        .unwrap();
    assert_eq!(signal.confidence, 0.9);
    assert_eq!(signal.signals, ["gazetteer_failure_title_target"]);
    let unlinked = brief(
        &api,
        &rig.root,
        "b.md",
        &IngestOpts {
            dry_run: true,
            ..IngestOpts::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(unlinked.suggestion.candidates[0].confidence, 0.8);
    assert_eq!(
        unlinked.suggestion.candidates[0].signals,
        Vec::<String>::new()
    );
    assert_eq!(unlinked.about_candidates, Vec::new());
    assert!(rig.document("a.md").is_none());
    assert!(rig.document("b.md").is_none());
}

#[tokio::test]
async fn placement_repeat_and_next_use_actual_persistent_job() {
    let rig = common::real::rig("kaban-e38-brief", "placement");
    std::fs::write(
        rig.root.join("a.md"),
        "# Example\n\nA harmless source body.\n",
    )
    .unwrap();
    let api = api(&rig);
    let first = brief(&api, &rig.root, "a.md", &IngestOpts::default())
        .await
        .unwrap();
    assert!(first.document.indexed);
    assert!(
        rig.store
            .get(&NodeId::parse(&first.document.id).unwrap(), false)
            .unwrap()
            .is_some()
    );
    for chunk in &first.chunk_plan.chunks {
        assert!(
            rig.store
                .get(&NodeId::parse(&chunk.id).unwrap(), false)
                .unwrap()
                .is_some()
        );
    }
    let seq = rig.last_seq();
    let original = rig
        .store
        .job_get(&JobId::parse(&first.job).unwrap(), api.workspace())
        .unwrap()
        .unwrap();
    let repeat = brief(&api, &rig.root, "a.md", &IngestOpts::default())
        .await
        .unwrap();
    assert_eq!(repeat.job, first.job);
    assert_eq!(
        rig.store
            .job_get(&JobId::parse(&repeat.job).unwrap(), api.workspace())
            .unwrap()
            .unwrap(),
        original,
        "repeat ordinary brief preserves the original contract and counters"
    );
    assert_eq!(rig.last_seq(), seq, "repeat brief writes no graph changes");
    let job = JobId::parse(&first.job).unwrap();
    let page = kaban_ingest::jobs::next::next(&api, &rig.root, &job, 0, 0, PageBudget::default())
        .await
        .unwrap();
    assert!(page.chunk.text.contains("harmless source"));
    assert!(matches!(
        kaban_ingest::jobs::next::next(&api, &rig.root, &job, 100, 0, PageBudget::default()).await,
        Err(kaban_ingest::jobs::types::JobError::Selector)
    ));
}

#[tokio::test]
async fn preview_does_not_persist_and_unknown_type_warns_with_same_actual_identity() {
    let rig = common::real::rig("kaban-e38-brief", "preview");
    std::fs::write(rig.root.join("a.md"), "# Example\n\nA source body.\n").unwrap();
    let api = api(&rig);
    let opts = IngestOpts {
        dry_run: true,
        r#type: Some("unknown-type".into()),
        ..Default::default()
    };
    let preview = brief(&api, &rig.root, "a.md", &opts).await.unwrap();
    assert!(!preview.document.indexed);
    assert!(preview.suggestion.note.contains("unknown"));
    assert!(rig.document("a.md").is_none());
    assert_eq!(rig.last_seq(), 0);
    assert!(
        rig.store
            .job_get(&JobId::parse(&preview.job).unwrap(), api.workspace())
            .unwrap()
            .is_none()
    );
    let actual = brief(
        &api,
        &rig.root,
        "a.md",
        &IngestOpts {
            dry_run: false,
            ..opts
        },
    )
    .await
    .unwrap();
    assert_eq!(preview.job, actual.job);
    assert_eq!(preview.document.id, actual.document.id);
    assert!(actual.document.indexed);
}

#[test]
fn every_shipped_kind_has_exact_closed_branch_and_core_fact_stays_permissive() {
    let rig = common::real::rig("kaban-e38-brief", "schema");
    let api = api(&rig);
    api.registry().with_ontology(|ontology| {
        let mut ontology = ontology.clone();
        let ids: Vec<_> = ontology.kinds.keys().cloned().collect();
        for id in ids {
            if ontology.kinds[&id].extract.as_ref().and_then(|e| e.enabled) == Some(false) {
                continue;
            }
            ontology.default_kind.clear();
            ontology.default_kind.insert(id.ns.clone(), id.clone());
            let schema = kaban_ingest::jobs::schema::target_schema(
                &ontology,
                "no_mapping",
                &ValidatorLimits::default(),
            )
            .unwrap();
            kaban_ingest::jobs::schema::lint(&schema.json_schema).unwrap();
            assert!(!schema.allowed_edges.iter().any(|edge| matches!(
                edge[1].as_str(),
                "SUPERSEDES" | "INVALIDATES" | "CONTRADICTS" | "GOVERNS"
            )));
            let branch = &schema.json_schema["properties"]["records"]["items"]["oneOf"][0];
            assert_eq!(
                branch["properties"]["kind"]["enum"],
                json!([kaban_core::stored_name(&id)])
            );
            let props = &branch["properties"]["props"];
            assert_eq!(props["additionalProperties"], false);
            for internal in kaban_ingest::extract::structured_prep::schema::INTERNAL_PROPERTIES {
                assert!(props["properties"].get(internal).is_none());
            }
            if id.name == "Fact" {
                assert_eq!(props["properties"], json!({}));
                assert_eq!(
                    kaban_core::validate(
                        &ontology.kinds[&id].props_schema,
                        &json!({"undeclared":"permitted by core"})
                    ),
                    [] as [kaban_core::PropsError; 0]
                );
            }
        }
    });
    assert!(kaban_ingest::jobs::schema::lint(&json!({"oneOf":[{"type":"string"}]})).is_err());
    assert!(
        kaban_ingest::jobs::schema::lint(&json!({"type":"object","additionalProperties":true}))
            .is_err()
    );
    let nested = json!({"type":"object","additionalProperties":false,"properties":{"props":{"type":"object","additionalProperties":false,"properties":{"records":{"type":"array","items":{"oneOf":[{"type":"object","additionalProperties":false,"properties":{"kind":{"type":"string","enum":["Fact"]},"props":{"type":"object","additionalProperties":false,"properties":{}}}}]}}}}}});
    assert!(
        kaban_ingest::jobs::schema::lint(&nested).is_err(),
        "union exception must not extend into props"
    );
}

#[tokio::test]
async fn thousand_chunk_plan_pages_are_serialized_byte_bounded() {
    let rig = common::real::rig("kaban-e38-brief", "plan-pages");
    let body = (0..1001).fold(String::new(), |mut body, n| {
        write!(
            body,
            "# Heading {n}\n\nDistinct harmless body number {n}.\n\n"
        )
        .unwrap();
        body
    });
    std::fs::write(rig.root.join("a.md"), body).unwrap();
    let api = api(&rig);
    let opts = IngestOpts {
        dry_run: true,
        max_result_bytes: 24 * 1024,
        ..Default::default()
    };
    let first = brief(&api, &rig.root, "a.md", &opts).await.unwrap();
    assert!(first.handling_plan.chunks > 1000);
    assert!(first.chunk_plan.pages > 1);
    let mut ids = vec![];
    for p in 0..first.chunk_plan.pages {
        let page = if p == 0 {
            first.clone()
        } else {
            brief(
                &api,
                &rig.root,
                "a.md",
                &IngestOpts {
                    plan_page: p,
                    ..opts.clone()
                },
            )
            .await
            .unwrap()
        };
        assert!(serde_json::to_vec(&page).unwrap().len() <= opts.max_result_bytes);
        ids.extend(page.chunk_plan.chunks.into_iter().map(|c| c.n));
    }
    assert_eq!(
        ids,
        (0..u32::try_from(first.handling_plan.chunks).unwrap()).collect::<Vec<_>>()
    );
    assert!(matches!(
        brief(
            &api,
            &rig.root,
            "a.md",
            &IngestOpts {
                plan_page: first.chunk_plan.pages,
                ..opts
            }
        )
        .await,
        Err(kaban_ingest::jobs::types::JobError::Selector)
    ));
}

#[tokio::test]
async fn changed_options_go_stale_without_graph_changes_and_code_refuses() {
    let rig = common::real::rig("kaban-e38-brief", "contract");
    std::fs::write(rig.root.join("a.md"), "A harmless source body.\n").unwrap();
    std::fs::write(rig.root.join("a.rs"), "fn main() {}\n").unwrap();
    let api = api(&rig);
    brief(&api, &rig.root, "a.md", &IngestOpts::default())
        .await
        .unwrap();
    let seq = rig.last_seq();
    assert!(matches!(
        brief(
            &api,
            &rig.root,
            "a.md",
            &IngestOpts {
                symbols: true,
                ..Default::default()
            }
        )
        .await,
        Err(kaban_ingest::jobs::types::JobError::Stale)
    ));
    assert_eq!(rig.last_seq(), seq);
    assert!(
        brief(&api, &rig.root, "a.rs", &IngestOpts::default())
            .await
            .is_err()
    );
    assert!(rig.document("a.rs").is_none());
}

#[tokio::test]
async fn hidden_document_refuses_without_registry_graph_or_canary_persistence() {
    const CANARY: &str = "SYNTHETIC-DUMP-778199";
    let policy = || {
        kaban_policy::Policy::from_file(kaban_policy::Policy::parse(b"rules:\n  - id: hidden\n    regex: 'SYNTHETIC-DUMP-[0-9]+'\n    action: reject\n    on_document: skip_file\n").unwrap(), "test".into()).unwrap()
    };
    let rig = common::real::rig_on_disk("kaban-e38-brief", "hidden", policy());
    let api = KabanApi::new(rig.store.clone(), policy(), rig.store.signer(), None);
    std::fs::write(rig.root.join("hidden.md"), format!("# Data\n\n{CANARY}\n")).unwrap();
    assert!(
        brief(&api, &rig.root, "hidden.md", &IngestOpts::default())
            .await
            .is_err()
    );
    assert!(
        brief(
            &api,
            &rig.root,
            "hidden.md",
            &IngestOpts {
                dry_run: true,
                ..Default::default()
            }
        )
        .await
        .is_err()
    );
    assert_eq!(rig.last_seq(), 0);
    assert!(rig.document("hidden.md").is_none());
    assert!(
        !rig.disk_bytes()
            .windows(CANARY.len())
            .any(|bytes| bytes == CANARY.as_bytes())
    );
}

#[tokio::test]
async fn dialect_briefs_keep_verbatim_advice_and_candidate_only_gazetteer() {
    let rig = common::real::rig("kaban-e38-brief", "dialects");
    let api = api(&rig);
    let remembered = api
        .remember(
            serde_json::from_value(
                json!({"kind":"Fact","text":"Nightjar Database","summary":"Nightjar Database"}),
            )
            .unwrap(),
        )
        .unwrap();
    std::fs::write(
        rig.root.join("a.md"),
        "# Choice\n\nWe use Nightjar Database for storage.\n",
    )
    .unwrap();
    for dialect in [
        kaban_api::ingest_jobs::IngestDialect::Llm,
        kaban_api::ingest_jobs::IngestDialect::Gliner,
        kaban_api::ingest_jobs::IngestDialect::None,
    ] {
        let result = brief(
            &api,
            &rig.root,
            "a.md",
            &IngestOpts {
                r#with: dialect,
                link: "gazetteer".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert!(result.handling_plan.instructions.iter().any(
            |text| text == kaban_ingest::extract::structured_prep::dialect::DUPLICATE_AVOIDANCE
        ));
        assert!(
            result
                .candidates
                .iter()
                .any(|candidate| candidate.id == remembered.id)
        );
        if dialect == kaban_api::ingest_jobs::IngestDialect::Gliner {
            assert!(
                result
                    .handling_plan
                    .instructions
                    .iter()
                    .any(|text| text.contains("cannot compose rationale"))
            );
        }
        let document = rig.document("a.md").unwrap();
        let edges = rig
            .store
            .traverse(&kaban_core::TraverseQuery {
                start_ids: vec![NodeId::parse(&document.id).unwrap()],
                max_depth: 3,
                workspace_id: Some(api.workspace().to_string()),
                ..Default::default()
            })
            .unwrap();
        // The public graph reader must see no candidate promotion to a MENTIONS edge.
        assert!(!edges.edges.iter().any(|edge| edge.rel == "MENTIONS"));
    }
}

#[tokio::test]
async fn loaded_ontology_extends_brief_kinds_and_disabled_kinds_do_not_return() {
    let rig = common::real::rig("kaban-e38-brief", "loaded-schema");
    std::fs::create_dir_all(rig.root.join("incidents")).unwrap();
    std::fs::write(
        rig.root.join("incidents/outage.md"),
        "# Timeline\n\nA harmless outage description.\n",
    )
    .unwrap();
    let file = rig.root.join("incidents.yaml");
    std::fs::write(&file, include_str!("fixtures/structured/incidents.yaml")).unwrap();
    let registry = std::sync::Arc::new(
        kaban_api::OntologyRegistry::new(Some(rig.root.to_path_buf())).unwrap(),
    );
    let api = api(&rig).with_registry(registry.clone());
    registry.load_file(&api, &file).unwrap();
    let brief = brief(
        &api,
        &rig.root,
        "incidents/outage.md",
        &IngestOpts::default(),
    )
    .await
    .unwrap();
    assert_eq!(
        brief.target_schema.kinds,
        ["Failure", "Improvement", "incidents:Incident"]
    );
    kaban_ingest::jobs::schema::lint(&brief.target_schema.json_schema).unwrap();
    let mut ontology = kaban_core::shipped::resolved().unwrap();
    let disabled = ontology
        .kinds
        .values_mut()
        .find(|kind| kind.id.name == "Decision")
        .unwrap();
    disabled.extract.as_mut().unwrap().enabled = Some(false);
    let target =
        kaban_ingest::jobs::schema::target_schema(&ontology, "adr", &ValidatorLimits::default())
            .unwrap();
    assert_eq!(target.kinds, ["Fact"]);
    assert_eq!(
        target.json_schema["properties"]["records"]["items"]["oneOf"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn golden_gliner_brief_advice_and_closed_schema() {
    let rig = common::real::rig("kaban-e38-brief", "golden");
    std::fs::write(
        rig.root.join("a.md"),
        "# Example\n\nA harmless source body.\n",
    )
    .unwrap();
    let api = api(&rig);
    let result = brief(
        &api,
        &rig.root,
        "a.md",
        &IngestOpts {
            r#with: IngestDialect::Gliner,
            dry_run: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    // Gated static contract only: scratch-root-dependent keyed document/chunk identities
    // are exercised above rather than becoming unstable golden fields.
    insta::assert_json_snapshot!(
        "gliner_contract",
        json!({"instructions":result.handling_plan.instructions,"target_schema":result.target_schema,"extractor_hints":result.extractor_hints})
    );
}
