//! The text-search seam (spec 05 SS-07, spec 11 T7): recall reaches its lexical and fuzzy seeds
//! only through [`TextIndex`]. An adapter owns its query language and ranking inputs; callers
//! see typed requests and `(rid, score)` hits (TXT-05: identity and score only, everything else
//! is read from the rows). The Tantivy adapter (`kaban-index-tantivy`) searches the catalog's
//! text tiers and packs at one pinned generation ([`TextSnapshot`]), so a commit is searchable
//! as soon as it returns.
//!
//! Contract every adapter keeps:
//! - Hits are candidates, not answers. The caller re-reads each `rid` from the record store and
//!   re-checks lifecycle, workspace, kinds and tags before returning it; an adapter applies the
//!   [`SeedFilter`] fields it can and may return rows that fail the rest.
//! - Order is total and deterministic: score order as the adapter defines it, ties by `rid`.
//! - [`Searched::Unavailable`] (an index that cannot answer) is never an empty
//!   [`Searched::Hits`]: "no matches" and "cannot answer" stay distinct.
//! - The pages of one recall search one snapshot: a commit between two pages never shifts the
//!   continuation offset or repeats a hit.
//!
//! [`IndexRow`] is one canonical record as the index is built from it.

use crate::query_budget::QueryBudget;
use crate::{CodeRefFilter, RecallQuery, Result};
use std::path::Path;
use std::sync::Arc;

/// Which retrieval an adapter runs for a [`TextQuery`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchMode {
    /// Term matching with the adapter's analyzer and ranking (FTS5: BM25 over `node_fts`).
    Lexical,
    /// Whole, case-folded identifier lookup.
    Exact,
    /// Misspelling-tolerant matching.
    Fuzzy,
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

/// A candidate record: its identity and score only (TXT-05); the caller reads everything else
/// from the row.
#[derive(Debug, Clone, PartialEq)]
pub struct TextHit {
    /// The record's local row id (never a cross-database identity, SS-27).
    pub rid: i64,
    /// Adapter score; only the order within one result is meaningful.
    pub score: f64,
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

/// One sealed text tier: a byte range of the container (spec 11 T7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExtentSpan {
    /// Start of the range.
    pub offset: u64,
    /// Length of the range.
    pub len: u64,
}

/// The text index at one catalog generation (spec 11 T7): the sealed tiers in `kbx`, oldest
/// first, then the pending sediment segments in commit order. Each tier and segment carries
/// the identity tombstones of its commits, which hide the documents of every older tier and
/// segment (a flat list could not order a delete before a later re-add of the same rid). The
/// caller's pin keeps every span protected for as long as the snapshot is borrowed.
#[derive(Debug, Clone, Copy)]
pub struct TextSnapshot<'a> {
    /// The pinned catalog generation.
    pub generation: u64,
    /// The container the tiers live in.
    pub kbx: &'a Path,
    /// The sealed tiers, oldest first.
    pub tiers: &'a [ExtentSpan],
    /// The pending sediment segments, oldest first.
    pub sediment: &'a [&'a [u8]],
}

/// The text-search seam. Adapters are constructed with their dependencies (the ontology, the
/// recall configuration); the trait carries only the read (`search`): a purge rewrites the
/// catalog's tiers, never the adapter. Record content never crosses here.
pub trait TextIndex {
    /// Seed candidates for a recall from the catalog snapshot `at` (spec 11 T7), charged to
    /// `budget`.
    ///
    /// # Errors
    /// A backend failure or the budget's refusal; an index that cannot answer is
    /// [`Searched::Unavailable`], not an error.
    fn search(
        &self,
        at: &TextSnapshot<'_>,
        query: &TextQuery<'_>,
        budget: &QueryBudget,
    ) -> Result<Searched>;
}

impl<T: TextIndex + ?Sized> TextIndex for &T {
    fn search(
        &self,
        at: &TextSnapshot<'_>,
        query: &TextQuery<'_>,
        budget: &QueryBudget,
    ) -> Result<Searched> {
        (**self).search(at, query, budget)
    }
}

