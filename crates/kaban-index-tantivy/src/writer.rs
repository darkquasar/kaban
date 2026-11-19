//! The index writer (spec 05 SS-09): Tantivy's exclusive directory lock, taken here and held
//! for the [`Writer`]'s lifetime, is the one publication fence. A lock held elsewhere (another
//! process, a stopped one included, or another writer in this process) is [`Error::WriterHeld`]
//! at once, never a wait. Every commit carries its [`Payload`] (`PreparedCommit::set_payload`),
//! so the segment set and the watermark publish together or not at all. The lock is released
//! only after the writer's background merges joined (`wait_merging_threads`, on every exit:
//! [`Writer::finish`], an error path, a session's drop alike): in Tantivy 0.26 a dropped
//! `IndexWriter` kills its updater and joins the indexing workers but not a merge already past
//! its liveness check in `save_metas`, which could otherwise rewrite `meta.json` after a
//! successor acquired the lock (T2 review t2-1).

use crate::payload::Payload;
use crate::schema::Fields;
use crate::{Error, Result, document, kbx_dir};
use kaban_core::query_budget::QueryBudget;
use kaban_core::text_index::{Canonical, IndexRow, Rows};
use tantivy::directory::error::LockError;
use tantivy::directory::{GarbageCollectionResult, OwnedBytes, RamDirectory};
use tantivy::fastfield::AliveBitSet;
use tantivy::index::SegmentId;
use tantivy::indexer::merge_filtered_segments;
use tantivy::schema::Term;
use tantivy::{
    DocId, Index, IndexReader, IndexSettings, IndexWriter, ReloadPolicy, SingleSegmentIndexWriter,
    TantivyError,
};

/// The writer's memory budget: one indexing thread, so a rebuild is one segment per commit.
const WRITER_BUDGET_BYTES: usize = 32 << 20;

/// See the module doc.
pub(crate) struct Writer {
    /// `Some` until [`Writer::finish`] or the drop joins the merges and releases the lock.
    inner: Option<IndexWriter>,
    fields: Fields,
}

impl Writer {
    /// Take the directory lock, or [`Error::WriterHeld`] when another writer holds it.
    pub(crate) fn acquire(index: &Index, fields: Fields) -> Result<Self> {
        match index.writer_with_num_threads(1, WRITER_BUDGET_BYTES) {
            Err(TantivyError::LockFailure(LockError::LockBusy, _)) => Err(Error::WriterHeld),
            Err(e) => Err(e.into()),
            Ok(inner) => Ok(Self {
                inner: Some(inner),
                fields,
            }),
        }
    }

    #[allow(clippy::expect_used)] // the one invariant of the field: taken by finish or drop only
    fn inner(&self) -> &IndexWriter {
        self.inner
            .as_ref()
            .expect("the index writer is taken only by finish or drop")
    }

    #[allow(clippy::expect_used)] // see `inner`
    fn inner_mut(&mut self) -> &mut IndexWriter {
        self.inner
            .as_mut()
            .expect("the index writer is taken only by finish or drop")
    }

    /// Stage one page: every `id` is deleted (a replacement, a tombstone, a purge and a hard
    /// delete alike), then the current active `rows` are added. Idempotent: a page staged
    /// twice yields the same index.
    pub(crate) fn apply(&self, ids: &[String], rows: &[IndexRow]) -> Result<()> {
        for id in ids {
            self.inner()
                .delete_term(Term::from_field_text(self.fields.id, id));
        }
        for row in rows {
            self.inner().add_document(document(&self.fields, row))?;
        }
        Ok(())
    }

    /// Stage the deletion of every `rid` (a purge, SS-12): the row's postings in every
    /// segment, whatever revision wrote them.
    pub(crate) fn delete_rids(&self, rids: &[i64]) {
        for rid in rids {
            self.inner()
                .delete_term(Term::from_field_i64(self.fields.rid, *rid));
        }
    }

    /// Merge `segments` into one (only alive documents are written) and wait for it.
    pub(crate) fn merge(&mut self, segments: &[SegmentId]) -> Result<()> {
        self.inner_mut().merge(segments).wait()?;
        Ok(())
    }

    /// Delete the files no committed segment uses any more and report what could not go.
    pub(crate) fn garbage_collect(&self) -> Result<GarbageCollectionResult> {
        Ok(self.inner().garbage_collect_files().wait()?)
    }

    /// Publish what is staged under `payload`: the one publication step.
    pub(crate) fn commit(&mut self, payload: &Payload) -> Result<()> {
        let json = payload.to_json()?;
        let mut prepared = self.inner_mut().prepare_commit()?;
        prepared.set_payload(&json);
        prepared.commit()?;
        Ok(())
    }

