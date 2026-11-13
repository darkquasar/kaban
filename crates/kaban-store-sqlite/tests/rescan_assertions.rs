//! The policy rescan over `assertion.value` (engine-spec EN-S9; engine-plan E2.2 item 11):
//! a claim whose value carries a string the new policy denies is masked in place, set
//! `rejected`, and its conflict group is re-decided over the remaining supporters; every other
//! column of the claim row is byte-identical; an unchanged policy touches nothing. The judge
//! closure stands in for the policy (the store knows none). Invented data.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::Scratch;
use kaban_core::{
    Actor, AuditEntry, AuditOp, AuditStore, BatchAssertion, BatchSubject, ChangeLogStore,
    ClaimClass, Fixed, Governance, GovernanceStore, IngestStore, Layer, MemoryStore, Method,
    NewEvent, NewNode, NodeId, Op, Rank, SignerKind, WorkspaceId, WriteBatch, WriteCtx,
};
use kaban_store_sqlite::SqliteStore;
use rusqlite::{Connection, OpenFlags};
use serde_json::{Value, json};

/// The string the "new policy" denies and what replaces it (the rescan's mask text).
const CANARY: &str = "hunter2";
const MASK: &str = "[REDACTED:old-password]";
/// The `index_meta` key the store records the pass under.
const MARKER: &str = "assertion_policy_hash";

fn ctx() -> WriteCtx {
    WriteCtx::new("v1", WorkspaceId::default(), Actor::Agent)
}

fn upsert(id: &NodeId, text: &str) -> Op {
    Op::UpsertNode {
        id: id.clone(),
        node: NewNode {
            kind: "Fact".into(),
            text: text.into(),
            ..Default::default()
        },
    }
}

/// An `edge` claim about `src -RELATES_TO-> dst` carrying `value` (the props the projection
/// copies onto the edge).
fn edge_claim(
    src: &NodeId,
    dst: &NodeId,
    layer: Layer,
    method: Method,
    raw: u32,
    lineage: &str,
    value: Value,
) -> BatchAssertion {
    BatchAssertion {
        subject: BatchSubject::Edge {
            src: src.clone(),
            rel: "RELATES_TO".into(),
            dst: dst.clone(),
        },
        claim_class: ClaimClass::Edge,
        predicate: None,
        object: None,
        value,
        layer,
        method,
        extractor_id: Some("fixture".into()),
        extractor_version: Some("1".into()),
        source_id: Some("doc-1".into()),
        lineage_id: lineage.into(),
        raw_score: Fixed::new(raw).unwrap(),
        rank: Rank::Normal,
        rank_reason: None,
        calibration_version: 1,
        supersedes: None,
    }
}

