//! The E1 1,000-record scale test (engine-spec EN-N8 scale half, EN-F5, EN-F5a, EN-F49;
//! engine-plan E1.9; LESSONS 2026-10-26: one informational scale test per milestone). Two user
//! ontologies (a governed and a plain kind each, one rel each) are declared through the store's
//! public `SchemaStore` seam exactly as the service's load seam writes them, 1,000 records with
//! 500 links are written across them, then 600 discovered labels (`fixtures/discovered-600.json`)
//! land in one workspace under the shipped cap of 500 (500 accepted, 100 rejected). Afterwards
//! `PRAGMA foreign_key_check` is empty, `recall` and `traverse` exclude the candidates unless
//! `include_candidates`, one `x:` kind is promoted, `prune --dry-run` lists every single-node
//! kind and writes nothing, and the `schema_describe` body (`schema()` plus `kind_stats()`) at
//! the cap is timed and printed (the §9.1 `[discovered] cap` row; informational, never a gate).
//!
//! Ignored by default (it writes about 1,600 rows through single-row transactions); runner:
//!
//! ```text
//! cargo nextest run -p kaban-store-sqlite --test scale_e1 --run-ignored only --no-capture
//! ```

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion
#![allow(clippy::print_stdout)] // the timings are the test's informational output

mod common;

use common::{Scratch, ctx, nid};
use kaban_core::config::PromoteWhen;
use kaban_core::ontology::resolve::RelEnd;
use kaban_core::{
    Actor, GovernedSet, IngestStore, LinkReq, MemoryStore, NamespaceSet, NewNode, Node, NodeId, Op,
    Qualified, ReadFilters, RecallQuery, RememberOutcome, SchemaStore, TraverseDirection,
    TraverseQuery, TypeDeclaration, TypeDef, WorkspaceId, WriteBatch, WriteCtx, resolve,
    stored_name,
};
use kaban_store_sqlite::SqliteStore;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Records per ontology (two ontologies, two kinds each: `RECORDS_PER_KIND` of each).
const RECORDS_PER_ONTOLOGY: usize = 500;
const RECORDS_PER_KIND: usize = RECORDS_PER_ONTOLOGY / 2;
/// One link per record pair: `Service OPERATES Runbook`, `Vendor SUPPLIES Product`.
const LINKS: usize = 2 * RECORDS_PER_KIND;
/// The shipped `[discovered] cap` (engine-spec EN-F5, `adr.yaml`).
const CAP: usize = 500;
/// Labels in the fixture; `LABELS - CAP` are refused.
const LABELS: usize = 600;

/// `ops`: the governed `Runbook` (acceptance human, serialized under `runbooks`) and the plain
/// `Service`, linked `Service OPERATES Runbook`.
const OPS_YAML: &str = r#"ontology: ops
version: 1
kinds:
  Runbook:
    description: "An operational runbook."
    props_schema: {type: object, additionalProperties: false, properties: {owner: {type: string}}}
    governed: {acceptance: human, serialize: {dir: runbooks, format: md-frontmatter},
               export: {digest: true, marker: false, heading: Runbooks}, display: {label: Runbook, plural: Runbooks},
               governs_paths: true, dedup: none}
  Service:
    description: "A deployed service."
rels:
  OPERATES: {from: [ops:Service], to: [ops:Runbook], directed: true, description: "The service is operated by the runbook."}
"#;

/// `shop`: the governed `Contract` (agent tier, store only) and the plain `Vendor` and
/// `Product`, linked `Vendor SUPPLIES Product`.
const SHOP_YAML: &str = r#"ontology: shop
version: 1
kinds:
  Contract:
    description: "A supply contract kept in the store only."
    governed: {acceptance: agent, serialize: none, export: {digest: false, marker: false},
               display: {label: Contract, plural: Contracts}, governs_paths: false, dedup: none}
  Vendor:
    description: "A vendor."
  Product:
    description: "A product."
