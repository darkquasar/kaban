//! Physical erasure after a purge (spec 05 SS-12, design-3, amendment 5). The purge row already
//! removed the record from the index through the projector (a deletion by id); what is left is
//! the postings and positions the row's revisions wrote into segment files. Under the writer
//! lock (so no staged projection commits meanwhile): delete by `rid`, commit under the
//! unchanged payload, find the segments whose term dictionary still names a purged `rid`
//! (`doc_freq` reads the dictionary, which deletions never touch, so segments holding older
//! revisions are found too), merge them into one (a merge writes alive documents only),
//! replace the serving snapshot whatever the payload says, collect the files, and check the
//! directory listing: the claim for the index artefacts is what this process observed, never
//! an inference from a GC result or about another process's mappings.

use crate::writer::Writer;
use crate::{Result, TantivyIndex};
use std::collections::HashSet;
use std::path::Path;
use tantivy::index::SegmentId;
use tantivy::schema::Term;
use tantivy::{Index, SegmentReader};

/// What the erasure observed.
pub(crate) struct Erased {
    /// Segments whose dictionary named a purged `rid` (merged away).
    pub(crate) affected: usize,
    /// Files the garbage collection could not delete (a mapping still held).
    pub(crate) failed_to_delete: usize,
    /// No file of an affected segment is in the directory listing any more.
    pub(crate) files_gone: bool,
}

/// The steps of the module doc, under `writer`'s lock, for an index whose last commit is
/// `payload`.
pub(crate) fn erase(index: &TantivyIndex, writer: &mut Writer, rids: &[i64]) -> Result<Erased> {
    let payload = index.payload()?;
    writer.delete_rids(rids);
    if let Some(payload) = &payload {
        writer.commit(payload)?;
    }
    let affected = affected_segments(&index.index, index.fields.rid, rids)?;
    if !affected.is_empty() {
        writer.merge(&affected)?;
    }
    // amendment 5: the serving pair follows even though the payload did not change
    index.served.reload(&index.index)?;
    let collected = writer.garbage_collect()?;
    let files_gone = files_gone(index.dir.as_deref(), &index.index, &affected);
    Ok(Erased {
        affected: affected.len(),
        failed_to_delete: collected.failed_to_delete_files.len(),
        files_gone,
    })
}

/// The searchable segments whose `rid` dictionary names any of `rids`, through short-lived
/// segment readers (no searcher is held across the merge).
fn affected_segments(
    index: &Index,
    rid_field: tantivy::schema::Field,
    rids: &[i64],
) -> Result<Vec<SegmentId>> {
    let mut affected = Vec::new();
    for segment in index.searchable_segments()? {
        let reader = SegmentReader::open(&segment)?;
        let inverted = reader.inverted_index(rid_field)?;
        let named = rids
            .iter()
            .map(|rid| inverted.doc_freq(&Term::from_field_i64(rid_field, *rid)))
            .collect::<std::io::Result<Vec<u32>>>()?
            .into_iter()
            .any(|freq| freq > 0);
        if named {
            affected.push(segment.id());
        }
    }
    Ok(affected)
}

/// The file names of the index directory (`dir` on disk; the managed set of an index in
/// memory); `None` when the listing failed.
fn listing(dir: Option<&Path>, index: &Index) -> Option<HashSet<String>> {
    match dir.map(std::fs::read_dir) {
        Some(Ok(entries)) => Some(
            entries
                .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
                .collect(),
        ),
        Some(Err(_)) => None,
        None => Some(
            index
                .directory()
                .list_managed_files()
                .into_iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect(),
        ),
    }
}

/// Whether no file of an `affected` segment remains in the directory listing.
fn files_gone(dir: Option<&Path>, index: &Index, affected: &[SegmentId]) -> bool {
    let Some(names) = listing(dir, index) else {
        return false;
    };
    affected.iter().all(|segment| {
        let prefix = segment.uuid_string();
        !names.iter().any(|name| name.starts_with(&prefix))
    })
}

/// Whether the directory listing holds any segment file (`<segment uuid>.<ext>`, whatever
/// wrote it: a committed segment, or one an interrupted or unbuilt writer left behind);
/// Tantivy's own bookkeeping (`meta.json`, the managed list, the lock files) does not count.
/// A listing that failed counts as present: nothing is claimed gone unseen.
pub(crate) fn artefacts_present(dir: Option<&Path>, index: &Index) -> bool {
    let Some(names) = listing(dir, index) else {
        return true;
    };
    names.iter().any(|name| is_segment_file(name))
}

