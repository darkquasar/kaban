//! The Tantivy [`TextIndex`] adapter (spec 05 SS-08): an index-only projection of the record
//! store's active rows beside the SQLite database, ontology-driven (the kinds' `fts_fields`,
//! `ident_fields` and `column_weights` shape what is indexed and how it ranks, never a kind
//! name), serving lexical and fuzzy seeds in the total order `(score desc, rid asc)` from a
//! snapshot whose searcher and watermark are bound under one lock.
//!
//! ```text
//!   +----------------+  IndexRow / Canonical  +---------------------+  TextIndex  +----------+
//!   | Rows (SQLite,  | =====================> | TantivyIndex        | <========= | recall   |
//!   | read-only conn)|  rebuild: full scan    | schema analyzers    |  search,   | (store)  |
//!   +----------------+                        | query reader payload|  pin       +----------+
//!                                             +---------------------+
//!                                                 | Served: one lock binds searcher + payload
//!                                                 v
//!                                             +---------------------+
//!                                             | <db stem>.index/    |  meta.json payload =
//!                                             |   tantivy/ (mmap)   |  {seq, lineage, versions}
//!                                             +---------------------+
//! ```
//!
//! The index is built by [`TextIndex::rebuild`] and kept current by the change-feed projector
//! ([`projector`], SS-09), both publishing through the [`writer`] whose directory lock is the
//! one fence; a purge's physical erasure (`purge`, SS-12) runs under the same lock. The
//! adapter never holds the store: it is built from the index directory, the ontology, the
//! recall configuration and a [`Rows`] implementation over the store's own read-only
//! connection (design amendment 1).

pub mod admission;
mod analyzers;
mod captured;
mod payload;
pub mod projector;
mod purge;
mod query;
mod reader;
mod schema;
mod writer;

