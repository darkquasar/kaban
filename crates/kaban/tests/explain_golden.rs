//! The EN-F14 explanation goldens over the EN-F13 fixture (engine-plan E2.4 item 6): one insta
//! snapshot per variant (`forward`, `no_llm`, `a3_twice`, `human_first`), each the three
//! groups of chunk `c17` in group-key order, exactly as `kaban explain <key>` prints them; and
//! the `doctor --priors` check over a fresh store (EN-F16). The snapshots are accepted by hand
//! and read at cherry-pick (PROTOCOL §3): a move names the code change that moved it.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::Scratch;
use kaban_api::{ApiConfig, DoctorContext, KabanApi};
use kaban_core::{
    Actor, BatchAssertion, BatchSubject, ClaimClass, DatabaseFileModes, Fixed, GraphStore,
    IngestStore, Layer, MemoryStore, Method, NamespaceSet, NewNode, NodeId, OntologyFile, Op, Rank,
    WorkspaceId, WriteBatch, WriteCtx, resolve,
};
use kaban_policy::Policy;
use kaban_store_sqlite::{LATEST_SCHEMA_VERSION, SqliteStore};
use serde_json::{Value, json};
use std::sync::Arc;

fn fixture() -> Value {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../kaban-store-sqlite/fixtures/reconcile/orange-apple.json"
    );
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// A store reconciling against the shipped files plus the fixture's `infra` ontology.
fn store_with_infra(s: &Scratch, fx: &Value) -> SqliteStore {
    let mut files = kaban_core::shipped::files().unwrap();
    files.push(OntologyFile::from_yaml(fx["ontology"].as_str().unwrap()).unwrap());
    let ontology = resolve(&files, &NamespaceSet::new()).unwrap();
    SqliteStore::open(&s.db(), &s.home().join("keys"))
        .unwrap()
        .with_ontology(Arc::new(ontology))
        .unwrap()
}

fn u32_of(v: &Value) -> u32 {
    u32::try_from(v.as_u64().unwrap()).unwrap()
}

