//! Lifecycle semantics in the store (core-spec §4, F5, F6; DEL-08 survivors of the claim-era
//! file): purge keeps a ghost row with one purge changes row, traversal and recall never expand
//! through a hidden bridge, and a user rel declaring `on_link: archive_target` in the ontology's
//! rule set archives its target while a rel without it is inert.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::{Scratch, created, ctx, nid};
use kaban_core::{
    ForgetMode, Lifecycle, MemoryStore, NodeId, RecallQuery, SchemaStore, TraverseQuery,
    TraverseResult,
};
use kaban_store_sqlite::SqliteStore;

fn found(store: &SqliteStore, query: &str, id: &str) -> bool {
    store
        .recall(&RecallQuery {
            query: query.into(),
            limit: 5,
            ..Default::default()
        })
        .unwrap()
        .iter()
        .any(|h| h.node.id == id)
}

fn node_id(id: &str) -> NodeId {
    NodeId::parse(id).unwrap()
}

fn traverse_from(store: &SqliteStore, id: &str) -> kaban_core::Result<TraverseResult> {
    store.traverse(&TraverseQuery {
        start_ids: vec![node_id(id)],
        rels: vec![],
        direction: kaban_core::TraverseDirection::Both,
        max_depth: 2,
        node_kinds: vec![],
        limit: 50,
        workspace_id: None,
        ..Default::default()
    })
}