    /// Replace the whole index with the active rows at one canonical position and publish it
    /// with that position's payload in one commit. Nothing staged before (a chunk of the
    /// publication that found the rebuild necessary) survives into it: the writer is rolled
    /// back to its last commit first, which keeps the directory lock (the fence) and discards
    /// the queued and buffered operations that `delete_all_documents` leaves in place (T8d
    /// review t8d-1: it clears the registered segments and rewinds the opstamp, not the
    /// indexing queue).
    pub(crate) fn rebuild(&mut self, rows: &dyn Rows) -> kaban_core::Result<Canonical> {
        self.inner_mut().rollback().map_err(Error::from)?;
        self.inner().delete_all_documents().map_err(Error::from)?;
        let fields = self.fields;
        let inner = self.inner();
        let at = rows.active(&mut |row| {
            inner
                .add_document(document(&fields, &row))
                .map_err(Error::from)?;
            Ok(())
        })?;
        self.commit(&Payload::new(&at))?;
        Ok(at)
    }

    /// Join the background merges of this writer and release the lock, reporting a merge
    /// that failed. The drop does the same (logging instead), so no exit path releases the
    /// lock while a merge of this writer could still publish.
    pub(crate) fn finish(mut self) -> Result<()> {
        if let Some(inner) = self.inner.take() {
            inner.wait_merging_threads()?;
        }
        Ok(())
    }

    /// Leave every segment as it is for the rest of this writer's life (a pack capture,
    /// SS-18): no background merge starts after its commits, so nothing rewrites `meta.json`
    /// or collects segment files while the directory is copied (T4 review t4-4). A writer
    /// freshly acquired has no merge in flight, so the file set is frozen from here on.
    pub(crate) fn freeze_merges(&self) {
        self.inner()
            .set_merge_policy(Box::new(tantivy::merge_policy::NoMergePolicy));
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.take()
            && let Err(e) = inner.wait_merging_threads()
        {
            tracing::warn!(error = %e, "a merge of the index writer failed while it was released");
        }
    }
}

/// The RAM arena the writer of one staged segment is given (spec 11 T7).
const SEGMENT_ARENA_BYTES: usize = 4 << 20;

/// One commit's text as one pack (`crate::kbx_dir`): the identity `tombstones` of every rid
/// the commit changed and one segment of `rows`, their current active rows, built in RAM.
/// The writer's arena (only when there are rows to index) and the encoded pack are charged
/// to `budget` before they are allocated; the rows are the caller's, charged by it.
///
/// # Errors
/// The budget's refusal; Tantivy failures.
pub fn segment(
    rows: &[IndexRow],
    tombstones: &[i64],
    budget: &QueryBudget,
) -> kaban_core::Result<Vec<u8>> {
    Ok(build_segment(rows, tombstones, budget)?)
}

fn build_segment(rows: &[IndexRow], tombstones: &[i64], budget: &QueryBudget) -> Result<Vec<u8>> {
    let ram = RamDirectory::create();
    if rows.is_empty() {
        return kbx_dir::encode(tombstones, None, &ram, budget);
    }
    let _arena = budget.reserve(SEGMENT_ARENA_BYTES as u64)?;
    let (schema, fields) = crate::schema::schema();
    let index = Index::create(ram.clone(), schema, IndexSettings::default())?;
    crate::analyzers::register(&index);
    let mut writer: SingleSegmentIndexWriter =
        SingleSegmentIndexWriter::new(index, SEGMENT_ARENA_BYTES)?;
    for row in rows {
        budget.check()?;
        writer.add_document(document(&fields, row))?;
    }
    budget.check()?;
    let index = writer.finalize()?;
    budget.check()?;
    let segment = index.searchable_segments()?.into_iter().next();
    let pack = kbx_dir::encode(tombstones, segment.as_ref(), &ram, budget)?;
    budget.check()?;
    Ok(pack)
}

/// Merge `packs` (oldest first) into one pack: every document a newer pack's tombstone hides,
/// or whose rid is in `drop`, is left out, and the union of the tombstones is carried on to
/// hide the documents of older packs. `None` when one pack would come out unchanged. The
/// packs are the caller's, charged by it; what the merge allocates from them (the decoded
/// tombstones, the hidden documents, the alive sets, the merged segment, which holds no more
/// than the packs' own bytes, and the encoded pack) is charged to `budget` before it is
/// allocated.
///
/// # Errors
/// The budget's refusal; a malformed pack; Tantivy failures.
pub fn merge(
    packs: &[OwnedBytes],
    drop: &[i64],
    budget: &QueryBudget,
) -> kaban_core::Result<Option<Vec<u8>>> {
    Ok(merge_packs(packs, drop, budget)?)
}

