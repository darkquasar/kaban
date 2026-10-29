//! Public ingest producer and dictionary-gate regressions.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod common;

use common::Scratch;
use kaban_api::{ApiConfig, KabanApi};
use kaban_core::config::EngineConfig;
use kaban_core::{
    Actor, MemoryStore, NewNode, NodeId, RememberOutcome, TraverseQuery, WorkspaceId, WriteCtx,
};
use kaban_ingest::{
    ChunkConfig, DocumentPreparer, IngestConfig, Ingestor, WalkConfig, run_blocking,
};
use kaban_policy::Policy;
use kaban_store_sqlite::SqliteStore;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

fn fixture(name: &str, policy: Policy) -> (Scratch, Arc<SqliteStore>, Arc<KabanApi>) {
    let mut engine = EngineConfig::default();
    engine.link.default = "gazetteer".into();
    fixture_config(name, policy, engine)
}

#[test]
fn symbols_uses_typed_boolean_and_malformed_strings_remain_gated_content() {
    let (_root, _store, api) = fixture("symbols-control", Policy::empty());
    for symbols in [false, true] {
        let raw = serde_json::json!({"paths": ["note.md"], "symbols": symbols});
        api.prepare_args("index_docs", &raw).unwrap();
        let request: kaban_api::IndexDocsRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(request.symbols, symbols);
    }
    let raw = serde_json::json!({"paths": ["note.md"], "symbols": "true"});
    api.prepare_args("index_docs", &raw).unwrap();
    assert!(serde_json::from_value::<kaban_api::IndexDocsRequest>(raw).is_err());
    let marker = format!("{}{}", "AKIA", "QWERTYUIOPASDFGH");
    let raw = serde_json::json!({"paths": ["note.md"], "symbols": marker});
    assert!(matches!(
        api.prepare_args("index_docs", &raw),
        Err(kaban_api::ApiError::Rejected { .. })
    ));
}

#[test]
fn foreign_workspace_placeholder_is_refused_without_local_publication() {
    struct ForeignPreparer {
        inner: DocumentPreparer,
        store: Arc<SqliteStore>,
        saved: std::sync::Mutex<Option<(NodeId, serde_json::Value)>>,
    }
    impl kaban_ingest::Preparer for ForeignPreparer {
        fn begin_run(
            &self,
            documents: &std::collections::BTreeSet<NodeId>,
        ) -> Result<String, kaban_ingest::IngestError> {
            self.inner.begin_run(documents)
        }
        fn prepare(
            &self,
            input: kaban_ingest::PrepareInput,
            ctx: &kaban_ingest::ExtractCtx,
        ) -> Result<kaban_ingest::Prepared, kaban_ingest::ExtractError> {
            let prepared = self.inner.prepare(input, ctx)?;
            let set = &prepared.batch.gazetteer_claim_sets[0];
            assert!(set.source_rev.is_none());
            assert_eq!(set.expected_generation, 0);
            let foreign = WorkspaceId::new("foreign");
            let batch = kaban_core::WriteBatch::new(
                foreign.clone(),
                vec![kaban_core::Op::UpsertNode {
                    id: set.document.clone(),
                    node: NewNode {
                        kind: "Document".into(),
                        text: "note.md".into(),
                        props: Some(serde_json::json!({
                            "owner": kaban_core::INGEST_OWNER, "source_id": set.source_id,
                            "status": "missing", "version": 0, "size": 0,
                            "content_hash": null, "extractor": "", "extractor_version": "",
                            "config_hash": ""
                        })),
                        ..Default::default()
                    },
                }],
            );
            kaban_core::IngestStore::apply_batch(
                self.store.as_ref(),
                batch,
                &WriteCtx::new("v1", foreign, Actor::Agent),
            )
            .unwrap();
            let node = self.store.get(&set.document, true).unwrap().unwrap();
            *self.saved.lock().unwrap() =
                Some((set.document.clone(), serde_json::to_value(node).unwrap()));
            Ok(prepared)
        }
    }
    let (root, store, api) = fixture("foreign-placeholder", Policy::empty());
    std::fs::write(root.join("note.md"), "# Note\n\nLocal document body.\n").unwrap();
    let cfg = IngestConfig::default();
    let preparer = Arc::new(ForeignPreparer {
        inner: DocumentPreparer::new(
            &api,
            store.clone(),
            store.signer(),
            ChunkConfig::default(),
            cfg.clone(),
        ),
        store: store.clone(),
        saved: std::sync::Mutex::new(None),
    });
    let runner = Ingestor::new(api, store.clone(), store.signer(), preparer.clone(), cfg);
    assert!(matches!(
        run_blocking(
            &runner,
            &WalkConfig::new(root.to_path_buf()),
            CancellationToken::new(),
        ),
        Err(kaban_ingest::IngestError::Store(
            kaban_core::StoreError::NotFound(_)
        ))
    ));
    for kind in ["Document", "Section", "Chunk"] {
        assert!(
            kaban_core::IngestStore::list_nodes(
                store.as_ref(),
                kind,
                WorkspaceId::default().as_str(),
                0,
                100,
            )
            .unwrap()
            .is_empty(),
            "refusal publishes no local {kind}"
        );
    }
    let saved = preparer.saved.lock().unwrap();
    let (id, before) = saved.as_ref().unwrap();
    let after = store.get(id, true).unwrap().unwrap();
    assert_eq!(serde_json::to_value(after).unwrap(), *before);
}

