//! Write stamps (spec 05 SS-26): every record and change row a store writes carries the
//! process's writer (`bind_actor`, `local` by default), this copy's instance (`db_id`) and the
//! pack lineage, in the same transaction, for public mutations and internal maintenance alike.
//! Rows written before migration 0009 carry no stamp and read as unknown; the audit chain is
//! unchanged.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::{Scratch, created, ctx, nid};
use kaban_core::{
    ActorId, AuditEntry, AuditOp, AuditStore, ChangeLogStore, Governance, GovernanceStore,
    IngestStore, LeaseStore, LinkReq, MemoryStore, NewEvent, NewNode, Node, RememberOutcome,
    RescanAction, SignerKind, WorkspaceId,
};
use kaban_store_sqlite::SqliteStore;
use rusqlite::{Connection, OptionalExtension};

/// `(actor_id, instance_id, pack_lineage_id)` of one row.
type Stamp = (Option<String>, Option<String>, Option<String>);

fn stamp_of(conn: &Connection, table: &str, id: &str) -> Stamp {
    conn.query_row(
        &format!("SELECT actor_id, instance_id, pack_lineage_id FROM {table} WHERE id = ?1"),
        [id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )
    .unwrap()
}

/// Every change row naming `id`, oldest first: `(op, stamp)`.
fn change_stamps(conn: &Connection, id: &str) -> Vec<(String, Stamp)> {
    let mut st = conn
        .prepare(
            "SELECT op, actor_id, instance_id, pack_lineage_id FROM changes WHERE id = ?1 ORDER BY seq",
        )
        .unwrap();
    st.query_map([id], |r| Ok((r.get(0)?, (r.get(1)?, r.get(2)?, r.get(3)?))))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

fn meta(conn: &Connection, key: &str) -> Option<String> {
    conn.query_row("SELECT value FROM index_meta WHERE key = ?1", [key], |r| {
        r.get(0)
    })
    .optional()
    .unwrap()
}

fn actor(name: &str) -> ActorId {
    ActorId::parse(name).unwrap()
}

fn bound(s: &Scratch, name: &str) -> SqliteStore {
    SqliteStore::open_as(&s.db(), &s.keys(), &actor(name)).unwrap()
}

const AGENT: &str = "agent-7";
const SESSION: &str = "session-42";
const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

/// A Decision with real provenance: a producing agent and session, accepted at `COMMIT` by an
/// event carrying the same session, with two audit rows appended beside it (SS-26: all of it
/// must survive stamping, maintenance and migration byte for byte).
fn with_provenance(store: &SqliteStore, text: &str) -> Node {
    let RememberOutcome::Created(n) = store
        .remember(
            NewNode {
                kind: "Decision".into(),
                text: text.into(),
                agent_id: Some(AGENT.into()),
                session_id: Some(SESSION.into()),
                ..Default::default()
            },
            &[],
            &ctx(),
        )
        .unwrap()
    else {
        panic!("fresh node expected")
    };
    store
        .transition(
            &nid(&n.id),
            Governance::Accepted,
            NewEvent {
                reason: "seeded".into(),
                signer_kind: SignerKind::HumanCli,
                session_id: Some(SESSION.into()),
                commit_sha: Some(COMMIT.into()),
                ..Default::default()
            },
            &ctx(),
        )
        .unwrap();
    store
        .audit_append(&[audit_row(1, &n.id), audit_row(2, &n.id)])
        .unwrap();
    n
}

fn audit_row(i: usize, node: &str) -> AuditEntry {
    AuditEntry {
        op: AuditOp::Redact,
        tool: "remember".into(),
        field: "text".into(),
        workspace_id: WorkspaceId::default(),
        policy_version: "pv".into(),
        input_hmac: format!("{i:064x}"),
        rules: vec!["aws-access-key-id".into()],
        node_id: Some(node.to_owned()),
    }
}

/// Everything SS-26 must preserve, as stored: the node's producer and commit columns, its
/// events and the whole audit table.
fn provenance(conn: &Connection, id: &str) -> Vec<Vec<rusqlite::types::Value>> {
    let rows = |sql: &str, params: &[&str]| -> Vec<Vec<rusqlite::types::Value>> {
        conn.prepare(sql)
            .unwrap()
            .query_map(rusqlite::params_from_iter(params), |row| {
                (0..row.as_ref().column_count())
                    .map(|i| row.get(i))
                    .collect()
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    };
    let mut out = rows(
        "SELECT agent_id, session_id, repo, commit_sha, accepted_commit, governance FROM node WHERE id = ?1",
        &[id],
    );
    out.extend(rows(
        "SELECT node_id, kind, reason, path_scope, signer, signer_kind, host, session_id, ts, commit_sha, workspace_id
         FROM events WHERE node_id = ?1 ORDER BY seq",
        &[id],
    ));
    out.extend(rows("SELECT * FROM audit ORDER BY seq", &[]));
    out
}

#[test]
fn two_actors_on_one_instance_stamp_public_and_maintenance_writes() {
    let s = Scratch::new("kaban-stamp", "two-actors");
    let alice = bound(&s, "alice");
    let bob = bound(&s, "bob");
    let conn = Connection::open(s.db()).unwrap();
    let instance = meta(&conn, "db_id");
    let lineage = meta(&conn, "pack_lineage_id");
    assert!(lineage.is_some(), "a fresh database gets its pack lineage");
    assert_ne!(lineage, instance, "the lineage is its own identity");
    let by = |name: &str| (Some(name.to_owned()), instance.clone(), lineage.clone());

    // public mutations: each actor's own records and change rows
    let a = with_provenance(&alice, "stamped by alice");
    let seeded = provenance(&conn, &a.id);
    assert_eq!(seeded.len(), 4, "node, event, two audit rows: {seeded:?}");
    let RememberOutcome::Created(b) = bob
        .remember(
            NewNode {
                kind: "Idea".into(),
                text: "stamped by bob".into(),
                ..Default::default()
            },
            &[LinkReq {
                rel: "RELATES_TO".into(),
                to: Some(nid(&a.id)),
                ..Default::default()
            }],
            &ctx(),
        )
        .unwrap()
    else {
        panic!("fresh node expected")
    };
    assert_eq!(stamp_of(&conn, "node", &a.id), by("alice"));
    assert_eq!(stamp_of(&conn, "node", &b.id), by("bob"));
    assert_eq!(
        change_stamps(&conn, &a.id),
        vec![
            ("insert".into(), by("alice")),
            ("update".into(), by("alice"))
        ],
        "the write and its acceptance"
    );
    assert_eq!(
        change_stamps(&conn, &b.id),
        vec![("insert".into(), by("bob"))],
        "the stamp copy onto the record emits no second change row"
    );
    let edge: String = conn
        .query_row("SELECT id FROM edge", [], |r| r.get(0))
        .unwrap();
    assert_eq!(stamp_of(&conn, "edge", &edge), by("bob"));
    assert_eq!(
        change_stamps(&conn, &edge),
        vec![("insert".into(), by("bob"))]
    );

    // internal maintenance: bob's rescan restamps alice's record under the new policy version
    let report = bob
        .rescan("pv-rescanned", 100, &mut |_| RescanAction::Keep)
        .unwrap();
    assert!(report.scanned >= 2, "{report:?}");
    assert_eq!(stamp_of(&conn, "node", &a.id), by("bob"));
    assert_eq!(
        change_stamps(&conn, &a.id),
        vec![
            ("insert".into(), by("alice")),
            ("update".into(), by("alice")),
            ("update".into(), by("bob"))
        ]
    );

    // no change row escapes the stamp, and existing provenance is untouched byte for byte
    let unstamped: i64 = conn
        .query_row(
            "SELECT count(*) FROM changes WHERE actor_id IS NULL OR instance_id IS NULL OR pack_lineage_id IS NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(unstamped, 0);
    assert_eq!(provenance(&conn, &a.id), seeded);
    assert_eq!(
        seeded[0],
        vec![
            rusqlite::types::Value::Text(AGENT.into()),
            rusqlite::types::Value::Text(SESSION.into()),
            rusqlite::types::Value::Null,
            rusqlite::types::Value::Null,
            rusqlite::types::Value::Text(COMMIT.into()),
            rusqlite::types::Value::Text("accepted".into()),
        ]
    );
    let verified = bob.audit_verify().unwrap();
    assert!(verified.ok && verified.entries == 2, "{verified:?}");
}

#[test]
fn an_unbound_store_stamps_local() {
    let s = Scratch::new("kaban-stamp", "local");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let n = created(&store, "Fact", "nobody configured an actor");
    let conn = Connection::open(s.db()).unwrap();
    let (who, instance, lineage) = stamp_of(&conn, "node", &n.id);
    assert_eq!(who.as_deref(), Some(ActorId::LOCAL));
    assert_eq!(instance.as_deref(), Some(store.db_id()));
    assert_eq!(lineage, meta(&conn, "pack_lineage_id"));
    assert_eq!(
        change_stamps(&conn, &n.id),
        vec![("insert".into(), (who, instance, lineage))]
    );
}

#[test]
fn a_connection_kaban_did_not_open_cannot_write_a_canonical_row() {
    let s = Scratch::new("kaban-stamp", "unbound");
    let alice = bound(&s, "alice");
    let a = created(&alice, "Fact", "written by alice");
    let conn = Connection::open(s.db()).unwrap();
    let before = (stamp_of(&conn, "node", &a.id), change_stamps(&conn, &a.id));

    // a plain connection has no writer: a canonical write is refused, never written unstamped
    // or under alice's stamp
    let err = conn
        .execute(
            "UPDATE node SET text = 'edited by nobody' WHERE id = ?1",
            [&a.id],
        )
        .unwrap_err();
    assert!(err.to_string().contains("kaban_writer"), "{err}");
    let err = conn
        .execute(
            "INSERT INTO events(node_id, kind, reason, signer_kind, ts, workspace_id)
             VALUES (?1, 'accepted', 'r', 'agent', 0, 'default')",
            [&a.id],
        )
        .unwrap_err();
    assert!(err.to_string().contains("kaban_writer"), "{err}");
    kaban_store_sqlite::register_body_reader(&conn).unwrap();
    let text: String = conn
        .query_row(
            "SELECT kaban_body(text) FROM node WHERE id = ?1",
            [&a.id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(text, "written by alice");
    assert_eq!(
        (stamp_of(&conn, "node", &a.id), change_stamps(&conn, &a.id)),
        before
    );
    let events: i64 = conn
        .query_row("SELECT count(*) FROM events", [], |r| r.get(0))
        .unwrap();
    assert_eq!(events, 0);

    // a second connection kaban opened as bob writes the same record: the record and its
    // change row say bob
    let bob = bound(&s, "bob");
    bob.rescan("pv-bob", 100, &mut |_| RescanAction::Keep)
        .unwrap();
    let instance = meta(&conn, "db_id");
    let lineage = meta(&conn, "pack_lineage_id");
    assert_eq!(
        stamp_of(&conn, "node", &a.id),
        (Some("bob".into()), instance.clone(), lineage.clone())
    );
    assert_eq!(
        change_stamps(&conn, &a.id)
            .last()
            .map(|(op, stamp)| (op.as_str(), stamp.0.as_deref())),
        Some(("update", Some("bob")))
    );
}

#[test]
fn events_and_assertions_keep_their_stamp_after_their_change_rows_are_pruned() {
    let s = Scratch::new("kaban-stamp", "event-assertion");
    let alice = bound(&s, "alice");
    let a = created(&alice, "Decision", "a governed record");
    let b = created(&alice, "Fact", "its neighbour");
    // a governance event and a human claim (an assertion row), both written as alice
    alice
        .transition(
            &nid(&a.id),
            Governance::Accepted,
            NewEvent {
                reason: "seeded".into(),
                signer_kind: SignerKind::System,
                ..Default::default()
            },
            &ctx(),
        )
        .unwrap();
    alice
        .link(
            &nid(&a.id),
            &nid(&b.id),
            "RELATES_TO",
            Some(0.7),
            None,
            &ctx(),
        )
        .unwrap();
    let conn = Connection::open(s.db()).unwrap();
    let by_alice = (
        Some("alice".to_owned()),
        meta(&conn, "db_id"),
        meta(&conn, "pack_lineage_id"),
    );
    let event_stamp = || -> Stamp {
        conn.query_row(
            "SELECT actor_id, instance_id, pack_lineage_id FROM events WHERE node_id = ?1",
            [&a.id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap()
    };
    let assertion_stamp = || -> Stamp {
        conn.query_row(
            "SELECT actor_id, instance_id, pack_lineage_id FROM assertion WHERE layer = 'human'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap()
    };
    assert_eq!(event_stamp(), by_alice);
    assert_eq!(assertion_stamp(), by_alice);

    // the existing pruning removes their change rows: the records still say who wrote them
    let lease = alice.lease_acquire("probe", "test", 60).unwrap().unwrap();
    alice.changes_register("probe", lease.fence).unwrap();
    assert!(alice.changes_prune(0).unwrap() > 0);
    let left: i64 = conn
        .query_row(
            "SELECT count(*) FROM changes WHERE entity IN ('event', 'assertion')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(left, 0, "the event and assertion change rows are gone");
    assert_eq!(event_stamp(), by_alice);
    assert_eq!(assertion_stamp(), by_alice);
}
