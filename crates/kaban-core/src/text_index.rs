//! The text-search seam (spec 05 SS-07): recall reaches its lexical and fuzzy seeds only through
//! [`TextIndex`]. An adapter owns its query language, ranking inputs and storage; callers see
//! typed requests and `(rid, revision, score)` hits. FTS5 (in `kaban-store-sqlite`) is the one
//! production adapter; it is transactional, so its writes are visible on commit and
//! [`TextIndex::visible`] always answers [`Visibility::Current`].
//!
//! Contract every adapter keeps:
//! - Hits are candidates, not answers. The caller re-reads each `rid` from the record store and
//!   re-checks lifecycle, workspace, kinds and tags before returning it; an adapter applies the
//!   [`SeedFilter`] fields it can and may return rows that fail the rest.
//! - Order is total and deterministic: score order as the adapter defines it, ties by `rid`.
//! - [`Searched::Unavailable`] (index missing, disabled or not yet built) is never an empty
//!   [`Searched::Hits`]: "no matches" and "cannot answer" stay distinct.
//! - The pages of one recall come from one index generation ([`TextIndex::pin`]): a commit
//!   between two pages never shifts the continuation offset or repeats a hit.
//!
//! An adapter with its own storage (spec 05 SS-08) reads the canonical records it projects
//! through [`Rows`]: the record store's view of what the index holds per row ([`IndexRow`]), the
//! identity a projection is bound to ([`IndexIdentity`]) and the canonical position it may claim
//! ([`Canonical`]). Record bodies cross here only on their way into the index.

use crate::changelog::Change;
use crate::{CodeRefFilter, ReadFilters, RecallQuery, Result};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The directory of an index with its own storage beside the database file `db`:
/// `<parent>/<stem>.index/tantivy/`. The one place the layout is defined: the adapter opens
/// it, a pack captures it (SS-18) and a fork receives its copy there (SS-20).
#[must_use]
pub fn index_dir(db: &Path) -> PathBuf {
    let stem = db
        .file_stem()
        .map_or_else(|| "kaban".to_owned(), |s| s.to_string_lossy().into_owned());
    db.parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!("{stem}.index"))
        .join("tantivy")
}

/// Which retrieval an adapter runs for a [`TextQuery`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchMode {
    /// Term matching with the adapter's analyzer and ranking (FTS5: BM25 over `node_fts`).
    Lexical,
    /// Misspelling-tolerant matching.
    Fuzzy,
    /// The whole query text, lowercased, equals an indexed name (an identifier value) exactly:
    /// a term lookup, no ranking beyond `rid` order. The caller revalidates case.
    Exact,
}

/// The record filters a seed search must honour, borrowed from the recall request.
#[derive(Debug, Clone, Copy)]
pub struct SeedFilter<'a> {
    /// Only these kinds (empty = any).
    pub kinds: &'a [String],
    /// At least one of these tags (empty = any).
    pub tags: &'a [String],
    /// Only this workspace.
    pub workspace_id: Option<&'a str>,
    /// Only notes linked `ABOUT` / `EVIDENCE_FOR` to a matching `CodeRef`.
    pub code_ref: Option<&'a CodeRefFilter>,
    /// Candidate visibility (EN-F49).
    pub read: ReadFilters,
}

impl<'a> SeedFilter<'a> {
    /// The filters of a recall request.
    #[must_use]
    pub fn of(q: &'a RecallQuery) -> Self {
        Self {
            kinds: &q.kinds,
            tags: &q.tags,
            workspace_id: q.workspace_id.as_deref(),
            code_ref: q.r#ref.as_ref(),
            read: q.filters,
        }
    }
}

/// One seed search.
#[derive(Debug, Clone, Copy)]
pub struct TextQuery<'a> {
    /// The user's query text, unparsed; the adapter owns the query syntax.
    pub text: &'a str,
    /// Lexical or fuzzy retrieval.
    pub mode: MatchMode,
    /// The record filters the search honours.
    pub filter: SeedFilter<'a>,
    /// At most this many hits.
    pub limit: usize,
    /// Lexical only: an adapter that widens a strict match (FTS5: AND, then OR) stops widening
    /// once it has at least this many hits.
    pub sufficient: usize,
    /// Skip this many hits of the full ordered result (continuation for refill).
    pub skip: usize,
}