#[test]
fn parallel_first_publication_upgrades_earlier_link_placeholder() {
    struct PairedPreparer {
        inner: DocumentPreparer,
        prepared: std::sync::Barrier,
    }
    impl kaban_ingest::Preparer for PairedPreparer {
        fn begin_run(
            &self,
            documents: &std::collections::BTreeSet<NodeId>,
        ) -> Result<String, kaban_ingest::IngestError> {
            self.inner.begin_run(documents)
        }
        fn prepare(
            &self,
            input: kaban_ingest::PrepareInput,
            ctx: &kaban_ingest::ExtractCtx,
        ) -> Result<kaban_ingest::Prepared, kaban_ingest::ExtractError> {
            let result = self.inner.prepare(input, ctx);
            // Both source guards are captured before either document can commit.
            self.prepared.wait();
            result
        }
    }
    let (root, store, api) = fixture("parallel-placeholder", Policy::empty());
    std::fs::write(root.join("alpha.md"), "# Alpha\n\nSee [[beta]].\n").unwrap();
    std::fs::write(root.join("beta.md"), "# Beta\n\nPublic beta body.\n").unwrap();
    let cfg = IngestConfig {
        workers: 2,
        ..Default::default()
    };
    let preparer = Arc::new(PairedPreparer {
        inner: DocumentPreparer::new(
            &api,
            store.clone(),
            store.signer(),
            ChunkConfig::default(),
            cfg.clone(),
        ),
        prepared: std::sync::Barrier::new(2),
    });
    let runner = Ingestor::new(api, store.clone(), store.signer(), preparer, cfg);
    let walk = WalkConfig::new(root.to_path_buf());
    let report = run_blocking(&runner, &walk, CancellationToken::new()).unwrap();
    assert_eq!(report.documents, 2, "both new documents must publish");
    assert_eq!(report.skipped, 0);
    assert_eq!(report.chunks_upserted, 2);
    let second = run_blocking(&runner, &walk, CancellationToken::new()).unwrap();
    assert_eq!(
        second.documents, 2,
        "new cross-document dictionary targets converge on the next pass"
    );
    let third = run_blocking(&runner, &walk, CancellationToken::new()).unwrap();
    assert_eq!(third.documents, 0);
    assert_eq!(third.skipped, 2);
    assert_eq!(third.outbox_rows, 0);
}

#[test]
fn stale_publication_is_skipped_without_claiming_a_committed_document() {
    stale_publication_case("stale-count", false);
}

#[test]
fn existing_real_source_race_is_not_rebound_to_a_new_revision() {
    stale_publication_case("stale-existing-source", true);
}