fn raw(s: &Scratch) -> Connection {
    Connection::open_with_flags(s.db(), OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap()
}

/// Every `assertion` row as `(column name, text)` pairs, ordered by id.
fn claim_rows(conn: &Connection) -> Vec<Vec<(String, Option<String>)>> {
    let mut st = conn.prepare("SELECT * FROM assertion ORDER BY id").unwrap();
    let names: Vec<String> = st.column_names().map(str::to_owned).collect();
    st.query_map([], |r| {
        names
            .iter()
            .enumerate()
            .map(|(i, name)| {
                let text = match r.get_ref(i)? {
                    rusqlite::types::ValueRef::Null => None,
                    rusqlite::types::ValueRef::Integer(i) => Some(i.to_string()),
                    rusqlite::types::ValueRef::Real(f) => Some(f.to_string()),
                    rusqlite::types::ValueRef::Text(t) => {
                        Some(String::from_utf8_lossy(t).into_owned())
                    }
                    rusqlite::types::ValueRef::Blob(b) => Some(format!("{b:?}")),
                };
                Ok((name.clone(), text))
            })
            .collect()
    })
    .unwrap()
    .collect::<rusqlite::Result<_>>()
    .unwrap()
}

fn statuses(conn: &Connection) -> Vec<(String, String)> {
    conn.prepare(
        "SELECT a.layer, s.disposition FROM assertion_status s JOIN assertion a ON a.id = s.assertion_id ORDER BY a.id",
    )
    .unwrap()
    .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
    .unwrap()
    .collect::<rusqlite::Result<_>>()
    .unwrap()
}

/// `(layer, props)` of the one projected edge.
fn edge(conn: &Connection) -> (String, Value) {
    let (layer, props): (String, String) = conn
        .query_row("SELECT layer, props FROM edge", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    (layer, serde_json::from_str(&props).unwrap())
}

/// The policy stand-in: every string leaf containing the canary is masked and one redact
/// audit entry is pushed; a value without it is kept.
fn judge(claim: &kaban_core::Assertion, audit: &mut Vec<AuditEntry>) -> Option<Value> {
    fn mask(v: &Value) -> Value {
        match v {
            Value::String(s) => Value::String(s.replace(CANARY, MASK)),
            Value::Array(a) => Value::Array(a.iter().map(mask).collect()),
            Value::Object(o) => {
                Value::Object(o.iter().map(|(k, x)| (k.clone(), mask(x))).collect())
            }
            other => other.clone(),
        }
    }
    let masked = mask(claim.value.as_value());
    if masked == *claim.value.as_value() {
        return None;
    }
    audit.push(AuditEntry {
        op: AuditOp::Redact,
        tool: "rescan".into(),
        field: "assertion.value".into(),
        workspace_id: WorkspaceId::new(claim.workspace_id.as_str()),
        policy_version: "v2".into(),
        input_hmac: "fixture".into(),
        rules: vec!["old-password".into()],
        node_id: None,
    });
    Some(masked)
}

/// Slice (critical-tdd): the winning claim's value carries the canary; the rescan masks it,
/// sets it `rejected`, re-decides the group so the other supporter wins and the edge is
/// re-projected from it, leaves every other column byte-identical, and records the pass.
#[test]
fn a_claim_value_the_new_policy_denies_is_masked_rejected_and_its_group_redecided() {
    let s = Scratch::new("e22c-rescan-claims", "mask");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let (a, b) = (NodeId::new(), NodeId::new());
    let mut batch = WriteBatch::new(
        WorkspaceId::default(),
        vec![upsert(&a, "node a"), upsert(&b, "node b")],
    );
    batch.assertions = vec![
        // llm prior 0.75 x 1.0 = 0.75 leads gliner 0.6 x 0.9 = 0.54: the llm claim wins
        edge_claim(
            &a,
            &b,
            Layer::Llm,
            Method::Llm,
            10_000,
            "l1",
            json!({"note": format!("deploy token {CANARY} rotated")}),
        ),
        edge_claim(&a, &b, Layer::Gliner, Method::Gliner, 9000, "l2", json!({})),
    ];
    store.apply_batch(batch, &ctx()).unwrap();
    let conn = raw(&s);
    let before = claim_rows(&conn);
    assert_eq!(before.len(), 2);
    assert_eq!(
        statuses(&conn),
        vec![
            ("llm".to_owned(), "winner".to_owned()),
            ("gliner".to_owned(), "dissent".to_owned())
        ]
    );
    let (layer, props) = edge(&conn);
    assert_eq!(layer, "llm");
    assert!(props["note"].as_str().unwrap().contains(CANARY));
    let audit_before = store.audit_verify().unwrap().entries;
    let changes_before = store.changes_read(0, 10_000).unwrap().len();

    // Act: the policy changed (version v2); the pass judges every claim value
    let report = store.rescan_assertions("v2", &mut judge).unwrap();

    assert_eq!((report.scanned, report.kept, report.rejected), (2, 1, 1));
    let after = claim_rows(&conn);
    assert_eq!(after.len(), 2);
    // the masked row moves in `value` and in its retirement (`rank`, `rank_reason`; RE4-1),
    // nothing else; the kept row is byte-identical
    for (i, (row_before, row_after)) in before.iter().zip(&after).enumerate() {
        for ((name, b), (_, a)) in row_before.iter().zip(row_after) {
            if i == 0 && ["value", "rank", "rank_reason"].contains(&name.as_str()) {
                continue;
            }
            assert_eq!(a, b, "column {name} of row {i} is byte-identical");
        }
    }
    let column_of = |rows: &[Vec<(String, Option<String>)>], i: usize, col: &str| {
        rows[i]
            .iter()
            .find(|(n, _)| n == col)
            .and_then(|(_, v)| v.clone())
    };
    assert_eq!(
        column_of(&after, 0, "value").unwrap(),
        format!("{{\"note\":\"deploy token {MASK} rotated\"}}"),
        "the canary is masked in place (canonical JSON)"
    );
    assert_eq!(
        (
            column_of(&after, 0, "rank").as_deref(),
            column_of(&after, 0, "rank_reason").as_deref()
        ),
        (Some("deprecated"), Some("policy:old-password")),
        "the masked claim is retired under the rule the judge named"
    );
    assert_eq!(
        statuses(&conn),
        vec![
            ("llm".to_owned(), "rejected".to_owned()),
            ("gliner".to_owned(), "winner".to_owned())
        ],
        "the masked claim is out of the vote; the group is re-decided over the rest"
    );
    let (layer, props) = edge(&conn);
    assert_eq!(
        layer, "gliner",
        "the edge is re-projected from the new winner"
    );
    assert!(props.get("note").is_none(), "{props}");
    assert_eq!(props["provenance"].as_array().unwrap().len(), 1);
    assert_eq!(
        store.audit_verify().unwrap().entries,
        audit_before + 1,
        "the redact verdict rides in the same transaction"
    );
    assert!(store.changes_read(0, 10_000).unwrap().len() > changes_before);
    assert_eq!(store.index_meta().unwrap().get(MARKER), Some("v2"));
}

/// Slice (E2 interim review RE4-1): a masked claim is retired for good, not for the rescan's
/// own transaction. An unrelated write to its group afterwards (a third supporter) re-decides
/// the group without it: its status stays `rejected`, the edge stays the clean supporter's,
/// and the row carries `rank = deprecated` with the policy's rule id as the reason.
#[test]
fn a_masked_claim_does_not_vote_after_an_unrelated_write_to_its_group() {
    let s = Scratch::new("e22c-rescan-claims", "retired");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let (a, b) = (NodeId::new(), NodeId::new());
    let mut batch = WriteBatch::new(
        WorkspaceId::default(),
        vec![upsert(&a, "node a"), upsert(&b, "node b")],
    );
    batch.assertions = vec![
        edge_claim(
            &a,
            &b,
            Layer::Llm,
            Method::Llm,
            10_000,
            "l1",
            json!({"note": format!("deploy token {CANARY} rotated")}),
        ),
        edge_claim(&a, &b, Layer::Gliner, Method::Gliner, 9000, "l2", json!({})),
    ];
    store.apply_batch(batch, &ctx()).unwrap();
    let report = store.rescan_assertions("v2", &mut judge).unwrap();
    assert_eq!(report.rejected, 1);
    let conn = raw(&s);
    assert_eq!(edge(&conn).0, "gliner");

    // Act: an unrelated write to the group (a weak third supporter) re-decides it
    let mut batch = WriteBatch::new(WorkspaceId::default(), vec![]);
    batch.assertions = vec![edge_claim(
        &a,
        &b,
        Layer::Embed,
        Method::Embed,
        1000,
        "e1",
        json!({}),
    )];
    store.apply_batch(batch, &ctx()).unwrap();

    assert_eq!(
        statuses(&conn),
        vec![
            ("llm".to_owned(), "rejected".to_owned()),
            ("gliner".to_owned(), "winner".to_owned()),
            ("embed".to_owned(), "dissent".to_owned()),
        ],
        "the masked claim stays out of every later vote"
    );
    let (layer, props) = edge(&conn);
    assert_eq!(layer, "gliner");
    assert!(props.get("note").is_none(), "{props}");
    let (rank, reason): (String, Option<String>) = conn
        .query_row(
            "SELECT rank, rank_reason FROM assertion WHERE lineage_id = 'l1'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        (rank.as_str(), reason.as_deref()),
        ("deprecated", Some("policy:old-password")),
        "retired under the rule the judge named"
    );
}

/// A Decision whose props satisfy its kind's schema, accepted by a person: an `EVIDENCE_FOR`
/// edge into it is protected (EN-F12c `accepted_record_edge`).
fn accepted_decision(store: &SqliteStore) -> NodeId {
    let decision = match store
        .remember(
            NewNode {
                kind: "Decision".into(),
                text: "ship weekly".into(),
                props: Some(json!({"rationale": "a weekly cadence"})),
                ..Default::default()
            },
            &[],
            &ctx(),
        )
        .unwrap()
    {
        kaban_core::RememberOutcome::Created(n) => NodeId::parse(&n.id).unwrap(),
        other @ kaban_core::RememberOutcome::Existing { .. } => panic!("{other:?}"),
    };
    store
        .transition(
            &decision,
            Governance::Accepted,
            NewEvent {
                reason: "reviewed".into(),
                signer_kind: SignerKind::HumanCli,
                ..Default::default()
            },
            &WriteCtx::new("v1", WorkspaceId::default(), Actor::HumanCli),
        )
        .unwrap();
    decision
}

/// Slice (E2 interim review RE4-2; EN-S9 at rest): the props of a protected edge were copied
/// from the supporter the rescan masks. The protection of an accepted-record edge (EN-F12c)
/// must not keep the denied bytes: after the pass no `edge.props` and no `assertion.value`
/// carries them, whatever the protection says about the row otherwise.
#[test]
fn a_masked_supporter_of_an_accepted_record_edge_leaves_no_denied_bytes_in_edge_props() {
    let s = Scratch::new("e22c-rescan-claims", "accepted-record");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let decision = accepted_decision(&store);
    let chunk = NodeId::new();
    let evidence = |layer, method, raw, lineage, value| {
        let mut claim = edge_claim(&chunk, &decision, layer, method, raw, lineage, value);
        claim.subject = BatchSubject::Edge {
            src: chunk.clone(),
            rel: "EVIDENCE_FOR".into(),
            dst: decision.clone(),
        };
        claim
    };
    let mut batch = WriteBatch::new(
        WorkspaceId::default(),
        vec![upsert(&chunk, "evidence text")],
    );
    batch.assertions = vec![
        // llm 0.75 x 1.0 leads gliner 0.6 x 0.9: the llm claim wins and its value is copied
        // onto the edge; the first machine edge into the record is created normally
        evidence(
            Layer::Llm,
            Method::Llm,
            10_000,
            "l1",
            json!({"note": format!("deploy token {CANARY} rotated")}),
        ),
        evidence(Layer::Gliner, Method::Gliner, 9000, "g1", json!({})),
    ];
    store.apply_batch(batch, &ctx()).unwrap();
    let conn = raw(&s);
    let count = |sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get(0)).unwrap() };
    assert_eq!(
        count("SELECT count(*) FROM edge WHERE rel = 'EVIDENCE_FOR' AND layer = 'llm'"),
        1
    );
    assert_eq!(
        count(&format!(
            "SELECT count(*) FROM edge WHERE instr(props, '{CANARY}') > 0"
        )),
        1,
        "the denied bytes sit in the protected row before the pass"
    );

    let report = store.rescan_assertions("v2", &mut judge).unwrap();

    assert_eq!(report.rejected, 1, "{report:?}");
    assert_eq!(
        count(&format!(
            "SELECT count(*) FROM assertion WHERE instr(value, '{CANARY}') > 0"
        )),
        0,
        "the claim value is masked"
    );
    assert_eq!(
        count(&format!(
            "SELECT count(*) FROM edge WHERE instr(props, '{CANARY}') > 0"
        )),
        0,
        "no edge row keeps the denied bytes, protected or not"
    );
    let llm: String = conn
        .query_row(
            "SELECT s.disposition FROM assertion a JOIN assertion_status s ON s.assertion_id = a.id WHERE a.lineage_id = 'l1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(llm, "rejected");
}

/// Slice (E2 interim review RE4-3; two `SqliteStore` opens on one file, as `busy_open.rs`):
/// process A completes the pass under policy v2; process B, still on the old policy, inserts
/// a claim the new policy denies; A's next pass under v2 judges that claim (every row above
/// the high-water id) and masks it, and a further pass with nothing new writes nothing.
#[test]
fn a_claim_inserted_by_a_second_connection_after_the_rescan_is_masked_by_the_next_one() {
    let s = Scratch::new("e22c-rescan-claims", "high-water");
    let proc_a = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let (src, dst) = (NodeId::new(), NodeId::new());
    let mut batch = WriteBatch::new(
        WorkspaceId::default(),
        vec![upsert(&src, "node src"), upsert(&dst, "node dst")],
    );
    batch.assertions = vec![edge_claim(
        &src,
        &dst,
        Layer::Gliner,
        Method::Gliner,
        9000,
        "g1",
        json!({}),
    )];
    proc_a.apply_batch(batch, &ctx()).unwrap();
    let first = proc_a.rescan_assertions("v2", &mut judge).unwrap();
    assert_eq!((first.scanned, first.rejected), (1, 0));
    assert_eq!(proc_a.index_meta().unwrap().get(MARKER), Some("v2"));

    // B (an older process) inserts under the old policy: nobody judged this value
    let proc_b = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let mut batch = WriteBatch::new(WorkspaceId::default(), vec![]);
    batch.assertions = vec![edge_claim(
        &src,
        &dst,
        Layer::Llm,
        Method::Llm,
        10_000,
        "l1",
        json!({"note": format!("deploy token {CANARY}")}),
    )];
    proc_b.apply_batch(batch, &ctx()).unwrap();
    let conn = raw(&s);
    assert_eq!(edge(&conn).0, "llm", "the unjudged claim won its group");

    // Act: A's next pass under the same policy
    let second = proc_a.rescan_assertions("v2", &mut judge).unwrap();

    assert_eq!(
        (second.scanned, second.kept, second.rejected),
        (1, 0, 1),
        "the new row alone is judged: {second:?}"
    );
    let value: String = conn
        .query_row(
            "SELECT value FROM assertion WHERE lineage_id = 'l1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(!value.contains(CANARY), "{value}");
    assert_eq!(
        statuses(&conn),
        vec![
            ("gliner".to_owned(), "winner".to_owned()),
            ("llm".to_owned(), "rejected".to_owned())
        ]
    );
    assert_eq!(edge(&conn).0, "gliner");
    // nothing new: the pass returns without judging or writing
    let changes = proc_a.changes_read(0, 10_000).unwrap().len();
    let mut calls = 0;
    let third = proc_a
        .rescan_assertions("v2", &mut |claim, audit| {
            calls += 1;
            judge(claim, audit)
        })
        .unwrap();
    assert_eq!((third, calls), (kaban_core::RescanReport::default(), 0));
    assert_eq!(proc_a.changes_read(0, 10_000).unwrap().len(), changes);
}

/// An unchanged policy touches nothing: the judge is never called, no row and no `ChangeLog`
/// row changes, the report is empty.
#[test]
fn an_unchanged_policy_touches_nothing() {
    let s = Scratch::new("e22c-rescan-claims", "unchanged");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let (a, b) = (NodeId::new(), NodeId::new());
    let mut batch = WriteBatch::new(
        WorkspaceId::default(),
        vec![upsert(&a, "node a"), upsert(&b, "node b")],
    );
    batch.assertions = vec![edge_claim(
        &a,
        &b,
        Layer::Llm,
        Method::Llm,
        9000,
        "l1",
        json!({"note": format!("deploy token {CANARY}")}),
    )];
    store.apply_batch(batch, &ctx()).unwrap();
    let first = store.rescan_assertions("v2", &mut judge).unwrap();
    assert_eq!((first.scanned, first.rejected), (1, 1));
    let conn = raw(&s);
    let rows = claim_rows(&conn);
    let st = statuses(&conn);
    let changes = store.changes_read(0, 10_000).unwrap().len();
    let audit = store.audit_verify().unwrap().entries;

    let mut calls = 0;
    let report = store
        .rescan_assertions("v2", &mut |claim, audit| {
            calls += 1;
            judge(claim, audit)
        })
        .unwrap();

    assert_eq!(report, kaban_core::RescanReport::default());
    assert_eq!(calls, 0, "an equal marker never judges a row");
    assert_eq!(claim_rows(&conn), rows);
    assert_eq!(statuses(&conn), st);
    assert_eq!(store.changes_read(0, 10_000).unwrap().len(), changes);
    assert_eq!(store.audit_verify().unwrap().entries, audit);
}