/// A segment file name starts with the segment's 32 hex-digit uuid and a dot.
fn is_segment_file(name: &str) -> bool {
    let bytes = name.as_bytes();
    bytes.len() > 33 && bytes[32] == b'.' && bytes[..32].iter().all(u8::is_ascii_hexdigit)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion
mod tests {
    use crate::TantivyIndex;
    use crate::projector::Projector;
    use kaban_core::config::Recall;
    use kaban_core::text_index::{MatchMode, Searched, SeedFilter, TextIndex, TextQuery};
    use kaban_core::{
        Actor, ChangeEntity, ChangeLogStore, ForgetMode, MemoryStore, NewNode, NodeId, RecallQuery,
        RememberOutcome, WorkspaceId, WriteCtx,
    };
    use kaban_store_sqlite::SqliteStore;
    use kaban_store_sqlite::index_rows::IndexRows;
    use std::path::PathBuf;

    /// A private scratch directory removed on drop.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let d = std::env::temp_dir().join(format!("ss12-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&d);
            std::fs::create_dir_all(&d).unwrap();
            Self(d)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn ctx() -> WriteCtx {
        WriteCtx::new("v1", WorkspaceId::default(), Actor::Agent)
    }

    fn hits(index: &dyn TextIndex, text: &str) -> Vec<i64> {
        let q = RecallQuery {
            query: text.into(),
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
        match index.search(&query).unwrap() {
            Searched::Hits { hits, .. } => hits.into_iter().map(|h| h.rid).collect(),
            Searched::Unavailable => panic!("the index serves"),
        }
    }

    /// The race with a staged projection (SS-12): a writer holding the lock has staged, not
    /// committed, a page that would re-add the record's text. The purge under that lock is
    /// refused rather than claimed (`writer held by another process`, never cleaned), the
    /// purged content never returns through a revalidated recall whatever the index serves,
    /// and once the holder committed (the staged text landed) and dropped the lock, the retry
    /// scrubs it and the dictionary no longer names the record.
    #[test]
    fn a_staged_projection_defers_the_erasure_and_the_retry_scrubs_what_it_restored() {
        let s = Scratch::new("staged-race");
        let db = s.0.join("kaban.db");
        let store = SqliteStore::open(&db, &s.0.join("keys")).unwrap();
        let index = TantivyIndex::open(
            &db.with_file_name("adapter"),
            &kaban_core::shipped::resolved().unwrap(),
            &Recall::default(),
            Box::new(IndexRows::open(&db).unwrap()),
        )
        .unwrap();
        let RememberOutcome::Created(doomed) = store
            .remember(
                NewNode {
                    kind: "Fact".into(),
                    text: "the zqxcanary7731 note".into(),
                    ..Default::default()
                },
                &[],
                &ctx(),
            )
            .unwrap()
        else {
            panic!("fresh")
        };
        assert_eq!(
            Projector::new(&index, &store, &store, "test:first", 2)
                .catch_up()
                .unwrap()
                .halt,
            None
        );
        let rid = hits(&index, "zqxcanary7731")[0];
        // the staged page: the record's current (pre-purge) text, not committed
        let writer = index.acquire_writer().unwrap();
        let page = store.changes_read(0, 2048).unwrap();
        let ids: Vec<String> = page
            .iter()
            .filter(|c| c.entity == ChangeEntity::Node)
            .map(|c| c.id.clone())
            .collect();
        let rows = index.rows().current(&ids).unwrap();
        writer.apply(&ids, &rows).unwrap();

        store
            .forget(
                &NodeId::parse(&doomed.id).unwrap(),
                ForgetMode::Purge,
                &ctx(),
            )
            .unwrap();
        let refused = index.purge(&[rid]).unwrap();
        assert!(!refused.cleaned);
        assert_eq!(
            refused.detail.as_deref(),
            Some("writer held by another process")
        );
        // the holder commits the staged page (the pre-purge text lands) and releases the lock
        let mut writer = writer;
        let payload = index.payload().unwrap().unwrap();
        writer.commit(&payload).unwrap();
        writer.finish().unwrap();
        index.refresh().unwrap();
        assert_eq!(
            hits(&index, "zqxcanary7731"),
            vec![rid],
            "restored by the stale page"
        );
        let conn = rusqlite::Connection::open(&db).unwrap();
        let active: i64 = conn
            .query_row(
                "SELECT count(*) FROM node WHERE rid = ?1 AND lifecycle = 'active'",
                [rid],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(active, 0, "revalidation never returns it");

        let retried = index.purge(&[rid]).unwrap();
        assert!(!retried.cleaned, "qualified: other readers unknown");
        assert!(
            retried
                .detail
                .as_deref()
                .is_some_and(|d| d.starts_with("index files removed")),
            "{retried:?}"
        );
        assert_eq!(hits(&index, "zqxcanary7731"), Vec::<i64>::new());
        let dictionary_names = index
            .served
            .snapshot(&index.index)
            .unwrap()
            .searcher
            .segment_readers()
            .iter()
            .any(|segment| {
                segment
                    .inverted_index(index.fields.fuzzy)
                    .unwrap()
                    .terms()
                    .get(b"zqxcanary7731")
                    .unwrap()
                    .is_some()
            });
        assert!(!dictionary_names, "the retry scrubbed the postings");
    }
}