/// `(entity, op, id)` of every changes row with `seq > after`, in order.
fn changes_after(db: &std::path::Path, after: i64) -> Vec<(String, String, String)> {
    let raw = rusqlite::Connection::open(db).unwrap();
    let mut st = raw
        .prepare("SELECT entity, op, id FROM changes WHERE seq > ? ORDER BY seq")
        .unwrap();
    st.query_map([after], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
}

/// C2.3 slice 5 (core-spec F5 purge, S5, MD-24): purge removes text, the FTS entry and the
/// edges, writes exactly one explicit `op='purge'` changes row (plus one `delete` row per removed
/// edge, no node `update`/`delete` row) and keeps the id as a `purged` ghost row so it never
/// reappears: the id is `NotFound` for links and forget, hidden from `get`, and the same text
/// is `created` again.
#[test]
fn purge_keeps_a_ghost_row_and_emits_one_purge_changes_row() {
    let s = Scratch::new("kaban-lifecycle", "purge");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let victim = created(&store, "Fact", "echidna spines are modified hairs");
    let left = created(&store, "Fact", "echidnas are monotremes");
    let right = created(&store, "Idea", "add an echidna mascot");
    store
        .link(
            &node_id(&left.id),
            &node_id(&victim.id),
            "RELATES_TO",
            None,
            None,
            &ctx(),
        )
        .unwrap();
    store
        .link(
            &node_id(&victim.id),
            &node_id(&right.id),
            "ABOUT",
            None,
            None,
            &ctx(),
        )
        .unwrap();
    let before = i64::try_from(
        store
            .forget(&node_id(&right.id), ForgetMode::Archive, &ctx())
            .unwrap()
            .outbox_seq,
    )
    .unwrap();

    let outcome = store
        .forget(&node_id(&victim.id), ForgetMode::Purge, &ctx())
        .unwrap();
    assert_eq!(outcome.lifecycle, Lifecycle::Purged);
    assert!(i64::try_from(outcome.outbox_seq).unwrap() > before);

    let rows = changes_after(&s.db(), before);
    let purge_rows: Vec<_> = rows.iter().filter(|r| r.1 == "purge").collect();
    assert_eq!(purge_rows.len(), 1, "{rows:?}");
    assert_eq!(purge_rows[0].0, "node");
    assert_eq!(purge_rows[0].2, victim.id);
    assert_eq!(
        rows.iter()
            .filter(|r| r.0 == "edge" && r.1 == "delete")
            .count(),
        2,
        "one delete row per removed edge: {rows:?}"
    );
    assert!(
        rows.iter()
            .all(|r| !(r.0 == "node" && r.2 == victim.id && r.1 != "purge")),
        "no node update/delete row for a purge (MD-24): {rows:?}"
    );
    assert_eq!(
        i64::try_from(outcome.outbox_seq).unwrap(),
        before + i64::try_from(rows.len()).unwrap(),
        "outbox_seq is the last row of this write"
    );

    assert!(store.get(&node_id(&victim.id), false).unwrap().is_none());
    let ghost = store.get(&node_id(&victim.id), true).unwrap().unwrap();
    assert_eq!(ghost.lifecycle, Lifecycle::Purged);
    assert_eq!(ghost.text, "");
    assert_eq!(ghost.content_hash, "");
    assert!(ghost.summary.is_none() && ghost.tags.is_empty());
    assert_eq!(ghost.props, serde_json::json!({}));
    assert!(
        !found(&store, "echidna spines", &victim.id),
        "FTS entry removed"
    );
    let t = traverse_from(&store, &left.id).unwrap();
    assert!(t.edges.is_empty() && t.nodes.len() == 1, "{t:?}");
    for attempt in [
        store
            .link(
                &node_id(&left.id),
                &node_id(&victim.id),
                "RELATES_TO",
                None,
                None,
                &ctx(),
            )
            .map(drop),
        store
            .forget(&node_id(&victim.id), ForgetMode::Purge, &ctx())
            .map(drop),
        traverse_from(&store, &victim.id).map(drop),
    ] {
        assert!(
            matches!(attempt, Err(kaban_core::StoreError::NotFound(_))),
            "{attempt:?}"
        );
    }
    let again = created(&store, "Fact", "echidna spines are modified hairs");
    assert_ne!(again.id, victim.id);
    // purge of a tombstoned record is allowed (any -> purged)
    store
        .forget(&node_id(&left.id), ForgetMode::Tombstone, &ctx())
        .unwrap();
    assert_eq!(
        store
            .forget(&node_id(&left.id), ForgetMode::Purge, &ctx())
            .unwrap()
            .lifecycle,
        Lifecycle::Purged
    );
}

// ---- M2 review R2-4 (C2.10): lifecycle eligibility while every frontier is built ----

/// `A(active) -> B -> C(active)` with `B` hidden by `mode`; returns `(a, b, c)` ids.
fn bridge(store: &SqliteStore, mode: ForgetMode) -> (String, String, String) {
    let a = created(store, "Fact", "quokka alpha anchors the chain");
    let b = created(store, "Fact", "quokka bravo is the hidden bridge");
    let c = created(store, "Fact", "quokka charlie sits behind the bridge");
    // an unconstrained causal rel (`DERIVED_FROM`, core, `[]`): the archived frontier of
    // recall crosses causal rels only, and `CAUSED_BY` is `from: [Failure]`, enforced as data
    // since E1.8 (E1.7 R1)
    for (src, dst) in [(&a, &b), (&b, &c)] {
        store
            .link(
                &node_id(&src.id),
                &node_id(&dst.id),
                "DERIVED_FROM",
                None,
                None,
                &ctx(),
            )
            .unwrap();
    }
    store.forget(&node_id(&b.id), mode, &ctx()).unwrap();
    (a.id, b.id, c.id)
}

/// R2-4: traverse expands through active nodes only. An archived or tombstoned `B` must not
/// bridge `A` to `C` at depth 2, and no edge touching `B` is returned.
#[test]
fn traverse_does_not_expand_through_an_archived_or_tombstoned_bridge() {
    for mode in [ForgetMode::Archive, ForgetMode::Tombstone] {
        let store = SqliteStore::open_memory().unwrap();
        let (a, b, c) = bridge(&store, mode);
        let t = traverse_from(&store, &a).unwrap();
        let ids: Vec<&str> = t.nodes.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![a.as_str()],
            "{mode:?}: only the start is reachable: {t:?}"
        );
        assert!(
            !ids.contains(&b.as_str()) && !ids.contains(&c.as_str()),
            "{mode:?}: the hidden node bridged to C"
        );
        assert!(t.edges.is_empty(), "{mode:?}: {:?}", t.edges);
    }
}

/// R2-4 (core-spec §4, F6: traversal defaults to active nodes): an archived start id is as
/// good as absent, like a tombstoned one.
#[test]
fn an_archived_traverse_start_is_not_found() {
    let store = SqliteStore::open_memory().unwrap();
    let (_, b, _) = bridge(&store, ForgetMode::Archive);
    assert!(
        matches!(
            traverse_from(&store, &b),
            Err(kaban_core::StoreError::NotFound(ref id)) if *id == b
        ),
        "an archived start id must be NotFound"
    );
}

fn recall_ids(store: &SqliteStore, query: &str, include_archived: bool) -> Vec<String> {
    store
        .recall(&RecallQuery {
            query: query.into(),
            hops: 2,
            limit: 10,
            include_archived,
            ..Default::default()
        })
        .unwrap()
        .into_iter()
        .map(|h| h.node.id)
        .collect()
}

