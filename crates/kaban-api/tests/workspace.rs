//! Workspace isolation (core-spec S6; core-plan C1.7; m2 N10): the API is bound to one
//! workspace; reads never cross it unless the request says `workspace_id: "*"`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::Scratch;
use kaban_api::{
    ApiConfig, ApiError, KabanApi, LinkRequest, RecallRequest, RememberRequest, StatsRequest,
    TraverseRequest,
};
use kaban_core::ActorId;
use kaban_core::{MemoryStore, WorkspaceId};
use kaban_policy::Policy;
use kaban_store_sqlite::SqliteStore;
use std::sync::Arc;

fn api_bound_to(store: &Arc<SqliteStore>, workspace: &str) -> KabanApi {
    KabanApi::with_config(
        Arc::clone(store) as Arc<dyn kaban_core::GraphStore>,
        Policy::empty(),
        store.signer(),
        None,
        ApiConfig {
            workspace: WorkspaceId::new(workspace),
            ..Default::default()
        },
    )
}

fn shared_store() -> Arc<SqliteStore> {
    Arc::new(SqliteStore::open_memory().unwrap())
}

/// A file-backed store whose database the test can also open raw.
fn file_store(name: &str) -> (Scratch, Arc<SqliteStore>) {
    let scratch = Scratch::new("kaban-ws", name);
    let store = Arc::new(SqliteStore::open(&scratch.db(), &scratch.keys()).unwrap());
    (scratch, store)
}

/// A cross-workspace edge written behind the store's back (legacy data: since R3-3 no write
/// path creates one), so the read-side isolation below is tested against a real bridge.
fn raw_edge(scratch: &Scratch, from: &str, rel: &str, to: &str) {
    let raw = rusqlite::Connection::open(scratch.db()).unwrap();
    SqliteStore::bind_writer(&raw, &ActorId::default()).unwrap();
    raw.busy_timeout(std::time::Duration::from_secs(5)).unwrap();
    let n = raw
        .execute(
            "INSERT INTO edge(id, src, dst, rel, confidence, created_at, props, workspace_id) \
             SELECT ?1, s.rid, d.rid, ?2, NULL, ?3, '{}', s.workspace_id \
             FROM node s, node d WHERE s.id = ?4 AND d.id = ?5",
            rusqlite::params![kaban_core::new_id(), rel, kaban_core::now_iso(), from, to],
        )
        .unwrap();
    assert_eq!(n, 1, "both endpoints exist");
}

fn remember(api: &KabanApi, text: &str, workspace: Option<&str>) -> String {
    api.remember(RememberRequest {
        kind: "Fact".into(),
        text: text.into(),
        workspace_id: workspace.map(str::to_owned),
        ..Default::default()
    })
    .unwrap()
    .id
}

fn recall(api: &KabanApi, query: &str, workspace: Option<&str>) -> Vec<String> {
    api.recall(RecallRequest {
        query: query.into(),
        workspace_id: workspace.map(str::to_owned),
        ..Default::default()
    })
    .unwrap()
    .results
    .into_iter()
    .map(|c| c.id)
    .collect()
}

#[test]
fn a_note_in_another_workspace_is_invisible_unless_the_request_selects_all() {
    let (scratch, store) = file_store("invisible");
    let ws_a = api_bound_to(&store, "ws-a");
    let ws_b = api_bound_to(&store, "ws-b");

    // written through the ws-a binding without naming a workspace: it lands in ws-a
    let a = remember(
        &ws_a,
        "ingress gateway timeout tuned to ninety seconds",
        None,
    );
    // written through the ws-b binding; the edge to the ws-a note is legacy data (R3-3)
    let b = remember(&ws_b, "gateway timeout follow-up in workspace b", None);
    raw_edge(&scratch, &b, "RELATES_TO", &a);

    // recall bound to ws-b does not see the ws-a note, even through the graph hop
    let seen = recall(&ws_b, "ingress gateway timeout", None);
    assert!(
        !seen.contains(&a),
        "ws-a note leaked into a ws-b recall: {seen:?}"
    );
    assert!(seen.contains(&b), "{seen:?}");
    // the explicit wildcard crosses workspaces
    let all = recall(&ws_b, "ingress gateway timeout", Some("*"));
    assert!(all.contains(&a) && all.contains(&b), "{all:?}");
    // an explicit other workspace is honoured too
    assert_eq!(
        recall(&ws_b, "ingress gateway timeout", Some("ws-a")),
        vec![a.clone()]
    );

    // traverse bound to ws-b: the ws-a neighbour is not returned, and starting from it fails
    let t = ws_b
        .traverse(TraverseRequest {
            start_ids: vec![b.clone()],
            ..Default::default()
        })
        .unwrap();
    assert!(
        t.nodes.iter().all(|n| n.id != a),
        "ws-a node in ws-b traverse"
    );
    assert!(
        t.edges.is_empty(),
        "edge to a hidden node returned: {:?}",
        t.edges
    );
    let err = ws_b
        .traverse(TraverseRequest {
            start_ids: vec![a.clone()],
            ..Default::default()
        })
        .unwrap_err();
    assert!(matches!(err, ApiError::NotFound(_)), "{err}");
    let t = ws_b
        .traverse(TraverseRequest {
            start_ids: vec![b.clone()],
            workspace_id: Some("*".into()),
            ..Default::default()
        })
        .unwrap();
    assert!(
        t.nodes.iter().any(|n| n.id == a) && t.edges.len() == 1,
        "{t:?}"
    );

    // "*" is a read selector, never a stored workspace
    let err = ws_b
        .remember(RememberRequest {
            kind: "Fact".into(),
            text: "cannot live in every workspace".into(),
            workspace_id: Some("*".into()),
            ..Default::default()
        })
        .unwrap_err();
    assert!(matches!(err, ApiError::Invalid(_)), "{err}");
}

