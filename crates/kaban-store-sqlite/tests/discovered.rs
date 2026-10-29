//! Discovered labels, candidate state and `kind_stats` (engine-spec EN-F5, EN-F5a, EN-F6,
//! EN-F49, EN-S3; engine-plan E1.8): an undeclared label written through `apply_batch` or
//! `remember` is placed under the loaded `discovered.policy` (`review` by default: an `x:` kind
//! with one `node_candidate` row, counted in `kind_stats`, invisible to the read paths unless
//! `include_candidates`), capped per workspace, promotable by a human and prunable.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::{ctx, nid};
use kaban_core::{
    IngestStore, MemoryStore, NewNode, NodeId, Op, WorkspaceId, WriteBatch, WriteCtx,
};
use kaban_store_sqlite::SqliteStore;

fn upsert(id: &NodeId, kind: &str, text: &str) -> Op {
    Op::UpsertNode {
        id: id.clone(),
        node: NewNode {
            kind: kind.into(),
            text: text.into(),
            ..Default::default()
        },
    }
}

/// Slice 1 (tracer bullet): under the default `review` policy an undeclared label `Vendor`
/// written by a batch is stored as the `x:Vendor` kind, counted once in the workspace's
/// `kind_stats`, and the node is not found by `recall` (it is a candidate).
#[test]
fn an_undeclared_label_becomes_a_counted_x_candidate_invisible_to_recall() {
    let store = SqliteStore::open_memory().unwrap();
    let ws = WorkspaceId::default();
    let id = NodeId::parse(&kaban_core::new_id()).unwrap();
    let report = store
        .apply_batch(
            WriteBatch::new(
                ws.clone(),
                vec![upsert(&id, "Vendor", "acme supplies widgets")],
            ),
            &ctx(),
        )
        .unwrap();
    assert_eq!(report.inserted, 1, "{report:?}");

    let node = store
        .get(&id, false)
        .unwrap()
        .expect("the candidate node exists");
    assert_eq!(node.kind, "x:Vendor");
    assert_eq!(node.governance, kaban_core::Governance::None);

    let stats = store.kind_stats(&ws).unwrap();
    assert_eq!(stats.len(), 1, "{stats:?}");
    assert_eq!(stats[0].ns, "x");
    assert_eq!(stats[0].kind, "Vendor");
    assert_eq!(stats[0].n_nodes, 1);
    assert_eq!(stats[0].n_sources, 1);
    assert!(stats[0].mean_score.is_none(), "mean_score is E2's");

    let hits = store
        .recall(&kaban_core::RecallQuery {
            query: "widgets".into(),
            limit: 10,
            ..Default::default()
        })
        .unwrap();
    assert!(
        hits.is_empty(),
        "a candidate never seeds a recall: {hits:?}"
    );
}

/// The 600 labels of `fixtures/discovered-600.json` (`Label000`..`Label599`, the documented
/// deterministic rule).
fn fixture_labels() -> Vec<String> {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/discovered-600.json"
    );
    let doc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let labels: Vec<String> = doc["labels"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_owned())
        .collect();
    assert_eq!(labels.len(), 600);
    assert_eq!(labels[7], "Label007");
    labels
}

/// One batch per label (one node each, a fresh id per pass) in `ws`.
fn write_labels(store: &SqliteStore, ws: &WorkspaceId, labels: &[String]) -> u32 {
    let mut ctx = ctx();
    ctx.workspace_id = ws.clone();
    let mut inserted = 0;
    for label in labels {
        let id = NodeId::parse(&kaban_core::new_id()).unwrap();
        let report = store
            .apply_batch(
                WriteBatch::new(
                    ws.clone(),
                    vec![upsert(&id, label, &format!("a {label} mention"))],
                ),
                &ctx,
            )
            .unwrap();
        inserted += report.inserted;
    }
    inserted
}

fn x_type_rows(store: &SqliteStore) -> usize {
    kaban_core::SchemaStore::schema(store)
        .unwrap()
        .iter()
        .filter(|d| d.ns.as_deref() == Some("x"))
        .count()
}

fn n_rejected(store: &SqliteStore, ws: &WorkspaceId) -> u64 {
    store
        .prune_kinds(
            ws,
            &kaban_core::config::PromoteWhen::default(),
            std::time::Duration::from_secs(1),
            true,
        )
        .unwrap()
        .n_rejected
}