/// R2-4: recall's hop expansion never runs through a tombstoned node, and through an archived
/// one only when `include_archived` opts in. The query matches `A` alone; `C` is reachable
/// only across `B`.
#[test]
fn recall_does_not_expand_through_a_hidden_bridge_unless_archived_and_opted_in() {
    let store = SqliteStore::open_memory().unwrap();
    let (a, b, c) = bridge(&store, ForgetMode::Archive);
    let ids = recall_ids(&store, "alpha anchors", false);
    assert_eq!(
        ids,
        vec![a.clone()],
        "archived B must not bridge to C: {ids:?}"
    );
    let ids = recall_ids(&store, "alpha anchors", true);
    assert!(
        ids.contains(&b) && ids.contains(&c),
        "with include_archived the archived B and C behind it are reachable: {ids:?}"
    );

    let store = SqliteStore::open_memory().unwrap();
    let (a, _, _) = bridge(&store, ForgetMode::Tombstone);
    for opt in [false, true] {
        let ids = recall_ids(&store, "alpha anchors", opt);
        assert_eq!(
            ids,
            vec![a.clone()],
            "tombstoned B must never bridge (include_archived={opt}): {ids:?}"
        );
    }
}

/// The shipped files plus `fixtures/ontology/incidents.yaml` resolved as one, as the registry
/// would hold them after a user load: `incidents:REPLACES{on_link: archive_target}` and
/// `incidents:MENTIONED_IN` without it.
fn incidents_set() -> std::sync::Arc<kaban_core::RuleSet> {
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../kaban-core/fixtures/ontology/incidents.yaml");
    let text = std::fs::read_to_string(&fixture).unwrap();
    let mut files = kaban_core::shipped::files().unwrap();
    files.push(kaban_core::OntologyFile::from_yaml(&text).unwrap());
    let ontology = kaban_core::resolve(&files, &kaban_core::NamespaceSet::new()).unwrap();
    std::sync::Arc::new(kaban_core::RuleSet::from_ontology(&ontology))
}

/// A store that knows the `incidents` rels (the `schema_type` rows the registry writes through
/// `schema_extend`), holding the `incidents` rule set (the registry adopts it on load).
fn incidents_store() -> SqliteStore {
    use kaban_core::TypeDef;
    let store = SqliteStore::open_memory().unwrap();
    for name in ["incidents:REPLACES", "incidents:MENTIONED_IN"] {
        store
            .schema_extend(TypeDef {
                kind: "rel".into(),
                name: name.into(),
                ns: None,
                description: name.into(),
                from_kinds: vec![],
                to_kinds: vec![],
                directed: true,
                props_schema: None,
                builtin: false,
                declaration: kaban_core::TypeDeclaration::default(),
            })
            .unwrap();
    }
    store.adopt_rules(incidents_set());
    store
}

/// EN-F4: `on_link: archive_target` on a user rel archives its target like `SUPERSEDES`; a
/// rel without it is inert; under the shipped set the same rel is just an edge.
#[test]
fn a_user_rel_with_on_link_archive_target_archives_its_target_and_one_without_does_not() {
    let store = incidents_store();
    let old_fact = created(&store, "Fact", "the wombat cache holds 2 GiB");
    let new_fact = created(&store, "Fact", "the wombat cache holds 4 GiB");
    let mentioned = created(&store, "Fact", "the wombat cache is in eu-west");
    let note = created(&store, "Fact", "cache sizing note");
    for (src, dst, rel) in [
        (&new_fact, &old_fact, "incidents:REPLACES"),
        (&note, &mentioned, "incidents:MENTIONED_IN"),
    ] {
        store
            .link(&nid(&src.id), &nid(&dst.id), rel, None, None, &ctx())
            .unwrap();
    }
    let replaced = store.get(&nid(&old_fact.id), true).unwrap().unwrap();
    assert_eq!(replaced.lifecycle, Lifecycle::Archived);
    let untouched = store.get(&nid(&mentioned.id), true).unwrap().unwrap();
    assert_eq!(untouched.lifecycle, Lifecycle::Active);

    // the shipped set alone declares no effect for the rel
    let plain = incidents_store();
    plain.adopt_rules(kaban_core::RuleSet::shipped());
    let a = created(&plain, "Fact", "plain a");
    let b = created(&plain, "Fact", "plain b");
    plain
        .link(
            &nid(&a.id),
            &nid(&b.id),
            "incidents:REPLACES",
            None,
            None,
            &ctx(),
        )
        .unwrap();
    assert_eq!(
        plain.get(&nid(&b.id), true).unwrap().unwrap().lifecycle,
        Lifecycle::Active
    );
}