/// One fixture row as the claim a producer batch would carry about chunk `c17`
/// (`orange_apple.rs:55-83`).
fn claim(row: &Value, chunk: &NodeId) -> BatchAssertion {
    let text = |k: &str| row[k].as_str().unwrap();
    BatchAssertion {
        subject: BatchSubject::Span {
            chunk: chunk.clone(),
            offset: u32_of(&row["offset"]),
            len: u32_of(&row["len"]),
        },
        claim_class: ClaimClass::Type,
        predicate: None,
        object: None,
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

fn rows<'a>(fx: &'a Value, ids: &[&str]) -> Vec<&'a Value> {
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

/// The chunk `c17` written and the named rows (plus `extra`) submitted as one batch.
fn submit(
    store: &SqliteStore,
    fx: &Value,
    chunk: &NodeId,
    ids: &[&str],
    extra: Vec<BatchAssertion>,
) {
    let mut b = WriteBatch::new(
        WorkspaceId::default(),
        vec![Op::UpsertNode {
            id: chunk.clone(),
            node: NewNode {
                kind: "Chunk".into(),
                text: "c17".into(),
                ..Default::default()
            },
        }],
    );
    b.assertions = extra;
    b.assertions
        .extend(rows(fx, ids).into_iter().map(|r| claim(r, chunk)));
    store
        .apply_batch(
            b,
            &WriteCtx::new("v1", WorkspaceId::default(), Actor::Agent),
        )
        .unwrap();
}

/// The explanations of every group of the store (`n` of them), in group-key order (EN-F12a
/// rule (4)).
fn explanations(s: &Scratch, store: &SqliteStore, n: usize) -> Value {
    let conn = rusqlite::Connection::open(s.db()).unwrap();
    let keys: Vec<String> = conn
        .prepare("SELECT DISTINCT group_key FROM assertion_status ORDER BY group_key")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(keys.len(), n, "the groups of c17: {keys:?}");
    Value::Array(
        keys.iter()
            .map(|k| serde_json::to_value(store.explain_group(k).unwrap()).unwrap())
            .collect(),
    )
}

const FORWARD: [&str; 7] = ["a1", "a2", "a3", "a4", "a5", "a6", "a7"];

/// EN-F13 forward: span 27..32 Vendor `0.6556` over Fruit `0.3444` (margin `0.3111`), span
/// 27..40 Hardware alone, span 56..62 a three-way `candidate` below the margin.
#[test]
fn explain_forward() {
    let fx = fixture();
    let s = Scratch::new("e24-golden", "forward");
    let store = store_with_infra(&s, &fx);
    submit(&store, &fx, &NodeId::new(), &FORWARD, vec![]);
    insta::assert_json_snapshot!("explain_forward", explanations(&s, &store, 3));
}

/// EN-F13 "without a4/a6" (no LLM): span 27..32 unchanged, span 27..40 absent (a4 was its
/// one claim), span 56..62 Fruit `0.5531` over Vendor `0.4469`, still a `candidate`.
#[test]
fn explain_no_llm() {
    let fx = fixture();
    let s = Scratch::new("e24-golden", "no-llm");
    let store = store_with_infra(&s, &fx);
    submit(
        &store,
        &fx,
        &NodeId::new(),
        &["a1", "a2", "a3", "a5", "a7"],
        vec![],
    );
    insta::assert_json_snapshot!("explain_no_llm", explanations(&s, &store, 2));
}

/// EN-F13 "a3 twice": the full set, then a3 again from the same lineage (the same key, zero
/// new rows): the explanations are the forward ones.
#[test]
fn explain_a3_twice() {
    let fx = fixture();
    let s = Scratch::new("e24-golden", "a3-twice");
    let store = store_with_infra(&s, &fx);
    let chunk = NodeId::new();
    submit(&store, &fx, &chunk, &FORWARD, vec![]);
    submit(&store, &fx, &chunk, &["a3"], vec![]);
    insta::assert_json_snapshot!("explain_a3_twice", explanations(&s, &store, 3));
}

/// EN-F13 "human first": a human `preferred` link on span 27..32 before the full set: the
/// human lineage votes alone (`1.0000`), a1 and a3 sit under the same value, the group is
/// `protected_by` the human claim; the other two spans are the forward ones.
#[test]
fn explain_human_first() {
    let fx = fixture();
    let s = Scratch::new("e24-golden", "human-first");
    let store = store_with_infra(&s, &fx);
    let chunk = NodeId::new();
    submit(
        &store,
        &fx,
        &chunk,
        &[],
        vec![claim(&fx["human_first"], &chunk)],
    );
    submit(&store, &fx, &chunk, &FORWARD, vec![]);
    insta::assert_json_snapshot!("explain_human_first", explanations(&s, &store, 3));
}

/// EN-F16: the `priors` check of `doctor --priors` over a fresh store (the shipped ontology,
/// the EN-F12 default priors): every configured triple at `observed 0.5000 (accepted 0,
/// rejected 0)`. The detail is one `; `-joined line; the snapshot splits it for reading.
#[test]
fn doctor_priors_fresh() {
    let s = Scratch::new("e24-golden", "doctor-priors");
    let store = Arc::new(SqliteStore::open(&s.db(), &s.home().join("keys")).unwrap());
    let api = KabanApi::with_config(
        Arc::clone(&store) as Arc<dyn GraphStore>,
        Policy::empty(),
        store.signer(),
        None,
        ApiConfig::default(),
    );
    let report = api.doctor(&DoctorContext {
        files: DatabaseFileModes {
            db: DatabaseFileModes::OWNER_ONLY,
            wal: None,
            shm: None,
        },
        schema_version: LATEST_SCHEMA_VERSION,
        keys_dir: s.home().join("keys"),
        vacuum: false,
        priors: true,
    });
    let unresolved = report
        .checks
        .iter()
        .find(|c| c.name == "unresolved")
        .unwrap();
    assert_eq!(
        (unresolved.ok, unresolved.detail.as_str()),
        (true, "n: 0; groups: none")
    );
    let priors = report.checks.iter().find(|c| c.name == "priors").unwrap();
    let lines: Vec<&str> = priors.detail.split("; ").collect();
    insta::assert_json_snapshot!(
        "doctor_priors_fresh",
        json!({"name": priors.name, "ok": priors.ok, "detail": lines})
    );
}