/// Slice 2 (EN-F5 cap, EN-S3): 600 distinct labels in workspace A under the default cap of
/// 500 give 500 `kind_stats` rows, `n_rejected = 100` and 500 `x:` type rows; a second pass
/// of the same 600 changes none of the counts except the rejections (counted again, never
/// reset); the same 600 in workspace B yield B's own 500 and leave A untouched.
#[test]
fn six_hundred_labels_stop_at_the_cap_per_workspace_and_a_second_pass_adds_nothing() {
    let store = SqliteStore::open_memory().unwrap();
    let labels = fixture_labels();
    let a = WorkspaceId::new("ws-a");
    let b = WorkspaceId::new("ws-b");

    assert_eq!(write_labels(&store, &a, &labels), 500);
    assert_eq!(store.kind_stats(&a).unwrap().len(), 500);
    assert_eq!(n_rejected(&store, &a), 100);
    assert_eq!(x_type_rows(&store), 500);

    // second pass: the 500 known kinds take one more node each, the 100 are refused again
    assert_eq!(write_labels(&store, &a, &labels), 500);
    let stats = store.kind_stats(&a).unwrap();
    assert_eq!(stats.len(), 500);
    assert!(stats.iter().all(|s| s.n_nodes == 2), "{:?}", &stats[..2]);
    assert_eq!(n_rejected(&store, &a), 200);
    assert_eq!(x_type_rows(&store), 500);

    // workspace B is independent (EN-S3)
    assert_eq!(write_labels(&store, &b, &labels), 500);
    assert_eq!(store.kind_stats(&b).unwrap().len(), 500);
    assert_eq!(n_rejected(&store, &b), 100);
    assert_eq!(store.kind_stats(&a).unwrap().len(), 500);
    assert_eq!(n_rejected(&store, &a), 200);
    assert_eq!(
        x_type_rows(&store),
        500,
        "the type rows are shared, the cap is per workspace"
    );
}

/// A `remember` of `kind`/`text` in the default workspace; returns the node.
fn remembered(store: &SqliteStore, kind: &str, text: &str) -> kaban_core::Node {
    match store
        .remember(
            NewNode {
                kind: kind.into(),
                text: text.into(),
                path_scope: vec!["src/billing/**".into()],
                ..Default::default()
            },
            &[],
            &ctx(),
        )
        .unwrap()
    {
        kaban_core::RememberOutcome::Created(n) => n,
        other @ kaban_core::RememberOutcome::Existing { .. } => panic!("fresh store: {other:?}"),
    }
}

/// Slice 3 (EN-F49, EN-F5a): a candidate is absent from `recall`, `traverse` (as a start and
/// as a far endpoint) and `why` (`governing`) by default, and present in all three with
/// `include_candidates`; a declared neighbour is unaffected.
#[test]
fn a_candidate_is_hidden_from_recall_traverse_and_why_unless_included() {
    use kaban_core::{Governance, GoverningQuery, ReadFilters, TraverseQuery, WorkspaceFilter};
    let store = SqliteStore::open_memory().unwrap();
    let fact = remembered(&store, "Fact", "the billing fact about quokkas");
    let cand = remembered(&store, "Policy", "the quokka billing policy candidate");
    assert_eq!(cand.kind, "x:Policy");
    store
        .link(
            &nid(&fact.id),
            &nid(&cand.id),
            "RELATES_TO",
            None,
            None,
            &ctx(),
        )
        .unwrap();
    let include = ReadFilters {
        include_candidates: true,
    };

    // recall: the candidate neither seeds nor joins through the hop
    let recall = |filters: ReadFilters| {
        store
            .recall(&kaban_core::RecallQuery {
                query: "quokkas".into(),
                hops: 1,
                limit: 10,
                filters,
                ..Default::default()
            })
            .unwrap()
            .into_iter()
            .map(|h| h.node.id)
            .collect::<Vec<_>>()
    };
    assert_eq!(recall(ReadFilters::default()), vec![fact.id.clone()]);
    assert!(recall(include).contains(&cand.id), "{:?}", recall(include));

    // traverse: not a far endpoint, not a start
    let traverse = |start: &str, filters: ReadFilters| {
        store.traverse(&TraverseQuery {
            start_ids: vec![nid(start)],
            rels: vec![],
            direction: kaban_core::TraverseDirection::Both,
            max_depth: 1,
            node_kinds: vec![],
            governance: vec![],
            limit: 10,
            workspace_id: None,
            filters,
            ..Default::default()
        })
    };
    let from_fact = traverse(&fact.id, ReadFilters::default()).unwrap();
    assert!(
        from_fact.nodes.iter().all(|n| n.id != cand.id),
        "{from_fact:?}"
    );
    assert!(
        traverse(&cand.id, ReadFilters::default()).is_err(),
        "a candidate start is absent"
    );
    let from_fact = traverse(&fact.id, include).unwrap();
    assert!(
        from_fact.nodes.iter().any(|n| n.id == cand.id),
        "{from_fact:?}"
    );
    assert!(traverse(&cand.id, include).is_ok());

    // why: a candidate record never governs a path
    let governing = |filters: ReadFilters| {
        kaban_core::GovernanceStore::governing(
            &store,
            &GoverningQuery {
                kinds: vec!["x:Policy".into(), "Fact".into()],
                governance: vec![Governance::None],
                workspace: WorkspaceFilter::All,
                path_candidates: vec!["src/billing/invoice.rs".into(), "src/billing".into()],
                query: None,
                limit: 10,
                offset: 0,
                filters,
                anchor_prefilter: true,
            },
        )
        .unwrap()
        .into_iter()
        .map(|n| n.id)
        .collect::<Vec<_>>()
    };
    assert_eq!(governing(ReadFilters::default()), vec![fact.id.clone()]);
    assert_eq!(governing(include).len(), 2);
}

