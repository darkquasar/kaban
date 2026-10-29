//! The adapter at its seam (spec 05 SS-08): unavailable until built, filters before the top-k
//! with no leak, a misspelling found by the fuzzy mode, the total order and its continuation,
//! deterministic rebuilds, one generation per pin, the payload's compatibility and bound, and
//! an unbuilt index's purge.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

#[path = "tantivy_common/mod.rs"]
mod common;

use common::{FakeRows, Scratch, hits, index_row, note, rids, row, seed};
use kaban_core::config::Recall;
use kaban_core::text_index::{MatchMode, PurgeOutcome, Searched, TextIndex, TextQuery, Visibility};
use kaban_core::{Change, ChangeLogStore, ForgetMode, MemoryStore, NodeId, RecallQuery};
use kaban_index_tantivy::TantivyIndex;
use kaban_store_sqlite::SqliteStore;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// One move of the database's side of a payload case.
type Move = fn(&mut kaban_core::text_index::Canonical);

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

/// Before a rebuild the index answers `Unavailable` in both modes (never an empty page) and
/// claims nothing visible; after it, the hits carry the row's `rid`, `rev` and kind and the
/// watermark is the canonical position the rebuild read.
#[test]
fn an_unbuilt_index_is_unavailable_and_a_rebuild_makes_it_serve() {
    let s = Scratch::new("unbuilt");
    let store = s.store();
    let fact = note(&store, "default", "Fact", "quokka by the shore", &[]);
    let index = s.adapter();
    let q = query("quokka");
    for mode in [MatchMode::Lexical, MatchMode::Fuzzy] {
        assert_eq!(
            index.search(&seed(&q, mode, 10, 0)).unwrap(),
            Searched::Unavailable
        );
    }
    assert_eq!(index.visible().unwrap(), Visibility::Unavailable);

    index.rebuild().unwrap();

    let (found, exhausted) = hits(&index, &seed(&q, MatchMode::Lexical, 10, 0));
    let (rid, rev) = row(&s.db(), &fact);
    assert_eq!(found.len(), 1);
    assert_eq!(
        (found[0].rid, found[0].revision, found[0].kind.as_str()),
        (rid, rev, "Fact")
    );
    assert!(exhausted);
    let conn = rusqlite::Connection::open(s.db()).unwrap();
    let max_seq: i64 = conn
        .query_row("SELECT max(seq) FROM changes", [], |r| r.get(0))
        .unwrap();
    assert_eq!(index.visible().unwrap(), Visibility::Through(max_seq));
}