use crate::payload::Payload;
use crate::query::Weights;
use crate::reader::{Served, Serving};
use crate::schema::{ACTIVE, Fields};
use crate::writer::Writer;
use kaban_core::config::Recall;
use kaban_core::text_index::{
    Canonical, IndexRow, PurgeOutcome, Rows, Searched, TextIndex, TextQuery, Visibility,
};
use kaban_core::{BackendSource, Change, ChangeEntity, Ontology, StoreError};
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use tantivy::directory::MmapDirectory;
use tantivy::{Index, TantivyDocument, TantivyError};

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
    /// The commit payload did not (de)serialize.
    #[error("payload: {0}")]
    Payload(#[from] serde_json::Error),
    /// Another process holds the index writer (the directory lock); nothing waits for it.
    #[error("the index writer is held by another process")]
    WriterHeld,
    /// An erasure holds the index directory under its exclusive maintenance condition
    /// (SS-12) for longer than an opener waits; the opener is refused, never unmarked.
    #[error("the index directory is held for exclusive maintenance by another process")]
    MaintenanceHeld,
    /// A hit without one of its fast-column values: the index was not built by this crate.
    #[error("the index holds a hit without its {0}")]
    Corrupt(&'static str),
    /// A captured index directory (a pack's, SS-19) is not one this build reads: its schema,
    /// its payload (absent) or the payload's layout and versions.
    #[error("the captured index is not one this build reads: {0}")]
    Captured(&'static str),
    /// The index installed beside a database (SS-13b) is one this build reads but not one it
    /// may serve there: another lineage or another projection than the database's.
    #[error("the installed index is not fit for this database: {0}")]
    Unfit(&'static str),
}

/// The adapter's result.
pub type Result<T> = std::result::Result<T, Error>;

impl From<Error> for StoreError {
    fn from(error: Error) -> Self {
        Self::Backend {
            source: BackendSource::new(error),
        }
    }
}

/// See the crate doc.
pub struct TantivyIndex {
    index: Index,
    fields: Fields,
    /// The ranking weights of the ontology and recall configuration the store serves under;
    /// replaced by [`Self::configure`] when the store binds a loaded ontology or a recall
    /// configuration after its open.
    weights: RwLock<Weights>,
    served: Served,
    rows: Box<dyn Rows>,
    /// The index directory (`None` in memory): the listing a purge checks its claim against.
    dir: Option<PathBuf>,
    /// This adapter's presence mark on the directory (`None` in memory), held for its
    /// lifetime: what an erasure's exclusivity is established against (SS-12).
    presence: Option<admission::Presence>,
    /// The database side of the exclusive maintenance condition (SS-12), when the composition
    /// root wired one; without it a purge never claims the index artefacts cleaned.
    database_hold: Option<Box<dyn admission::DatabaseHold>>,
}

impl std::fmt::Debug for TantivyIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TantivyIndex").finish_non_exhaustive()
    }
}

impl TantivyIndex {
    /// The index directory of the database file `db` ([`kaban_core::text_index::index_dir`]:
    /// `<parent>/<stem>.index/tantivy/`), the one directory a pack copies.
    #[must_use]
    pub fn index_dir(db: &Path) -> PathBuf {
        kaban_core::text_index::index_dir(db)
    }

    /// The directory this index lives in (`None` in memory).
    #[must_use]
    pub fn directory(&self) -> Option<&Path> {
        self.dir.as_deref()
    }

    /// Open the index in `dir` (created with the schema when absent; never rebuilt here) over
    /// `rows`, ranking with `ontology`'s weights and `recall`'s defaults. The adapter marks
    /// its presence on the directory for its lifetime ([`admission`], SS-12).
    ///
    /// # Errors
    /// The directory cannot be created or opened, or holds an index of another schema;
    /// [`Error::MaintenanceHeld`] when an erasure holds the directory past the wait.
    pub fn open(
        dir: &Path,
        ontology: &Ontology,
        recall: &Recall,
        rows: Box<dyn Rows>,
    ) -> Result<Self> {
        std::fs::create_dir_all(dir)?;
        let presence = admission::Presence::mark(dir)?;
        let (schema, fields) = schema::schema();
        let directory = MmapDirectory::open(dir).map_err(TantivyError::from)?;
        let index = Index::open_or_create(directory, schema)?;
        Self::over(
            index,
            fields,
            ontology,
            recall,
            rows,
            Some((dir.to_path_buf(), presence)),
        )
    }

    /// Open the index in `dir` as it exists, for a diagnosis (`kaban doctor`, SS-21): `None`
    /// when the directory holds no index (absent, or never committed), and nothing is created
    /// either way (T4 review t4-9); otherwise the adapter as [`Self::open`] gives it, its
    /// presence marked.
    ///
    /// # Errors
    /// The directory cannot be opened or holds an index of another schema;
    /// [`Error::MaintenanceHeld`] when an erasure holds the directory past the wait.
    pub fn open_existing(
        dir: &Path,
        ontology: &Ontology,
        recall: &Recall,
        rows: Box<dyn Rows>,
    ) -> Result<Option<Self>> {
        if !dir.is_dir() {
            return Ok(None);
        }
        let directory = MmapDirectory::open(dir).map_err(TantivyError::from)?;
        if !Index::exists(&directory).map_err(TantivyError::from)? {
            return Ok(None);
        }
        let presence = admission::Presence::mark(dir)?;
        let (schema, fields) = schema::schema();
        let index = Index::open(directory)?;
        if index.schema() != schema {
            return Err(TantivyError::SchemaError(
                "the index directory holds an index of another schema".into(),
            )
            .into());
        }
        Self::over(
            index,
            fields,
            ontology,
            recall,
            rows,
            Some((dir.to_path_buf(), presence)),
        )
        .map(Some)
    }

    /// Open the index directory a pack captured (`<pack>/index/tantivy`) for a `--pack` read
    /// (SS-19; T6 review t6-2): the directory as it stands, admitted as
    /// [`Self::admit_captured`] admits it, over `rows` (the pack's own immutable snapshot).
    /// Nothing is created, locked or marked: no presence mark and no directory of its own,
    /// since nothing may be written into an immutable pack, and no write reaches the adapter
    /// there (the pack's store refuses every write before any open).
    ///
    /// # Errors
    /// As [`Self::admit_captured`], and the reader failing to open.
    pub fn open_captured(
        dir: &Path,
        ontology: &Ontology,
        recall: &Recall,
        rows: Box<dyn Rows>,
    ) -> Result<Self> {
        let index = Index::open(captured::CapturedDirectory::open(dir)?)?;
        Self::admitted(&index)?;
        let (_, fields) = schema::schema();
        Self::over(index, fields, ontology, recall, rows, None)
    }

    /// Admit the index directory a pack captured (`<pack>/index/tantivy`, SS-19), reading
    /// only: the directory opens as a Tantivy index of this crate's schema, its last commit
    /// carries a payload whose layout and versions this build implements (lineage and
    /// projection are the manifest's to compare), and every segment the commit references
    /// opens, so every file it names is present and readable. Returns the payload as JSON for
    /// the manifest comparison. Nothing is created or written: no reader, no lock, no
    /// presence mark (T4 review t4-5).
    ///
    /// # Errors
    /// [`Error::Captured`] naming what is not an index of this build; Tantivy and i/o
    /// failures (a referenced file missing among them).
    pub fn admit_captured(dir: &Path) -> Result<serde_json::Value> {
        let directory = MmapDirectory::open(dir).map_err(TantivyError::from)?;
        let payload = Self::admitted(&Index::open(directory)?)?;
        Ok(serde_json::from_str(&payload.to_json()?)?)
    }

    /// Admit the index installed beside a database for the cutover decision (spec 05
    /// SS-13b; the store's `IndexAdmission`, reading only): `Ok(None)` when `dir` holds no
    /// index (the ordinary database: nothing is created), `Ok(Some(seq))` with the payload's
    /// watermark when the directory is admitted as [`Self::admit_captured`] admits a captured
    /// one and its payload is compatible with `at`'s identity (the lineage, the projection and
    /// the versions this build implements, never the instance: a fork keeps its parent's
    /// index), and `Err(class)` naming why not, a count name and never a user byte
    /// (`unreadable`, `schema`, `payload`, `versions`, `lineage`, `projection`; the cause is
    /// logged at debug). The watermark's position against the canonical head is the store's
    /// to judge: it reads `at` under the write lock it writes the marker under.
    ///
    /// # Errors
    /// The class of the refusal, never a failure: an index that cannot be judged is
    /// `unreadable`, and the store leaves FTS5 serving.
    pub fn admit_installed(
        dir: &Path,
        at: &Canonical,
    ) -> std::result::Result<Option<u64>, &'static str> {
        Self::installed(dir, at).map_err(|error| {
            let class = match &error {
                Error::Captured(class) | Error::Unfit(class) => class,
                Error::Payload(_) => "payload",
                _ => "unreadable",
            };
            tracing::debug!(%error, class, "the installed index is not admitted");
            class
        })
    }

    /// [`Self::admit_installed`] before its failures are named.
    fn installed(dir: &Path, at: &Canonical) -> Result<Option<u64>> {
        if !dir.is_dir() {
            return Ok(None);
        }
        let directory = MmapDirectory::open(dir).map_err(TantivyError::from)?;
        if !Index::exists(&directory).map_err(TantivyError::from)? {
            return Ok(None);
        }
        let payload = Self::admitted(&Index::open(directory)?)?;
        match payload.unfit(at) {
            Some(class) => Err(Error::Unfit(class)),
            None => Ok(Some(payload.seq)),
        }
    }

    /// `index` admitted as one this build reads, whole: this crate's schema, a payload on its
    /// last commit whose layout and versions this build implements, and every component the
    /// schema requires of every segment the commit references opening
    /// ([`Self::components_open`]: every file it names is present and readable).
    fn admitted(index: &Index) -> Result<Payload> {
        if index.schema() != schema::schema().0 {
            return Err(Error::Captured("schema"));
        }
        let payload = Payload::read(index)?.ok_or(Error::Captured("payload"))?;
        if !payload.implemented() {
            return Err(Error::Captured("versions"));
        }
        for segment in index.searchable_segments()? {
            Self::components_open(&tantivy::SegmentReader::open(&segment)?)?;
        }
        Ok(payload)
    }

    /// Every component the schema requires of an opened segment, opened (T5c review t5c-1).
    /// The segment open establishes the term dictionary, postings, fast, fieldnorm and
    /// delete files, but Tantivy 0.26.2 substitutes an empty positions table when `.pos`
    /// cannot open, so the inverted index of every indexed field is opened here too (its
    /// term dictionary, postings and, for a field that records them, positions: a phrase
    /// query would fail at recall otherwise), with the fast columns a hit is read from.
    fn components_open(reader: &tantivy::SegmentReader) -> Result<()> {
        for (field, entry) in reader.schema().fields() {
            if entry.is_indexed() {
                reader.inverted_index(field)?;
            }
        }
        let fast = reader.fast_fields();
        fast.i64(schema::RID)?;
        fast.i64(schema::REVISION)?;
        fast.str(schema::KIND)?.ok_or(TantivyError::SchemaError(
            "the kind column is missing".into(),
        ))?;
        Ok(())
    }

    /// An index in memory (tests): the same adapter without a directory.
    ///
    /// # Errors
    /// Tantivy cannot open a reader on it.
    pub fn in_ram(ontology: &Ontology, recall: &Recall, rows: Box<dyn Rows>) -> Result<Self> {
        let (schema, fields) = schema::schema();
        Self::over(
            Index::create_in_ram(schema),
            fields,
            ontology,
            recall,
            rows,
            None,
        )
    }

    fn over(
        index: Index,
        fields: Fields,
        ontology: &Ontology,
        recall: &Recall,
        rows: Box<dyn Rows>,
        dir: Option<(PathBuf, admission::Presence)>,
    ) -> Result<Self> {
        analyzers::register(&index);
        let served = Served::open(&index)?;
        let (dir, presence) = dir.unzip();
        Ok(Self {
            index,
            fields,
            weights: RwLock::new(Weights::of(ontology, recall)),
            served,
            rows,
            dir,
            presence,
            database_hold: None,
        })
    }

    /// Wire the database side of the exclusive maintenance condition (SS-12): the store's
    /// owner lock as the composition root hands it in. Without it a purge's index scope stays
    /// "outstanding or unknown".
    #[must_use]
    pub fn with_database_hold(mut self, hold: Box<dyn admission::DatabaseHold>) -> Self {
        self.database_hold = Some(hold);
        self
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

    /// The snapshot to serve now: the bound pair when its payload is compatible with the
    /// canonical identity and within the canonical history, else `None` (unavailable; design
    /// amendment 3). An index never committed with a payload is unavailable too.
    fn serving(&self) -> kaban_core::Result<Option<Serving>> {
        let serving = self.served.snapshot(&self.index)?;
        let Some(payload) = &serving.payload else {
            return Ok(None);
        };
        let at = self.rows.canonical()?;
        Ok((payload.compatible(&at) && payload.within(&at)).then_some(serving))
    }

    /// A search on `serving`. A segment this crate did not write (a hit without its `rid`,
    /// or without the column) makes the index unavailable, with the cause logged: nothing is
    /// served under a made-up retrieval key, and a rebuild replaces the segment.
    fn search_in(&self, serving: &Serving, q: &TextQuery<'_>) -> kaban_core::Result<Searched> {
        let weights = self
            .weights
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match query::run(&self.fields, &weights, &serving.searcher, q) {
            Ok(searched) => Ok(searched),
            Err(error @ Error::Corrupt(_)) => {
                tracing::error!(%error, "text index unavailable until rebuilt");
                Ok(Searched::Unavailable)
            }
            Err(error) => Err(error.into()),
        }
    }

    /// The writer, with the directory lock as the fence: a lock held elsewhere is
    /// [`Error::WriterHeld`] at once, never a wait.
    pub(crate) fn acquire_writer(&self) -> Result<Writer> {
        Writer::acquire(&self.index, self.fields)
    }

    /// The record store's view this index projects.
    pub(crate) fn rows(&self) -> &dyn Rows {
        self.rows.as_ref()
    }

    /// The payload of the last commit: the watermark and the resume point.
    pub(crate) fn payload(&self) -> Result<Option<Payload>> {
        Payload::read(&self.index)
    }

    /// Bind the serving pair to the current commit (after one made by this process).
    pub(crate) fn refresh(&self) -> Result<()> {
        self.served.snapshot(&self.index)?;
        Ok(())
    }

    /// Replace the whole index with the active rows at one canonical position and publish it
    /// with that position's payload in one commit through `writer`; the serving pair follows.
    pub(crate) fn rebuild_with(&self, writer: &mut Writer) -> kaban_core::Result<Canonical> {
        let at = writer.rebuild(self.rows.as_ref())?;
        self.refresh()?;
        Ok(at)
    }

    /// Stage one chunk of committed changes through `writer` (SS-09), nothing committed: every
    /// node id of the chunk is deleted (deduplicated before the rows are fetched), its current
    /// active row (read now; a state at least as new as the change) re-added, edge, event and
    /// assertion rows consumed with nothing to apply. Returns the chunk's last sequence and
    /// the decoded indexed bytes staged.
    pub(crate) fn stage(
        &self,
        writer: &mut Writer,
        changes: &[Change],
    ) -> kaban_core::Result<Chunk> {
        let seq = changes.last().map_or(0, |c| c.seq);
        let mut ids: Vec<String> = changes
            .iter()
            .filter(|c| c.entity == ChangeEntity::Node)
            .map(|c| c.id.clone())
            .collect();
        ids.sort_unstable();
        ids.dedup();
        let rows = self.rows.current(&ids)?;
        writer.apply(&ids, &rows)?;
        let bytes = rows.iter().map(decoded_bytes).sum();
        Ok(Chunk { seq, bytes })
    }
}

/// What one staged chunk amounts to.
pub(crate) struct Chunk {
    /// The chunk's last sequence.
    pub(crate) seq: u64,
    /// The decoded indexed fields of its rows.
    pub(crate) bytes: usize,
}

/// The decoded indexed fields of `row`: what the publication's byte bound counts.
fn decoded_bytes(row: &IndexRow) -> usize {
    let strings = |v: &[String]| v.iter().map(String::len).sum::<usize>();
    row.text.len()
        + row.summary.len()
        + row.stored_summary.as_ref().map_or(0, String::len)
        + strings(&row.tags)
        + strings(&row.ident_values)
        + strings(&row.anchor_keys)
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

/// The watermark of a served payload as the seam reports it: `Through(seq)` of the served
/// payload (a payload at 0, an index built over an empty history, is a watermark like any
/// other), `Unavailable` when nothing is served (T6 review t6-5).
fn through(serving: Option<&Serving>) -> Visibility {
    serving
        .and_then(|s| s.payload.as_ref())
        .map_or(Visibility::Unavailable, |p| {
            Visibility::Through(i64::try_from(p.seq).unwrap_or(i64::MAX))
        })
}

impl TextIndex for TantivyIndex {
    fn search(&self, query: &TextQuery<'_>) -> kaban_core::Result<Searched> {
        match self.serving()? {
            Some(serving) => self.search_in(&serving, query),
            None => Ok(Searched::Unavailable),
        }
    }

    /// `Through(seq)` of the compatible served payload; `Unavailable` when nothing is served
    /// (nothing is claimed visible).
    fn visible(&self) -> kaban_core::Result<Visibility> {
        Ok(through(self.serving()?.as_ref()))
    }

    /// One page published by a transient writer (a caller outside the projector's session);
    /// the resume point and the acknowledgement are the projector's.
    fn project(&self, changes: &[Change]) -> kaban_core::Result<()> {
        let at = self.rows.canonical()?;
        let mut writer = self.acquire_writer()?;
        let chunk = self.stage(&mut writer, changes)?;
        if !changes.is_empty() {
            writer.commit(&projector::payload_at(chunk.seq, &at))?;
        }
        writer.finish()?;
        self.refresh()?;
        Ok(())
    }

    fn rebuild(&self) -> kaban_core::Result<()> {
        let mut writer = self.acquire_writer()?;
        self.rebuild_with(&mut writer)?;
        writer.finish()?;
        Ok(())
    }

    /// The erasure of [`purge`] under the writer lock, taken before anything is inspected (a
    /// first build holding it has staged what a missing payload cannot vouch for). The
    /// outcome claims only what this process observed: a held lock, a file a mapping kept, a
    /// segment file still listed, or the files gone; and in a directory other processes may
    /// have open (shared mode B, SS-21) their mappings are outstanding or unknown, so even
    /// then the claim is qualified and `cleaned` stays false, unless the exclusive maintenance
    /// condition ([`admission`]) was established before the erasure and held through its
    /// listing: then no other process holds the database and no other reader, in any process,
    /// holds the index, and the gone files are cleaned. Without a payload, segment files are an
    /// unbuilt or interrupted build's and are never erasure: the erase collects them and the
    /// claim stays qualified; only an index directory without segment files holds nothing to
    /// erase.
    fn purge(&self, rids: &[i64]) -> kaban_core::Result<PurgeOutcome> {
        let not_cleaned = |detail: String| PurgeOutcome {
            cleaned: false,
            detail: Some(detail),
        };
        let mut writer = match self.acquire_writer() {
            Ok(writer) => writer,
            Err(Error::WriterHeld) => {
                return Ok(not_cleaned("writer held by another process".into()));
            }
            Err(e) => return Err(e.into()),
        };
        let unbuilt = self.payload()?.is_none();
        if unbuilt && !purge::artefacts_present(self.dir.as_deref(), &self.index) {
            writer.finish()?;
            return Ok(PurgeOutcome {
                cleaned: true,
                detail: None,
            });
        }
        let exclusive = admission::establish(
            &self.index,
            self.presence.as_ref(),
            self.database_hold.as_deref(),
        )?;
        let erased = purge::erase(self, &mut writer, rids)?;
        writer.finish()?;
        let outcome = if unbuilt {
            let now = if purge::artefacts_present(self.dir.as_deref(), &self.index) {
                "still present after the erase"
            } else {
                "removed by the erase"
            };
            not_cleaned(format!(
                "unbuilt or interrupted index artefacts present: {now}"
            ))
        } else if erased.failed_to_delete > 0 {
            not_cleaned(format!("files still mapped: {}", erased.failed_to_delete))
        } else if !erased.files_gone {
            not_cleaned(format!(
                "segment files still present: {} affected",
                erased.affected
            ))
        } else if exclusive.is_some() {
            PurgeOutcome {
                cleaned: true,
                detail: Some(format!(
                    "index files removed ({} segments merged away); exclusive maintenance: no other process held the database or the index",
                    erased.affected
                )),
            }
        } else {
            not_cleaned(format!(
                "index files removed ({} segments merged away); other readers outstanding or unknown",
                erased.affected
            ))
        };
        drop(exclusive);
        Ok(outcome)
    }

    /// One snapshot for every page of a recall: the pinned view searches the searcher taken
    /// here and reports its watermark, whatever commits land meanwhile.
    fn pin(&self) -> kaban_core::Result<Box<dyn TextIndex + '_>> {
        Ok(Box::new(Pinned {
            index: self,
            serving: self.serving()?,
        }))
    }
}

/// See [`TantivyIndex::pin`]: `None` pins an unavailable index.
struct Pinned<'a> {
    index: &'a TantivyIndex,
    serving: Option<Serving>,
}