/// Review R1-5: the workspace predicate applies while the frontier is built, not only when the
/// results are materialized, so a hidden bridge node cannot reveal or rank a node two hops away.
/// Layout: `start` (ws-b) -> `bridge` (ws-a) -> `result` (ws-b), along causal relationships.
#[test]
fn a_hidden_bridge_workspace_does_not_connect_two_visible_nodes_at_depth_two() {
    let (scratch, store) = file_store("bridge");
    let ws_a = api_bound_to(&store, "ws-a");
    let ws_b = api_bound_to(&store, "ws-b");
    let start = remember(&ws_b, "pager alert storm on the ingress tier", None);
    let bridge = remember(&ws_a, "root cause: connection pool exhaustion", None);
    let result = remember(&ws_b, "fix: raise the pool ceiling to four hundred", None);
    // the bridging edges are legacy data: since R3-3 no write path creates them
    for (from, to) in [(&start, &bridge), (&bridge, &result)] {
        raw_edge(&scratch, from, "FIXED_BY", to);
    }
    // recall, two hops, bound to ws-b: the result is not reached through the hidden bridge
    let hops2 = |ws: Option<&str>| {
        ws_b.recall(RecallRequest {
            query: "pager alert storm".into(),
            hops: Some(2),
            workspace_id: ws.map(str::to_owned),
            ..Default::default()
        })
        .unwrap()
    };
    let bound = hops2(None);
    assert!(
        bound
            .results
            .iter()
            .all(|c| c.id != result && c.id != bridge),
        "ranked through a hidden workspace: {bound:?}"
    );
    assert!(
        bound
            .results
            .iter()
            .all(|c| c.why.iter().flatten().all(|w| !w.starts_with("hop"))),
        "a hop through the hidden bridge left a trace in `why`: {bound:?}"
    );
    let all = hops2(Some("*"));
    assert!(
        all.results.iter().any(|c| c.id == result),
        "with \"*\" the two-hop result is reachable: {all:?}"
    );
    // traverse, depth two, bound to ws-b: neither the bridge nor the result
    let depth2 = |ws: Option<&str>| {
        ws_b.traverse(TraverseRequest {
            start_ids: vec![start.clone()],
            max_depth: Some(2),
            workspace_id: ws.map(str::to_owned),
            ..Default::default()
        })
        .unwrap()
    };
    let t = depth2(None);
    assert_eq!(
        t.nodes.iter().map(|n| n.id.as_str()).collect::<Vec<_>>(),
        vec![start.as_str()],
        "{t:?}"
    );
    assert!(t.edges.is_empty());
    let t = depth2(Some("*"));
    assert!(
        t.nodes.iter().any(|n| n.id == result) && t.edges.len() == 2,
        "{t:?}"
    );
    // stats are scoped to the bound workspace unless the request says "*"; file bytes are global
    let scoped = ws_b.stats(StatsRequest::default()).unwrap();
    assert_eq!(scoped.store["nodes"], serde_json::json!(2), "{scoped:?}");
    assert_eq!(scoped.store["edges"], serde_json::json!(0), "{scoped:?}");
    assert_eq!(
        scoped.store["scope"]["workspace"],
        serde_json::json!("ws-b")
    );
    assert_eq!(
        scoped.store["scope"]["db_bytes"],
        serde_json::json!("global")
    );
    let global = ws_b
        .stats(StatsRequest {
            workspace_id: Some("*".into()),
        })
        .unwrap();
    assert_eq!(global.store["nodes"], serde_json::json!(3), "{global:?}");
    assert_eq!(global.store["edges"], serde_json::json!(2), "{global:?}");
    assert_eq!(global.store["by_rel"]["FIXED_BY"], serde_json::json!(2));
    assert_eq!(global.store["scope"]["workspace"], serde_json::json!("*"));
}