fn declare(store: &SqliteStore, stored: &str) {
    kaban_core::SchemaStore::schema_extend(
        store,
        kaban_core::TypeDef {
            kind: "label".into(),
            name: stored.into(),
            ns: None,
            description: stored.into(),
            from_kinds: vec![],
            to_kinds: vec![],
            directed: true,
            props_schema: None,
            builtin: false,
            declaration: kaban_core::TypeDeclaration::default(),
        },
    )
    .unwrap();
}

fn event_changes_after(store: &SqliteStore, after: u64) -> usize {
    kaban_core::ChangeLogStore::changes_read(store, after, 1_000)
        .unwrap()
        .iter()
        .filter(|c| c.entity == kaban_core::ChangeEntity::Event)
        .count()
}

/// Slice 4 (EN-F5 promote, EN-F5a): `promote x:Vendor infra:Vendor` by a human renames the
/// three candidate nodes, moves the `kind_stats` row under `infra:Vendor`, deletes the
/// `discovered_kind` rows (the nodes become visible), drops the unreferenced `x:` type row and
/// writes exactly one `events` row. Negatives: an agent is refused (`human_only`), an
/// undeclared target is `unknown_kind`, nothing written either time.
#[test]
fn promote_renames_the_nodes_releases_the_candidates_and_writes_one_event() {
    use kaban_core::ontology::{Ns, Qualified};
    let store = SqliteStore::open_memory().unwrap();
    let ws = WorkspaceId::default();
    for i in 0..3 {
        remembered(&store, "Vendor", &format!("quokka vendor number {i}"));
    }
    declare(&store, "infra:Vendor");
    let infra_vendor = Qualified {
        ns: Ns::parse("infra").unwrap(),
        name: "Vendor".into(),
    };
    let before = kaban_core::ChangeLogStore::changes_read(&store, 0, 10_000)
        .unwrap()
        .last()
        .map_or(0, |c| c.seq);
    let recall_vendors = || {
        store
            .recall(&kaban_core::RecallQuery {
                query: "quokka vendor".into(),
                limit: 10,
                ..Default::default()
            })
            .unwrap()
            .len()
    };
    assert_eq!(recall_vendors(), 0);

    // negatives first: nothing moves
    let agent = store.promote_kind(&ws, "x:Vendor", &infra_vendor, &kaban_core::Actor::Agent);
    assert!(
        matches!(agent, Err(kaban_core::StoreError::HumanOnly(_))),
        "{agent:?}"
    );
    let undeclared = Qualified {
        ns: Ns::parse("infra").unwrap(),
        name: "Supplier".into(),
    };
    let unknown = store.promote_kind(&ws, "x:Vendor", &undeclared, &kaban_core::Actor::HumanCli);
    assert_eq!(
        unknown.unwrap_err().to_string(),
        "unknown_kind",
        "the fixed EN-F7 text"
    );
    assert_eq!(store.kind_stats(&ws).unwrap()[0].ns, "x");
    assert_eq!(event_changes_after(&store, before), 0);

    let report = store
        .promote_kind(&ws, "x:Vendor", &infra_vendor, &kaban_core::Actor::HumanCli)
        .unwrap();
    assert_eq!((report.nodes, report.candidates_released), (3, 3));
    assert_eq!(report.to, "infra:Vendor");
    assert_eq!(event_changes_after(&store, before), 1, "exactly one event");
    let stats = store.kind_stats(&ws).unwrap();
    assert_eq!(stats.len(), 1);
    assert_eq!(
        (
            stats[0].ns.as_str(),
            stats[0].kind.as_str(),
            stats[0].n_nodes
        ),
        ("infra", "Vendor", 3)
    );
    assert_eq!(x_type_rows(&store), 0, "the x: type row is gone");
    assert_eq!(recall_vendors(), 3, "released candidates are visible");
    assert_eq!(
        store
            .list_nodes("infra:Vendor", ws.as_str(), 0, 10)
            .unwrap()
            .len(),
        3
    );
}

