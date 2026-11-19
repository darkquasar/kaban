//! Write stamps (spec 05 SS-26): every record and change row a store writes carries the
//! process's writer (`bind_actor`, `local` by default), this copy's instance (`db_id`) and the
//! pack lineage, in the same transaction, for public mutations and internal maintenance alike.
//! The non-governance oracles of the file of the same name deleted with governance (DEL-08).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::{Scratch, created, ctx, nid};
use kaban_core::{
    ActorId, IngestStore, LinkReq, MemoryStore, NewNode, Node, RememberOutcome, RescanAction,
    RescanCursor,
};
use kaban_store_sqlite::SqliteStore;
use rusqlite::{Connection, OptionalExtension};

/// `(actor_id, instance_id, pack_lineage_id)` of one row.
type Stamp = (Option<String>, Option<String>, Option<String>);

fn stamp_of(conn: &Connection, table: &str, id: &str) -> Stamp {
    conn.query_row(
        &format!("SELECT actor_id, instance_id, pack_lineage_id FROM {table} AS rec JOIN writer ON writer.id = rec.writer_id WHERE rec.id = ?1"),
        [id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )
    .unwrap()
}

/// Every change row naming `id`, oldest first: `(op, stamp)`.
fn change_stamps(conn: &Connection, id: &str) -> Vec<(String, Stamp)> {
    let mut st = conn
        .prepare(
            "SELECT op, actor_id, instance_id, pack_lineage_id FROM changes JOIN writer ON writer.id = changes.writer_id WHERE changes.id = ?1 ORDER BY seq",
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

fn bound(s: &Scratch, name: &str) -> SqliteStore {
    SqliteStore::open_as(&s.db(), &s.keys(), &ActorId::parse(name).unwrap()).unwrap()
}

/// One rescan walk from the start under `policy_version` that rejects (tombstones) the record
/// `id` and keeps the rest: a maintenance write by the store's own writer.
fn rescan_rejecting(
    store: &SqliteStore,
    policy_version: &str,
    id: &str,
) -> kaban_core::RescanReport {
    store
        .rescan(policy_version, RescanCursor::Start, 100, &mut |n: &Node| {
            if n.id == id {
                RescanAction::Reject
            } else {
                RescanAction::Keep
            }
        })
        .unwrap()
}

const AGENT: &str = "agent-7";
const SESSION: &str = "session-42";

/// The producing agent and session of a record (SS-26: they survive stamping and maintenance
/// byte for byte).
fn provenance(conn: &Connection, id: &str) -> (Option<String>, Option<String>) {
    conn.query_row(
        "SELECT agent_id, session_id FROM node WHERE id = ?1",
        [id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .unwrap()
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
    let RememberOutcome::Created(a) = alice
        .remember(
            NewNode {
                kind: "Decision".into(),
                text: "stamped by alice".into(),
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
    let seeded = provenance(&conn, &a.id);
    assert_eq!(seeded, (Some(AGENT.into()), Some(SESSION.into())));
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
        vec![("insert".into(), by("alice"))]
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

    // internal maintenance: bob's rescan rejects alice's record under a new policy version, and
    // the tombstone it writes is stamped bob
    let report = rescan_rejecting(&bob, "pv-rescanned", &a.id);
    assert!(report.scanned >= 2 && report.rejected == 1, "{report:?}");
    assert_eq!(stamp_of(&conn, "node", &a.id), by("bob"));
    assert_eq!(
        change_stamps(&conn, &a.id),
        vec![("insert".into(), by("alice")), ("update".into(), by("bob"))]
    );

    // no change row escapes the stamp, and the record's provenance is untouched
    let unstamped: i64 = conn
        .query_row(
            "SELECT count(*) FROM changes LEFT JOIN writer ON writer.id = changes.writer_id WHERE writer.id IS NULL OR actor_id IS NULL OR instance_id IS NULL OR pack_lineage_id IS NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(unstamped, 0);
    assert_eq!(provenance(&conn, &a.id), seeded);
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

    // a second connection kaban opened as bob writes the same record: the record and its
    // change row say bob
    let bob = bound(&s, "bob");
    rescan_rejecting(&bob, "pv-bob", &a.id);
    let instance = meta(&conn, "db_id");
    let lineage = meta(&conn, "pack_lineage_id");
    assert_eq!(
        stamp_of(&conn, "node", &a.id),
        (Some("bob".into()), instance, lineage)
    );
    assert_eq!(
        change_stamps(&conn, &a.id)
            .last()
            .map(|(op, stamp)| (op.as_str(), stamp.0.as_deref())),
        Some(("update", Some("bob")))
    );
}

#[test]
fn canonical_writes_share_a_writer_tuple_and_roll_it_back_with_the_change() {
    let s = Scratch::new("kaban-stamp", "shared-tuple");
    let store = bound(&s, "alice");
    let a = created(&store, "Fact", "first stamped fact");
    created(&store, "Fact", "second stamped fact");
    let conn = Connection::open(s.db()).unwrap();
    let has_writer: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='writer')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        has_writer,
        "canonical writes share one interned writer tuple"
    );
    let count_writers = || {
        conn.query_row("SELECT count(*) FROM writer", [], |r| r.get::<_, i64>(0))
            .unwrap()
    };
    assert_eq!(count_writers(), 1);
    let before = change_stamps(&conn, &a.id);
    SqliteStore::bind_writer(&conn, &ActorId::parse("rollback-actor").unwrap()).unwrap();
    conn.execute_batch("BEGIN IMMEDIATE").unwrap();
    conn.execute(
        "UPDATE node SET summary = 'temporary' WHERE id = ?",
        [&a.id],
    )
    .unwrap();
    assert_eq!(count_writers(), 2);
    conn.execute_batch("ROLLBACK").unwrap();
    assert_eq!(count_writers(), 1);
    assert_eq!(change_stamps(&conn, &a.id), before);
    assert_eq!(stamp_of(&conn, "node", &a.id).0.as_deref(), Some("alice"));
}