/// A candidate record.
#[derive(Debug, Clone, PartialEq)]
pub struct TextHit {
    /// The record's local row id (never a cross-database identity, SS-27).
    pub rid: i64,
    /// The record revision the index saw; the caller compares it with the current one.
    pub revision: i64,
    /// Adapter score; only the order within one result is meaningful.
    pub score: f64,
    /// The record's kind as indexed (seeds the per-kind graph expansion hints).
    pub kind: String,
}

/// The answer to a [`TextQuery`].
#[derive(Debug, Clone, PartialEq)]
pub enum Searched {
    /// Hits in adapter order; `exhausted` is true when no hit exists past this page.
    Hits {
        /// The hits of this page.
        hits: Vec<TextHit>,
        /// No hit exists past this page.
        exhausted: bool,
    },
    /// The adapter cannot answer this mode now (missing, disabled or not yet built).
    Unavailable,
}

/// How far committed writes are visible to searches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Visibility {
    /// Every committed write is searchable (a transactional adapter).
    Current,
    /// Searchable through this local `ChangeLog` sequence; later writes are pending. `0` is
    /// a watermark like any other: the payload of an index built over an empty history.
    Through(i64),
    /// Nothing is searchable: the adapter serves no payload (an index not yet built, or one
    /// that is not of this database). No watermark is claimed, so no committed write is
    /// visible; doctor reports the index missing, a barrier stays pending.
    Unavailable,
}

/// What [`TextIndex::purge`] achieved: whether the purged records' tokens left the index's
/// storage, with the adapter's reason when they did not (a held checkpoint, a failed merge).
/// Never an error: the purge itself already committed, so its hygiene is a reported outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PurgeOutcome {
    /// The index holds nothing of the purged records any more.
    pub cleaned: bool,
    /// Why not, when `cleaned` is false.
    pub detail: Option<String>,
}

/// The text-search seam. Adapters are constructed with their dependencies (a connection, a
/// reader, the ontology); the trait carries only the operations callers use: the two reads
/// (`search`, `visible`) and the three typed maintenance operations (`project`, `rebuild`,
/// `purge`). Record content never crosses here: a projection carries ids, and a rebuild reads
/// the canonical records through the store's existing operations.
pub trait TextIndex {
    /// Seed candidates for a recall.
    ///
    /// # Errors
    /// A backend failure; an absent or disabled index is [`Searched::Unavailable`], not an error.
    fn search(&self, query: &TextQuery<'_>) -> Result<Searched>;

    /// The visibility watermark.
    ///
    /// # Errors
    /// A backend failure reading it.
    fn visible(&self) -> Result<Visibility>;

    /// Project committed record changes, replacements and deletions alike, into the index: one
    /// typed operation in `ChangeLog` order. A transactional adapter whose storage is written
    /// with the record has nothing left to do; an adapter with its own storage applies them.
    ///
    /// # Errors
    /// A backend failure applying them.
    fn project(&self, changes: &[Change]) -> Result<()>;

    /// Rebuild the whole index from the canonical records, through the store's existing
    /// operations.
    ///
    /// # Errors
    /// A backend failure; the index is left as it was.
    fn rebuild(&self) -> Result<()>;

    /// Post-purge hygiene for the purged `rids`: make their tokens leave the index's storage
    /// and report whether they did.
    ///
    /// # Errors
    /// A backend failure before the hygiene ran; a held or failed step is a [`PurgeOutcome`].
    fn purge(&self, rids: &[i64]) -> Result<PurgeOutcome>;

    /// A view bound to one index generation for the pages of one recall: its `search` and
    /// `visible` answer from the generation pinned here, however many commits land meanwhile,
    /// so a continuation (`skip`) never shifts or repeats a hit. The maintenance operations
    /// forward to the index. Every adapter says how it keeps this, there is no default: an
    /// adapter with its own storage pins one searcher, a transactional adapter pins a read
    /// snapshot of its connection for the view's lifetime, and a fake whose lists are fixed for
    /// its lifetime is its own pinned view. A view that merely borrows a live adapter promises
    /// a generation it cannot keep.
    ///
    /// # Errors
    /// A backend failure taking the snapshot.
    fn pin(&self) -> Result<Box<dyn TextIndex + '_>>;
}

impl<T: TextIndex + ?Sized> TextIndex for &T {
    fn search(&self, query: &TextQuery<'_>) -> Result<Searched> {
        (**self).search(query)
    }
    fn visible(&self) -> Result<Visibility> {
        (**self).visible()
    }
    fn project(&self, changes: &[Change]) -> Result<()> {
        (**self).project(changes)
    }
    fn rebuild(&self) -> Result<()> {
        (**self).rebuild()
    }
    fn purge(&self, rids: &[i64]) -> Result<PurgeOutcome> {
        (**self).purge(rids)
    }
    fn pin(&self) -> Result<Box<dyn TextIndex + '_>> {
        (**self).pin()
    }
}

