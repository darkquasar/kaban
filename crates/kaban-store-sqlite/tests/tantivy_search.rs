//! The text index at its seam, through the store's catalog (spec 05 SS-08, spec 11 T7): filters
//! before the top-k with no leak, a misspelling found by the fuzzy mode, and the total order
//! and its continuation.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::Scratch;
use kaban_core::text_index::{MatchMode, Searched, SeedFilter, TextHit, TextQuery};
use kaban_core::{
    Actor, ForgetMode, MemoryStore, NewNode, Node, NodeId, RecallQuery, RememberOutcome,
    WorkspaceId, WriteCtx,
};
use kaban_store_sqlite::SqliteStore;

fn ctx(ws: &str) -> WriteCtx {
    WriteCtx::new(WorkspaceId::new(ws), Actor::Agent)
}

/// Remember a fresh `kind` note in workspace `ws` and return it.
fn note(store: &SqliteStore, ws: &str, kind: &str, text: &str, tags: &[&str]) -> Node {
    let outcome = store
        .remember(
            NewNode {
                kind: kind.into(),
                text: text.into(),
                tags: tags.iter().map(|t| (*t).to_owned()).collect(),
                workspace_id: Some(ws.into()),
                ..Default::default()
            },
            &[],
            &ctx(ws),
        )
        .unwrap();
    match outcome {
        RememberOutcome::Created(n) => n,
        RememberOutcome::Existing { node, .. } => panic!("fresh store: {node:?}"),
    }
}

/// The `rid` of `node`, read beside the store.
fn row(db: &std::path::Path, node: &Node) -> i64 {
    rusqlite::Connection::open(db)
        .unwrap()
        .query_row("SELECT rid FROM node WHERE id = ?1", [&node.id], |r| {
            r.get(0)
        })
        .unwrap()
}

/// A seed query of recall's shape over `q`'s filters.
fn seed(q: &RecallQuery, mode: MatchMode, limit: usize, skip: usize) -> TextQuery<'_> {
    TextQuery {
        text: &q.query,
        mode,
        filter: SeedFilter::of(q),
        limit,
        sufficient: limit,
        skip,
    }
}

/// The hits of `query` at the store's latest generation, with the `exhausted` flag.
fn hits(store: &SqliteStore, query: &TextQuery<'_>) -> (Vec<TextHit>, bool) {
    match store.search_text(query).unwrap() {
        Searched::Hits { hits, exhausted } => (hits, exhausted),
        Searched::Unavailable => panic!("the text index is unavailable"),
    }
}

/// The rids of `query`'s hits.
fn rids(store: &SqliteStore, query: &TextQuery<'_>) -> Vec<i64> {
    hits(store, query).0.into_iter().map(|h| h.rid).collect()
}

fn query(text: &str) -> RecallQuery {
    RecallQuery {
        query: text.into(),
        ..Default::default()
    }
}

fn in_workspace(text: &str, ws: &str) -> RecallQuery {
    RecallQuery {
        workspace_id: Some(ws.into()),
        ..query(text)
    }
}

/// The filter fields bound the match set before the top-k: with thirty `noise` notes ranked
/// above it, the one `wanted` note is the page's only hit at `limit` 5; a tag matches exactly;
/// another workspace's row and a tombstoned row are never
/// returned; a kind narrows; and a query word equal to a workspace or kind name matches no
/// text (the filter fields are not content).
#[test]
fn filters_apply_before_top_k_and_never_leak() {
    let s = Scratch::new("t7-search", "filters");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    for i in 0..30 {
        note(
            &store,
            "ws1",
            "Fact",
            &format!("quokka note {i}"),
            &["noise"],
        );
    }
    let wanted = note(
        &store,
        "ws1",
        "Fact",
        "quokka sighting recorded after a long walk along the shore of rottnest island where \
         the whole colony gathered under the trees near the settlement and the visitor centre",
        &["wanted"],
    );
    let other_ws = note(
        &store,
        "ws2",
        "Fact",
        "quokka in another workspace",
        &["wanted"],
    );
    let idea = note(&store, "ws1", "Idea", "quokka thought for the ferry", &[]);
    let gone = note(
        &store,
        "ws1",
        "Fact",
        "quokka that was forgotten",
        &["wanted"],
    );
    store
        .forget(
            &NodeId::parse(&gone.id).unwrap(),
            ForgetMode::Tombstone,
            &ctx("ws1"),
        )
        .unwrap();
    let index = &store;
    let rid_of = |n| row(&s.db(), n);

    let tagged = RecallQuery {
        tags: vec!["wanted".into()],
        ..in_workspace("quokka", "ws1")
    };
    assert_eq!(
        rids(index, &seed(&tagged, MatchMode::Lexical, 5, 0)),
        [rid_of(&wanted)],
        "the tag filter runs before the top-k; ws2 and the tombstoned row never"
    );
    let prefixes = RecallQuery {
        tags: vec!["want".into(), "ois".into()],
        ..in_workspace("quokka", "ws1")
    };
    assert_eq!(
        rids(index, &seed(&prefixes, MatchMode::Lexical, 5, 0)),
        [] as [i64; 0]
    );
    let everything = rids(
        index,
        &seed(&in_workspace("quokka", "ws1"), MatchMode::Lexical, 100, 0),
    );
    assert_eq!(everything.len(), 32, "30 noise + wanted + idea");
    assert!(!everything.contains(&rid_of(&other_ws)));
    assert!(!everything.contains(&rid_of(&gone)));
    let all_ws = rids(index, &seed(&query("quokka"), MatchMode::Lexical, 100, 0));
    assert!(
        all_ws.contains(&rid_of(&other_ws)),
        "no workspace filter: both"
    );
    assert!(!all_ws.contains(&rid_of(&gone)), "tombstoned: nowhere");
    let kinds = RecallQuery {
        kinds: vec!["Idea".into()],
        ..in_workspace("quokka", "ws1")
    };
    assert_eq!(
        rids(index, &seed(&kinds, MatchMode::Lexical, 10, 0)),
        [rid_of(&idea)]
    );
    for word in ["ws1", "Fact", "Idea", "active"] {
        for mode in [MatchMode::Lexical, MatchMode::Fuzzy] {
            assert!(
                rids(index, &seed(&in_workspace(word, "ws1"), mode, 10, 0)).is_empty(),
                "{word} {mode:?}: a filter value is not text"
            );
        }
    }
}