fn stale_publication_case(name: &str, initially_existing: bool) {
    struct RacingPreparer {
        inner: DocumentPreparer,
        store: Arc<SqliteStore>,
        policy_version: String,
    }
    impl kaban_ingest::Preparer for RacingPreparer {
        fn begin_run(
            &self,
            documents: &std::collections::BTreeSet<NodeId>,
        ) -> Result<String, kaban_ingest::IngestError> {
            self.inner.begin_run(documents)
        }
        fn prepare(
            &self,
            input: kaban_ingest::PrepareInput,
            ctx: &kaban_ingest::ExtractCtx,
        ) -> Result<kaban_ingest::Prepared, kaban_ingest::ExtractError> {
            let prepared = self.inner.prepare(input, ctx)?;
            let mut write =
                WriteCtx::new(&self.policy_version, WorkspaceId::default(), Actor::Agent);
            write.audit.clone_from(&prepared.audit);
            let committed = kaban_core::IngestStore::apply_batch(
                self.store.as_ref(),
                prepared.batch.clone(),
                &write,
            )
            .unwrap();
            assert_eq!(committed.stale_source, 0);
            // The queued original now has an obsolete SourceRev/generation. The store
            // rejects it, while the ingestor must not report the rejected work as placed.
            Ok(prepared)
        }
    }
    let policy = Policy::from_file(
        Policy::parse(b"words:\n  - {term: privatecanary7781, action: redact, id: masked}\n")
            .unwrap(),
        "stale-count".into(),
    )
    .unwrap();
    let (root, store, api) = fixture(name, policy);
    let target = target(&store, "Public Cart Architecture");
    if initially_existing {
        std::fs::write(root.join("note.md"), "Initial real source content.").unwrap();
        assert_eq!(ingest(&root, &store, &api).documents, 1);
    }
    std::fs::write(
        root.join("note.md"),
        "Public Cart Architecture governs checkout with privatecanary7781.",
    )
    .unwrap();
    let cfg = IngestConfig::default();
    let preparer = Arc::new(RacingPreparer {
        inner: DocumentPreparer::new(
            &api,
            store.clone(),
            store.signer(),
            ChunkConfig::default(),
            cfg.clone(),
        ),
        store: store.clone(),
        policy_version: api
            .policy_handle()
            .read()
            .unwrap()
            .effective_hash()
            .to_owned(),
    });
    let runner = Ingestor::new(api.clone(), store.clone(), store.signer(), preparer, cfg);
    let report = run_blocking(
        &runner,
        &WalkConfig::new(root.to_path_buf()),
        CancellationToken::new(),
    )
    .unwrap();
    assert_eq!(
        report.documents, 0,
        "a stale publication did not commit this document"
    );
    assert_eq!(report.skipped, 1);
    assert_eq!(report.outbox_rows, 0);
    assert_eq!(report.chunks_upserted, 0);
    assert_eq!(report.rule_ids, std::collections::BTreeMap::new());
    assert!(
        mentioned(&store).contains(&target.to_string()),
        "the winning publication remains authoritative"
    );
}

fn fixture_config(
    name: &str,
    policy: Policy,
    engine: EngineConfig,
) -> (Scratch, Arc<SqliteStore>, Arc<KabanApi>) {
    let root = Scratch::new("gazetteer", name);
    let store = Arc::new(SqliteStore::open_memory().unwrap());
    let api = Arc::new(KabanApi::with_config(
        store.clone(),
        policy,
        store.signer(),
        None,
        ApiConfig {
            engine: Arc::new(engine),
            ..Default::default()
        },
    ));
    (root, store, api)
}

