//! The text packs of the catalog (spec 11 T7): [`segment`] builds one commit's pack in RAM and
//! [`merge`] seals pending packs into one tier, each charged to its budget before it allocates.

use crate::schema::Fields;
use crate::{Error, Result, document, kbx_dir};
use kaban_core::query_budget::QueryBudget;
use kaban_core::text_index::IndexRow;
use tantivy::directory::{OwnedBytes, RamDirectory};
use tantivy::fastfield::AliveBitSet;
use tantivy::indexer::merge_filtered_segments;
use tantivy::{
    DocId, Index, IndexReader, IndexSettings, ReloadPolicy, SingleSegmentIndexWriter, Term,
};

/// The RAM arena the writer of one staged segment is given (spec 11 T7): Tantivy sizes its
/// term table from it.
const SEGMENT_ARENA_BYTES: usize = 4 << 20;
/// What a writer holds before its first document grows it: Tantivy's arena pages and term
/// table for this schema (measured: 10 MiB at the 4 MiB arena above).
const WRITER_BASE_BYTES: u64 = 12 << 20;
/// What a writer is reserved per byte of indexed input ([`crate::indexed_bytes`]) before it
/// runs (measured: under 1 for prose, about 3 for words no other row holds; the growth past the
/// reservation is charged per document).
const WRITER_BYTES_PER_INPUT_BYTE: u64 = 1;

