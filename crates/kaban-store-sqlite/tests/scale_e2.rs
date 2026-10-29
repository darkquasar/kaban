//! The E2 1,000-record scale test (engine-spec EN-N8 scale half, EN-F8a, EN-F12, EN-F49,
//! EN-F51; engine-plan E2.12 item 2; LESSONS 2026-10-26: one informational scale test per
//! milestone). Two user ontologies are declared through the store's public `SchemaStore` seam
//! as `scale_e1.rs` does, 1,000 records with 500 inline human links are written across them,
//! 600 discovered labels (`fixtures/discovered-600.json`) land under the shipped cap of 500,
//! 5,000 `edge` assertions from three lineages (`l1` gliner, `l2` llm, `l3` gazetteer) over
//! 3,000 `RELATES_TO` triples are reconciled in ten batches, and 200 ER identity pairs in the
//! E2.10 `er` fixture shape (`{match, W, contributions}` under `reconcile:er`) are merged.
//! Afterwards `PRAGMA foreign_key_check` is empty, the verdict counts by status are asserted
//! (from the batch reports and from the `assertion_status` rows), the depth-3 `traverse` time
//! is recorded (informational, never a gate), and an 8-process slice hammers the
//! `[discovered] cap` of a second workspace with 800 distinct labels: exactly 500 are admitted.
//!
//! Ignored by default (about 1,600 single-row transactions plus the batches and eight child
//! processes); runner:
//!
//! ```text
//! cargo nextest run -p kaban-store-sqlite --run-ignored only -E 'binary(scale_e2)'
//! ```

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion
#![allow(clippy::print_stdout)] // the timings and the child reports are the test's informational output

mod common;

use common::{Scratch, ctx, nid};
use kaban_core::ontology::resolve::RelEnd;
use kaban_core::{
    Actor, AssertionStatus, BatchAssertion, BatchSubject, ClaimClass, Fixed, GovernedSet,
    IngestStore, Layer, LinkReq, MemoryStore, Method, NamespaceSet, NewNode, Node, NodeId, Op,
    Qualified, Rank, ReadFilters, RememberOutcome, SchemaStore, TraverseDirection, TraverseQuery,
    TypeDeclaration, TypeDef, WorkspaceId, WriteBatch, WriteCtx, resolve, stored_name,
};
use kaban_store_sqlite::SqliteStore;
use serde_json::json;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Instant;

/// Records per ontology (two ontologies, two kinds each: `RECORDS_PER_KIND` of each).
const RECORDS_PER_ONTOLOGY: usize = 500;
const RECORDS_PER_KIND: usize = RECORDS_PER_ONTOLOGY / 2;
const RECORDS: usize = 2 * RECORDS_PER_ONTOLOGY;
/// One human link per record pair: `Service OPERATES Runbook`, `Vendor SUPPLIES Product`.
const LINKS: usize = 2 * RECORDS_PER_KIND;
/// The shipped `[discovered] cap` (engine-spec EN-F5, `adr.yaml`).
const CAP: usize = 500;
/// Labels in the fixture; `LABELS - CAP` are refused.
const LABELS: usize = 600;
/// Edge assertions: five per record (two triples with two agreeing lineages, one with a single
/// lineage), 3,000 groups.
const ASSERTIONS: usize = 5 * RECORDS;
const ASSERTION_GROUPS: usize = 3 * RECORDS;
const ASSERTION_BATCHES: usize = 10;
/// ER pairs in the `er` fixture shape, all `match: true`.
const ER_PAIRS: usize = 200;
/// The hammer: eight processes, 100 distinct labels each, one workspace, the shipped cap.
const HAMMER_PROCESSES: usize = 8;
const HAMMER_LABELS_PER_PROCESS: usize = 100;
const HAMMER_WORKSPACE: &str = "hammer";
const HAMMER_INDEX: &str = "KABAN_SCALE_E2_HAMMER_INDEX";
const HAMMER_DIR: &str = "KABAN_SCALE_E2_DIR";
const TEST_NAME: &str = "one_thousand_records_five_thousand_assertions_and_two_hundred_er_pairs";

