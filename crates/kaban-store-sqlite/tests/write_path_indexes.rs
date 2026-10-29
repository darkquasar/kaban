//! Bounded write-path plans and public behavior on invented data (OWN-120).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use common::{Scratch, created, ctx, nid};
use kaban_core::ActorId;
use kaban_core::{
    BatchAssertion, BatchSubject, ClaimClass, Fixed, ForgetMode, IngestStore, Layer, MemoryStore,
    Method, NewNode, Rank, RememberOutcome, WorkspaceId, WriteBatch, WriteCtx,
};
use kaban_store_sqlite::SqliteStore;
use rusqlite::{Connection, ToSql, params};
use serde_json::json;

fn plan(conn: &Connection, sql: &str, values: &[&dyn ToSql]) -> Vec<String> {
    conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
        .unwrap()
        .query_map(values, |row| row.get(3))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

#[test]
#[allow(clippy::print_stderr)] // Test-only query-plan evidence, never the MCP stdout channel.
fn retirement_and_duplicate_queries_use_bounded_index_lookups() {
    let s = Scratch::new("s30-write-store", "plans");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    created(&store, "Fact", "plan source");
    created(&store, "Fact", "plan object");
    let conn = Connection::open(s.db()).unwrap();
    SqliteStore::bind_writer(&conn, &ActorId::default()).unwrap();
    // Populated immutable claims and statuses, including NULL objects and shared groups.
    conn.execute_batch(
        "WITH RECURSIVE fixture(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM fixture WHERE i<512)
         INSERT INTO assertion(key,workspace_id,subject_kind,subject_rid,claim_class,object_rid,
             value,layer,method,lineage_id,raw_score,ts)
         SELECT 'plan-'||i,'default','node',1,'type',CASE WHEN i%4=0 THEN 2 END,
             '\"Fact\"','human','human','lineage-'||i,9000,1 FROM fixture;
         INSERT INTO assertion_status(assertion_id,disposition,group_key,updated_at)
         SELECT id,'winner','group-'||(id%32),1 FROM assertion;",
    )
    .unwrap();
    let queries = [
        (
            "groups_of_node",
            "SELECT DISTINCT s.group_key FROM assertion_status s JOIN assertion a ON a.id = s.assertion_id WHERE a.subject_rid = ?1 OR a.chunk_rid = ?1 OR a.object_rid = ?1 ORDER BY s.group_key",
        ),
        (
            "claim_ids_of_node",
            "SELECT id FROM assertion WHERE subject_rid = ?1 OR chunk_rid = ?1 OR object_rid = ?1 ORDER BY id",
        ),
        (
            "retire_rank",
            "UPDATE assertion SET rank = 'deprecated', rank_reason = ?2 WHERE rank <> 'deprecated' AND (subject_rid = ?1 OR chunk_rid = ?1 OR object_rid = ?1)",
        ),
        (
            "reject_status",
            "UPDATE assertion_status SET disposition = 'rejected', updated_at = ?2 WHERE disposition <> 'rejected' AND assertion_id IN (SELECT id FROM assertion WHERE subject_rid = ?1 OR chunk_rid = ?1 OR object_rid = ?1)",
        ),
    ];
    let mut unbounded = Vec::new();
    for (name, sql) in queries {
        let rid = 2_i64;
        let reason = "forgotten";
        let now = 2_i64;
        let values: Vec<&dyn ToSql> = match name {
            "retire_rank" => vec![&rid, &reason],
            "reject_status" => vec![&rid, &now],
            _ => vec![&rid],
        };
        let details = plan(&conn, sql, &values);
        eprintln!("{name}: {details:?}");
        if details.iter().any(|detail| detail.starts_with("SCAN ")) {
            unbounded.push(name);
        }
    }
    let duplicate = plan(
        &conn,
        "SELECT * FROM node WHERE lifecycle='active' AND workspace_id=?1 AND dup_key=?2 ORDER BY created_at, id",
        &[&"default", &"shared"],
    );
    eprintln!("find_dup_key: {duplicate:?}");
    assert!(
        unbounded.is_empty(),
        "retirement must not scan claim/status tables: {unbounded:?}"
    );
    assert!(
        duplicate
            .iter()
            .any(|detail| detail.contains("workspace_id=? AND dup_key=?")),
        "duplicate lookup must constrain both indexed coordinates: {duplicate:?}"
    );
}

#[test]
fn duplicate_lookup_is_nonunique_ordered_workspace_scoped_and_active_only() {
    let s = Scratch::new("s30-write-store", "duplicates");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let remember = |text: &str, workspace: &str, key: Option<&str>| {
        let write = WriteCtx::new("pv", WorkspaceId::new(workspace), kaban_core::Actor::Agent);
        match store
            .remember(
                NewNode {
                    kind: "Fact".into(),
                    text: text.into(),
                    workspace_id: Some(workspace.into()),
                    dup_key: key.map(str::to_owned),
                    ..Default::default()
                },
                &[],
                &write,
            )
            .unwrap()
        {
            RememberOutcome::Created(node) => node,
            other @ RememberOutcome::Existing { .. } => panic!("fresh fixture: {other:?}"),
        }
    };
    let a = remember("first duplicate", "default", Some("shared"));
    let b = remember("second duplicate", "default", Some("shared"));
    let archived = remember("archived duplicate", "default", Some("shared"));
    let hidden = remember("hidden duplicate", "default", Some("shared"));
    let other = remember("other tenant duplicate", "other", Some("shared"));
    remember("unkeyed note", "default", None);
    remember("different key", "default", Some("different"));
    store
        .forget(&nid(&archived.id), ForgetMode::Archive, &ctx())
        .unwrap();
    store
        .forget(&nid(&hidden.id), ForgetMode::Tombstone, &ctx())
        .unwrap();
    // Equal timestamps explicitly exercise the id tie-break without a timing assertion.
    let conn = Connection::open(s.db()).unwrap();
    SqliteStore::bind_writer(&conn, &ActorId::default()).unwrap();
    conn.execute(
        "UPDATE node SET created_at='2026-10-26T08:16:02Z' WHERE id IN (?1,?2)",
        params![a.id, b.id],
    )
    .unwrap();
    let mut expected = vec![a.id, b.id];
    expected.sort();
    let ids = |workspace| {
        store
            .find_dup_key(workspace, "shared")
            .unwrap()
            .into_iter()
            .map(|n| n.id)
            .collect::<Vec<_>>()
    };
    assert_eq!(ids("default"), expected);
    assert_eq!(ids("other"), vec![other.id]);
    assert!(store.find_dup_key("default", "missing").unwrap().is_empty());
}

fn edge(src: &kaban_core::NodeId, dst: &kaban_core::NodeId, lineage: &str) -> BatchAssertion {
    BatchAssertion {
        subject: BatchSubject::Edge {
            src: src.clone(),
            rel: "RELATES_TO".into(),
            dst: dst.clone(),
        },
        claim_class: ClaimClass::Edge,
        predicate: None,
        object: None,
        value: json!({}),
        layer: Layer::Human,
        method: Method::Human,
        extractor_id: None,
        extractor_version: None,
        source_id: None,
        lineage_id: lineage.into(),
        raw_score: Fixed::new(9000).unwrap(),
        rank: Rank::Normal,
        rank_reason: None,
        calibration_version: 1,
        supersedes: None,
    }
}

#[test]
fn forgetting_retires_incoming_outgoing_and_overlapping_claims_once() {
    let s = Scratch::new("s30-write-store", "retirement");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let target = nid(&created(&store, "Fact", "forgotten target").id);
    let a = nid(&created(&store, "Fact", "retained source").id);
    let b = nid(&created(&store, "Fact", "retained destination").id);
    let mut human_retired = edge(&a, &target, "human-retired");
    human_retired.rank = Rank::Deprecated;
    human_retired.rank_reason = Some("human decision".into());
    let mut overlap = edge(&target, &b, "overlap");
    overlap.subject = BatchSubject::Node(target.clone());
    overlap.claim_class = ClaimClass::Type;
    overlap.object = Some(target.clone());
    overlap.value = json!("Fact");
    let mut batch = WriteBatch::new(WorkspaceId::default(), vec![]);
    batch.assertions = vec![
        edge(&a, &target, "incoming"),
        human_retired,
        edge(&target, &b, "outgoing"),
        overlap,
        edge(&a, &b, "unrelated"),
    ];
    store.apply_batch(batch, &ctx()).unwrap();
    let conn = Connection::open(s.db()).unwrap();
    let before: i64 = conn
        .query_row("SELECT coalesce(max(seq),0) FROM changes", [], |r| r.get(0))
        .unwrap();
    store
        .forget(&target, ForgetMode::Tombstone, &ctx())
        .unwrap();
    let rows: Vec<(String,String,Option<String>,String)> = conn.prepare(
        "SELECT a.lineage_id,a.rank,a.rank_reason,s.disposition FROM assertion a JOIN assertion_status s ON s.assertion_id=a.id ORDER BY a.lineage_id"
    ).unwrap().query_map([],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).unwrap().collect::<rusqlite::Result<_>>().unwrap();
    assert_eq!(rows.len(), 5, "immutable claims are retained");
    for (lineage, rank, reason, status) in &rows {
        if lineage == "unrelated" {
            assert_eq!(
                (rank.as_str(), reason.as_deref(), status.as_str()),
                ("normal", None, "winner")
            );
        } else {
            let expected_reason = if lineage == "human-retired" {
                "human decision"
            } else {
                "forgotten"
            };
            assert_eq!(
                (rank.as_str(), reason.as_deref(), status.as_str()),
                ("deprecated", Some(expected_reason), "rejected")
            );
        }
    }
    let status_changes: i64 = conn
        .query_row(
            "SELECT count(*) FROM changes WHERE seq>?1 AND entity='assertion' AND op='status'",
            [before],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        status_changes, 4,
        "overlapping coordinates/shared groups must not duplicate status changes"
    );
    let edges: i64 = conn
        .query_row("SELECT count(*) FROM edge", [], |r| r.get(0))
        .unwrap();
    assert_eq!(edges, 1, "only the unrelated edge remains");
}