/// M3 review R3-3 (Major): a process bound to workspace B that knows an id from workspace A
/// cannot create an edge to it, in either direction, and the `SUPERSEDES` side effect never
/// reaches the other workspace's node: the endpoint is `NotFound` (no existence leak), the
/// store holds no edge and the ws-a node stays active.
#[test]
fn a_link_bound_to_one_workspace_cannot_reach_or_mutate_a_node_in_another() {
    let store = shared_store();
    let ws_a = api_bound_to(&store, "ws-a");
    let ws_b = api_bound_to(&store, "ws-b");
    let a = remember(
        &ws_a,
        "retry budget of three attempts on the ingress tier",
        None,
    );
    let b = remember(&ws_b, "a newer retry budget written in workspace b", None);
    for (from, to) in [(&b, &a), (&a, &b)] {
        let err = ws_b
            .link(LinkRequest {
                from: from.clone(),
                to: to.clone(),
                rel: "SUPERSEDES".into(),
                ..Default::default()
            })
            .unwrap_err();
        assert!(matches!(err, ApiError::NotFound(_)), "{err}");
    }
    let a_node = store
        .get(&kaban_core::NodeId::parse(&a).unwrap(), true)
        .unwrap()
        .unwrap();
    assert_eq!(
        a_node.lifecycle,
        kaban_core::Lifecycle::Active,
        "the ws-a node was mutated from ws-b: {a_node:?}"
    );
    let global = ws_b
        .stats(StatsRequest {
            workspace_id: Some("*".into()),
        })
        .unwrap();
    assert_eq!(global.store["edges"], serde_json::json!(0), "{global:?}");
}

/// R3-3, the inline path: a `remember` in workspace B whose `links` name a workspace A node
/// with `SUPERSEDES` fails as `NotFound`, stores nothing (the write is one transaction) and
/// leaves the ws-a node active; a `props.evidence[]` id from workspace A is ignored like any
/// unknown id, never linked.
#[test]
fn an_inline_remember_link_cannot_supersede_a_node_in_another_workspace() {
    let store = shared_store();
    let ws_a = api_bound_to(&store, "ws-a");
    let ws_b = api_bound_to(&store, "ws-b");
    let a = remember(&ws_a, "connection pool ceiling of two hundred", None);
    let err = ws_b
        .remember(RememberRequest {
            kind: "Fact".into(),
            text: "connection pool ceiling raised to four hundred".into(),
            links: vec![kaban_api::LinkSpec {
                rel: "SUPERSEDES".into(),
                to: Some(a.clone()),
                from: None,
                confidence: None,
            }],
            ..Default::default()
        })
        .unwrap_err();
    assert!(matches!(err, ApiError::NotFound(_)), "{err}");
    let a_node = store
        .get(&kaban_core::NodeId::parse(&a).unwrap(), true)
        .unwrap()
        .unwrap();
    assert_eq!(
        a_node.lifecycle,
        kaban_core::Lifecycle::Active,
        "{a_node:?}"
    );
    let scoped = ws_b.stats(StatsRequest::default()).unwrap();
    assert_eq!(
        scoped.store["nodes"],
        serde_json::json!(0),
        "the failed remember stored a node: {scoped:?}"
    );

    // a CodeRef of workspace A named as evidence is not visible from B: ignored, not linked
    let code_ref = ws_a
        .remember(RememberRequest {
            kind: "CodeRef".into(),
            text: "src/pool.rs".into(),
            ..Default::default()
        })
        .unwrap()
        .id;
    let r = ws_b
        .remember(RememberRequest {
            kind: "Fact".into(),
            text: "the pool module is documented elsewhere".into(),
            props: Some(serde_json::json!({ "evidence": [{ "id": code_ref }] })),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(r.links_created, 0, "{r:?}");
    let global = ws_b
        .stats(StatsRequest {
            workspace_id: Some("*".into()),
        })
        .unwrap();
    assert_eq!(global.store["edges"], serde_json::json!(0), "{global:?}");
}

/// Rows written before workspaces existed (NULL) belong to `default`. Since schema version 2
/// `node.workspace_id` is `NOT NULL` (F17) and migration 0002 moves legacy NULL rows to
/// `default`; the regression lives in `kaban-store-sqlite/tests/migrate.rs`
/// (`legacy_null_workspace_rows_and_their_edges_land_in_the_default_workspace`). A fresh v2
/// store cannot hold a NULL workspace at all:
#[test]
fn a_null_workspace_cannot_be_written_since_schema_v2() {
    let dir = std::env::temp_dir().join(format!("kaban-ws-legacy-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("kaban.db");
    let store = Arc::new(SqliteStore::open(&db, &dir.join("keys")).unwrap());
    let default_ws = api_bound_to(&store, "default");
    let a = remember(&default_ws, "legacy anchor about a flaky retry loop", None);
    let raw = rusqlite::Connection::open(&db).unwrap();
    SqliteStore::bind_writer(&raw, &ActorId::default()).unwrap();
    let err = raw
        .execute("UPDATE node SET workspace_id = NULL WHERE id = ?", [&a])
        .unwrap_err();
    assert!(err.to_string().contains("NOT NULL"), "{err}");
    drop(raw);
    drop(store);
    let _ = std::fs::remove_dir_all(&dir);
}
