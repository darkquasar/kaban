//! The EN-F14a coverage fixture (engine-plan E2.5): one item per status over the EN-F13
//! "orange or apple" rows (`fixtures/reconcile/orange-apple.json`), a discovered `x:` kind,
//! a proposed and an accepted Decision, a superseded one, a tombstoned Fact and an equal-rev
//! conflict row. Shared by `explain_status.rs`, `review_golden.rs` and
//! `no_probability_words.rs` through `#[path]`.

#![allow(dead_code)] // each test binary uses a subset
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

use kaban_api::{AcceptRequest, ApiConfig, Caller, KabanApi, RememberRequest};
use kaban_core::{
    Actor, BatchAssertion, BatchSubject, ClaimClass, ConflictRow, EdgeDiff, Fixed, IngestStore,
    Layer, Method, NamespaceSet, NewNode, NodeId, OntologyFile, Op, Owner, Rank, SchemaStore,
    SyncStore, TypeDeclaration, TypeDef, WorkspaceId, WriteBatch, WriteCtx, resolve, stored_name,
};
use kaban_policy::Policy;
use kaban_store_sqlite::SqliteStore;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;

/// Private scratch directory (db + keys), removed on drop.
pub(crate) struct Scratch(pub(crate) PathBuf);
impl Scratch {
    pub(crate) fn new(name: &str) -> Self {
        let d = std::env::temp_dir().join(format!("e25b-status-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        Self(d)
    }
    pub(crate) fn db(&self) -> PathBuf {
        self.0.join("kaban.db")
    }
    pub(crate) fn keys(&self) -> PathBuf {
        self.0.join("keys")
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// `fixtures/reconcile/orange-apple.json`, parsed.
pub(crate) fn fixture() -> Value {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../kaban-store-sqlite/fixtures/reconcile/orange-apple.json"
    );
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// A store reconciling against the shipped files plus the fixture's `infra` ontology, the
/// `infra` kinds declared on the store too (`tests/common/orange.rs` of the store crate).
pub(crate) fn store_with_infra(s: &Scratch, fx: &Value) -> Arc<SqliteStore> {
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
    Arc::new(store)
}

fn u32_of(v: &Value) -> u32 {
    u32::try_from(v.as_u64().unwrap()).unwrap()
}

pub(crate) fn entity_id(fx: &Value, name: &str) -> NodeId {
    NodeId::parse(fx["nodes"][name]["id"].as_str().unwrap()).unwrap()
}

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

/// One fixture row as the claim a producer batch would carry about chunk `c17`.
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

/// A human `preferred` claim on span 56..62 of `chunk` (two of them disagreeing make an
/// `unresolved: preferred_conflict` group, EN-F12b).
pub(crate) fn human_claim(chunk: &NodeId, value: &str, lineage: &str) -> BatchAssertion {
    BatchAssertion {
        subject: BatchSubject::Span {
            chunk: chunk.clone(),
            offset: 56,
            len: 6,
        },
        claim_class: ClaimClass::Type,
        predicate: None,
        object: None,
        value: json!(value),
        layer: Layer::Human,
        method: Method::Link,
        extractor_id: None,
        extractor_version: None,
        source_id: Some("c17".into()),
        lineage_id: lineage.to_owned(),
        raw_score: Fixed::ONE,
        rank: Rank::Preferred,
        rank_reason: None,
        calibration_version: 1,
        supersedes: None,
    }
}

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

/// The fixture's entity nodes and the chunk `c17` written and the named rows (plus `extra`)
/// submitted as one batch (agent actor, policy `v1`).
pub(crate) fn submit(
    store: &SqliteStore,
    fx: &Value,
    chunk: &NodeId,
    ids: &[&str],
    extra: Vec<BatchAssertion>,
) {
    let mut ops = entity_ops(fx);
    ops.push(Op::UpsertNode {
        id: chunk.clone(),
        node: NewNode {
            kind: "Chunk".into(),
            text: fx["chunk"]["text"].as_str().unwrap().into(),
            ..Default::default()
        },
    });
    let mut b = WriteBatch::new(WorkspaceId::default(), ops);
    b.edge_diff = EdgeDiff::Owned {
        owner: Owner::KabanIngest,
    };
    b.assertions = extra;
    b.assertions
        .extend(rows(fx, ids).into_iter().map(|r| claim(fx, r, chunk)));
    store
        .apply_batch(
            b,
            &WriteCtx::new("v1", WorkspaceId::default(), Actor::Agent),
        )
        .unwrap();
}

/// The service over `store` (empty policy, the shipped registry, the default workspace).
pub(crate) fn api_over(store: &Arc<SqliteStore>) -> KabanApi {
    KabanApi::with_config(
        Arc::clone(store) as Arc<dyn kaban_core::GraphStore>,
        Policy::empty(),
        store.signer(),
        None,
        ApiConfig::default(),
    )
}

pub(crate) fn human() -> Caller {
    Caller {
        kind: Actor::HumanCli,
        ..Default::default()
    }
}

/// The claim key of the fixture row `id` (its stable key, EN-F12a), read from the store.
pub(crate) fn key_of(s: &Scratch, id: &str) -> String {
    let conn = rusqlite::Connection::open(s.db()).unwrap();
    conn.query_row(
        "SELECT key FROM assertion WHERE extractor_id = ?1",
        [id],
        |r| r.get(0),
    )
    .unwrap()
}

/// The coverage fixture: every item by name.
pub(crate) struct Items {
    pub(crate) store: Arc<SqliteStore>,
    pub(crate) api: KabanApi,
    /// Chunk `c17` with the forward rows: a `candidate: below_margin` span (56..62).
    pub(crate) chunk: NodeId,
    /// A second chunk whose span 56..62 carries two disagreeing human `preferred` claims.
    pub(crate) chunk_unresolved: NodeId,
    /// A record of an undeclared kind: `x:Widget`, `candidate: discovered_kind`.
    pub(crate) discovered: String,
    /// A proposed Decision.
    pub(crate) proposed: String,
    /// An accepted Decision.
    pub(crate) accepted: String,
    /// A Decision superseded by `accepted`.
    pub(crate) superseded: String,
    /// A tombstoned Fact.
    pub(crate) forgotten: String,
    /// An active Fact with nothing pending.
    pub(crate) active: String,
    /// A Convention whose file and database disagree at the same rev.
    pub(crate) conflict: String,
}

pub(crate) fn items(s: &Scratch) -> Items {
    let fx = fixture();
    let store = store_with_infra(s, &fx);
    let chunk = NodeId::new();
    submit(
        &store,
        &fx,
        &chunk,
        &["a1", "a2", "a3", "a4", "a5", "a6", "a7"],
        vec![],
    );
    let chunk_unresolved = NodeId::new();
    submit(
        &store,
        &fx,
        &chunk_unresolved,
        &[],
        vec![
            human_claim(&chunk_unresolved, "infra:Team", "human:cli:1"),
            human_claim(&chunk_unresolved, "infra:Vendor", "human:cli:2"),
        ],
    );
    let api = api_over(&store);
    let remember = |kind: &str, text: &str| -> String {
        let props = (kind == "Decision").then(|| json!({"rationale": "because"}));
        api.remember(RememberRequest {
            kind: kind.into(),
            text: text.into(),
            props,
            ..Default::default()
        })
        .unwrap()
        .id
    };
    let discovered = remember("Widget", "a widget nobody declared");
    let proposed = remember("Decision", "keep the proposed decision pending");
    let accepted = remember("Decision", "the accepted decision");
    let superseded = remember("Decision", "the older decision");
    let forgotten = remember("Fact", "a fact to forget");
    let active = remember("Fact", "a plain fact");
    let conflict = remember("Convention", "a convention in conflict");
    let accept = |id: &str| {
        api.accept(
            AcceptRequest {
                id: id.into(),
                reason: Some("reviewed".into()),
                path_scope: vec!["src/**".into()],
                ..Default::default()
            },
            &human(),
        )
        .unwrap();
    };
    accept(&accepted);
    accept(&superseded);
    api.supersede(&accepted, &superseded, &human()).unwrap();
    api.forget(kaban_api::ForgetRequest {
        id: forgotten.clone(),
        mode: None,
    })
    .unwrap();
    store
        .conflict_upsert(&ConflictRow {
            node_id: conflict.clone(),
            file_rev: 1,
            db_rev: 1,
            file_hash: "f".repeat(64),
            db_hash: "d".repeat(64),
            detected_at: 1_793_004_407,
        })
        .unwrap();
    Items {
        store,
        api,
        chunk,
        chunk_unresolved,
        discovered,
        proposed,
        accepted,
        superseded,
        forgotten,
        active,
        conflict,
    }
}
