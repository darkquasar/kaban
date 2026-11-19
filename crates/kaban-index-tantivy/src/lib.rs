//! The Tantivy [`TextIndex`] adapter over the catalog (spec 11 T7): text packs and tiers of
//! the store's catalog searched in the total order `(score desc, rid asc)`, ontology-driven
//! (the kinds' `fts_fields`, `ident_fields` and `column_weights` shape what is indexed and how
//! it ranks, never a kind name). A commit's pack is built by [`segment`], a seal merges packs
//! with [`merge`], and a search reads one pinned [`TextSnapshot`] of tiers and packs.
//!
//! ```text
//!   +----------------+  IndexRow    +---------------------+  TextIndex  +----------+
//!   | TextClass      | ===========> | segment / merge     |             | recall   |
//!   | (store catalog)|  stage, seal | (packs: kbx_dir)    |             | (store)  |
//!   +----------------+              +---------------------+             +----------+
//!                                                                          |
//!   +----------------+  TextSnapshot  +---------------------+  search       |
//!   | Pin (catalog)  | =============> | TantivyIndex        | <=============+
//!   +----------------+  tiers, packs  | schema, weights     |
//!                                     +---------------------+
//! ```
//!
//! The adapter holds no storage of its own: it is built from the ontology and the recall
//! configuration, and every search is charged to the caller's query budget.

mod analyzers;
mod kbx_dir;
mod query;
mod schema;
mod writer;

pub use crate::kbx_dir::has_segment;
use crate::query::Weights;
use crate::schema::{ACTIVE, Fields};
pub use crate::writer::{merge, segment};
use kaban_core::config::Recall;
use kaban_core::query_budget::{BudgetExceeded, QueryBudget};
use kaban_core::text_index::{IndexRow, Searched, TextIndex, TextQuery, TextSnapshot};
use kaban_core::{BackendSource, Ontology, StoreError};
use std::path::Path;
use std::sync::RwLock;
use tantivy::directory::{MmapDirectory, OwnedBytes};
use tantivy::{
    Directory as _, DocAddress, HasLen as _, IndexReader, ReloadPolicy, TantivyDocument,
    TantivyError,
};