/// `ops`: the governed `Runbook` and the plain `Service`, linked `Service OPERATES Runbook`.
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

/// `shop`: the governed `Contract` and the plain `Vendor` and `Product`, linked
/// `Vendor SUPPLIES Product`.
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
/// (`kaban-api/src/ontology.rs::write_namespace`); returns the governed set.
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

/// One undeclared-label upsert (the discovered path).
fn label_upsert(label: &str, text: &str) -> Op {
    Op::UpsertNode {
        id: NodeId::parse(&kaban_core::new_id()).unwrap(),
        node: NewNode {
            kind: label.to_owned(),
            text: text.to_owned(),
            ..Default::default()
        },
    }
}

/// An `edge` claim about `src -RELATES_TO-> dst` from one of the three lineages.
fn edge_claim(src: &Node, dst: &Node, lineage: usize) -> BatchAssertion {
    let (layer, method, raw, lineage_id) = match lineage {
        0 => (Layer::Gliner, Method::Gliner, 9000, "l1"),
        1 => (Layer::Llm, Method::Llm, 8000, "l2"),
        _ => (Layer::Gazetteer, Method::Alias, 7000, "l3"),
    };
    BatchAssertion {
        subject: BatchSubject::Edge {
            src: nid(&src.id),
            rel: "RELATES_TO".into(),
            dst: nid(&dst.id),
        },
        claim_class: ClaimClass::Edge,
        predicate: None,
        object: None,
        value: json!({}),
        layer,
        method,
        extractor_id: Some("scale".into()),
        extractor_version: Some("1".into()),
        source_id: Some("doc-scale".into()),
        lineage_id: lineage_id.into(),
        raw_score: Fixed::new(raw).unwrap(),
        rank: Rank::Normal,
        rank_reason: None,
        calibration_version: 1,
        supersedes: None,
    }
}

/// An ER identity claim in the E2.10 `er` fixture shape (`kaban-store-sqlite/tests/merge.rs`
/// `identity_claim`): `subject` one node, `object` the other, `{match, W, contributions}`
/// under the `reconcile:er` lineage at full confidence.
fn identity_claim(a: &Node, b: &Node) -> BatchAssertion {
    BatchAssertion {
        subject: BatchSubject::Node(nid(&a.id)),
        claim_class: ClaimClass::Identity,
        predicate: None,
        object: Some(nid(&b.id)),
        value: json!({"match": true, "W": 81549, "contributions": [
            {"field": "title", "level": "agree", "w": 64919},
            {"field": "kind", "level": "agree", "w": 16630}
        ]}),
        layer: Layer::Reconcile,
        method: Method::Er,
        extractor_id: None,
        extractor_version: None,
        source_id: None,
        lineage_id: "reconcile:er".into(),
        raw_score: Fixed::new(10_000).unwrap(),
        rank: Rank::Normal,
        rank_reason: None,
        calibration_version: 0,
        supersedes: None,
    }
}

/// Submit `claims` as one batch and return its verdict counts.
fn submit(
    store: &SqliteStore,
    ctx: &WriteCtx,
    claims: Vec<BatchAssertion>,
) -> BTreeMap<AssertionStatus, u32> {
    let mut b = WriteBatch::new(WorkspaceId::default(), vec![]);
    b.assertions = claims;
    store.apply_batch(b, ctx).unwrap().verdicts
}

/// `PRAGMA foreign_key_check` on a second connection to the same file (the store exposes no
/// raw connection; the pragma reads the whole database).
fn foreign_key_violations(db: &std::path::Path) -> Vec<String> {
    let conn = rusqlite::Connection::open(db).unwrap();
    let mut st = conn.prepare("PRAGMA foreign_key_check").unwrap();
    let rows = st.query_map([], |r| r.get::<_, String>(0)).unwrap();
    rows.collect::<Result<Vec<_>, _>>().unwrap()
}

