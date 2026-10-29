//! The served snapshot (spec 05 SS-11, design-2): one lock binds the searcher and the payload
//! it serves. [`Served::snapshot`] reads the committed identity (the payload, the segment ids
//! and their delete opstamps: a merge or a purge publishes a new segment set under an
//! unchanged payload, and a reader keyed on the payload alone would keep the merged-away
//! segments and their mappings; astra, projection research), and when it differs from the
//! served one reloads the reader until two consecutive reads agree around the reload, so the
//! searcher loaded in between serves exactly that commit. The reader never reloads on its own
//! (`ReloadPolicy::Manual`): a reload happens here, under the lock, or nowhere.

use crate::Result;
use crate::payload::Payload;
use std::sync::Mutex;
use tantivy::index::SegmentId;
use tantivy::{Index, IndexReader, ReloadPolicy, Searcher};

/// See the module doc.
pub(crate) struct Served {
    reader: IndexReader,
    /// `None` until the first snapshot bound a pair.
    serving: Mutex<Option<Serving>>,
}

/// A searcher and the commit it serves: its payload (`None`: an index never committed with
/// one, which serves nothing) and its segment set.
#[derive(Clone)]
pub(crate) struct Serving {
    pub(crate) searcher: Searcher,
    pub(crate) payload: Option<Payload>,
    segments: Vec<(SegmentId, Option<u64>)>,
}

/// The committed identity a reload is keyed on, from one read of the metadata.
#[derive(PartialEq, Eq)]
struct Committed {
    payload: Option<Payload>,
    segments: Vec<(SegmentId, Option<u64>)>,
}

impl Committed {
    fn read(index: &Index) -> Result<Self> {
        let metas = index.load_metas()?;
        Ok(Self {
            payload: Payload::of(&metas)?,
            segments: metas
                .segments
                .iter()
                .map(|segment| (segment.id(), segment.delete_opstamp()))
                .collect(),
        })
    }
}

impl Served {
    /// A manual-reload reader on `index`; nothing is bound yet.
    pub(crate) fn open(index: &Index) -> Result<Self> {
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()?;
        Ok(Self {
            reader,
            serving: Mutex::new(None),
        })
    }

    /// Replace the served pair whatever the identity says (spec 05 SS-12, design amendment 5:
    /// a purge's erasure must not be served from a searcher bound before it).
    pub(crate) fn reload(&self, index: &Index) -> Result<Serving> {
        *self
            .serving
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        self.snapshot(index)
    }

    /// The pair for the index's current commit: the served one when its identity is still
    /// the committed one, else a reloaded searcher bound to the identity read around the
    /// reload.
    pub(crate) fn snapshot(&self, index: &Index) -> Result<Serving> {
        let mut serving = self
            .serving
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut committed = Committed::read(index)?;
        if let Some(current) = serving.as_ref()
            && current.payload == committed.payload
            && current.segments == committed.segments
        {
            return Ok(current.clone());
        }
        loop {
            self.reader.reload()?;
            let after = Committed::read(index)?;
            if after == committed {
                break;
            }
            // a commit landed between the read and the reload: read again around a new reload
            committed = after;
        }
        let bound = Serving {
            searcher: self.reader.searcher(),
            payload: committed.payload,
            segments: committed.segments,
        };
        *serving = Some(bound.clone());
        Ok(bound)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion
mod tests {
    use super::Served;
    use crate::payload::Payload;
    use crate::schema;
    use crate::writer::Writer;
    use kaban_core::text_index::{Canonical, IndexIdentity, IndexRow};
    use tantivy::Index;

    fn row(rid: i64) -> IndexRow {
        IndexRow {
            id: format!("id-{rid}"),
            rid,
            revision: 1,
            kind: "Fact".into(),
            workspace_id: "default".into(),
            tags: vec![],
            text: "quokka by the shore".into(),
            summary: String::new(),
            stored_summary: None,
            ident_values: vec![],
            anchor_keys: vec![],
        }
    }

    /// Lane T8d.2 (astra, metadata identity): a merge publishes a new segment set under the
    /// payload it found, so a reader keyed on payload equality would keep serving the merged
    /// segments and their mappings. The identity is the segment set and its deletes too: the
    /// snapshot after the merge is a reloaded searcher over the one merged segment, under the
    /// same payload.
    #[test]
    fn a_merge_under_an_unchanged_payload_reloads_the_served_searcher() {
        let (schema, fields) = schema::schema();
        let index = Index::create_in_ram(schema);
        crate::analyzers::register(&index);
        let at = Canonical {
            seq: 3,
            identity: IndexIdentity {
                instance_id: "db".into(),
                pack_lineage_id: "L".into(),
                projection_signature: "sig".into(),
                ident_terms_version: "2".into(),
            },
        };
        let mut writer = Writer::acquire(&index, fields).unwrap();
        for rid in 0..3 {
            writer.apply(&[], &[row(rid)]).unwrap();
            writer.commit(&Payload::new(&at)).unwrap();
        }
        let served = Served::open(&index).unwrap();
        let before = served.snapshot(&index).unwrap();
        assert_eq!(before.searcher.segment_readers().len(), 3);

        let segments = index.searchable_segment_ids().unwrap();
        writer.merge(&segments).unwrap();
        let after = served.snapshot(&index).unwrap();

        assert_eq!(after.payload, before.payload, "the payload did not change");
        assert_eq!(
            after.searcher.segment_readers().len(),
            1,
            "the merge-only publication reloaded the searcher"
        );
        writer.finish().unwrap();
    }
}