impl<T: TextIndex + ?Sized> TextIndex for Arc<T> {
    fn search(
        &self,
        at: &TextSnapshot<'_>,
        query: &TextQuery<'_>,
        budget: &QueryBudget,
    ) -> Result<Searched> {
        (**self).search(at, query, budget)
    }
}

/// One canonical record as the text index is built from it (spec 05 SS-08): the
/// retrieval keys (`id`, the delete key; `rid` and `revision`, the hit), the filter fields and
/// the searchable inputs. `summary` is the record's `fts_fields` projection, computed by the
/// store exactly as the FTS5 `summary` column is; `ident_values` and `anchor_keys` are the
/// identifier inputs beside text, summary and tags, so the adapter derives the identifier terms
/// with [`crate::ident::split_terms`] and never reads a derived column.
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

/// A scripted in-memory adapter for tests of callers: fixed hits and availability per mode,
/// whatever the snapshot.
#[cfg(any(test, feature = "test-fake"))]
pub mod fake {
    use super::{MatchMode, Searched, TextHit, TextIndex, TextQuery, TextSnapshot};
    use crate::Result;
    use crate::query_budget::QueryBudget;

    /// See the module doc.
    #[derive(Debug, Clone, Default)]
    pub struct FakeIndex {
        /// The full lexical order; `None` = [`Searched::Unavailable`].
        pub lexical: Option<Vec<TextHit>>,
        /// The full fuzzy order; `None` = [`Searched::Unavailable`].
        pub fuzzy: Option<Vec<TextHit>>,
    }

    impl TextIndex for FakeIndex {
        fn search(
            &self,
            _at: &TextSnapshot<'_>,
            query: &TextQuery<'_>,
            _budget: &QueryBudget,
        ) -> Result<Searched> {
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
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion
mod tests {
    use super::fake::FakeIndex;
    use super::{MatchMode, Searched, SeedFilter, TextHit, TextIndex, TextQuery, TextSnapshot};
    use crate::RecallQuery;
    use crate::query_budget::QueryBudget;

    fn hit(rid: i64) -> TextHit {
        TextHit { rid, score: 1.0 }
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

    fn search(idx: &dyn TextIndex, q: &TextQuery<'_>) -> Searched {
        let at = TextSnapshot {
            generation: 0,
            kbx: std::path::Path::new(""),
            tiers: &[],
            sediment: &[],
        };
        idx.search(&at, q, &QueryBudget::new(u64::MAX, None))
            .unwrap()
    }

    /// Unavailable stays distinct from an empty answer, per mode.
    #[test]
    fn unavailable_is_never_an_empty_page() {
        let q = RecallQuery::default();
        let idx = FakeIndex {
            lexical: Some(vec![]),
            fuzzy: None,
        };
        assert_eq!(
            search(&idx, &query(&q, MatchMode::Lexical, 0)),
            Searched::Hits {
                hits: vec![],
                exhausted: true
            }
        );
        assert_eq!(
            search(&idx, &query(&q, MatchMode::Fuzzy, 0)),
            Searched::Unavailable
        );
    }

    /// Continuation pages through the full order, through a reference or an `Arc` (the blanket
    /// impls forward).
    #[test]
    fn skip_continues_through_a_reference_or_an_arc() {
        let q = RecallQuery::default();
        let idx = std::sync::Arc::new(FakeIndex {
            lexical: Some(vec![hit(1), hit(2), hit(3)]),
            fuzzy: None,
        });
        let page = Searched::Hits {
            hits: vec![hit(3)],
            exhausted: true,
        };
        assert_eq!(search(&idx, &query(&q, MatchMode::Lexical, 2)), page);
        let by_ref: &dyn TextIndex = &*idx;
        assert_eq!(search(&by_ref, &query(&q, MatchMode::Lexical, 2)), page);
    }
}