/// A misspelled identifier is found by the fuzzy mode and not by the lexical one; the fuzzy
/// page honours the same filters; a query with no fuzzy-sized token is an empty page, not
/// `Unavailable`.
#[test]
fn fuzzy_finds_a_misspelling_under_the_same_filters() {
    let s = Scratch::new("t7-search", "fuzzy");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let target = note(
        &store,
        "ws1",
        "Fact",
        "TransactionCoordinator retries the commit",
        &[],
    );
    note(&store, "ws1", "Fact", "the payment gateway timed out", &[]);
    let elsewhere = note(
        &store,
        "ws2",
        "Fact",
        "TransactionCoordinator elsewhere",
        &[],
    );
    let index = &store;
    let q = in_workspace("TransactionCordinator", "ws1");
    assert_eq!(
        rids(index, &seed(&q, MatchMode::Lexical, 10, 0)),
        [] as [i64; 0]
    );
    assert_eq!(
        rids(index, &seed(&q, MatchMode::Fuzzy, 10, 0)),
        [row(&s.db(), &target)]
    );
    let both = rids(
        index,
        &seed(&query("TransactionCordinator"), MatchMode::Fuzzy, 10, 0),
    );
    assert_eq!(both.len(), 2);
    assert!(both.contains(&row(&s.db(), &elsewhere)));
    assert_eq!(
        index
            .search_text(&seed(&query("ab"), MatchMode::Fuzzy, 10, 0))
            .unwrap(),
        Searched::Hits {
            hits: vec![],
            exhausted: true
        }
    );
}

/// The order is `(score desc, rid asc)`: identical texts come in `rid` order; a continuation
/// pages through the same order without a repeat; the AND-then-OR widening is decided once
/// per query (the FTS5 adapter's contract).
#[test]
fn order_is_score_then_rid_and_pages_continue() {
    let s = Scratch::new("t7-search", "order");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    for text in [
        "quokka wombat crossing at dusk",
        "quokka wombat burrow by the track",
        "quokka wombat island ferry",
        "quokka sighting near the settlement",
        "quokka colony under the trees",
        "wombat tunnel beside the creek",
        "wombat grass on the ridge",
    ] {
        note(&store, "default", "Fact", text, &[]);
    }
    // three notes of one length and one term frequency: one BM25 score, three rids
    for word in ["alpha", "bravo", "delta"] {
        note(&store, "default", "Fact", &format!("same text {word}"), &[]);
    }
    let index = &store;
    let same = hits(index, &seed(&query("same text"), MatchMode::Lexical, 10, 0)).0;
    assert_eq!(same.len(), 3);
    assert!(
        same.windows(2)
            .all(|w| w[0].score.to_bits() == w[1].score.to_bits() && w[0].rid < w[1].rid)
    );

    let q = query("quokka wombat");
    let page = |limit: usize, sufficient: usize, skip: usize| -> Vec<i64> {
        let mut query = seed(&q, MatchMode::Lexical, limit, skip);
        query.sufficient = sufficient;
        rids(index, &query)
    };
    let by_two = |sufficient: usize| -> Vec<Vec<i64>> {
        let mut pages = Vec::new();
        for p in 0.. {
            let page = page(2, sufficient, 2 * p);
            if page.is_empty() {
                return pages;
            }
            pages.push(page);
        }
        unreachable!()
    };
    let both = page(10, 2, 0);
    assert_eq!(both.len(), 3, "the AND matches: {both:?}");
    let pages = by_two(2);
    assert_eq!(pages.iter().map(Vec::len).collect::<Vec<_>>(), [2, 1]);
    assert_eq!(pages.concat(), both, "every page from the AND expression");
    let widened = page(10, 5, 0);
    assert_eq!(widened.len(), 7, "widened to OR: {widened:?}");
    let pages = by_two(5);
    assert_eq!(pages.iter().map(Vec::len).collect::<Vec<_>>(), [2, 2, 2, 1]);
    assert_eq!(pages.concat(), widened, "every page from the OR expression");
    let (_, exhausted) = hits(index, &seed(&q, MatchMode::Lexical, 7, 0));
    assert!(exhausted);
    let (_, exhausted) = hits(index, &seed(&q, MatchMode::Lexical, 6, 0));
    assert!(!exhausted);
}