/// The `assertion_status` rows by disposition (the at-rest view of the verdicts).
fn status_rows(db: &std::path::Path) -> BTreeMap<String, i64> {
    let conn = rusqlite::Connection::open(db).unwrap();
    let mut st = conn
        .prepare("SELECT disposition, count(*) FROM assertion_status GROUP BY disposition")
        .unwrap();
    let rows = st
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
        .unwrap();
    rows.collect::<Result<BTreeMap<_, _>, _>>().unwrap()
}

/// The `node_identity` redirects in place (`canonical_rid` set).
fn redirects(db: &std::path::Path) -> i64 {
    rusqlite::Connection::open(db)
        .unwrap()
        .query_row(
            "SELECT count(*) FROM node_identity WHERE canonical_rid IS NOT NULL",
            [],
            |r| r.get(0),
        )
        .unwrap()
}

/// The child branch of the hammer: open the parent's store, write 100 distinct undeclared
/// labels into the `hammer` workspace one batch each, print the admitted and refused counts.
fn hammer_child(index: usize) {
    let dir = PathBuf::from(std::env::var_os(HAMMER_DIR).expect("the parent sets the dir"));
    let store = SqliteStore::open(&dir.join("kaban.db"), &dir.join("keys")).unwrap();
    let ws = WorkspaceId::new(HAMMER_WORKSPACE);
    let ctx = WriteCtx::new("pv", ws.clone(), Actor::Agent);
    let (mut inserted, mut skipped) = (0u32, 0u32);
    for k in 0..HAMMER_LABELS_PER_PROCESS {
        let label = format!("Hammer{index:02}x{k:03}");
        let report = store
            .apply_batch(
                WriteBatch::new(
                    ws.clone(),
                    vec![label_upsert(
                        &label,
                        &format!("a {label} mention under the cap"),
                    )],
                ),
                &ctx,
            )
            .unwrap_or_else(|e| panic!("hammer {index} label {k}: {e:?}"));
        inserted += report.inserted;
        skipped += report.skipped;
    }
    println!("hammer {index} inserted {inserted} skipped {skipped}");
}