rels:
  SUPPLIES: {from: [shop:Vendor], to: [shop:Product], directed: true, description: "The vendor supplies the product."}
"#;

/// Declare the two user ontologies in `store` the way the service's load seam does
/// (`kaban-api/src/ontology.rs::write_namespace`): the shipped files plus the two user files
/// resolved together, every non-builtin kind and rel upserted under its stored name with the
/// canonical declaration columns. Returns the governed set of the resolved ontology.
fn declare_ontologies(store: &SqliteStore) -> Arc<GovernedSet> {
    let mut files = kaban_core::shipped::files().unwrap();
    files.push(kaban_core::OntologyFile::from_yaml(OPS_YAML).unwrap());
    files.push(kaban_core::OntologyFile::from_yaml(SHOP_YAML).unwrap());
    let ontology = resolve(&files, &NamespaceSet::new()).unwrap();
    let user = |q: &Qualified| matches!(q.ns.as_str(), "ops" | "shop");
    let ends = |ends: &[RelEnd]| -> Vec<String> {
        ends.iter()
            .filter_map(|end| match end {
                RelEnd::Kind(q) => Some(stored_name(q)),
                RelEnd::Path => None,
            })
            .collect()
    };
    for kind in ontology.kinds.values().filter(|k| user(&k.id)) {
        store
            .schema_extend(TypeDef {
                kind: "label".into(),
                name: stored_name(&kind.id),
                ns: None,
                description: kind.description.clone(),
                from_kinds: vec![],
                to_kinds: vec![],
                directed: true,
                props_schema: Some(kind.props_schema.as_value().clone()),
                builtin: false,
                declaration: TypeDeclaration::of(kind),
            })
            .unwrap();
    }
    for rel in ontology.rels.values().filter(|r| user(&r.id)) {
        store
            .schema_extend(TypeDef {
                kind: "rel".into(),
                name: stored_name(&rel.id),
                ns: None,
                description: rel
                    .description
                    .clone()
                    .unwrap_or_else(|| stored_name(&rel.id)),
                from_kinds: ends(&rel.from),
                to_kinds: ends(&rel.to),
                directed: rel.directed,
                props_schema: None,
                builtin: false,
                declaration: TypeDeclaration::default(),
            })
            .unwrap();
    }
    Arc::new(GovernedSet::from_ontology(&ontology))
}

/// `remember` one `kind` record with `text` and the inline links; a fresh store never reports
/// an existing node.
fn created(store: &SqliteStore, ctx: &WriteCtx, kind: &str, text: &str, links: &[LinkReq]) -> Node {
    match store
        .remember(
            NewNode {
                kind: kind.into(),
                text: text.into(),
                ..Default::default()
            },
            links,
            ctx,
        )
        .unwrap()
    {
        RememberOutcome::Created(n) => n,
        RememberOutcome::Existing { node, .. } => panic!("fresh store: {node:?}"),
    }
}

/// The 600 labels of `fixtures/discovered-600.json` (`Label000`..`Label599`).
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
    assert_eq!(labels.len(), LABELS);
    labels
}

/// `PRAGMA foreign_key_check` on a second connection to the same file (the store exposes no
/// raw connection; the pragma reads the whole database).
fn foreign_key_violations(db: &std::path::Path) -> Vec<String> {
    let conn = rusqlite::Connection::open(db).unwrap();
    let mut st = conn.prepare("PRAGMA foreign_key_check").unwrap();
    let rows = st.query_map([], |r| r.get::<_, String>(0)).unwrap();
    rows.collect::<Result<Vec<_>, _>>().unwrap()
}