impl<T: TextIndex + ?Sized> TextIndex for Arc<T> {
    fn search(&self, query: &TextQuery<'_>) -> Result<Searched> {
        (**self).search(query)
    }
    fn visible(&self) -> Result<Visibility> {
        (**self).visible()
    }
    fn project(&self, changes: &[Change]) -> Result<()> {
        (**self).project(changes)
    }
    fn rebuild(&self) -> Result<()> {
        (**self).rebuild()
    }
    fn purge(&self, rids: &[i64]) -> Result<PurgeOutcome> {
        (**self).purge(rids)
    }
    fn pin(&self) -> Result<Box<dyn TextIndex + '_>> {
        (**self).pin()
    }
}

/// One canonical record as an index with its own storage projects it (spec 05 SS-08): the
/// retrieval keys (`id`, the delete key; `rid` and `revision`, the hit), the filter fields and
/// the searchable inputs. `summary` is the record's `fts_fields` projection, computed by the
/// store exactly as the FTS5 `summary` column is; `ident_values` and `anchor_keys` are the
/// identifier inputs beside text, summary and tags, so the adapter derives the identifier terms
/// with [`crate::ident::ident_terms`] and never reads a derived column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexRow {
    /// The record UUID.
    pub id: String,
    /// The local row id (the hit key and tie-break).
    pub rid: i64,
    /// The revision the row was read at.
    pub revision: i64,
    /// The stored kind name.
    pub kind: String,
    /// The workspace.
    pub workspace_id: String,
    /// The tags.
    pub tags: Vec<String>,
    /// The record text.
    pub text: String,
    /// The projected summary (stored summary plus the kind's `fts_fields`): the FTS5 `summary`
    /// column's text.
    pub summary: String,
    /// The stored summary as written: the identifier derivation's and the fuzzy index's input.
    pub stored_summary: Option<String>,
    /// The values of the kind's `ident_fields`.
    pub ident_values: Vec<String>,
    /// The record's anchor keys, in `(anchor_kind, key)` order.
    pub anchor_keys: Vec<String>,
}

/// What a projection with its own storage is bound to. Compatibility is the lineage and the
/// versions (an index built for one lineage serves every instance of it, a fork included);
/// `instance_id` is carried for receipts only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexIdentity {
    /// This database (`db_id`).
    pub instance_id: String,
    /// The pack lineage the database belongs to.
    pub pack_lineage_id: String,
    /// The committed `fts_projection_signature` (the summary projection and the triggers).
    pub projection_signature: String,
    /// The committed `ident_terms_version`.
    pub ident_terms_version: String,
}

/// The canonical position an index may claim: the newest `ChangeLog` sequence and the identity
/// at the time of one consistent read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Canonical {
    /// `max(changes.seq)`, 0 when the log is empty.
    pub seq: u64,
    /// The identity the read saw.
    pub identity: IndexIdentity,
}

/// The record store as an index with its own storage sees it: active rows by id (a projection
/// page), every active row (a rebuild) and the canonical position. Implemented by the SQLite
/// store over its own read-only connection and by test fakes; the adapter holds one as a boxed
/// dependency and never a connection.
pub trait Rows: Send + Sync {
    /// The active rows among `ids`, in `rid` order; an id that is absent, tombstoned, archived
    /// or purged yields no row (the caller deletes by id first, so the absence is the deletion).
    ///
    /// # Errors
    /// A backend failure.
    fn current(&self, ids: &[String]) -> Result<Vec<IndexRow>>;

