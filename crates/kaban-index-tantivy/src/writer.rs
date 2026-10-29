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
use crate::{Error, Result, document};
use kaban_core::text_index::{Canonical, IndexRow, Rows};
use tantivy::directory::GarbageCollectionResult;
use tantivy::directory::error::LockError;
use tantivy::index::SegmentId;
use tantivy::schema::Term;
use tantivy::{Index, IndexWriter, TantivyError};

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