#[test]
fn over_budget_skips_linking_without_losing_l0_and_forced_symbols_lifts_cap() {
    let mut engine = EngineConfig::default();
    engine.link.default = "gazetteer".into();
    engine.gazetteer.max_automaton_mb = 1;
    let policy = Policy::from_file(
        Policy::parse(b"defaults: {max_field_chars: 1000000}\n").unwrap(),
        "budget".into(),
    )
    .unwrap();
    let (root, store, api) = fixture_config("budget", policy, engine);
    // Long, unique gated title paths make an automaton above the minimum 1 MiB cap.
    for prefix in ['a', 'b', 'c', 'd'] {
        let text = format!("{prefix}{}", "architecture".repeat(20_000));
        store
            .remember(
                NewNode {
                    kind: "Fact".into(),
                    text: format!("Long pattern {prefix}"),
                    props: Some(serde_json::json!({"title": text})),
                    ..Default::default()
                },
                &[],
                &WriteCtx::new("v1", WorkspaceId::default(), Actor::Agent),
            )
            .unwrap();
    }
    let id = target(&store, "Public Cart Architecture");
    let rows = kaban_core::IngestStore::list_nodes(
        store.as_ref(),
        "Fact",
        WorkspaceId::default().as_str(),
        0,
        500,
    )
    .unwrap();
    assert!(
        rows.iter().any(|node| node
            .props
            .get("title")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|title| title.len() > 200_000)),
        "long dictionary title must be stored"
    );
    std::fs::write(
        root.join("note.md"),
        "Public Cart Architecture governs checkout.",
    )
    .unwrap();
    let normal = ingest(&root, &store, &api);
    assert_eq!(normal.documents, 1, "L0 survives a skipped gazetteer");
    let telemetry = normal.gazetteer.unwrap();
    assert_eq!(telemetry.outcome, "skipped_budget", "{telemetry:?}");
    assert_eq!(mentioned(&store).len(), 0);
    let forced = ingest_with(
        &root,
        &store,
        &api,
        IngestConfig {
            symbols: true,
            ..Default::default()
        },
    );
    let telemetry = forced.gazetteer.unwrap();
    assert_eq!(telemetry.outcome, "ok");
    assert_eq!(telemetry.budget_lifted_by.as_deref(), Some("--symbols"));
    assert!(mentioned(&store).contains(&id.to_string()));
    let mut raised = api.engine().clone();
    raised.gazetteer.max_automaton_mb = 128;
    let budget = kaban_api::layers::budgets(&raised, &kaban_api::layers::RunFlags::default())
        .into_iter()
        .find(|budget| budget.key == "gazetteer.max_automaton_mb")
        .unwrap();
    assert_eq!(budget.source, kaban_api::layers::BudgetSource::Config);
    assert!(!budget.cap_lifted);
    let raised_api = Arc::new(KabanApi::with_config(
        store.clone(),
        Policy::from_file(
            Policy::parse(b"defaults: {max_field_chars: 1000000}\n").unwrap(),
            "budget".into(),
        )
        .unwrap(),
        store.signer(),
        None,
        ApiConfig {
            engine: Arc::new(raised),
            ..Default::default()
        },
    ));
    let raised_report = ingest(&root, &store, &raised_api);
    let telemetry = raised_report.gazetteer.unwrap();
    assert_eq!(telemetry.outcome, "ok");
    assert_eq!(telemetry.budget_lifted_by, None);
    assert!(mentioned(&store).contains(&id.to_string()));
}

proptest::proptest! {
    #![proptest_config(proptest::test_runner::Config::with_cases(24))]
    #[test]
    fn redact_hits_never_reach_ingested_chunks_or_reports(suffix in "[a-zA-Z0-9]{0,24}") {
        let marker = "privatecanary7781";
        let policy = Policy::from_file(
            Policy::parse(b"words:\n  - {term: privatecanary7781, action: redact, id: masked}\n").unwrap(),
            "redact-property".into(),
        ).unwrap();
        let (root, store, api) = fixture("redact-property", policy);
        let id = target(&store, "Public Cart Architecture");
        std::fs::write(root.join("note.md"), format!("Public Cart Architecture uses {marker} {suffix}."))
            .unwrap();
        let report = ingest(&root, &store, &api);
        proptest::prop_assert!(mentioned(&store).contains(&id.to_string()));
        let report = serde_json::to_string(&report).unwrap();
        proptest::prop_assert!(!report.contains(marker));
        for node in kaban_core::IngestStore::list_nodes(store.as_ref(), "Chunk", WorkspaceId::default().as_str(), 0, 500)
            .unwrap()
        {
            proptest::prop_assert!(!node.text.contains(marker));
            proptest::prop_assert!(!serde_json::to_string(&node.props).unwrap().contains(marker));
        }
    }
}

fn target(store: &SqliteStore, title: &str) -> NodeId {
    match store
        .remember(
            NewNode {
                kind: "Fact".into(),
                text: title.into(),
                ..Default::default()
            },
            &[],
            &WriteCtx::new("v1", WorkspaceId::default(), Actor::Agent),
        )
        .unwrap()
    {
        RememberOutcome::Created(n) | RememberOutcome::Existing { node: n, .. } => {
            NodeId::parse(&n.id).unwrap()
        }
    }
}

fn ingest(
    root: &Scratch,
    store: &Arc<SqliteStore>,
    api: &Arc<KabanApi>,
) -> kaban_ingest::IngestReport {
    ingest_with(root, store, api, IngestConfig::default())
}

fn ingest_with(
    root: &Scratch,
    store: &Arc<SqliteStore>,
    api: &Arc<KabanApi>,
    cfg: IngestConfig,
) -> kaban_ingest::IngestReport {
    let preparer = Arc::new(DocumentPreparer::new(
        api,
        store.clone(),
        store.signer(),
        ChunkConfig::default(),
        cfg.clone(),
    ));
    let runner = Ingestor::new(api.clone(), store.clone(), store.signer(), preparer, cfg);
    run_blocking(
        &runner,
        &WalkConfig::new(root.to_path_buf()),
        CancellationToken::new(),
    )
    .unwrap()
}