impl TextIndex for Pinned<'_> {
    fn search(&self, query: &TextQuery<'_>) -> kaban_core::Result<Searched> {
        match &self.serving {
            Some(serving) => self.index.search_in(serving, query),
            None => Ok(Searched::Unavailable),
        }
    }

    fn visible(&self) -> kaban_core::Result<Visibility> {
        Ok(through(self.serving.as_ref()))
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

    /// Already pinned: the same generation again.
    fn pin(&self) -> kaban_core::Result<Box<dyn TextIndex + '_>> {
        Ok(Box::new(Pinned {
            index: self.index,
            serving: self.serving.clone(),
        }))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion
mod tests {
    use super::TantivyIndex;
    use crate::payload::Payload;
    use crate::schema::ACTIVE;
    use kaban_core::RecallQuery;
    use kaban_core::config::Recall;
    use kaban_core::ident::IDENT_TERMS_VERSION;
    use kaban_core::text_index::{
        Canonical, IndexIdentity, IndexRow, MatchMode, Replay, Rows, Searched, SeedFilter,
        TextIndex, TextQuery,
    };
    use tantivy::TantivyDocument;

    /// A record store with no rows at one fixed canonical position.
    struct Fixed(Canonical);

    impl Rows for Fixed {
        fn current(&self, _ids: &[String]) -> kaban_core::Result<Vec<IndexRow>> {
            Ok(vec![])
        }
        fn active(
            &self,
            _visit: &mut dyn FnMut(IndexRow) -> kaban_core::Result<()>,
        ) -> kaban_core::Result<Canonical> {
            Ok(self.0.clone())
        }
        fn canonical(&self) -> kaban_core::Result<Canonical> {
            Ok(self.0.clone())
        }
        fn replay(&self, _after: u64, _limit: usize) -> kaban_core::Result<Replay> {
            Ok(Replay {
                oldest: 0,
                at: self.0.clone(),
                changes: vec![],
            })
        }
    }

    /// A segment this crate never writes (T1 review t1-3): a document published under a
    /// compatible payload but without its `rid` column. The search and the pinned search
    /// report `Unavailable`; no hit is served under a made-up retrieval key.
    #[test]
    fn a_segment_without_the_rid_column_is_unavailable_never_a_made_up_hit() {
        let at = Canonical {
            seq: 1,
            identity: IndexIdentity {
                instance_id: "db-1".into(),
                pack_lineage_id: "lineage-1".into(),
                projection_signature: "sig-1".into(),
                ident_terms_version: IDENT_TERMS_VERSION.into(),
            },
        };
        let index = TantivyIndex::in_ram(
            &kaban_core::shipped::resolved().unwrap(),
            &Recall::default(),
            Box::new(Fixed(at.clone())),
        )
        .unwrap();
        let mut writer = index.index.writer_with_num_threads(1, 32 << 20).unwrap();
        let mut doc = TantivyDocument::default();
        doc.add_text(index.fields.id, "id-1");
        doc.add_i64(index.fields.revision, 1);
        doc.add_text(index.fields.kind, "Fact");
        doc.add_text(index.fields.workspace, "default");
        doc.add_text(index.fields.lifecycle, ACTIVE);
        doc.add_text(index.fields.content[0], "quokka by the shore");
        writer.add_document(doc).unwrap();
        let mut prepared = writer.prepare_commit().unwrap();
        prepared.set_payload(&Payload::new(&at).to_json().unwrap());
        prepared.commit().unwrap();
        drop(writer);

        let q = RecallQuery {
            query: "quokka".into(),
            ..Default::default()
        };
        let query = TextQuery {
            text: &q.query,
            mode: MatchMode::Lexical,
            filter: SeedFilter::of(&q),
            limit: 10,
            sufficient: 10,
            skip: 0,
        };
        assert_eq!(index.search(&query).unwrap(), Searched::Unavailable);
        assert_eq!(
            index.pin().unwrap().search(&query).unwrap(),
            Searched::Unavailable
        );
    }

    /// The segment files of the index (every managed file but Tantivy's own bookkeeping).
    fn segment_files(index: &TantivyIndex) -> Vec<String> {
        index
            .index
            .directory()
            .list_managed_files()
            .into_iter()
            .map(|p| p.to_string_lossy().into_owned())
            .filter(|name| name != "meta.json" && !name.starts_with('.'))
            .collect()
    }

    /// "No payload" is not erasure (T2 review t2-2, SS-12). A first build holds the lock
    /// with a canary staged: the purge is refused, never claimed. The build dies before any
    /// commit, leaving its segment files behind without a payload: the purge takes the lock
    /// first, reports the artefacts as not cleaned, and its erase removes them; only an index
    /// directory without segment files is cleaned.
    #[test]
    fn a_purge_of_an_unbuilt_index_with_staged_artefacts_is_not_cleaned_until_they_are_gone() {
        let at = Canonical {
            seq: 1,
            identity: IndexIdentity {
                instance_id: "db-1".into(),
                pack_lineage_id: "lineage-1".into(),
                projection_signature: "sig-1".into(),
                ident_terms_version: IDENT_TERMS_VERSION.into(),
            },
        };
        let index = TantivyIndex::in_ram(
            &kaban_core::shipped::resolved().unwrap(),
            &Recall::default(),
            Box::new(Fixed(at)),
        )
        .unwrap();
        let canary = IndexRow {
            id: "id-1".into(),
            rid: 1,
            revision: 1,
            kind: "Fact".into(),
            workspace_id: "default".into(),
            tags: vec![],
            text: "the zqxcanary7731 note".into(),
            summary: String::new(),
            stored_summary: None,
            ident_values: vec![],
            anchor_keys: vec![],
        };
        // the first build, staged and holding the lock
        let building = index.acquire_writer().unwrap();
        building.apply(&[], std::slice::from_ref(&canary)).unwrap();
        let refused = index.purge(&[1]).unwrap();
        assert!(!refused.cleaned, "{refused:?}");
        assert_eq!(
            refused.detail.as_deref(),
            Some("writer held by another process")
        );
        // the build dies before its commit: segment files, no payload
        drop(building);
        assert!(index.payload().unwrap().is_none());
        assert!(!segment_files(&index).is_empty(), "the staged files remain");

        let outcome = index.purge(&[1]).unwrap();

        assert!(!outcome.cleaned, "{outcome:?}");
        assert!(
            outcome
                .detail
                .as_deref()
                .is_some_and(|d| d.starts_with("unbuilt or interrupted index artefacts present")),
            "{outcome:?}"
        );
        assert_eq!(
            segment_files(&index),
            Vec::<String>::new(),
            "the erase removed the staged files"
        );
        assert_eq!(
            index.purge(&[1]).unwrap(),
            kaban_core::text_index::PurgeOutcome {
                cleaned: true,
                detail: None
            },
            "an index directory without segment files is cleaned"
        );
    }
}