    /// Every active row in `rid` order through `visit`, with the canonical position read in
    /// the same transaction, so the rows are exactly the state at `Canonical::seq`.
    ///
    /// # Errors
    /// A backend failure, or the first error `visit` returns.
    fn active(&self, visit: &mut dyn FnMut(IndexRow) -> Result<()>) -> Result<Canonical>;

    /// The canonical position now.
    ///
    /// # Errors
    /// A backend failure.
    fn canonical(&self) -> Result<Canonical>;

    /// One page of the `ChangeLog` after `after` (at most `limit` rows, oldest first) together
    /// with the oldest surviving sequence and the canonical position, all read in one
    /// transaction (spec 05 SS-09, design amendment 6): the projector decides from one snapshot
    /// whether its payload still joins the surviving history, so a prune cannot slip between
    /// the coverage check and the page it covers.
    ///
    /// # Errors
    /// A backend failure.
    fn replay(&self, after: u64, limit: usize) -> Result<Replay>;
}

/// What [`Rows::replay`] reads in one transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Replay {
    /// The oldest surviving `changes.seq`, 0 when the log is empty.
    pub oldest: u64,
    /// The canonical position the page was read at.
    pub at: Canonical,
    /// The rows after the requested position, oldest first.
    pub changes: Vec<Change>,
}

/// A scripted in-memory adapter for tests of callers: fixed hits, availability per mode, a
/// visibility watermark that can lag, and a record of the changes projected into it.
#[cfg(any(test, feature = "test-fake"))]
pub mod fake {
    use super::{MatchMode, PurgeOutcome, Searched, TextHit, TextIndex, TextQuery, Visibility};
    use crate::Result;
    use crate::changelog::Change;
    use std::sync::Mutex;

    /// See the module doc.
    #[derive(Debug)]
    pub struct FakeIndex {
        /// The full lexical order; `None` = [`Searched::Unavailable`].
        pub lexical: Option<Vec<TextHit>>,
        /// The full fuzzy order; `None` = [`Searched::Unavailable`].
        pub fuzzy: Option<Vec<TextHit>>,
        /// What [`TextIndex::visible`] answers.
        pub visible: Visibility,
        /// Every change handed to [`TextIndex::project`], in call order (a lock, so the fake
        /// can be shared across threads like the production adapter).
        pub projected: Mutex<Vec<Change>>,
    }

    impl FakeIndex {
        /// The changes projected so far, in call order.
        #[must_use]
        pub fn projected(&self) -> Vec<Change> {
            self.projected
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }
    }

    impl Default for FakeIndex {
        /// Nothing scripted: both modes unavailable, every write visible, nothing projected.
        fn default() -> Self {
            Self {
                lexical: None,
                fuzzy: None,
                visible: Visibility::Current,
                projected: Mutex::new(Vec::new()),
            }
        }
    }

    impl Clone for FakeIndex {
        fn clone(&self) -> Self {
            Self {
                lexical: self.lexical.clone(),
                fuzzy: self.fuzzy.clone(),
                visible: self.visible,
                projected: Mutex::new(self.projected()),
            }
        }
    }

    impl TextIndex for FakeIndex {
        fn search(&self, query: &TextQuery<'_>) -> Result<Searched> {
            let hits = match query.mode {
                MatchMode::Lexical | MatchMode::Exact => &self.lexical,
                MatchMode::Fuzzy => &self.fuzzy,
            };
            Ok(hits.as_ref().map_or(Searched::Unavailable, |all| {
                let page: Vec<TextHit> = all
                    .iter()
                    .skip(query.skip)
                    .take(query.limit)
                    .cloned()
                    .collect();
                let exhausted = query.skip + page.len() >= all.len();
                Searched::Hits {
                    hits: page,
                    exhausted,
                }
            }))
        }

        fn visible(&self) -> Result<Visibility> {
            Ok(self.visible)
        }

        fn project(&self, changes: &[Change]) -> Result<()> {
            self.projected
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .extend_from_slice(changes);
            Ok(())
        }

        fn rebuild(&self) -> Result<()> {
            Ok(())
        }

        fn purge(&self, _rids: &[i64]) -> Result<PurgeOutcome> {
            Ok(PurgeOutcome {
                cleaned: true,
                detail: None,
            })
        }