/// Why an adapter operation failed. Converted into [`StoreError::Backend`] at the seam, so a
/// caller sees the content-free rendering and an operator downcasts to this.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Tantivy failed.
    #[error("tantivy: {0}")]
    Tantivy(#[from] TantivyError),
    /// A file operation failed.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// A pack's index meta did not serialize.
    #[error("index meta: {0}")]
    Meta(#[from] serde_json::Error),
    /// A pack or a hit without one of its parts: the bytes were not written by this crate.
    #[error("the index holds a hit without its {0}")]
    Corrupt(&'static str),
    /// The query budget refused a charge, or passed its deadline or was cancelled (PC-17).
    #[error(transparent)]
    Budget(#[from] BudgetExceeded),
}

/// The adapter's result.
pub type Result<T> = std::result::Result<T, Error>;

impl From<Error> for StoreError {
    fn from(error: Error) -> Self {
        match error {
            // the budget's own refusal, so a caller (compaction's skip) recognizes it
            Error::Budget(refused) => Self::BudgetExceeded(refused),
            error => Self::Backend {
                source: BackendSource::new(error),
            },
        }
    }
}

/// See the crate doc.
pub struct TantivyIndex {
    fields: Fields,
    /// The ranking weights of the ontology and recall configuration the store serves under;
    /// replaced by [`Self::configure`] when the store binds a loaded ontology or a recall
    /// configuration after its open.
    weights: RwLock<Weights>,
}

impl std::fmt::Debug for TantivyIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TantivyIndex").finish_non_exhaustive()
    }
}

impl TantivyIndex {
    /// The adapter ranking with `ontology`'s weights and `recall`'s defaults.
    #[must_use]
    pub fn new(ontology: &Ontology, recall: &Recall) -> Self {
        Self {
            fields: schema::schema().1,
            weights: RwLock::new(Weights::of(ontology, recall)),
        }
    }

    /// Rank with `ontology`'s per-kind column weights and `recall`'s defaults from the next
    /// search on: the store binds its loaded ontology and recall configuration after the open
    /// that built this adapter (spec 05 OWN-M). Nothing indexed changes.
    pub fn configure(&self, ontology: &Ontology, recall: &Recall) {
        *self
            .weights
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Weights::of(ontology, recall);
    }

    /// A search on the catalog snapshot `at` (spec 11 T7): its tiers mapped from the
    /// container (the caller's pin protects them while the search runs; nothing here outlives
    /// it, and the mapping is uncharged), its sediment segments copied, the documents a newer
    /// tombstone hides left out of every page. Every allocation of the search is reserved on
    /// `budget` before it is made, in the order the bytes appear: the copies, the decoded
    /// tombstones, the hidden documents, the collector and the page (PC-17).
    fn search_snapshot(
        &self,
        at: &TextSnapshot<'_>,
        q: &TextQuery<'_>,
        budget: &QueryBudget,
    ) -> Result<Searched> {
        budget.check()?;
        let mut charge = budget.reserve(0)?;
        let mut packs: Vec<OwnedBytes> =
            kbx_dir::charged_vec(&mut charge, at.tiers.len() + at.sediment.len())?;
        if !at.tiers.is_empty() {
            let (Some(dir), Some(name)) = (at.kbx.parent(), at.kbx.file_name()) else {
                return Err(Error::Corrupt("container path"));
            };
            let container = MmapDirectory::open(dir)
                .map_err(TantivyError::from)?
                .open_read(Path::new(name))
                .map_err(TantivyError::from)?;
            for span in at.tiers {
                let start = usize::try_from(span.offset).map_err(|_| Error::Corrupt("tier"))?;
                let end = start
                    .checked_add(usize::try_from(span.len).map_err(|_| Error::Corrupt("tier"))?)
                    .filter(|end| *end <= container.len())
                    .ok_or(Error::Corrupt("tier"))?;
                packs.push(container.slice(start..end).read_bytes()?);
            }
        }
        for segment in at.sediment {
            kbx_dir::charge(&mut charge, segment.len())?;
            packs.push(OwnedBytes::new(segment.to_vec()));
        }
        if packs.is_empty() {
            return Ok(Searched::Hits {
                hits: vec![],
                exhausted: true,
            });
        }
        let opened = kbx_dir::open(&packs, budget)?;
        let reader: IndexReader = opened
            .index
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()?;
        let searcher = reader.searcher();
        budget.check()?;
        let (per_segment, found_with) = opened.hidden(&searcher, &self.fields, &[], budget)?;
        let mut hidden: Vec<DocAddress> =
            kbx_dir::charged_vec(&mut charge, per_segment.iter().map(Vec::len).sum())?;
        for (ord, docs) in per_segment.iter().enumerate() {
            let ord = u32::try_from(ord).map_err(|_| Error::Corrupt("segment ordinal"))?;
            hidden.extend(docs.iter().map(|doc| DocAddress::new(ord, *doc)));
        }
        drop((per_segment, found_with));
        hidden.sort_unstable();
        let weights = self
            .weights
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let out = query::run_hiding(&self.fields, &weights, &searcher, q, &hidden, budget)?;
        budget.check()?;
        Ok(out)
    }
}

/// One row as a document: the keys, the filter fields, the four content fields and the fuzzy
/// copy. The `ident_terms` field takes the five inputs of the identifier derivation (text, the
/// stored summary, the tags, the `ident_fields` values, the anchor keys), each expanded by its
/// analyzer; the FTS5 `tags` column is the JSON array, whose tokens are the tags' own.
fn document(fields: &Fields, row: &IndexRow) -> TantivyDocument {
    let mut doc = TantivyDocument::default();
    doc.add_text(fields.id, &row.id);
    doc.add_i64(fields.rid, row.rid);
    doc.add_i64(fields.revision, row.revision);
    doc.add_text(fields.kind, &row.kind);
    doc.add_text(fields.workspace, &row.workspace_id);
    doc.add_text(fields.lifecycle, ACTIVE);
    for tag in &row.tags {
        doc.add_text(fields.tag, tag);
    }
    let [text, summary, tags, ident] = fields.content;
    doc.add_text(text, &row.text);
    doc.add_text(summary, &row.summary);
    doc.add_text(tags, row.tags.join(" "));
    for input in std::iter::once(&row.text)
        .chain(row.stored_summary.as_ref())
        .chain(&row.tags)
        .chain(&row.ident_values)
        .chain(&row.anchor_keys)
    {
        doc.add_text(ident, input);
    }
    doc.add_text(fields.fuzzy, &row.text);
    if let Some(stored) = &row.stored_summary {
        doc.add_text(fields.fuzzy, stored);
    }
    doc
}

impl TextIndex for TantivyIndex {
    /// A segment this crate did not write makes the snapshot unavailable, with the cause
    /// logged: nothing is served under a made-up retrieval key.
    fn search(
        &self,
        at: &TextSnapshot<'_>,
        query: &TextQuery<'_>,
        budget: &QueryBudget,
    ) -> kaban_core::Result<Searched> {
        match self.search_snapshot(at, query, budget) {
            Ok(searched) => Ok(searched),
            Err(error @ Error::Corrupt(_)) => {
                tracing::error!(%error, "the catalog's text snapshot is unavailable");
                Ok(Searched::Unavailable)
            }
            Err(error) => Err(error.into()),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion
mod tests {
    use super::TantivyIndex;
    use kaban_core::config::Recall;
    use kaban_core::query_budget::{BudgetExceeded, Limit, QueryBudget};
    use kaban_core::text_index::{
        ExtentSpan, IndexRow, MatchMode, SeedFilter, TextIndex, TextQuery, TextSnapshot,
    };
    use kaban_core::{RecallQuery, StoreError};
    use tantivy::directory::OwnedBytes;

    /// One active row of the default workspace.
    fn row(rid: i64, text: &str) -> IndexRow {
        IndexRow {
            id: format!("id-{rid}"),
            rid,
            revision: 1,
            kind: "Fact".into(),
            workspace_id: "default".into(),
            tags: vec![],
            text: text.into(),
            summary: String::new(),
            stored_summary: None,
            ident_values: vec![],
            anchor_keys: vec![],
        }
    }

    /// The adapter under the shipped ontology.
    fn adapter() -> TantivyIndex {
        TantivyIndex::new(
            &kaban_core::shipped::resolved().unwrap(),
            &Recall::default(),
        )
    }

    /// A lexical query for `text` over the whole snapshot.
    fn lexical<'a>(text: &'a str, q: &'a RecallQuery) -> TextQuery<'a> {
        TextQuery {
            text,
            mode: MatchMode::Lexical,
            filter: SeedFilter::of(q),
            limit: 10,
            sufficient: 10,
            skip: 0,
        }
    }

    /// PC-17 (T7 midpoint review, memory): the tiers' mapping is the caller's and uncharged,
    /// but their decoded tombstones are the search's own allocation. A ceiling below the size
    /// of the pinned tier's tombstone set refuses the search with the budget's memory error,
    /// and every charge is given back.
    #[test]
    fn a_search_below_the_size_of_the_tiers_tombstones_is_refused_by_the_budget() {
        // Arrange: one tier of one document and 10,000 tombstones (80,000 bytes decoded),
        // written into a container file
        let tombstones: Vec<i64> = (1..=10_000).collect();
        let unlimited = QueryBudget::new(u64::MAX, None);
        let pack =
            crate::segment(&[row(1, "quokka by the shore")], &tombstones, &unlimited).unwrap();
        let dir = std::env::temp_dir().join(format!("kaban-t7b-tier-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let kbx = dir.join("kaban.kbx");
        std::fs::write(&kbx, &pack).unwrap();
        let tiers = [ExtentSpan {
            offset: 0,
            len: pack.len() as u64,
        }];
        let at = TextSnapshot {
            generation: 1,
            kbx: &kbx,
            tiers: &tiers,
            sediment: &[],
        };
        let budget = QueryBudget::new(40_000, None);
        let q = RecallQuery::default();

        // Act
        let searched = adapter().search(&at, &lexical("quokka", &q), &budget);

        // Assert
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(
            matches!(
                searched,
                Err(StoreError::BudgetExceeded(BudgetExceeded {
                    limit: Limit::Memory,
                    ..
                }))
            ),
            "expected the budget's memory refusal, got {searched:?}"
        );
        assert_eq!(budget.live_bytes(), 0, "every charge was given back");
    }

    /// The rids of the hits for "quokka" over `sediment` (oldest first), sorted, repeats kept.
    fn rids(sediment: &[&[u8]]) -> Vec<i64> {
        let at = TextSnapshot {
            generation: 1,
            kbx: std::path::Path::new("unused.kbx"),
            tiers: &[],
            sediment,
        };
        let q = RecallQuery::default();
        let budget = QueryBudget::new(u64::MAX, None);
        let searched = adapter()
            .search(&at, &lexical("quokka", &q), &budget)
            .unwrap();
        let kaban_core::text_index::Searched::Hits { hits, .. } = searched else {
            panic!("unavailable");
        };
        let mut rids: Vec<i64> = hits.iter().map(|h| h.rid).collect();
        rids.sort_unstable();
        rids
    }

    /// The tombstones `pack` carries.
    fn tombstones(pack: &[u8]) -> Vec<i64> {
        let budget = QueryBudget::new(u64::MAX, None);
        let opened = crate::kbx_dir::open(&[OwnedBytes::new(pack.to_vec())], &budget).unwrap();
        opened
            .all_tombstones(&mut budget.reserve(0).unwrap())
            .unwrap()
    }

    /// A fold (spec 11 T7): a merged run of tiers keeps a tombstone only while an older tier
    /// of the chain holds a document of its rid, so an update folded above its old version
    /// never brings the old one back, and a fold of the whole chain keeps no tombstone.
    #[test]
    fn a_fold_keeps_only_the_tombstones_an_older_tier_still_needs() {
        // Arrange: rid 1's first version, its update (a tombstone and the new version), rid 2
        let unlimited = QueryBudget::new(u64::MAX, None);
        let old = crate::segment(&[row(1, "quokka first version")], &[], &unlimited).unwrap();
        let update = crate::segment(&[row(1, "quokka second version")], &[1], &unlimited).unwrap();
        let other = crate::segment(&[row(2, "quokka neighbour")], &[], &unlimited).unwrap();
        let bytes = |p: &[u8]| OwnedBytes::new(p.to_vec());

        // Act: the newest two folded above the old tier, and the whole chain folded
        let run = crate::merge(
            &[bytes(&update), bytes(&other)],
            &[],
            Some(&[bytes(&old)]),
            &unlimited,
        )
        .unwrap()
        .unwrap();
        let whole = crate::merge(
            &[bytes(&old), bytes(&update), bytes(&other)],
            &[],
            Some(&[]),
            &unlimited,
        )
        .unwrap()
        .unwrap();

        // Assert
        assert_eq!(
            rids(&[&old, &run]),
            vec![1, 2],
            "the old version stays hidden"
        );
        assert_eq!(tombstones(&run), vec![1]);
        assert_eq!(rids(&[&whole]), vec![1, 2]);
        assert_eq!(tombstones(&whole), Vec::<i64>::new());
    }

    /// A purge (spec 11 T7): the rewritten tier holds neither the purged rid's documents nor
    /// its tombstones, and a tier without a document of it is left as it is.
    #[test]
    fn a_purged_tier_loses_the_rids_documents_and_tombstones() {
        let unlimited = QueryBudget::new(u64::MAX, None);
        let tier = crate::segment(
            &[row(1, "quokka doomed"), row(2, "quokka kept")],
            &[1, 2],
            &unlimited,
        )
        .unwrap();
        let tier = OwnedBytes::new(tier);

        let purged = crate::merge(std::slice::from_ref(&tier), &[1], None, &unlimited)
            .unwrap()
            .unwrap();
        let untouched = crate::merge(&[tier], &[3], None, &unlimited).unwrap();

        assert_eq!(rids(&[&purged]), vec![2]);
        assert_eq!(tombstones(&purged), vec![2]);
        assert_eq!(untouched, None);
    }

    /// PC-17 (T7 midpoint review, cancellation): a search over many packs checks the budget
    /// per pack, so a cancellation at the first pack stops it there with the budget's error.
    #[test]
    fn a_search_cancelled_at_its_first_pack_visits_no_other_pack() {
        // Arrange: 50 pending packs of one document each and no tombstone (nothing to charge
        // per pack); the budget is cancelled while the search visits its first pack
        let unlimited = QueryBudget::new(u64::MAX, None);
        let packs: Vec<Vec<u8>> = (1..=50)
            .map(|rid| crate::segment(&[row(rid, "quokka by the shore")], &[], &unlimited).unwrap())
            .collect();
        let sediment: Vec<&[u8]> = packs.iter().map(Vec::as_slice).collect();
        let at = TextSnapshot {
            generation: 1,
            kbx: std::path::Path::new("unused.kbx"),
            tiers: &[],
            sediment: &sediment,
        };
        let budget = QueryBudget::new(u64::MAX, None);
        let visits = std::rc::Rc::new(std::cell::Cell::new(0_usize));
        let (seen, cancel) = (visits.clone(), budget.clone());
        crate::kbx_dir::ON_PACK.set(Some(Box::new(move || {
            seen.set(seen.get() + 1);
            cancel.cancel();
        })));
        let q = RecallQuery::default();

        // Act
        let searched = adapter().search(&at, &lexical("quokka", &q), &budget);

        // Assert
        crate::kbx_dir::ON_PACK.set(None);
        assert!(
            matches!(
                searched,
                Err(StoreError::BudgetExceeded(BudgetExceeded {
                    limit: Limit::Cancelled,
                    ..
                }))
            ),
            "expected the budget's cancellation, got {searched:?}"
        );
        assert_eq!(visits.get(), 1, "packs visited after the cancellation");
    }
}