fn merge_packs(
    packs: &[OwnedBytes],
    drop: &[i64],
    budget: &QueryBudget,
) -> Result<Option<Vec<u8>>> {
    let opened = kbx_dir::open(packs, budget)?;
    let (_, fields) = crate::schema::schema();
    let reader: IndexReader = opened
        .index
        .reader_builder()
        .reload_policy(ReloadPolicy::Manual)
        .try_into()?;
    let searcher = reader.searcher();
    budget.check()?;
    let (hidden, _found_with) = opened.hidden(&searcher, &fields, drop, budget)?;
    if packs.len() == 1 && hidden.iter().all(Vec::is_empty) {
        return Ok(None);
    }
    let mut charge = budget.reserve(0)?;
    let tombstones = opened.all_tombstones(&mut charge)?;
    let readers = searcher.segment_readers();
    let metas = opened.index.searchable_segment_metas()?;
    let mut segments = kbx_dir::charged_vec(&mut charge, readers.len())?;
    let mut alive = kbx_dir::charged_vec(&mut charge, readers.len())?;
    let mut alive_docs: u64 = 0;
    for (reader, dead) in readers.iter().zip(&hidden) {
        budget.check()?;
        let max_doc = reader.max_doc();
        alive_docs += u64::from(max_doc) - dead.len() as u64;
        let meta = metas
            .iter()
            .find(|m| m.id() == reader.segment_id())
            .ok_or(Error::Corrupt("text pack"))?;
        segments.push(opened.index.segment(meta.clone()));
        alive.push(Some(alive_set(max_doc, dead, &mut charge)?));
    }
    let ram = RamDirectory::create();
    if alive_docs == 0 {
        return kbx_dir::encode(&tombstones, None, &ram, budget).map(Some);
    }
    // the merged segment is written into RAM: no more than the bytes of the packs it reads
    kbx_dir::charge(&mut charge, packs.iter().map(OwnedBytes::len).sum())?;
    budget.check()?;
    let merged = merge_filtered_segments(&segments, IndexSettings::default(), alive, ram.clone())?;
    budget.check()?;
    let segment = merged.searchable_segments()?.into_iter().next();
    let pack = kbx_dir::encode(&tombstones, segment.as_ref(), &ram, budget)?;
    budget.check()?;
    Ok(Some(pack))
}

/// The alive set of a segment of `max_doc` documents without `dead`, in Tantivy's file layout
/// (the bound, then one little-endian word per 64 documents), charged on `charge` first.
fn alive_set(
    max_doc: u32,
    dead: &[DocId],
    charge: &mut kaban_core::query_budget::BudgetLease,
) -> Result<AliveBitSet> {
    let words = max_doc.div_ceil(64) as usize;
    let mut bits: Vec<u64> = kbx_dir::charged_vec(charge, words)?;
    bits.resize(words, u64::MAX);
    if let Some(last) = bits.last_mut()
        && !max_doc.is_multiple_of(64)
    {
        *last = (1 << (max_doc % 64)) - 1;
    }
    for doc in dead {
        bits[(doc / 64) as usize] &= !(1 << (doc % 64));
    }
    let mut bytes: Vec<u8> = kbx_dir::charged_vec(charge, 4 + words * 8)?;
    bytes.extend(max_doc.to_le_bytes());
    for word in bits {
        bytes.extend(word.to_le_bytes());
    }
    Ok(AliveBitSet::open(OwnedBytes::new(bytes)))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion
mod tests {
    use super::Writer;
    use crate::payload::Payload;
    use crate::schema;
    use kaban_core::text_index::{Canonical, IndexIdentity, IndexRow};
    use tantivy::Index;

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

    /// T2 review t2-1 (SS-09 "no former holder publishes after a successor acquires the
    /// lock"): a merge in flight when the writer is released is joined before the lock goes.
    /// The successor acquires the lock only once the merged segment set is the committed one
    /// (one segment), and the metadata does not change under it afterwards.
    #[test]
    fn a_merge_in_flight_is_joined_before_the_lock_is_released() {
        let (schema, fields) = schema::schema();
        let index = Index::create_in_ram(schema);
        crate::analyzers::register(&index);
        let at = Canonical {
            seq: 1,
            identity: IndexIdentity {
                instance_id: "db".into(),
                pack_lineage_id: "L".into(),
                projection_signature: "sig".into(),
                ident_terms_version: "2".into(),
            },
        };
        let mut writer = Writer::acquire(&index, fields).unwrap();
        for segment in 0..6 {
            let rows: Vec<IndexRow> = (0..1500)
                .map(|i| {
                    row(
                        segment * 1500 + i,
                        "quokka by the shore under the trees at the ferry",
                    )
                })
                .collect();
            writer.apply(&[], &rows).unwrap();
            writer.commit(&Payload::new(&at)).unwrap();
        }
        let segments = index.searchable_segment_ids().unwrap();
        assert_eq!(segments.len(), 6);
        // the merge starts and is not waited for; the writer is released at once
        let merging = writer.inner_mut().merge(&segments);
        drop(writer);

        let successor = Writer::acquire(&index, fields).unwrap();

        let at_acquisition = index.load_metas().unwrap().segments.len();
        assert_eq!(
            at_acquisition, 1,
            "the merge published before the lock was released"
        );
        drop(merging);
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert_eq!(
            index.load_metas().unwrap().segments.len(),
            1,
            "nothing changed under the successor"
        );
        successor.finish().unwrap();
    }
}