#[cfg(test)]
thread_local! {
    /// The documents [`segment`] handed its writer: the tests count them.
    static DOCS_ADDED: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// One commit's text as one pack (`crate::kbx_dir`): the identity `tombstones` of every rid
/// the commit changed and one segment of `rows`, their current active rows, built in RAM.
/// The writer's memory (only when there are rows to index) is reserved from the rows before
/// it runs, [`WRITER_BASE_BYTES`] plus [`WRITER_BYTES_PER_INPUT_BYTE`] per indexed byte, and
/// any growth the writer reports past that is charged after each document, so the charge
/// stays at least what the writer holds; the segment it writes is no larger than that. The
/// encoded pack is charged before it is allocated; the rows are the caller's, charged by it.
///
/// # Errors
/// The budget's refusal (rows too large for it are refused before the writer runs); Tantivy
/// failures.
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
    let input = rows.iter().map(crate::indexed_bytes).sum::<u64>();
    let mut held = budget.reserve(
        input
            .saturating_mul(WRITER_BYTES_PER_INPUT_BYTE)
            .saturating_add(WRITER_BASE_BYTES),
    )?;
    let (schema, fields) = crate::schema::schema();
    let index = Index::create(ram.clone(), schema, IndexSettings::default())?;
    crate::analyzers::register(&index);
    let mut writer: SingleSegmentIndexWriter =
        SingleSegmentIndexWriter::new(index, SEGMENT_ARENA_BYTES)?;
    for row in rows {
        budget.check()?;
        #[cfg(test)]
        DOCS_ADDED.set(DOCS_ADDED.get() + 1);
        writer.add_document(document(&fields, row))?;
        let used = u64::try_from(writer.mem_usage()).unwrap_or(u64::MAX);
        if let Some(over) = used.checked_sub(held.bytes()).filter(|over| *over > 0) {
            kbx_dir::charge(&mut held, usize::try_from(over).unwrap_or(usize::MAX))?;
        }
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
/// or whose rid is in `drop` (a purge), is left out, and the union of the tombstones but
/// `drop`'s is carried on to hide the documents of older packs; with `older` (a fold: the
/// packs older than `packs` in the chain), only the tombstones of a rid some document of
/// `older` holds are carried. `None` when one pack holds no document to leave out. The packs
/// are the caller's, charged by it; what the merge allocates from them (the decoded
/// tombstones, the hidden documents, the alive sets, the merged segment, which holds no more
/// than the packs' own bytes, and the encoded pack) is charged to `budget` before it is
/// allocated.
///
/// # Errors
/// The budget's refusal; a malformed pack; Tantivy failures.
pub fn merge(
    packs: &[OwnedBytes],
    drop: &[i64],
    older: Option<&[OwnedBytes]>,
    budget: &QueryBudget,
) -> kaban_core::Result<Option<Vec<u8>>> {
    Ok(merge_packs(packs, drop, older, budget)?)
}

fn merge_packs(
    packs: &[OwnedBytes],
    drop: &[i64],
    older: Option<&[OwnedBytes]>,
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
    let mut tombstones = opened.all_tombstones(&mut charge)?;
    tombstones.retain(|rid| !drop.contains(rid));
    if let Some(older) = older {
        let held = held_rids(older, &tombstones, &fields, budget, &mut charge)?;
        tombstones.retain(|rid| held.binary_search(rid).is_ok());
    }
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

/// The rids among `rids` (sorted) that some document of `packs` holds, sorted, charged on
/// `charge` first: the tombstones a fold must keep to hide those documents.
fn held_rids(
    packs: &[OwnedBytes],
    rids: &[i64],
    fields: &Fields,
    budget: &QueryBudget,
    charge: &mut kaban_core::query_budget::BudgetLease,
) -> Result<Vec<i64>> {
    if packs.is_empty() || rids.is_empty() {
        return Ok(Vec::new());
    }
    let opened = kbx_dir::open(packs, budget)?;
    let reader: IndexReader = opened
        .index
        .reader_builder()
        .reload_policy(ReloadPolicy::Manual)
        .try_into()?;
    let searcher = reader.searcher();
    let mut held = kbx_dir::charged_vec(charge, rids.len())?;
    for (i, rid) in rids.iter().enumerate() {
        if i % kbx_dir::TOMBSTONE_BATCH == 0 {
            budget.check()?;
        }
        if searcher.doc_freq(&Term::from_field_i64(fields.rid, *rid))? > 0 {
            held.push(*rid);
        }
    }
    Ok(held)
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
    use super::DOCS_ADDED;
    use kaban_core::StoreError;
    use kaban_core::query_budget::{BudgetExceeded, Limit, QueryBudget};
    use kaban_core::text_index::IndexRow;
    use std::fmt::Write as _;

    /// An active row of about `bytes` bytes of words no other row holds.
    fn large_row(rid: i64, bytes: usize) -> IndexRow {
        let mut text = String::new();
        let mut state = u64::try_from(rid)
            .unwrap()
            .wrapping_mul(0x9E37_79B9_7F4A_7C15)
            | 1;
        while text.len() < bytes {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            write!(text, "w{:012x} ", state & 0xFFFF_FFFF_FFFF).unwrap();
        }
        IndexRow {
            id: format!("id-{rid}"),
            rid,
            revision: 1,
            kind: "Fact".into(),
            workspace_id: "default".into(),
            tags: vec![],
            text,
            summary: String::new(),
            stored_summary: None,
            ident_values: vec![],
            anchor_keys: vec![],
        }
    }

    /// PC-17 (T7 end review M1): the writer's memory is reserved from the rows it indexes
    /// before it runs, so rows whose indexed size exceeds the ceiling are refused with the
    /// budget's memory error before any document reaches the writer.
    #[test]
    fn rows_whose_indexed_size_exceeds_the_budget_are_refused_before_the_writer_runs() {
        // Arrange: three rows of 1 MiB of text each (indexed three times over: 9 MiB) under an
        // 8 MiB ceiling, above the fixed 4 MiB arena
        let rows: Vec<IndexRow> = (1..=3).map(|rid| large_row(rid, 1 << 20)).collect();
        let budget = QueryBudget::new(8 << 20, None);
        DOCS_ADDED.set(0);

        // Act
        let pack = crate::segment(&rows, &[], &budget);

        // Assert
        assert!(
            matches!(
                pack,
                Err(StoreError::BudgetExceeded(BudgetExceeded {
                    limit: Limit::Memory,
                    ..
                }))
            ),
            "expected the budget's memory refusal, got {:?}",
            pack.map(|p| p.len())
        );
        assert_eq!(DOCS_ADDED.get(), 0, "documents the writer indexed");
        assert_eq!(budget.live_bytes(), 0, "every charge was given back");
    }
}