/// Slice 5 (EN-F5 prune, cap release): with a cap of 12, twelve single-node kinds fill the
/// workspace and a thirteenth is refused; `prune --dry-run` lists the ten kinds below
/// `min_count = 2` whose `last_seen` is at least zero seconds old and writes nothing; `prune`
/// tombstones exactly their ten nodes and keeps the two kinds with two nodes; afterwards ten
/// new labels are accepted and the eleventh is refused again. `n_rejected` is never reset.
#[test]
fn prune_lists_then_tombstones_only_below_the_thresholds_and_releases_the_cap() {
    use kaban_core::config::PromoteWhen;
    use std::time::Duration;
    let store = SqliteStore::open_memory().unwrap();
    let ws = WorkspaceId::default();
    let mut ctx = ctx();
    ctx.discovered.cap = 12;
    let write = |label: &str, text: &str| {
        let id = NodeId::parse(&kaban_core::new_id()).unwrap();
        store
            .apply_batch(
                WriteBatch::new(ws.clone(), vec![upsert(&id, label, text)]),
                &ctx,
            )
            .unwrap()
    };
    for i in 0..12 {
        write(&format!("Kind{i:02}"), &format!("quokka text {i}"));
    }
    // two kinds reach two nodes
    write("Kind00", "second quokka text for kind zero");
    write("Kind01", "second quokka text for kind one");
    let refused = write("Kind12", "beyond the cap");
    assert_eq!((refused.inserted, refused.skipped), (0, 1), "{refused:?}");
    assert_eq!(
        refused.outcomes[0].outcome,
        kaban_core::OutcomeKind::CapReached
    );

    let below = PromoteWhen {
        min_count: 2,
        ..PromoteWhen::default()
    };
    let dry = store
        .prune_kinds(&ws, &below, Duration::ZERO, true)
        .unwrap();
    assert!(dry.dry_run);
    assert_eq!(dry.kinds.len(), 10, "{dry:?}");
    assert_eq!(dry.kinds[0], "x:Kind02");
    assert_eq!((dry.nodes_tombstoned, dry.n_rejected), (0, 1));
    assert_eq!(
        store.kind_stats(&ws).unwrap().len(),
        12,
        "a dry run writes nothing"
    );
    assert_eq!(
        store
            .list_nodes("x:Kind02", ws.as_str(), 0, 10)
            .unwrap()
            .len(),
        1
    );

    // a one-day age keeps everything (nothing is that old)
    let young = store
        .prune_kinds(&ws, &below, Duration::from_hours(24), false)
        .unwrap();
    assert!(young.kinds.is_empty(), "{young:?}");

    let wet = store
        .prune_kinds(&ws, &below, Duration::ZERO, false)
        .unwrap();
    assert_eq!(wet.kinds, dry.kinds);
    assert_eq!(wet.nodes_tombstoned, 10);
    let stats = store.kind_stats(&ws).unwrap();
    assert_eq!(stats.len(), 2);
    assert!(stats.iter().all(|s| s.n_nodes == 2), "{stats:?}");
    assert_eq!(
        store
            .list_nodes("x:Kind02", ws.as_str(), 0, 10)
            .unwrap()
            .len(),
        0,
        "tombstoned"
    );
    assert_eq!(x_type_rows(&store), 2);

    // the cap is released: ten new labels are accepted, the eleventh is refused
    for i in 20..30 {
        assert_eq!(
            write(&format!("Kind{i}"), &format!("quokka text {i}")).inserted,
            1
        );
    }
    let refused = write("Kind30", "beyond the cap again");
    assert_eq!(refused.skipped, 1);
    assert_eq!(n_rejected(&store, &ws), 2, "reported, never reset");
}

/// The governed set of the shipped files plus `fixtures/ontology/incidents.yaml` (its
/// `Incident` is governed), without the `incidents` kinds declared in the store: a label
/// `Incident` is then undeclared and shadows a governed kind.
fn incidents_governed() -> std::sync::Arc<kaban_core::GovernedSet> {
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../kaban-core/fixtures/ontology/incidents.yaml");
    let text = std::fs::read_to_string(&fixture).unwrap();
    let mut files = kaban_core::shipped::files().unwrap();
    files.push(kaban_core::OntologyFile::from_yaml(&text).unwrap());
    let ontology = kaban_core::resolve(&files, &kaban_core::NamespaceSet::new()).unwrap();
    std::sync::Arc::new(kaban_core::GovernedSet::from_ontology(&ontology))
}

