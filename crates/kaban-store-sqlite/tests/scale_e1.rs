//! The E1 1,000-record scale test (engine-spec EN-N8 scale half; engine-plan E1.9; LESSONS
//! 2026-10-26: one informational scale test per milestone). Two user ontologies (two kinds
//! each, one rel each) are declared through the store's public `SchemaStore` seam exactly as
//! the service's load seam writes them, 1,000 records with 500 links are written across them,
//! and afterwards `PRAGMA foreign_key_check` is empty and the seeding time is printed
//! (informational, never a gate). The discovered-label half went with discovered kinds (PC-14).
//!
//! Ignored by default (it writes about 1,000 rows through single-row transactions); runner:
//!
//! ```text
//! cargo nextest run -p kaban-store-sqlite --test scale_e1 --run-ignored only --no-capture
//! ```

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion
#![allow(clippy::print_stdout)] // the timings are the test's informational output

mod common;

use common::{Scratch, ctx, nid};
use kaban_core::ontology::resolve::RelEnd;
use kaban_core::{
    IngestStore, LinkReq, MemoryStore, NamespaceSet, NewNode, Node, Qualified, RememberOutcome,
    RuleSet, SchemaStore, TraverseDirection, TraverseQuery, TypeDeclaration, TypeDef, WorkspaceId,
    WriteCtx, resolve, stored_name,
};
use kaban_store_sqlite::SqliteStore;
use std::sync::Arc;
use std::time::Instant;

/// Records per ontology (two ontologies, two kinds each: `RECORDS_PER_KIND` of each).
const RECORDS_PER_ONTOLOGY: usize = 500;
const RECORDS_PER_KIND: usize = RECORDS_PER_ONTOLOGY / 2;
/// One link per record pair: `Service OPERATES Runbook`, `Vendor SUPPLIES Product`.
const LINKS: usize = 2 * RECORDS_PER_KIND;

/// `ops`: the `Runbook` with a `rules` block and the plain `Service`, linked
/// `Service OPERATES Runbook`.
const OPS_YAML: &str = r#"ontology: ops
version: 1
kinds:
  Runbook:
    description: "An operational runbook."
    props_schema: {type: object, additionalProperties: false, properties: {owner: {type: string}}}
    rules: {display: {label: Runbook, plural: Runbooks}, dedup: none}
  Service:
    description: "A deployed service."
rels:
  OPERATES: {from: [ops:Service], to: [ops:Runbook], directed: true, description: "The service is operated by the runbook."}
"#;

/// `shop`: the `Contract` with a `rules` block and the plain `Vendor` and `Product`, linked
/// `Vendor SUPPLIES Product`.
const SHOP_YAML: &str = r#"ontology: shop
version: 1
kinds:
  Contract:
    description: "A supply contract kept in the store only."
    rules: {display: {label: Contract, plural: Contracts}, dedup: none}
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
/// canonical declaration columns. Returns the rule set of the resolved ontology.
fn declare_ontologies(store: &SqliteStore) -> Arc<RuleSet> {
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
    Arc::new(RuleSet::from_ontology(&ontology))
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

/// `PRAGMA foreign_key_check` on a second connection to the same file (the store exposes no
/// raw connection; the pragma reads the whole database).
fn foreign_key_violations(db: &std::path::Path) -> Vec<String> {
    let conn = rusqlite::Connection::open(db).unwrap();
    let mut st = conn.prepare("PRAGMA foreign_key_check").unwrap();
    let rows = st.query_map([], |r| r.get::<_, String>(0)).unwrap();
    rows.collect::<Result<Vec<_>, _>>().unwrap()
}

#[test]
#[ignore = "scale test (about 1,000 single-row transactions); run with --run-ignored only"]
fn one_thousand_records_across_two_ontologies() {
    let s = Scratch::new("kaban-scale-e1", "records");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let ws = WorkspaceId::default();
    let rules = declare_ontologies(&store);
    let ctx = ctx();
    assert!(
        rules.rules("ops:Runbook").is_some(),
        "the user kind's rules are in the set"
    );
    assert!(rules.rules("ops:Service").is_none());

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
    assert_eq!(runbooks[0].governance, kaban_core::Governance::None);
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
    let traverse = |start: &str| {
        store.traverse(&TraverseQuery {
            start_ids: vec![nid(start)],
            rels: vec![],
            direction: TraverseDirection::Both,
            max_depth: 1,
            node_kinds: vec![],
            limit: 50,
            workspace_id: None,
            ..Default::default()
        })
    };
    let from_service = traverse(&services[3].id).unwrap();
    assert_eq!(from_service.edges.len(), 1, "{from_service:?}");
    assert_eq!(from_service.edges[0].rel, "ops:OPERATES");
    assert_eq!(from_service.edges[0].dst, runbooks[3].id);

    // EN-F8a / EN-N8: no dangling foreign key after 1,000 records and 500 edges
    assert_eq!(foreign_key_violations(&s.db()), Vec::<String>::new());

    println!(
        "scale_e1: {} records ({LINKS} links) across ops and shop seeded in {seeding:?}; \
         foreign_key_check empty",
        2 * RECORDS_PER_ONTOLOGY,
    );
}