#[test]
fn unchanged_root_converges_cross_document_mentions_then_skips() {
    let (root, store, api) = fixture("root-convergence", Policy::empty());
    std::fs::write(
        root.join("alpha.md"),
        "# Alpha Guide\n\nSee Cart Architecture for details.\n",
    )
    .unwrap();
    std::fs::write(
        root.join("beta.md"),
        "# Cart Architecture\n\nPublic system overview.\n",
    )
    .unwrap();
    assert_eq!(ingest(&root, &store, &api).documents, 2);
    let target = kaban_core::IngestStore::list_nodes(store.as_ref(), "Document", "default", 0, 100)
        .unwrap()
        .into_iter()
        .find(|n| n.props["path"] == "beta.md")
        .unwrap()
        .id;
    assert!(!mentioned(&store).contains(&target));
    ingest(&root, &store, &api);
    assert!(
        mentioned(&store).contains(&target),
        "second unchanged root ingestion must admit newly available cross-document dictionary targets"
    );
    let third = ingest(&root, &store, &api);
    assert_eq!((third.documents, third.skipped), (0, 2));
}

#[test]
fn root_target_title_and_alias_changes_invalidate_referring_document() {
    let (root, store, api) = fixture("root-title-alias", Policy::empty());
    std::fs::write(
        root.join("alpha.md"),
        "# Alpha Guide\n\nSee New Cart Architecture and Cart Alias for details.\n",
    )
    .unwrap();
    std::fs::write(
        root.join("beta.md"),
        "# Original Cart Design\n\nPublic system overview.\n",
    )
    .unwrap();
    ingest(&root, &store, &api);
    ingest(&root, &store, &api);
    let target = kaban_core::IngestStore::list_nodes(store.as_ref(), "Document", "default", 0, 100)
        .unwrap()
        .into_iter()
        .find(|n| n.props["path"] == "beta.md")
        .unwrap()
        .id;
    std::fs::write(root.join("beta.md"), "---\ntitle: New Cart Architecture\naliases: [Cart Alias]\n---\n\nPublic system overview.\n").unwrap();
    ingest(&root, &store, &api);
    ingest(&root, &store, &api);
    assert!(
        mentioned(&store).contains(&target),
        "title/alias changes in another walked Document must invalidate the referring document"
    );
    let settled = ingest(&root, &store, &api);
    assert_eq!((settled.documents, settled.skipped), (0, 2));

    // A canonical alias-only edit also belongs to the referring document's dictionary
    // identity. This uses an already-supported top-level alias, not a new frontmatter rule.
    std::fs::write(
        root.join("alpha.md"),
        "# Alpha Guide\n\nSee Cart Alias for details.\n",
    )
    .unwrap();
    ingest(&root, &store, &api);
    ingest(&root, &store, &api);
    assert!(!mentioned(&store).contains(&target));
    let id = NodeId::parse(&target).unwrap();
    let mut node = store.get(&id, false).unwrap().unwrap();
    node.props["aliases"] = serde_json::json!(["Cart Alias"]);
    let batch = kaban_core::WriteBatch::new(
        WorkspaceId::default(),
        vec![kaban_core::Op::UpsertNode {
            id,
            node: NewNode {
                kind: node.kind,
                text: node.text,
                summary: node.summary,
                props: Some(node.props),
                ..Default::default()
            },
        }],
    );
    kaban_core::IngestStore::apply_batch(
        store.as_ref(),
        batch,
        &WriteCtx::new("builtin", WorkspaceId::default(), Actor::Agent),
    )
    .unwrap();
    ingest(&root, &store, &api);
    let chunks =
        kaban_core::IngestStore::list_nodes(store.as_ref(), "Chunk", "default", 0, 100).unwrap();
    let mentions = chunks
        .iter()
        .flat_map(|chunk| {
            store
                .traverse(&TraverseQuery {
                    start_ids: vec![NodeId::parse(&chunk.id).unwrap()],
                    rels: vec!["MENTIONS".into()],
                    max_depth: 1,
                    ..Default::default()
                })
                .unwrap()
                .edges
        })
        .filter(|edge| edge.dst == target)
        .collect::<Vec<_>>();
    assert!(
        mentions.iter().any(|edge| edge.props["method"] == "alias"),
        "alias-only target changes must be observed even within the same walk"
    );
    // The target's own source reingestion replaces this explicit alias. That dictionary
    // removal must converge as well, then the settled third pass is unchanged again.
    ingest(&root, &store, &api);
    let settled = ingest(&root, &store, &api);
    assert_eq!((settled.documents, settled.skipped), (0, 2));
}