/// The filter fields bound the match set before the top-k: with thirty `noise` notes ranked
/// above it, the one `wanted` note is the page's only hit at `limit` 5; a tag matches exactly;
/// another workspace's row and a tombstoned row (gone from the rebuilt index) are never
/// returned; a kind narrows; and a query word equal to a workspace or kind name matches no
/// text (the filter fields are not content).
#[test]
fn filters_apply_before_top_k_and_never_leak() {
    let s = Scratch::new("filters");
    let store = s.store();
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
            &common::ctx("ws1"),
        )
        .unwrap();
    let index = s.adapter();
    index.rebuild().unwrap();
    let rid_of = |n| row(&s.db(), n).0;

    let tagged = RecallQuery {
        tags: vec!["wanted".into()],
        ..in_workspace("quokka", "ws1")
    };
    assert_eq!(
        rids(&index, &seed(&tagged, MatchMode::Lexical, 5, 0)),
        [rid_of(&wanted)],
        "the tag filter runs before the top-k; ws2 and the tombstoned row never"
    );
    let prefixes = RecallQuery {
        tags: vec!["want".into(), "ois".into()],
        ..in_workspace("quokka", "ws1")
    };
    assert_eq!(
        rids(&index, &seed(&prefixes, MatchMode::Lexical, 5, 0)),
        [] as [i64; 0]
    );
    let everything = rids(
        &index,
        &seed(&in_workspace("quokka", "ws1"), MatchMode::Lexical, 100, 0),
    );
    assert_eq!(everything.len(), 32, "30 noise + wanted + idea");
    assert!(!everything.contains(&rid_of(&other_ws)));
    assert!(!everything.contains(&rid_of(&gone)));
    let all_ws = rids(&index, &seed(&query("quokka"), MatchMode::Lexical, 100, 0));
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
        rids(&index, &seed(&kinds, MatchMode::Lexical, 10, 0)),
        [rid_of(&idea)]
    );
    for word in ["ws1", "Fact", "Idea", "active"] {
        for mode in [MatchMode::Lexical, MatchMode::Fuzzy] {
            assert!(
                rids(&index, &seed(&in_workspace(word, "ws1"), mode, 10, 0)).is_empty(),
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
    let s = Scratch::new("fuzzy");
    let store = s.store();
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
    let index = s.adapter();
    index.rebuild().unwrap();
    let q = in_workspace("TransactionCordinator", "ws1");
    assert_eq!(
        rids(&index, &seed(&q, MatchMode::Lexical, 10, 0)),
        [] as [i64; 0]
    );
    assert_eq!(
        rids(&index, &seed(&q, MatchMode::Fuzzy, 10, 0)),
        [row(&s.db(), &target).0]
    );
    let both = rids(
        &index,
        &seed(&query("TransactionCordinator"), MatchMode::Fuzzy, 10, 0),
    );
    assert_eq!(both.len(), 2);
    assert!(both.contains(&row(&s.db(), &elsewhere).0));
    assert_eq!(
        index
            .search(&seed(&query("ab"), MatchMode::Fuzzy, 10, 0))
            .unwrap(),
        Searched::Hits {
            hits: vec![],
            exhausted: true
        }
    );
}

/// The order is `(score desc, rid asc)`: identical texts come in `rid` order; a continuation
/// pages through the same order without a repeat; the AND-then-OR widening is decided once
/// per query (the FTS5 adapter's contract); and two rebuilds give identical hits, scores
/// included.
#[test]
fn order_is_score_then_rid_pages_continue_and_rebuilds_are_deterministic() {
    let s = Scratch::new("order");
    let store = s.store();
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
    let index = s.adapter();
    index.rebuild().unwrap();
    let same = hits(
        &index,
        &seed(&query("same text"), MatchMode::Lexical, 10, 0),
    )
    .0;
    assert_eq!(same.len(), 3);
    assert!(
        same.windows(2)
            .all(|w| w[0].score.to_bits() == w[1].score.to_bits() && w[0].rid < w[1].rid)
    );

    let q = query("quokka wombat");
    let page = |limit: usize, sufficient: usize, skip: usize| -> Vec<i64> {
        let mut query = seed(&q, MatchMode::Lexical, limit, skip);
        query.sufficient = sufficient;
        rids(&index, &query)
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
    let (_, exhausted) = hits(&index, &seed(&q, MatchMode::Lexical, 7, 0));
    assert!(exhausted);
    let (_, exhausted) = hits(&index, &seed(&q, MatchMode::Lexical, 6, 0));
    assert!(!exhausted);

    let first = hits(&index, &seed(&q, MatchMode::Lexical, 10, 0));
    index.rebuild().unwrap();
    assert_eq!(hits(&index, &seed(&q, MatchMode::Lexical, 10, 0)), first);
    let again = s.adapter();
    again.rebuild().unwrap();
    assert_eq!(hits(&again, &seed(&q, MatchMode::Lexical, 10, 0)), first);
}

/// A pinned view serves the generation it was taken at: a row added and rebuilt meanwhile
/// is in a fresh search and not in the pinned one, whose watermark is the old one; the pin of
/// an unbuilt index is unavailable.
#[test]
fn a_pin_serves_one_generation_across_a_rebuild() {
    let s = Scratch::new("pin");
    let store = s.store();
    note(&store, "default", "Fact", "quokka one", &[]);
    let index = s.adapter();
    let unbuilt = index.pin().unwrap();
    assert_eq!(
        unbuilt
            .search(&seed(&query("quokka"), MatchMode::Lexical, 10, 0))
            .unwrap(),
        Searched::Unavailable
    );
    index.rebuild().unwrap();
    let pinned = index.pin().unwrap();
    let before = index.visible().unwrap();
    let q = query("quokka");
    assert_eq!(
        rids(pinned.as_ref(), &seed(&q, MatchMode::Lexical, 10, 0)).len(),
        1
    );

    let two = note(&store, "default", "Fact", "quokka two", &[]);
    index.rebuild().unwrap();

    let fresh = rids(&index, &seed(&q, MatchMode::Lexical, 10, 0));
    assert_eq!(fresh.len(), 2);
    assert!(fresh.contains(&row(&s.db(), &two).0));
    assert_eq!(
        rids(pinned.as_ref(), &seed(&q, MatchMode::Lexical, 10, 0)).len(),
        1
    );
    assert_eq!(pinned.visible().unwrap(), before);
    assert_ne!(index.visible().unwrap(), before);
    assert_eq!(
        rids(
            pinned.pin().unwrap().as_ref(),
            &seed(&q, MatchMode::Lexical, 10, 0)
        )
        .len(),
        1
    );
}

/// Serving enforces the payload's compatibility and the canonical bound: another lineage or
/// projection makes the built index unavailable (another instance of the lineage, a fork, does
/// not, and neither does the database's committed identifier label: the index derives its
/// terms with the build's rule, checked in `payload.rs`), a payload ahead of the surviving
/// history is unavailable, and the same index serves again once the position fits.
/// T6 review t6-5: an index never committed with a payload serves nothing and says so; one
/// rebuilt over an empty history at sequence 0 serves that payload, a watermark like any
/// other, and the two are told apart at the seam.
#[test]
fn no_payload_is_unavailable_while_an_empty_rebuild_is_a_watermark_at_zero() {
    let (rows, _canonical) = FakeRows::new(Vec::new(), 0);
    let index = TantivyIndex::in_ram(
        &kaban_core::shipped::resolved().unwrap(),
        &Recall::default(),
        Box::new(rows),
    )
    .unwrap();
    assert_eq!(index.visible().unwrap(), Visibility::Unavailable);
    assert_eq!(
        index
            .search(&seed(&query("quokka"), MatchMode::Lexical, 10, 0))
            .unwrap(),
        Searched::Unavailable
    );
    index.rebuild().unwrap();
    assert_eq!(index.visible().unwrap(), Visibility::Through(0));
    assert!(matches!(
        index.search(&seed(&query("quokka"), MatchMode::Lexical, 10, 0)).unwrap(),
        Searched::Hits { ref hits, exhausted: true } if hits.is_empty()
    ));
}

#[test]
fn an_incompatible_or_ahead_payload_is_unavailable_until_the_position_fits() {
    let (rows, canonical) = FakeRows::new(
        vec![
            index_row(1, "default", "Fact", "quokka one"),
            index_row(2, "default", "Fact", "quokka two"),
        ],
        5,
    );
    let index = TantivyIndex::in_ram(
        &kaban_core::shipped::resolved().unwrap(),
        &Recall::default(),
        Box::new(rows),
    )
    .unwrap();
    index.rebuild().unwrap();
    let q = query("quokka");
    let serves = |index: &TantivyIndex| {
        let served = matches!(
            index.search(&seed(&q, MatchMode::Lexical, 10, 0)).unwrap(),
            Searched::Hits { ref hits, .. } if hits.len() == 2
        );
        let pinned = matches!(
            index.pin().unwrap().search(&seed(&q, MatchMode::Lexical, 10, 0)).unwrap(),
            Searched::Hits { ref hits, .. } if hits.len() == 2
        );
        assert_eq!(served, pinned, "pin and search agree");
        served
    };
    assert!(serves(&index));
    assert_eq!(index.visible().unwrap(), Visibility::Through(5));
    let built = canonical.lock().unwrap().clone();
    let cases: [(&str, bool, Move); 6] = [
        ("another instance of the lineage (a fork)", true, |c| {
            c.identity.instance_id = "db-2".into();
        }),
        ("a later position", true, |c| c.seq = 9),
        ("another lineage", false, |c| {
            c.identity.pack_lineage_id = "lineage-2".into();
        }),
        ("another projection", false, |c| {
            c.identity.projection_signature = "sig-2".into();
        }),
        ("the database's identifier label alone", true, |c| {
            c.identity.ident_terms_version = "3".into();
        }),
        ("a payload ahead of the history", false, |c| c.seq = 4),
    ];
    for (name, available, change) in cases {
        {
            let mut at = canonical.lock().unwrap();
            *at = built.clone();
            change(&mut at);
        }
        assert_eq!(serves(&index), available, "{name}");
        let visible = index.visible().unwrap();
        assert_eq!(
            visible == Visibility::Through(5),
            available,
            "{name}: {visible:?}"
        );
    }
    *canonical.lock().unwrap() = built;
    assert!(serves(&index), "serves again once the position fits");
}

/// An index directory never written (no payload and no segment file) holds nothing of any
/// record: its purge takes the lock and is cleaned with nothing to say. (Segment files
/// without a payload are an interrupted build's and are never claimed cleaned: the adapter's
/// own staged-artefacts case.)
#[test]
fn a_purge_of_an_unbuilt_index_has_nothing_to_erase() {
    let (rows, _canonical) = FakeRows::new(vec![index_row(1, "default", "Fact", "quokka")], 1);
    let index = TantivyIndex::in_ram(
        &kaban_core::shipped::resolved().unwrap(),
        &Recall::default(),
        Box::new(rows),
    )
    .unwrap();
    assert_eq!(
        index.purge(&[1]).unwrap(),
        kaban_core::text_index::PurgeOutcome {
            cleaned: true,
            detail: None
        }
    );
}

/// The adapter behind a recall, recording the recall's pins and the pages of its pinned
/// view, and publishing a new matching row between the first page and the next (T2 review
/// t2-8): a second store handle commits it, a transient writer publishes it.
struct CommitBetweenPages {
    index: Arc<TantivyIndex>,
    db: PathBuf,
    keys: PathBuf,
    pins: AtomicUsize,
    /// `(skip, hits)` of every page served by a pinned view.
    pages: Mutex<Vec<(usize, usize)>>,
    published: AtomicBool,
}

impl CommitBetweenPages {
    /// Commit "the late arrival" through a second handle and publish every change since the
    /// serving watermark with a transient writer: a new generation lands between two pages.
    fn publish_late_arrival(&self) {
        let store = SqliteStore::open(&self.db, &self.keys).unwrap();
        note(&store, "default", "Fact", "quokka the late arrival", &[]);
        let Visibility::Through(seq) = self.index.visible().unwrap() else {
            panic!("a watermark");
        };
        let changes: Vec<Change> = store
            .changes_read(u64::try_from(seq).unwrap(), 2048)
            .unwrap();
        self.index.project(&changes).unwrap();
    }
}

impl TextIndex for CommitBetweenPages {
    fn search(&self, query: &TextQuery<'_>) -> kaban_core::Result<Searched> {
        self.index.search(query)
    }
    fn visible(&self) -> kaban_core::Result<Visibility> {
        self.index.visible()
    }
    fn project(&self, changes: &[Change]) -> kaban_core::Result<()> {
        self.index.project(changes)
    }
    fn rebuild(&self) -> kaban_core::Result<()> {
        self.index.rebuild()
    }
    fn purge(&self, rids: &[i64]) -> kaban_core::Result<PurgeOutcome> {
        self.index.purge(rids)
    }
    fn pin(&self) -> kaban_core::Result<Box<dyn TextIndex + '_>> {
        self.pins.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(PinnedPages {
            outer: self,
            view: self.index.pin()?,
        }))
    }
}

/// One pinned view of [`CommitBetweenPages`].
struct PinnedPages<'a> {
    outer: &'a CommitBetweenPages,
    view: Box<dyn TextIndex + 'a>,
}