#[test]
#[ignore = "scale test (about 1,600 single-row transactions); run with --run-ignored only"]
fn one_thousand_records_across_two_ontologies_and_six_hundred_discovered_labels() {
    let s = Scratch::new("kaban-scale-e1", "records");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let ws = WorkspaceId::default();
    let governed = declare_ontologies(&store);
    let ctx = ctx().with_governed(governed.clone());
    assert!(
        governed.contains("ops:Runbook"),
        "the user governed kind is in the set"
    );
    assert!(!governed.contains("ops:Service"));

    // 1,000 records: 250 of each kind, every pair linked through the ontology's rel
    let seeding = Instant::now();
    let mut runbooks = Vec::with_capacity(RECORDS_PER_KIND);
    let mut services = Vec::with_capacity(RECORDS_PER_KIND);
    for i in 0..RECORDS_PER_KIND {
        let runbook = created(
            &store,
            &ctx,
            "ops:Runbook",
            &format!("runbook {i}: restart the quokka service tier {}", i % 7),
            &[],
        );
        let service = created(
            &store,
            &ctx,
            "ops:Service",
            &format!("service {i}: the quokka tier {} api", i % 7),
            &[LinkReq {
                rel: "ops:OPERATES".into(),
                from: None,
                to: Some(nid(&runbook.id)),
                confidence: None,
            }],
        );
        runbooks.push(runbook);
        services.push(service);
    }
    let mut vendors = Vec::with_capacity(RECORDS_PER_KIND);
    for i in 0..RECORDS_PER_KIND {
        let product = created(
            &store,
            &ctx,
            "shop:Product",
            &format!("product {i}: a wombat widget of size {}", i % 11),
            &[],
        );
        vendors.push(created(
            &store,
            &ctx,
            "shop:Vendor",
            &format!("vendor {i}: supplies wombat widgets"),
            &[LinkReq {
                rel: "shop:SUPPLIES".into(),
                from: None,
                to: Some(nid(&product.id)),
                confidence: None,
            }],
        ));
    }
    let seeding = seeding.elapsed();
    assert_eq!(
        runbooks[0].governance,
        kaban_core::Governance::Proposed,
        "a governed kind written by an agent is proposed: {:?}",
        runbooks[0]
    );
    assert_eq!(services[0].governance, kaban_core::Governance::None);
    for (kind, n) in [
        ("ops:Runbook", RECORDS_PER_KIND),
        ("ops:Service", RECORDS_PER_KIND),
        ("shop:Vendor", RECORDS_PER_KIND),
        ("shop:Product", RECORDS_PER_KIND),
    ] {
        assert_eq!(
            store.list_nodes(kind, ws.as_str(), 0, 1_000).unwrap().len(),
            n,
            "{kind}"
        );
    }
    let traverse = |start: &str, filters: ReadFilters| {
        store.traverse(&TraverseQuery {
            start_ids: vec![nid(start)],
            rels: vec![],
            direction: TraverseDirection::Both,
            max_depth: 1,
            node_kinds: vec![],
            governance: vec![],
            limit: 50,
            workspace_id: None,
            filters,
            ..Default::default()
        })
    };
    let from_service = traverse(&services[3].id, ReadFilters::default()).unwrap();
    assert_eq!(from_service.edges.len(), 1, "{from_service:?}");
    assert_eq!(from_service.edges[0].rel, "ops:OPERATES");
    assert_eq!(from_service.edges[0].dst, runbooks[3].id);

    // 600 discovered labels in the workspace: 500 placed as `x:` candidates, 100 refused
    let labels = fixture_labels();
    let discovering = Instant::now();
    let mut inserted = 0;
    for label in &labels {
        let id = NodeId::parse(&kaban_core::new_id()).unwrap();
        let report = store
            .apply_batch(
                WriteBatch::new(
                    ws.clone(),
                    vec![Op::UpsertNode {
                        id,
                        node: NewNode {
                            kind: label.clone(),
                            text: format!("a {label} mention in the numbat ledger"),
                            ..Default::default()
                        },
                    }],
                ),
                &ctx,
            )
            .unwrap();
        inserted += usize::try_from(report.inserted).unwrap();
    }
    let discovering = discovering.elapsed();
    assert_eq!(inserted, CAP);
    let stats = store.kind_stats(&ws).unwrap();
    assert_eq!(stats.len(), CAP);
    assert!(
        stats.iter().all(|k| k.ns == "x" && k.n_nodes == 1),
        "{:?}",
        &stats[..2]
    );
    let prune_default = store
        .prune_kinds(&ws, &PromoteWhen::default(), Duration::ZERO, true)
        .unwrap();
    assert_eq!(
        usize::try_from(prune_default.n_rejected).unwrap(),
        LABELS - CAP
    );
    // one candidate linked to a declared record: the far endpoint the read paths must hide
    let candidate = store
        .list_nodes("x:Label007", ws.as_str(), 0, 10)
        .unwrap()
        .remove(0);
    store
        .link(
            &nid(&services[7].id),
            &nid(&candidate.id),
            "RELATES_TO",
            None,
            None,
            &ctx,
        )
        .unwrap();

    // EN-F8a / EN-N8: no dangling foreign key after 1,000 records, 501 edges and 600 labels
    assert_eq!(foreign_key_violations(&s.db()), Vec::<String>::new());

    // EN-F49: candidates are absent from recall (seed and hop) and traverse (far endpoint and
    // start) by default, present with `include_candidates`
    let recall = |query: &str, filters: ReadFilters| {
        store
            .recall(&RecallQuery {
                query: query.into(),
                hops: 1,
                limit: 1_000,
                filters,
                ..Default::default()
            })
            .unwrap()
    };
    let include = ReadFilters {
        include_candidates: true,
    };
    assert!(
        recall("numbat ledger", ReadFilters::default()).is_empty(),
        "a candidate never seeds a recall"
    );
    let hidden_hop = recall("quokka tier api", ReadFilters::default());
    assert!(
        hidden_hop.iter().all(|h| !h.node.kind.starts_with("x:")),
        "a candidate never joins through the hop"
    );
    assert!(hidden_hop.iter().any(|h| h.node.id == services[7].id));
    // the store caps a recall page at 100 hits (`recall.rs`), so the full page is candidates
    let shown = recall("numbat ledger", include);
    assert_eq!(shown.len(), 100, "a full page with include_candidates");
    // the hop from `x:Label007` reaches its declared neighbour; every other hit is a candidate
    let non_candidates: Vec<&str> = shown
        .iter()
        .filter(|h| !h.node.kind.starts_with("x:Label"))
        .map(|h| h.node.id.as_str())
        .collect();
    assert!(
        non_candidates.is_empty() || non_candidates == [services[7].id.as_str()],
        "{non_candidates:?}"
    );
    let from_service = traverse(&services[7].id, ReadFilters::default()).unwrap();
    assert!(
        from_service.nodes.iter().all(|n| n.id != candidate.id),
        "{from_service:?}"
    );
    assert_eq!(
        from_service.edges.len(),
        1,
        "the OPERATES edge only: {from_service:?}"
    );
    assert!(
        traverse(&candidate.id, ReadFilters::default()).is_err(),
        "a candidate start is absent"
    );
    let from_service = traverse(&services[7].id, include).unwrap();
    assert!(from_service.nodes.iter().any(|n| n.id == candidate.id));
    assert_eq!(from_service.edges.len(), 2);

    // the schema_describe body at the cap: schema() plus kind_stats() (§9.1 `[discovered] cap`)
    let describing = Instant::now();
    let schema = store.schema().unwrap();
    let stats = store.kind_stats(&ws).unwrap();
    let describing = describing.elapsed();
    assert_eq!(
        schema
            .iter()
            .filter(|d| d.ns.as_deref() == Some("x"))
            .count(),
        CAP
    );
    assert!(schema.iter().any(|d| d.name == "ops:Runbook"
        && d.ns.as_deref() == Some("ops")
        && d.declaration.governed.is_some()));
    assert_eq!(stats.len(), CAP);

    // promote one `x:` kind to a declared plain kind: its record becomes visible, the counters
    // move under the target, the candidate row goes
    let target = Qualified {
        ns: kaban_core::Ns::parse("shop").unwrap(),
        name: "Vendor".into(),
    };
    let promoted = store
        .promote_kind(&ws, "x:Label007", &target, &Actor::HumanCli)
        .unwrap();
    assert_eq!(
        (promoted.nodes, promoted.candidates_released),
        (1, 1),
        "{promoted:?}"
    );
    assert_eq!(promoted.to, "shop:Vendor");
    let stats = store.kind_stats(&ws).unwrap();
    assert_eq!(stats.len(), CAP, "the row moved, none was added");
    assert_eq!(stats.iter().filter(|k| k.ns == "x").count(), CAP - 1);
    // E1.9a RE3-7: the row under the declared name counts every active `shop:Vendor` of the
    // workspace (the 250 remembered ones and the promoted record), recounted from the nodes
    assert!(
        stats.iter().any(|k| k.ns == "shop"
            && k.kind == "Vendor"
            && k.n_nodes == u64::try_from(RECORDS_PER_KIND).unwrap() + 1),
        "{:?}",
        stats.iter().find(|k| k.ns == "shop")
    );
    // the released record seeds the recall now (its declared neighbour follows through the hop)
    let visible = recall("Label007 numbat", ReadFilters::default());
    assert_eq!(
        visible.first().map(|h| h.node.id.as_str()),
        Some(candidate.id.as_str()),
        "{visible:?}"
    );
    assert_eq!(visible[0].node.kind, "shop:Vendor");
    assert!(
        visible.len() <= 2,
        "the promoted record and its neighbour at most: {visible:?}"
    );
    assert_eq!(
        store
            .list_nodes("shop:Vendor", ws.as_str(), 0, 1_000)
            .unwrap()
            .len(),
        RECORDS_PER_KIND + 1
    );

    // prune --dry-run: every remaining single-node `x:` kind is listed (min_count 2, age 0),
    // nothing is tombstoned, the counters stay, `n_rejected` is reported as is
    let below = PromoteWhen {
        min_count: 2,
        ..PromoteWhen::default()
    };
    let dry = store
        .prune_kinds(&ws, &below, Duration::ZERO, true)
        .unwrap();
    assert!(dry.dry_run);
    assert_eq!(dry.kinds.len(), CAP - 1, "{}", dry.kinds.len());
    assert!(dry.kinds.iter().all(|k| k.starts_with("x:Label")));
    assert_eq!(dry.nodes_tombstoned, 0);
    assert_eq!(usize::try_from(dry.n_rejected).unwrap(), LABELS - CAP);
    assert_eq!(
        store.kind_stats(&ws).unwrap().len(),
        CAP,
        "a dry run writes nothing"
    );
    assert_eq!(
        store
            .list_nodes("x:Label008", ws.as_str(), 0, 10)
            .unwrap()
            .len(),
        1
    );
    assert_eq!(foreign_key_violations(&s.db()), Vec::<String>::new());

    println!(
        "scale_e1: {} records ({LINKS} links) across ops and shop seeded in {seeding:?}; \
         {LABELS} discovered labels ({CAP} accepted, {} rejected) in {discovering:?}; \
         schema_describe body at the cap (schema {} rows + kind_stats {CAP} rows) in {describing:?}; \
         promote x:Label007 -> shop:Vendor ({} node, {} released); prune --dry-run lists {} kinds, \
         tombstones {}, n_rejected {}; foreign_key_check empty",
        2 * RECORDS_PER_ONTOLOGY,
        LABELS - CAP,
        schema.len(),
        promoted.nodes,
        promoted.candidates_released,
        dry.kinds.len(),
        dry.nodes_tombstoned,
        dry.n_rejected
    );
}