#[test]
fn disabling_linking_retires_prior_gazetteer_mentions() {
    let (root, store, api) = fixture("off-retirement", Policy::empty());
    let id = target(&store, "Cart Discount Architecture");
    std::fs::write(
        root.join("note.md"),
        "Cart Discount Architecture governs checkout.",
    )
    .unwrap();
    ingest(&root, &store, &api);
    assert!(mentioned(&store).contains(&id.to_string()));
    let documents = kaban_core::IngestStore::list_nodes(
        store.as_ref(),
        "Document",
        WorkspaceId::default().as_str(),
        0,
        500,
    )
    .unwrap();
    let document = NodeId::parse(&documents[0].id).unwrap();
    let on_generation = kaban_core::IngestStore::gazetteer_generation(
        store.as_ref(),
        &document,
        &WorkspaceId::default(),
    )
    .unwrap();
    ingest_with(
        &root,
        &store,
        &api,
        IngestConfig {
            link: Some("none".into()),
            ..Default::default()
        },
    );
    assert_eq!(
        mentioned(&store).len(),
        0,
        "disabled linking must retire prior gazetteer MENTIONS"
    );
    let off_generation = kaban_core::IngestStore::gazetteer_generation(
        store.as_ref(),
        &document,
        &WorkspaceId::default(),
    )
    .unwrap();
    assert!(off_generation > on_generation);
    ingest(&root, &store, &api);
    assert!(
        mentioned(&store).contains(&id.to_string()),
        "turning linking back on restores its own producer set"
    );
    let resumed_generation = kaban_core::IngestStore::gazetteer_generation(
        store.as_ref(),
        &document,
        &WorkspaceId::default(),
    )
    .unwrap();
    assert!(resumed_generation > off_generation);
    let repeated = ingest(&root, &store, &api);
    assert_eq!(repeated.outbox_rows, 0);
    assert_eq!(
        kaban_core::IngestStore::gazetteer_generation(
            store.as_ref(),
            &document,
            &WorkspaceId::default(),
        )
        .unwrap(),
        resumed_generation
    );
}

#[test]
fn turning_off_linking_preserves_human_mentions_and_other_document_claims() {
    let (root, store, api) = fixture("retirement-authority", Policy::empty());
    let automatic = target(&store, "Public Cart Architecture");
    let human = target(&store, "Human Override Target");
    std::fs::write(
        root.join("a.md"),
        "Public Cart Architecture governs checkout.",
    )
    .unwrap();
    std::fs::write(
        root.join("b.md"),
        "Public Cart Architecture documents checkout.",
    )
    .unwrap();
    ingest(&root, &store, &api);
    let chunks = kaban_core::IngestStore::list_nodes(
        store.as_ref(),
        "Chunk",
        WorkspaceId::default().as_str(),
        0,
        500,
    )
    .unwrap();
    let first = chunks
        .iter()
        .find(|node| node.text.contains("governs"))
        .unwrap();
    let first = NodeId::parse(&first.id).unwrap();
    store
        .link(
            &first,
            &human,
            "MENTIONS",
            None,
            None,
            &WriteCtx::new("builtin", WorkspaceId::default(), Actor::HumanCli),
        )
        .unwrap();
    let cfg = IngestConfig {
        link: Some("none".into()),
        ..Default::default()
    };
    let preparer = Arc::new(DocumentPreparer::new(
        &api,
        store.clone(),
        store.signer(),
        ChunkConfig::default(),
        cfg.clone(),
    ));
    let runner = Ingestor::new(api.clone(), store.clone(), store.signer(), preparer, cfg);
    let mut walk = WalkConfig::new(root.to_path_buf());
    walk.roots = vec![root.join("a.md")];
    run_blocking(&runner, &walk, CancellationToken::new()).unwrap();
    let edges = store
        .traverse(&TraverseQuery {
            start_ids: vec![first],
            rels: vec!["MENTIONS".into()],
            max_depth: 1,
            ..Default::default()
        })
        .unwrap()
        .edges;
    assert!(edges.iter().any(|edge| edge.dst == human.as_str()));
    assert!(!edges.iter().any(|edge| edge.dst == automatic.as_str()));
    assert!(
        mentioned(&store).contains(&automatic.to_string()),
        "the other document's producer claims are not retired"
    );
}