impl TextIndex for PinnedPages<'_> {
    fn search(&self, query: &TextQuery<'_>) -> kaban_core::Result<Searched> {
        let page = self.view.search(query)?;
        if let Searched::Hits { hits, .. } = &page {
            self.outer
                .pages
                .lock()
                .unwrap()
                .push((query.skip, hits.len()));
        }
        if !self.outer.published.swap(true, Ordering::SeqCst) {
            self.outer.publish_late_arrival();
        }
        Ok(page)
    }
    fn visible(&self) -> kaban_core::Result<Visibility> {
        self.view.visible()
    }
    fn project(&self, changes: &[Change]) -> kaban_core::Result<()> {
        self.outer.project(changes)
    }
    fn rebuild(&self) -> kaban_core::Result<()> {
        self.outer.rebuild()
    }
    fn purge(&self, rids: &[i64]) -> kaban_core::Result<PurgeOutcome> {
        self.outer.purge(rids)
    }
    fn pin(&self) -> kaban_core::Result<Box<dyn TextIndex + '_>> {
        self.view.pin()
    }
}

/// SS-11 through a real store (T2 review t2-8): a recall whose first page comes back short
/// (rows tombstoned after the build fail revalidation) refills from the same pinned
/// generation while a new generation is published between its pages. The recall takes one
/// pin; its pages continue exactly where the last stopped and together name every row of
/// the pinned generation once (no skip, no duplicate); the row published in between is not
/// among them and a fresh recall finds it.
#[test]
fn a_commit_between_two_pages_of_one_recall_neither_duplicates_nor_skips_under_one_pin() {
    let s = Scratch::new("commit-between-pages");
    let store = s.store();
    let index = Arc::new(s.adapter());
    let hook = Arc::new(CommitBetweenPages {
        index: Arc::clone(&index),
        db: s.db(),
        keys: s.keys(),
        pins: AtomicUsize::new(0),
        pages: Mutex::new(Vec::new()),
        published: AtomicBool::new(false),
    });
    let store = store.with_text_index(Box::new(Arc::clone(&hook)));
    // sixty matching rows: more than one page of fifty seeds at limit ten
    let notes: Vec<_> = (0..60)
        .map(|i| {
            note(
                &store,
                "default",
                "Fact",
                &format!("quokka number {i} by the shore"),
                &[],
            )
        })
        .collect();
    index.rebuild().unwrap();
    // tombstoned after the build: the pinned generation still lists them, revalidation
    // drops them, so the first page is short and the recall refills
    for n in &notes[..20] {
        store
            .forget(
                &NodeId::parse(&n.id).unwrap(),
                ForgetMode::Tombstone,
                &common::ctx("default"),
            )
            .unwrap();
    }

    let found = store
        .recall(&RecallQuery {
            query: "quokka shore".into(),
            limit: 10,
            hops: 0,
            ..Default::default()
        })
        .unwrap();

    assert_eq!(
        hook.pins.load(Ordering::SeqCst),
        1,
        "one pinned generation for the whole recall"
    );
    let pages = hook.pages.lock().unwrap().clone();
    assert!(pages.len() >= 2, "a refill happened: {pages:?}");
    let mut next = 0;
    for (skip, hits) in &pages {
        assert_eq!(
            *skip, next,
            "each page continues where the last stopped: {pages:?}"
        );
        next += hits;
    }
    assert_eq!(
        next, 60,
        "every row of the pinned generation once: {pages:?}"
    );
    assert_eq!(found.len(), 10);
    let mut ids: Vec<&str> = found.iter().map(|h| h.node.id.as_str()).collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), 10, "no duplicate");
    assert!(
        found.iter().all(|h| !h.node.text.contains("late arrival")),
        "the generation published between the pages is not served by the pinned one"
    );
    assert!(hook.published.load(Ordering::SeqCst));
    assert_eq!(store.recall(&query("late arrival")).unwrap().len(), 1);
}
