//! The EN-F13 fixture "orange or apple" as batch input (engine-plan E2.4: shared by the
//! `explain` suite; `tests/orange_apple.rs` keeps its own copy from E2.3): the fixture file, a
//! store reconciling against the shipped files plus the fixture's `infra` ontology, one
//! fixture row as a `BatchAssertion` (its `object` the fixture's entity node, E2.5) and the
//! one-batch `submit` (the entity nodes, the chunk with its real text, the claims).

use super::Scratch;
use kaban_core::{
    Actor, BatchAssertion, BatchSubject, ClaimClass, EdgeDiff, Fixed, IngestStore, Layer, Method,
    NamespaceSet, NewNode, NodeId, OntologyFile, Op, Owner, Rank, SchemaStore, TypeDeclaration,
    TypeDef, WorkspaceId, WriteBatch, WriteCtx, resolve, stored_name,
};
use kaban_store_sqlite::SqliteStore;
use serde_json::Value;
use std::sync::Arc;

/// `fixtures/reconcile/orange-apple.json`, parsed.
pub(crate) fn fixture() -> Value {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/fixtures/reconcile/orange-apple.json"
    );
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// A store reconciling against the shipped files plus the fixture's `infra` ontology, the
/// `infra` kinds declared on the store too (the service's registry load does it; a batch
/// upsert of an entity node of such a kind needs the `schema_type` row).
pub(crate) fn store_with_infra(s: &Scratch, fx: &Value) -> SqliteStore {
    let mut files = kaban_core::shipped::files().unwrap();
    files.push(OntologyFile::from_yaml(fx["ontology"].as_str().unwrap()).unwrap());
    let ontology = resolve(&files, &NamespaceSet::new()).unwrap();
    let infra: Vec<String> = ontology
        .kinds
        .keys()
        .filter(|q| q.ns.as_str() == "infra")
        .map(stored_name)
        .collect();
    let store = SqliteStore::open(&s.db(), &s.keys())
        .unwrap()
        .with_ontology(Arc::new(ontology))
        .unwrap();
    for name in infra {
        store
            .schema_extend(TypeDef {
                kind: "label".into(),
                name,
                ns: None,
                description: "declared".into(),
                from_kinds: vec![],
                to_kinds: vec![],
                directed: true,
                props_schema: None,
                builtin: false,
                declaration: TypeDeclaration::default(),
            })
            .unwrap();
    }
    store
}

fn u32_of(v: &Value) -> u32 {
    u32::try_from(v.as_u64().unwrap()).unwrap()
}

/// The fixed id of the fixture's entity node `name` (`nodes`): the target a producer resolved
/// and names in `object` (EN-F13 target selection).
pub(crate) fn entity_id(fx: &Value, name: &str) -> NodeId {
    NodeId::parse(fx["nodes"][name]["id"].as_str().unwrap()).unwrap()
}

/// The upserts of every fixture entity node, in name order (the same rows in every batch).
fn entity_ops(fx: &Value) -> Vec<Op> {
    fx["nodes"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(name, n)| Op::UpsertNode {
            id: entity_id(fx, name),
            node: NewNode {
                kind: n["kind"].as_str().unwrap().into(),
                text: n["text"].as_str().unwrap().into(),
                ..Default::default()
            },
        })
        .collect()
}

/// One fixture row as the claim a producer batch would carry about chunk `c17`; `object` is
/// the entity row the fixture names, when it names one.
pub(crate) fn claim(fx: &Value, row: &Value, chunk: &NodeId) -> BatchAssertion {
    let text = |k: &str| row[k].as_str().unwrap();
    BatchAssertion {
        subject: BatchSubject::Span {
            chunk: chunk.clone(),
            offset: u32_of(&row["offset"]),
            len: u32_of(&row["len"]),
        },
        claim_class: ClaimClass::Type,
        predicate: None,
        object: row
            .get("object")
            .and_then(Value::as_str)
            .map(|name| entity_id(fx, name)),
        value: row["value"].clone(),
        layer: Layer::parse(text("layer")).unwrap(),
        method: Method::parse(text("method")).unwrap(),
        extractor_id: Some(text("id").to_owned()),
        extractor_version: Some("1".into()),
        source_id: Some("c17".into()),
        lineage_id: text("lineage_id").to_owned(),
        raw_score: Fixed::new(u32_of(&row["raw_score"])).unwrap(),
        rank: row
            .get("rank")
            .and_then(Value::as_str)
            .and_then(Rank::parse)
            .unwrap_or(Rank::Normal),
        rank_reason: None,
        calibration_version: 1,
        supersedes: None,
    }
}

/// The fixture rows named by `ids`, in that order.
pub(crate) fn rows<'a>(fx: &'a Value, ids: &[&str]) -> Vec<&'a Value> {
    ids.iter()
        .map(|id| {
            fx["assertions"]
                .as_array()
                .unwrap()
                .iter()
                .find(|r| r["id"] == *id)
                .unwrap()
        })
        .collect()
}

/// The ids of a variant's `submit` list.
pub(crate) fn ids_of(v: &Value) -> Vec<&str> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap())
        .collect()
}

/// The fixture's entity nodes and the chunk `c17` (its real text) written and the named rows
/// submitted as one batch (agent actor, policy `v1`) under the producer's diff
/// (`kaban-ingest`'s choice: its own unnamed edges go, the projection's rows, `layer` set and
/// owner `kaban-reconcile` or `human`, are not its to drop); returns the report.
pub(crate) fn submit(
    store: &SqliteStore,
    fx: &Value,
    chunk: &NodeId,
    ids: &[&str],
    extra: Vec<BatchAssertion>,
) -> kaban_core::BatchReport {
    submit_with(
        store,
        fx,
        chunk,
        ids,
        vec![],
        extra,
        EdgeDiff::Owned {
            owner: Owner::KabanIngest,
        },
    )
}

/// [`submit`] with `extra_ops` upserted after the fixture's nodes and the chunk (an entity a
/// test invents as a second target) and the batch's `edge_diff` chosen by the caller
/// (`EdgeDiff::None` keeps a regression independent of the structural diff, RW1-1).
pub(crate) fn submit_with(
    store: &SqliteStore,
    fx: &Value,
    chunk: &NodeId,
    ids: &[&str],
    extra_ops: Vec<Op>,
    extra: Vec<BatchAssertion>,
    edge_diff: EdgeDiff,
) -> kaban_core::BatchReport {
    let mut ops = entity_ops(fx);
    ops.push(Op::UpsertNode {
        id: chunk.clone(),
        node: NewNode {
            kind: "Chunk".into(),
            text: fx["chunk"]["text"].as_str().unwrap().into(),
            ..Default::default()
        },
    });
    ops.extend(extra_ops);
    let mut b = WriteBatch::new(WorkspaceId::default(), ops);
    b.edge_diff = edge_diff;
    b.assertions = extra;
    b.assertions
        .extend(rows(fx, ids).into_iter().map(|r| claim(fx, r, chunk)));
    store
        .apply_batch(
            b,
            &WriteCtx::new("v1", WorkspaceId::default(), Actor::Agent),
        )
        .unwrap()
}