/// Slice 7 (EN-F5 policies and negatives): under `deny` an undeclared label is skipped with
/// `kind_not_allowed` and nothing is written; under `allow` it is stored `proposed` under `x:`
/// with no candidate row (visible to `recall`); a label that shadows a governed kind
/// (`Incident`, governed in the tenant's set but not declared in this store) is
/// `kind_not_allowed` under every policy; a label failing the identifier regex is
/// `invalid_identifier` and never stored; an undeclared foreign qualified name stays
/// `UnknownKind`.
#[test]
fn deny_skips_allow_proposes_governed_names_and_bad_identifiers_are_never_stored() {
    use kaban_core::ontology::meta::DiscoveredPolicyName;
    use kaban_core::{DiscoveredRules, Governance, OutcomeKind, StoreError};
    let store = SqliteStore::open_memory().unwrap();
    let ws = WorkspaceId::default();
    let rules = |policy| DiscoveredRules { policy, cap: 500 };
    let batch = |label: &str, ctx: &kaban_core::WriteCtx| {
        let id = NodeId::parse(&kaban_core::new_id()).unwrap();
        store.apply_batch(
            WriteBatch::new(ws.clone(), vec![upsert(&id, label, "a quokka label")]),
            ctx,
        )
    };

    // deny: skipped, nothing written
    let deny = ctx().with_discovered(rules(DiscoveredPolicyName::Deny));
    let report = batch("Vendor", &deny).unwrap();
    assert_eq!((report.inserted, report.skipped), (0, 1), "{report:?}");
    assert_eq!(report.outcomes[0].outcome, OutcomeKind::KindNotAllowed);
    assert_eq!(store.kind_stats(&ws).unwrap().len(), 0);
    assert_eq!(x_type_rows(&store), 0);
    let err = store
        .remember(
            NewNode {
                kind: "Vendor".into(),
                text: "a quokka label".into(),
                ..Default::default()
            },
            &[],
            &deny,
        )
        .unwrap_err();
    assert!(matches!(err, StoreError::KindNotAllowed), "{err:?}");
    assert_eq!(err.to_string(), "kind_not_allowed");

    // allow: proposed under x:, no candidate row, visible
    let allow = ctx().with_discovered(rules(DiscoveredPolicyName::Allow));
    assert_eq!(batch("Vendor", &allow).unwrap().inserted, 1);
    let nodes = store.list_nodes("x:Vendor", ws.as_str(), 0, 10).unwrap();
    assert_eq!(nodes.len(), 1);
    assert_eq!(nodes[0].governance, Governance::Proposed);
    let hits = store
        .recall(&kaban_core::RecallQuery {
            query: "quokka".into(),
            limit: 10,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(hits.len(), 1, "an allow-placed node is no candidate");

    // a governed name is refused under every policy
    for policy in [
        DiscoveredPolicyName::Review,
        DiscoveredPolicyName::Allow,
        DiscoveredPolicyName::Deny,
    ] {
        let ctx = ctx()
            .with_governed(incidents_governed())
            .with_discovered(rules(policy));
        let report = batch("Incident", &ctx).unwrap();
        assert_eq!(
            report.outcomes[0].outcome,
            OutcomeKind::KindNotAllowed,
            "{policy:?}"
        );
    }
    assert!(
        store
            .list_nodes("x:Incident", ws.as_str(), 0, 10)
            .unwrap()
            .is_empty()
    );

    // the identifier regex: never stored, the batch fails with the fixed text
    for bad in ["vendor", "Vendor-X", "x:vendor"] {
        let err = batch(bad, &ctx()).unwrap_err();
        assert_eq!(err.to_string(), "invalid_identifier", "{bad}: {err:?}");
    }
    // an undeclared foreign qualified name is not a label
    let err = batch("infra:Vendor", &ctx()).unwrap_err();
    assert!(matches!(err, StoreError::UnknownKind(_)), "{err:?}");
    assert_eq!(
        store.kind_stats(&ws).unwrap().len(),
        1,
        "only x:Vendor was ever counted"
    );
}

// ---- carried residuals (E1.7 R1, E1.7a) ----

/// E1.7 R1: the shipped rels' declared endpoints are enforced as data. `adr.yaml` declares
/// `SUPERSEDES {from: [Decision], to: [Decision]}`: a `Fact SUPERSEDES Fact` inline link is
/// refused with the fixed class `EndpointNotAllowed` (through `remember`, `link` and a batch
/// edge alike, one write path); the `core` rels declare `[]` and stay unconstrained
/// (`Fact RELATES_TO Fact`).
#[test]
fn shipped_rel_endpoints_are_enforced_and_core_rels_stay_unconstrained() {
    use kaban_core::{LinkReq, StoreError};
    let store = SqliteStore::open_memory().unwrap();
    let old = remembered(&store, "Fact", "the old quokka fact");
    let link = |rel: &str| {
        store.remember(
            NewNode {
                kind: "Fact".into(),
                text: format!("a newer quokka fact via {rel}"),
                ..Default::default()
            },
            &[LinkReq {
                rel: rel.into(),
                from: None,
                to: Some(nid(&old.id)),
                confidence: None,
            }],
            &ctx(),
        )
    };
    let refused = link("SUPERSEDES").unwrap_err();
    assert!(
        matches!(&refused, StoreError::EndpointNotAllowed { rel, from, to } if rel == "SUPERSEDES" && from == "Fact" && to == "Fact"),
        "{refused:?}"
    );
    assert!(
        link("RELATES_TO").is_ok(),
        "core rels declare [] = unconstrained"
    );
    let direct = store.link(
        &nid(&old.id),
        &nid(&remembered(&store, "Fact", "another quokka fact").id),
        "CAUSED_BY",
        None,
        None,
        &ctx(),
    );
    assert!(
        matches!(direct, Err(StoreError::EndpointNotAllowed { .. })),
        "CAUSED_BY is from Failure only: {direct:?}"
    );
    // the declared lists are visible on the builtin rows
    let supersedes = kaban_core::SchemaStore::schema(&store)
        .unwrap()
        .into_iter()
        .find(|d| d.kind == "rel" && d.name == "SUPERSEDES")
        .unwrap();
    assert_eq!(supersedes.from_kinds, vec!["Decision".to_owned()]);
    assert_eq!(supersedes.to_kinds, vec!["Decision".to_owned()]);
}

/// E1.7a residual: `remember` normalises a qualified spelling of a bare namespace at the
/// record seam: `user:Widget` is the declared `Widget`, stored bare, no `x:` placement.
#[test]
fn remember_normalises_a_qualified_spelling_of_a_bare_namespace() {
    let store = SqliteStore::open_memory().unwrap();
    declare(&store, "Widget");
    let node = remembered(&store, "user:Widget", "a quokka widget");
    assert_eq!(node.kind, "Widget");
    let node = remembered(&store, "adr:Fact", "a quokka fact");
    assert_eq!(node.kind, "Fact");
    assert_eq!(store.kind_stats(&WorkspaceId::default()).unwrap().len(), 0);
}

// ---- E1.9a: the E1-end correction (astra review RE3-3, RE3-5, RE3-7, RE3-8) ----

/// A `remember` of `kind`/`text` by the writer identity `session` in the default workspace.
fn remembered_by(store: &SqliteStore, kind: &str, text: &str, session: &str) -> kaban_core::Node {
    match store
        .remember(
            NewNode {
                kind: kind.into(),
                text: text.into(),
                session_id: Some(session.into()),
                ..Default::default()
            },
            &[],
            &ctx(),
        )
        .unwrap()
    {
        kaban_core::RememberOutcome::Created(n) => n,
        other @ kaban_core::RememberOutcome::Existing { .. } => panic!("fresh store: {other:?}"),
    }
}

fn infra(name: &str) -> kaban_core::ontology::Qualified {
    kaban_core::ontology::Qualified {
        ns: kaban_core::ontology::Ns::parse("infra").unwrap(),
        name: name.into(),
    }
}

fn stat<'a>(stats: &'a [kaban_core::KindStat], ns: &str, kind: &str) -> &'a kaban_core::KindStat {
    stats
        .iter()
        .find(|s| s.ns == ns && s.kind == kind)
        .unwrap_or_else(|| panic!("no ({ns}, {kind}) row in {stats:?}"))
}