        /// The fake's lists are fixed for its lifetime (a projection records, it never
        /// reorders them), so the fake itself is one generation.
        fn pin(&self) -> Result<Box<dyn TextIndex + '_>> {
            Ok(Box::new(self))
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion
mod tests {
    use super::fake::FakeIndex;
    use super::{
        MatchMode, PurgeOutcome, Searched, SeedFilter, TextHit, TextIndex, TextQuery, Visibility,
    };
    use crate::RecallQuery;
    use crate::changelog::{Change, ChangeEntity, ChangeOp};

    fn hit(rid: i64) -> TextHit {
        TextHit {
            rid,
            revision: 1,
            score: 1.0,
            kind: "Note".into(),
        }
    }

    fn query(q: &RecallQuery, mode: MatchMode, skip: usize) -> TextQuery<'_> {
        TextQuery {
            text: "x",
            mode,
            filter: SeedFilter::of(q),
            limit: 2,
            sufficient: 2,
            skip,
        }
    }

    /// Unavailable stays distinct from an empty answer, per mode.
    #[test]
    fn unavailable_is_never_an_empty_page() {
        let q = RecallQuery::default();
        let idx = FakeIndex {
            lexical: Some(vec![]),
            fuzzy: None,
            ..Default::default()
        };
        assert_eq!(
            idx.search(&query(&q, MatchMode::Lexical, 0)).unwrap(),
            Searched::Hits {
                hits: vec![],
                exhausted: true
            }
        );
        assert_eq!(
            idx.search(&query(&q, MatchMode::Fuzzy, 0)).unwrap(),
            Searched::Unavailable
        );
    }

    /// Continuation pages through the full order; a lagging watermark reports its position.
    #[test]
    fn skip_continues_and_visibility_can_lag() {
        let q = RecallQuery::default();
        let idx = FakeIndex {
            lexical: Some(vec![hit(1), hit(2), hit(3)]),
            fuzzy: None,
            visible: Visibility::Through(7),
            ..Default::default()
        };
        let Searched::Hits { hits, exhausted } =
            idx.search(&query(&q, MatchMode::Lexical, 2)).unwrap()
        else {
            panic!("available")
        };
        assert_eq!((hits, exhausted), (vec![hit(3)], true));
        assert_eq!(idx.visible().unwrap(), Visibility::Through(7));
        assert_eq!(FakeIndex::default().visible().unwrap(), Visibility::Current);
    }

    /// The typed operations on the fake: a projection is recorded in order (replacement and
    /// deletion through the one operation), a rebuild succeeds, a purge reports cleaned.
    #[test]
    fn the_fake_records_projections_and_reports_a_clean_purge() {
        let change = |seq, op| Change {
            seq,
            entity: ChangeEntity::Node,
            op,
            id: format!("n{seq}"),
            workspace_id: "default".into(),
            ts: 0,
            paths: None,
        };
        let idx = FakeIndex::default();
        idx.project(&[change(1, ChangeOp::Update)]).unwrap();
        idx.project(&[change(2, ChangeOp::Delete)]).unwrap();
        assert_eq!(
            idx.projected(),
            vec![change(1, ChangeOp::Update), change(2, ChangeOp::Delete)]
        );
        idx.rebuild().unwrap();
        assert_eq!(
            idx.purge(&[1, 2]).unwrap(),
            PurgeOutcome {
                cleaned: true,
                detail: None
            }
        );
    }

    /// The fake's pin is the fake itself, reached through a reference or an `Arc` (the
    /// blanket impls forward): the pinned view answers like the fake.
    #[test]
    fn the_fake_pins_itself_through_a_reference_or_an_arc() {
        let q = RecallQuery::default();
        let idx = std::sync::Arc::new(FakeIndex {
            lexical: Some(vec![hit(1), hit(2)]),
            visible: Visibility::Through(3),
            ..Default::default()
        });
        let pinned = idx.pin().unwrap();
        assert_eq!(
            pinned.search(&query(&q, MatchMode::Lexical, 1)).unwrap(),
            Searched::Hits {
                hits: vec![hit(2)],
                exhausted: true
            }
        );
        assert_eq!(pinned.visible().unwrap(), Visibility::Through(3));
        let by_ref: &dyn TextIndex = &*idx;
        assert_eq!(
            by_ref.pin().unwrap().visible().unwrap(),
            Visibility::Through(3)
        );
    }
}