#[test]
fn matcher_maps_unicode_offsets_and_rejects_partial_words() {
    let (_root, store, _api) = fixture("offsets", Policy::empty());
    let id = target(&store, "Cafe Architecture");
    let dictionary = kaban_ingest::build_dictionary(
        store.as_ref(),
        &WorkspaceId::default(),
        None,
        &Policy::empty(),
        &kaban_core::config::Gazetteer::default(),
    )
    .unwrap();
    let text = "π Café Architecture! Cafe Architectures";
    let hits = kaban_ingest::find_mentions(&dictionary, &NodeId::new(), text);
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].target, id);
    assert_eq!(
        &text[usize::try_from(hits[0].offset).unwrap()
            ..usize::try_from(hits[0].offset + hits[0].len).unwrap()],
        "Café Architecture"
    );
}

#[test]
fn matcher_keeps_ambiguity_candidate_only_and_excludes_short_stopwords() {
    let (_root, store, _api) = fixture("ambiguity", Policy::empty());
    target(&store, "Shared Architecture\nfirst");
    target(&store, "Shared Architecture\nsecond");
    target(&store, "the");
    let dictionary = kaban_ingest::build_dictionary(
        store.as_ref(),
        &WorkspaceId::default(),
        None,
        &Policy::empty(),
        &kaban_core::config::Gazetteer {
            max_targets: 1,
            ..Default::default()
        },
    )
    .unwrap();
    let hits = kaban_ingest::find_mentions(&dictionary, &NodeId::new(), "the Shared Architecture");
    assert_eq!(hits.len(), 2);
    assert!(hits.iter().all(|hit| hit.ambiguous));
}

#[test]
fn explicit_none_and_dry_run_never_write_mentions() {
    for (name, cfg) in [
        (
            "none",
            IngestConfig {
                link: Some("none".into()),
                ..Default::default()
            },
        ),
        (
            "dry",
            IngestConfig {
                dry_run: true,
                ..Default::default()
            },
        ),
    ] {
        let (root, store, api) = fixture(name, Policy::empty());
        target(&store, "Cart Discount Architecture");
        std::fs::write(
            root.join("note.md"),
            "Cart Discount Architecture governs checkout.",
        )
        .unwrap();
        let dry_run = cfg.dry_run;
        let report = ingest_with(&root, &store, &api, cfg);
        assert_eq!(
            report.documents, 1,
            "successful and projected documents remain counted"
        );
        assert!(report.chunks_upserted > 0);
        assert_eq!(report.outbox_rows == 0, dry_run);
        assert_eq!(mentioned(&store).len(), 0);
    }
}

fn mentioned(store: &SqliteStore) -> Vec<String> {
    let chunks = kaban_core::IngestStore::list_nodes(
        store,
        "Chunk",
        WorkspaceId::default().as_str(),
        0,
        500,
    )
    .unwrap();
    if chunks.is_empty() {
        return Vec::new();
    }
    store
        .traverse(&TraverseQuery {
            start_ids: chunks
                .iter()
                .map(|n| NodeId::parse(&n.id).unwrap())
                .collect(),
            rels: vec!["MENTIONS".into()],
            max_depth: 1,
            ..Default::default()
        })
        .unwrap()
        .edges
        .into_iter()
        .filter(|e| e.rel == "MENTIONS")
        .map(|e| e.dst)
        .collect()
}

#[test]
fn configured_gazetteer_writes_permitted_mentions_and_is_idempotent() {
    let (root, store, api) = fixture("producer", Policy::empty());
    let id = target(&store, "Cart Discount Architecture");
    std::fs::write(
        root.join("note.md"),
        "# Shopping\n\nCart Discount Architecture governs checkout.\n",
    )
    .unwrap();
    assert_eq!(ingest(&root, &store, &api).documents, 1);
    assert!(
        mentioned(&store).contains(&id.to_string()),
        "permitted configured gazetteer MENTIONS is missing"
    );
    assert_eq!(ingest(&root, &store, &api).outbox_rows, 0);
}