/// RE3-3 (EN-F5, F1 identity): `promote_kind` recomputes the dedup identity of every renamed
/// row with the target kind, so `remember infra:Vendor/T` after the promotion finds the
/// promoted record and `remember Vendor/T` starts a new candidate; a promotion whose new
/// identity collides with an existing active row of the target kind is refused whole with
/// the fixed class `promote_conflict`.
#[test]
fn promotion_recomputes_the_dedup_identity_and_refuses_a_collision_atomically() {
    use kaban_core::RememberOutcome;
    let store = SqliteStore::open_memory().unwrap();
    let ws = WorkspaceId::default();
    let promoted = remembered(&store, "Vendor", "quokka widgets T");
    declare(&store, "infra:Vendor");
    store
        .promote_kind(
            &ws,
            "x:Vendor",
            &infra("Vendor"),
            &kaban_core::Actor::HumanCli,
        )
        .unwrap();
    let again = |kind: &str| {
        store
            .remember(
                NewNode {
                    kind: kind.into(),
                    text: "quokka widgets T".into(),
                    path_scope: vec!["src/billing/**".into()],
                    ..Default::default()
                },
                &[],
                &ctx(),
            )
            .unwrap()
    };
    match again("infra:Vendor") {
        RememberOutcome::Existing { node, .. } => {
            assert_eq!(node.id, promoted.id, "the promoted record is the duplicate");
            assert_eq!(node.kind, "infra:Vendor");
        }
        other @ RememberOutcome::Created(_) => panic!("a second infra:Vendor/T row: {other:?}"),
    }
    match again("Vendor") {
        RememberOutcome::Created(node) => assert_eq!(node.kind, "x:Vendor"),
        other @ RememberOutcome::Existing { .. } => {
            panic!("Vendor/T is a new candidate: {other:?}")
        }
    }

    // the collision: an active infra:Vendor/S exists, so x:Vendor/S cannot take its identity
    let store = SqliteStore::open_memory().unwrap();
    let candidate = remembered(&store, "Vendor", "quokka widgets S");
    declare(&store, "infra:Vendor");
    remembered(&store, "infra:Vendor", "quokka widgets S");
    let before = kaban_core::ChangeLogStore::changes_read(&store, 0, 10_000)
        .unwrap()
        .last()
        .map_or(0, |c| c.seq);
    let refused = store
        .promote_kind(
            &ws,
            "x:Vendor",
            &infra("Vendor"),
            &kaban_core::Actor::HumanCli,
        )
        .unwrap_err();
    assert_eq!(refused.to_string(), "promote_conflict");
    let still = store.get(&nid(&candidate.id), false).unwrap().unwrap();
    assert_eq!(
        (still.kind.as_str(), still.rev),
        ("x:Vendor", candidate.rev)
    );
    assert_eq!(
        stat(&store.kind_stats(&ws).unwrap(), "x", "Vendor").n_nodes,
        1
    );
    assert_eq!(
        event_changes_after(&store, before),
        0,
        "nothing renamed, no event"
    );
    let hits = store
        .recall(&kaban_core::RecallQuery {
            query: "quokka widgets".into(),
            limit: 10,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(hits.len(), 1, "the candidate stays hidden: {hits:?}");
}

/// RE3-5 (EN-F5, EN-F5a): `prune_kinds` tombstones only the nodes that hold a
/// `discovered_kind` candidate row of the kind; a released node (accepted by a human) and an
/// `allow`-placed node survive, and while any node of the kind survives its `kind_stats` row
/// and `x:` type row stay (recounted); a kind whose nodes all go loses both. The dry run lists
/// the same kinds and writes nothing.
#[test]
fn prune_tombstones_only_the_candidate_rows_and_keeps_the_kind_while_a_node_survives() {
    use kaban_core::config::PromoteWhen;
    use kaban_core::ontology::meta::DiscoveredPolicyName;
    use kaban_core::{Actor, DiscoveredRules, Governance, GovernanceStore, NewEvent, SignerKind};
    use std::time::Duration;
    let store = SqliteStore::open_memory().unwrap();
    let ws = WorkspaceId::default();
    let candidate = remembered(&store, "Vendor", "quokka vendor a (candidate)");
    let released = remembered(&store, "Vendor", "quokka vendor b (released)");
    store
        .transition(
            &nid(&released.id),
            Governance::Accepted,
            NewEvent {
                reason: "reviewed".into(),
                signer_kind: SignerKind::HumanCli,
                ..Default::default()
            },
            &WriteCtx::new("pv", ws.clone(), Actor::HumanCli),
        )
        .unwrap();
    let allow = ctx().with_discovered(DiscoveredRules {
        policy: DiscoveredPolicyName::Allow,
        cap: 500,
    });
    let proposed = match store
        .remember(
            NewNode {
                kind: "Vendor".into(),
                text: "quokka vendor c (allow)".into(),
                ..Default::default()
            },
            &[],
            &allow,
        )
        .unwrap()
    {
        kaban_core::RememberOutcome::Created(n) => n,
        other @ kaban_core::RememberOutcome::Existing { .. } => panic!("{other:?}"),
    };
    assert_eq!(proposed.governance, Governance::Proposed);
    let other = remembered(&store, "Other", "quokka other d (candidate)");
    let below = PromoteWhen {
        min_count: 5,
        ..PromoteWhen::default()
    };
    let active = |kind: &str| store.list_nodes(kind, ws.as_str(), 0, 10).unwrap().len();

    let dry = store
        .prune_kinds(&ws, &below, Duration::ZERO, true)
        .unwrap();
    assert_eq!(dry.kinds, vec!["x:Other".to_owned(), "x:Vendor".to_owned()]);
    assert_eq!(dry.nodes_tombstoned, 0);
    assert_eq!(
        (active("x:Vendor"), active("x:Other")),
        (3, 1),
        "a dry run writes nothing"
    );

    let wet = store
        .prune_kinds(&ws, &below, Duration::ZERO, false)
        .unwrap();
    assert_eq!(wet.kinds, dry.kinds);
    assert_eq!(wet.nodes_tombstoned, 2, "the two candidate rows only");
    let gone = |id: &str| store.get(&nid(id), false).unwrap().is_none();
    assert!(gone(&candidate.id) && gone(&other.id));
    assert_eq!(
        active("x:Vendor"),
        2,
        "released and allow-placed nodes survive"
    );
    let stats = store.kind_stats(&ws).unwrap();
    assert_eq!(stats.len(), 1, "{stats:?}");
    assert_eq!(
        (stats[0].kind.as_str(), stats[0].n_nodes),
        ("Vendor", 2),
        "the surviving kind keeps a recounted row"
    );
    assert_eq!(x_type_rows(&store), 1, "x:Vendor kept, x:Other dropped");
    let hits = store
        .recall(&kaban_core::RecallQuery {
            query: "quokka vendor".into(),
            limit: 10,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(hits.len(), 2, "{hits:?}");
}

/// RE3-7 (EN-F6): `kind_stats` follow the nodes: a batch update that changes a row's source
/// identity refreshes `n_sources`, a retirement (`forget`) decrements `n_nodes`, and a
/// promotion into a kind that already has a row merges `n_sources` as the distinct union of
/// the writer identities, recounted from the nodes.
#[test]
fn kind_stats_follow_source_changes_retirement_and_promotion_merges() {
    use kaban_core::ForgetMode;
    let store = SqliteStore::open_memory().unwrap();
    let ws = WorkspaceId::default();
    let upsert_by = |id: &NodeId, text: &str, session: &str| {
        let report = store
            .apply_batch(
                WriteBatch::new(
                    ws.clone(),
                    vec![Op::UpsertNode {
                        id: id.clone(),
                        node: NewNode {
                            kind: "Vendor".into(),
                            text: text.into(),
                            session_id: Some(session.into()),
                            ..Default::default()
                        },
                    }],
                ),
                &ctx(),
            )
            .unwrap();
        assert_eq!(report.inserted + report.updated, 1, "{report:?}");
    };
    let one = NodeId::parse(&kaban_core::new_id()).unwrap();
    let two = NodeId::parse(&kaban_core::new_id()).unwrap();
    upsert_by(&one, "quokka vendor one", "s1");
    upsert_by(&two, "quokka vendor two", "s1");
    let vendor = || {
        let stats = store.kind_stats(&ws).unwrap();
        let s = stat(&stats, "x", "Vendor");
        (s.n_nodes, s.n_sources)
    };
    assert_eq!(vendor(), (2, 1));

    // the update branch: a new source identity on an existing row
    upsert_by(&two, "quokka vendor two, revised", "s2");
    assert_eq!(vendor(), (2, 2), "n_sources follows the update");

    // retirement
    store.forget(&one, ForgetMode::Tombstone, &ctx()).unwrap();
    assert_eq!(
        vendor(),
        (1, 1),
        "n_nodes and n_sources follow the retirement"
    );

    // a promotion merge: the target row already counts sources {a, b}; the promoted kind
    // brings {b, c}; the union is three, not max(2, 2)
    let store = SqliteStore::open_memory().unwrap();
    remembered_by(&store, "Vendor", "quokka vendor by a", "a");
    remembered_by(&store, "Vendor", "quokka vendor by b", "b");
    declare(&store, "infra:Vendor");
    store
        .promote_kind(
            &ws,
            "x:Vendor",
            &infra("Vendor"),
            &kaban_core::Actor::HumanCli,
        )
        .unwrap();
    remembered_by(&store, "Supplier", "quokka supplier by b", "b");
    remembered_by(&store, "Supplier", "quokka supplier by c", "c");
    store
        .promote_kind(
            &ws,
            "x:Supplier",
            &infra("Vendor"),
            &kaban_core::Actor::HumanCli,
        )
        .unwrap();
    let stats = store.kind_stats(&ws).unwrap();
    assert_eq!(stats.len(), 1, "{stats:?}");
    let merged = stat(&stats, "infra", "Vendor");
    assert_eq!((merged.n_nodes, merged.n_sources), (4, 3), "{merged:?}");
}

/// RE3-8 (core S3, EN-F7; `obs-no-sensitive-data`): every promotion-source refusal is the
/// fixed class `unknown_kind` with a path payload, never the label, and `kind_not_allowed`
/// carries no payload: `Display` and `Debug` of both are free of the caller's bytes.
#[test]
fn promotion_source_refusals_and_kind_not_allowed_carry_no_payload() {
    use kaban_core::ontology::meta::DiscoveredPolicyName;
    use kaban_core::{DiscoveredRules, StoreError};
    const CANARY: &str = "AKIAQWERTYUIOPASDFGH";
    let store = SqliteStore::open_memory().unwrap();
    let ws = WorkspaceId::default();
    let fact = kaban_core::ontology::Qualified {
        ns: kaban_core::ontology::Ns::parse("core").unwrap(),
        name: "Fact".into(),
    };
    for source in ["Vendor", "infra:Vendor", CANARY] {
        let err = store
            .promote_kind(&ws, source, &fact, &kaban_core::Actor::HumanCli)
            .unwrap_err();
        assert_eq!(err.to_string(), "unknown_kind", "{source}: {err:?}");
        let debug = format!("{err:?}");
        assert!(!debug.contains(source), "{source} echoed by Debug: {debug}");
        assert!(matches!(err, StoreError::Ontology(_)), "{err:?}");
    }
    let deny = ctx().with_discovered(DiscoveredRules {
        policy: DiscoveredPolicyName::Deny,
        cap: 500,
    });
    let err = store
        .remember(
            NewNode {
                kind: CANARY.into(),
                text: "a quokka label".into(),
                ..Default::default()
            },
            &[],
            &deny,
        )
        .unwrap_err();
    assert_eq!(err.to_string(), "kind_not_allowed");
    assert!(
        !format!("{err:?}").contains(CANARY) && !format!("{err:#?}").contains(CANARY),
        "{err:?}"
    );
}
