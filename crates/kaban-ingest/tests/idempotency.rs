//! Idempotency of the real pipeline (docs-ingest-plan.md DI1.5, DI1.7; docs-ingest-spec D5, §5):
//! `Ingestor` + `DocumentPreparer` over a scratch tree and an in-memory store. A second run on
//! an unchanged tree writes nothing (no node, edge, changes or audit row; the report says so) and
//! leaves `stats` byte-identical; over the whole fixture tree every table's row count is
//! unchanged.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::real::{Rig, rig, rig_on_disk};
use kaban_core::{AuditStore, MemoryStore, NodeId};
use kaban_policy::Policy;
use std::fs;

fn new_rig(name: &str) -> Rig {
    rig("kaban-ingest-idem", name)
}

/// Row counts of every table a run can touch: nodes and edges (`stats`), changes, audit.
#[derive(Debug, PartialEq, Eq)]
struct Rows {
    nodes: u64,
    edges: u64,
    outbox: usize,
    audit: u64,
}

fn rows(rig: &Rig) -> Rows {
    let stats = rig.store.stats(None).unwrap();
    Rows {
        nodes: stats["nodes"].as_u64().unwrap(),
        edges: stats["edges"].as_u64().unwrap(),
        outbox: rig.outbox_len(),
        audit: rig.store.audit_verify().unwrap().entries,
    }
}

#[test]
fn a_second_run_on_an_unchanged_tree_writes_nothing() {
    second_run_on_an_unchanged_tree_writes_nothing(&new_rig("unchanged"), false);
}

/// Spec 05 SS-17 (T7 review t7-5): the same run over a store that encodes bodies. The chunk
/// bodies are stored as `BLOB`s, the second run is skipped by the same content key, and the
/// identities the first run minted (chunk ids, content hashes, revisions) are the ones read
/// back: re-ingestion neither rewrites nor re-identifies a compressed body.
#[test]
fn a_second_run_on_an_unchanged_tree_writes_nothing_over_encoded_bodies() {
    let rig = rig_on_disk("kaban-ingest-idem", "unchanged-encoded", Policy::empty());
    rig.compress();
    second_run_on_an_unchanged_tree_writes_nothing(&rig, true);
}

/// `(id, content_hash, rev)` of every chunk of `rel`, in the order they were written.
fn chunk_identities(rig: &Rig, rel: &str) -> Vec<(String, String, i64)> {
    rig.chunk_ids(rel)
        .iter()
        .map(|id| {
            let n = rig
                .store
                .get(&NodeId::parse(id).unwrap(), true)
                .unwrap()
                .unwrap();
            (n.id, n.content_hash, n.rev)
        })
        .collect()
}

fn second_run_on_an_unchanged_tree_writes_nothing(rig: &Rig, compressed: bool) {
    // Arrange
    fs::write(
        rig.root.join("a.md"),
        "---\ntitle: A\n---\n# One\n\nFirst paragraph with [[b]] and #tag.\n\n## Two\n\nSecond paragraph, \
         long enough for the body codec to encode it once the store compresses: the checkout keeps \
         discounts in `pricing.rs`, retries are idempotent, and the decision record explains why.\n",
    )
    .unwrap();
    fs::write(rig.root.join("b.md"), "# B\n\nPlain text.\n").unwrap();
    let first = rig.run();
    let rows_after_first = rig.outbox_len();
    let stats_after_first = rig.store.stats(None).unwrap();
    let identities_after_first = chunk_identities(rig, "a.md");
    if compressed {
        assert!(
            rig.encoded_bodies() > 0,
            "the chunk bodies are stored encoded"
        );
        let chunks: Vec<String> = rig
            .chunk_ids("a.md")
            .iter()
            .map(|id| {
                rig.store
                    .get(&NodeId::parse(id).unwrap(), true)
                    .unwrap()
                    .unwrap()
                    .text
            })
            .collect();
        assert!(
            chunks.iter().any(|t| t.contains("retries are idempotent")),
            "a chunk body reads back decoded: {chunks:?}"
        );
    }

    // Act
    let second = rig.run();

    // Assert
    assert_eq!(
        first.documents, 2,
        "first run commits both documents: {first:?}"
    );
    assert_eq!(
        chunk_identities(rig, "a.md"),
        identities_after_first,
        "the second run keeps every chunk identity"
    );
    assert!(rows_after_first > 0, "the first run writes changes rows");
    assert_eq!(
        second.documents, 0,
        "second run commits nothing: {second:?}"
    );
    // the walker's own skips (a file-backed rig's store files under the denied `store/**`,
    // whose text-index segment files multiply as the store commits) are counted on both runs;
    // the two documents join them on the second
    assert!(
        second.skipped >= first.skipped + 2,
        "both documents skipped by the content key: {first:?} then {second:?}"
    );
    assert_eq!(second.chunks_upserted, 0);
    assert_eq!(second.outbox_rows, 0);
    assert_eq!(rig.outbox_len(), rows_after_first, "no new changes row");
    assert_eq!(rig.store.stats(None).unwrap(), stats_after_first);
}

/// DI1.7: over the whole Markdown fixture tree (the five goldens plus a text file; wikilinks
/// to siblings and to files that are not there), the second full-tree run writes zero rows in
/// every table: nodes, edges, changes and audit are counted before and after.
#[test]
fn a_full_tree_second_run_writes_zero_rows_in_every_table() {
    // Arrange
    let rig = new_rig("full-tree");
    let fixtures = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    fs::create_dir_all(rig.root.join("docs")).unwrap();
    for dir in ["md", "md-golden"] {
        for entry in fs::read_dir(fixtures.join(dir)).unwrap() {
            let entry = entry.unwrap();
            fs::copy(entry.path(), rig.root.join("docs").join(entry.file_name())).unwrap();
        }
    }
    fs::write(rig.root.join("notes.txt"), "plain text note\nsecond line\n").unwrap();
    let first = rig.run();
    let before = rows(&rig);

    // Act
    let second = rig.run();

    // Assert
    assert_eq!(first.documents, 6, "{first:?}");
    // clean documents under the empty policy leave no audit row; the count is still compared
    assert!(
        before.nodes > 6 && before.edges > 0 && before.outbox > 0,
        "{before:?}"
    );
    assert_eq!(second.documents, 0, "{second:?}");
    assert_eq!(second.skipped, 6, "{second:?}");
    assert_eq!(
        (
            second.chunks_upserted,
            second.chunks_tombstoned,
            second.outbox_rows
        ),
        (0, 0, 0),
        "{second:?}"
    );
    assert_eq!(rows(&rig), before, "a table gained or lost rows");
}