#[test]
fn dictionary_read_hides_rejected_targets_but_keeps_clean_targets() {
    let policy = Policy::from_file(
        Policy::parse(b"words:\n  - {term: forbidden-canary-7781, action: reject, id: hidden}\n")
            .unwrap(),
        "gate".into(),
    )
    .unwrap();
    let (root, store, api) = fixture("gate", policy);
    let hidden = target(&store, "forbidden-canary-7781");
    store.remember(NewNode { kind: "Fact".into(), text: "Otherwise clean target".into(), props: Some(serde_json::json!({"title": "forbidden-canary-7781", "aliases": ["Public Hidden Alias"]})), ..Default::default() }, &[], &WriteCtx::new("v1", WorkspaceId::default(), Actor::Agent)).unwrap();
    let clean = target(&store, "Public Cart Architecture");
    std::fs::write(
        root.join("note.md"),
        "# Shopping\n\nPublic Cart Architecture governs checkout. Public Hidden Alias is not admitted.\n",
    )
    .unwrap();
    ingest(&root, &store, &api);
    let hits = mentioned(&store);
    assert!(
        !hits.contains(&hidden.to_string()),
        "hidden dictionary target must not produce an edge"
    );
    assert!(
        hits.contains(&clean.to_string()),
        "clean dictionary source must still produce MENTIONS"
    );
    assert_eq!(
        hits.len(),
        1,
        "a rejected title hides even a clean alias target"
    );
}

#[test]
fn edited_document_retires_disappeared_mentions() {
    let (root, store, api) = fixture("edited", Policy::empty());
    let id = target(&store, "Cart Discount Architecture");
    std::fs::write(
        root.join("note.md"),
        "Cart Discount Architecture governs checkout.",
    )
    .unwrap();
    ingest(&root, &store, &api);
    assert!(mentioned(&store).contains(&id.to_string()));
    std::fs::write(root.join("note.md"), "Unrelated replacement text.").unwrap();
    ingest(&root, &store, &api);
    assert_eq!(
        mentioned(&store).len(),
        0,
        "edited document must retire disappeared MENTIONS"
    );
}

#[test]
fn dictionary_masks_title_and_alias_patterns_before_matching() {
    let policy = Policy::from_file(
        Policy::parse(b"words:\n  - {term: privatecanary7781, action: redact, id: masked}\n")
            .unwrap(),
        "mask".into(),
    )
    .unwrap();
    let (_root, store, _api) = fixture("masked-patterns", Policy::empty());
    let outcome = store.remember(
        NewNode {
            kind: "Fact".into(), text: "Clean record body".into(),
            props: Some(serde_json::json!({"title": "privatecanary7781", "aliases": ["privatecanary7781", "Public Cart Alias"]})),
            ..Default::default()
        }, &[], &WriteCtx::new("v1", WorkspaceId::default(), Actor::Agent),
    ).unwrap();
    let id = match outcome {
        RememberOutcome::Created(node) | RememberOutcome::Existing { node, .. } => {
            NodeId::parse(&node.id).unwrap()
        }
    };
    let dictionary = kaban_ingest::build_dictionary(
        store.as_ref(),
        &WorkspaceId::default(),
        None,
        &policy,
        &kaban_core::config::Gazetteer::default(),
    )
    .unwrap();
    assert_eq!(
        kaban_ingest::find_mentions(&dictionary, &NodeId::new(), "privatecanary7781").len(),
        0
    );
    let safe = kaban_ingest::find_mentions(&dictionary, &NodeId::new(), "Public Cart Alias");
    assert_eq!(safe.len(), 1);
    assert_eq!(safe[0].target, id);
    assert!(!format!("{dictionary:?}").contains("privatecanary7781"));
}

#[test]
fn document_frequency_excludes_lowercase_common_words_not_named_titles() {
    let (root, store, api) = fixture("frequency", Policy::empty());
    target(&store, "architecture");
    let named = target(&store, "Public Cart Architecture");
    std::fs::write(
        root.join("note.md"),
        "architecture is mentioned in this document.",
    )
    .unwrap();
    ingest_with(
        &root,
        &store,
        &api,
        IngestConfig {
            link: Some("none".into()),
            ..Default::default()
        },
    );
    let dictionary = kaban_ingest::build_dictionary(
        store.as_ref(),
        &WorkspaceId::default(),
        None,
        &Policy::empty(),
        &kaban_core::config::Gazetteer::default(),
    )
    .unwrap();
    let hits = kaban_ingest::find_mentions(
        &dictionary,
        &NodeId::new(),
        "architecture Public Cart Architecture",
    );
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].target, named);
}