#[test]
#[ignore = "scale test (about 1,600 single-row transactions, ten claim batches, eight child processes); run with --run-ignored only"]
fn one_thousand_records_five_thousand_assertions_and_two_hundred_er_pairs() {
    if let Some(index) = std::env::var_os(HAMMER_INDEX) {
        hammer_child(index.to_str().unwrap().parse().unwrap());
        return;
    }
    let s = Scratch::new("kaban-scale-e2", "records");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let ws = WorkspaceId::default();
    let governed = declare_ontologies(&store);
    let ctx = ctx().with_governed(governed);

    // 1,000 records: 250 of each kind, every pair linked by a human link through the rel
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
        services.push(created(
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
        ));
        runbooks.push(runbook);
    }
    let mut products = Vec::with_capacity(RECORDS_PER_KIND);
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
        products.push(product);
    }
    let seeding = seeding.elapsed();
    let all: Vec<&Node> = services
        .iter()
        .chain(&runbooks)
        .chain(&vendors)
        .chain(&products)
        .collect();
    assert_eq!(all.len(), RECORDS);
    assert_eq!(runbooks[0].governance, kaban_core::Governance::Proposed);
    assert_eq!(services[0].governance, kaban_core::Governance::None);

    // 600 discovered labels: 500 placed as `x:` candidates, 100 refused at the cap
    let labels = fixture_labels();
    let discovering = Instant::now();
    let mut inserted = 0;
    for label in &labels {
        let report = store
            .apply_batch(
                WriteBatch::new(
                    ws.clone(),
                    vec![label_upsert(
                        label,
                        &format!("a {label} mention in the numbat ledger"),
                    )],
                ),
                &ctx,
            )
            .unwrap();
        inserted += usize::try_from(report.inserted).unwrap();
    }
    let discovering = discovering.elapsed();
    assert_eq!(inserted, CAP);
    assert_eq!(store.kind_stats(&ws).unwrap().len(), CAP);

    // 5,000 edge assertions from three lineages over 3,000 triples, ten batches of 500:
    // for record j: `l1` and `l2` agree on j -> j+1, `l3` alone on j -> j+2, `l1` and `l2`
    // agree on j -> j+3 (indices modulo 1,000)
    let mut claims = Vec::with_capacity(ASSERTIONS);
    for (j, src) in all.iter().enumerate() {
        let at = |k: usize| all[(j + k) % RECORDS];
        claims.push(edge_claim(src, at(1), 0));
        claims.push(edge_claim(src, at(1), 1));
        claims.push(edge_claim(src, at(2), 2));
        claims.push(edge_claim(src, at(3), 0));
        claims.push(edge_claim(src, at(3), 1));
    }
    assert_eq!(claims.len(), ASSERTIONS);
    let reconciling = Instant::now();
    let mut verdicts: BTreeMap<AssertionStatus, u32> = BTreeMap::new();
    let per_batch = ASSERTIONS / ASSERTION_BATCHES;
    for chunk in claims.chunks(per_batch) {
        for (status, n) in submit(&store, &ctx, chunk.to_vec()) {
            *verdicts.entry(status).or_default() += n;
        }
    }
    let reconciling = reconciling.elapsed();
    // EN-F12: one verdict per decided group (3,000 triples), every group with a winner; the
    // 2,000 agreeing second-lineage claims are the non-winning members and sit at rest as
    // `dissent` rows (EN-F12b: agreement is dissent), asserted on the status rows below
    let decided: u32 = verdicts.values().sum();
    assert_eq!(
        usize::try_from(decided).unwrap(),
        ASSERTION_GROUPS,
        "one verdict per group: {verdicts:?}"
    );
    assert_eq!(
        verdicts.get(&AssertionStatus::Winner).copied(),
        Some(u32::try_from(ASSERTION_GROUPS).unwrap()),
        "every group has a winner: {verdicts:?}"
    );

    // 200 ER identity pairs: disjoint pairs of plain records, all `match: true`
    let mut er = Vec::with_capacity(ER_PAIRS);
    for i in 0..125 {
        er.push(identity_claim(&services[i], &services[i + 125]));
    }
    for i in 0..75 {
        er.push(identity_claim(&products[i], &products[i + 125]));
    }
    assert_eq!(er.len(), ER_PAIRS);
    let merging = Instant::now();
    let er_verdicts = submit(&store, &ctx, er);
    let merging = merging.elapsed();
    assert_eq!(
        er_verdicts.get(&AssertionStatus::Winner).copied(),
        Some(u32::try_from(ER_PAIRS).unwrap()),
        "{er_verdicts:?}"
    );
    assert_eq!(redirects(&s.db()), i64::try_from(ER_PAIRS).unwrap());

    // the at-rest statuses: 500 human link claims, 5,000 edge claims, 200 identity claims
    let rows = status_rows(&s.db());
    let total: i64 = rows.values().sum();
    assert_eq!(
        usize::try_from(total).unwrap(),
        LINKS + ASSERTIONS + ER_PAIRS,
        "{rows:?}"
    );
    assert_eq!(
        rows.get("winner").copied(),
        Some(i64::try_from(LINKS + ASSERTION_GROUPS + ER_PAIRS).unwrap()),
        "{rows:?}"
    );
    assert_eq!(
        rows.get("dissent").copied(),
        Some(i64::try_from(ASSERTIONS - ASSERTION_GROUPS).unwrap()),
        "the agreeing second lineage of 2,000 groups: {rows:?}"
    );
    assert_eq!(rows.get("unresolved"), None, "{rows:?}");
    assert_eq!(rows.get("rejected"), None, "{rows:?}");

    // EN-F8a / EN-N8: no dangling foreign key after all of it
    assert_eq!(foreign_key_violations(&s.db()), Vec::<String>::new());

    // the depth-3 traverse from one service over the projected RELATES_TO edges and the human
    // links (informational: the time is recorded, never asserted)
    let traversing = Instant::now();
    let reached = store
        .traverse(&TraverseQuery {
            start_ids: vec![nid(&services[3].id)],
            rels: vec![],
            direction: TraverseDirection::Both,
            max_depth: 3,
            node_kinds: vec![],
            governance: vec![],
            limit: 200,
            workspace_id: None,
            filters: ReadFilters::default(),
            ..Default::default()
        })
        .unwrap();
    let traversing = traversing.elapsed();
    assert!(reached.nodes.len() > 3, "{}", reached.nodes.len());
    assert!(
        reached.nodes.iter().all(|n| !n.kind.starts_with("x:")),
        "EN-F49: no candidate is reached"
    );

    // the 8-process slice: eight children of this binary write 800 distinct labels into the
    // `hammer` workspace under the shipped cap; exactly 500 are admitted, 300 refused
    // (`CapReached`), no child sees a store error
    let exe = std::env::current_exe().unwrap();
    let hammering = Instant::now();
    let children: Vec<_> = (0..HAMMER_PROCESSES)
        .map(|i| {
            Command::new(&exe)
                .args(["--exact", TEST_NAME, "--ignored", "--nocapture"])
                .env(HAMMER_INDEX, i.to_string())
                .env(HAMMER_DIR, &s.0)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    let (mut admitted, mut refused) = (0u32, 0u32);
    for (i, child) in children.into_iter().enumerate() {
        let out = child.wait_with_output().unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            out.status.success(),
            "hammer {i} failed ({}):\n{stdout}\n{stderr}",
            out.status
        );
        let line = stdout
            .lines()
            .find(|l| l.starts_with(&format!("hammer {i} ")))
            .unwrap_or_else(|| panic!("hammer {i} printed no report:\n{stdout}\n{stderr}"));
        let words: Vec<&str> = line.split(' ').collect();
        admitted += words[3].parse::<u32>().unwrap();
        refused += words[5].parse::<u32>().unwrap();
    }
    let hammering = hammering.elapsed();
    assert_eq!(
        (admitted, refused),
        (
            u32::try_from(CAP).unwrap(),
            u32::try_from(HAMMER_PROCESSES * HAMMER_LABELS_PER_PROCESS - CAP).unwrap()
        ),
        "the cap holds across eight processes"
    );
    let hammer_ws = WorkspaceId::new(HAMMER_WORKSPACE);
    let stats = store.kind_stats(&hammer_ws).unwrap();
    assert_eq!(stats.len(), CAP);
    assert!(stats.iter().all(|k| k.ns == "x" && k.n_nodes == 1));
    assert_eq!(
        store.kind_stats(&ws).unwrap().len(),
        CAP,
        "the default workspace's cap row count is untouched"
    );
    assert_eq!(foreign_key_violations(&s.db()), Vec::<String>::new());

    println!(
        "scale_e2: {RECORDS} records ({LINKS} human links) across ops and shop seeded in {seeding:?}; \
         {LABELS} discovered labels ({CAP} accepted, {} rejected) in {discovering:?}; \
         {ASSERTIONS} edge assertions from three lineages over {ASSERTION_GROUPS} groups in \
         {ASSERTION_BATCHES} batches reconciled in {reconciling:?} (verdicts {verdicts:?}); \
         {ER_PAIRS} ER pairs merged in {merging:?} ({} redirects); statuses at rest {rows:?}; \
         traverse depth 3 from one service: {} nodes, {} edges in {traversing:?} (informational); \
         hammer: {HAMMER_PROCESSES} processes x {HAMMER_LABELS_PER_PROCESS} labels in {hammering:?}, \
         admitted {admitted}, refused {refused}; foreign_key_check empty",
        LABELS - CAP,
        redirects(&s.db()),
        reached.nodes.len(),
        reached.edges.len(),
    );
}
