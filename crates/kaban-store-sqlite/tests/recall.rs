//! `recall` v3 in the store (core-spec F3; core-plan C3.5): filters run inside the FTS join so
//! `LIMIT` never starves a filtered recall; the `ref` filter restricts seeds to notes about one
//! `CodeRef`; `include_archived` opts archived rows into the hop frontier (M3-12).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::{created, ctx, nid};
use kaban_core::{ForgetMode, MemoryStore, NewNode, RecallQuery, RememberOutcome};
use kaban_store_sqlite::SqliteStore;

fn tagged(store: &SqliteStore, text: &str, tag: &str) -> String {
    match store
        .remember(
            NewNode {
                kind: "Fact".into(),
                text: text.into(),
                tags: vec![tag.into()],
                ..Default::default()
            },
            &[],
            &ctx(),
        )
        .unwrap()
    {
        RememberOutcome::Created(n) => n.id,
        RememberOutcome::Existing { node, .. } => panic!("fresh store: {node:?}"),
    }
}

/// Plan slice 1: sixty short notes carry the tag `noise`; one long note (ranked last by BM25)
/// carries `wanted`. With `tags: ["wanted"]` and `limit: 5` the target must be returned: the
/// tag filter runs before `LIMIT`, not over the top-k seeds.
#[test]
fn tag_filter_runs_before_limit_so_a_low_ranked_tagged_note_is_still_found() {
    let store = SqliteStore::open_memory().unwrap();
    for i in 0..60 {
        tagged(&store, &format!("quokka note {i}"), "noise");
    }
    let target = tagged(
        &store,
        "quokka sighting recorded after a long walk along the shore of rottnest island where the \
         whole colony gathered under the trees near the settlement and the visitor centre",
        "wanted",
    );
    let hits = store
        .recall(&RecallQuery {
            query: "quokka".into(),
            tags: vec!["wanted".into()],
            limit: 5,
            hops: 0,
            ..Default::default()
        })
        .unwrap();
    let ids: Vec<&str> = hits.iter().map(|h| h.node.id.as_str()).collect();
    assert_eq!(ids, vec![target.as_str()], "{ids:?}");
    // negative: tags match exactly, never by prefix or substring
    let none = store
        .recall(&RecallQuery {
            query: "quokka".into(),
            tags: vec!["want".into(), "ois".into()],
            limit: 5,
            hops: 0,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(none.len(), 0, "{none:?}");
}

/// M3-12 (MD-20): `include_archived` opts archived rows of any governance into the hop frontier
/// with the same rule as `include_superseded`; a tombstoned row stays unreachable either way.
#[test]
fn include_archived_reaches_an_archived_fact_through_expansion_but_never_a_tombstoned_one() {
    let ids = |mode: ForgetMode, include_archived: bool| {
        let store = SqliteStore::open_memory().unwrap();
        let a = created(&store, "Fact", "quokka alpha anchors the chain");
        let b = created(&store, "Fact", "quokka bravo is the archived neighbour");
        // `RELATES_TO` is unconstrained; `CAUSED_BY` is `from: [Failure]`, enforced as data
        // since E1.8 (E1.7 R1)
        store
            .link(&nid(&a.id), &nid(&b.id), "RELATES_TO", None, None, &ctx())
            .unwrap();
        store.forget(&nid(&b.id), mode, &ctx()).unwrap();
        let hits = store
            .recall(&RecallQuery {
                query: "alpha anchors".into(),
                hops: 1,
                limit: 10,
                include_archived,
                ..Default::default()
            })
            .unwrap();
        let found: Vec<String> = hits.into_iter().map(|h| h.node.id).collect();
        (a.id, b.id, found)
    };
    let (a, b, found) = ids(ForgetMode::Archive, true);
    assert_eq!(found, vec![a, b], "archived B is reachable on request");
    let (a, _, found) = ids(ForgetMode::Archive, false);
    assert_eq!(found, vec![a], "archived B stays hidden by default");
    let (a, _, found) = ids(ForgetMode::Tombstone, true);
    assert_eq!(found, vec![a], "a tombstoned B is reachable nowhere");
}
