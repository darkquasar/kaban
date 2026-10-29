//! Incremental behaviour of the real pipeline (docs-ingest-plan.md DI1.5, DI1.7; docs-ingest-spec
//! D3, D5, SR-6): editing one file touches only that document's nodes, deleting a paragraph
//! tombstones exactly its chunk and re-links `NEXT`, wikilinks resolve or create `missing`
//! placeholders that the real file later upgrades, tags become `TAGGED` edges, and prefix,
//! middle and suffix edits keep the ids of every unchanged chunk.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::real::{Rig, doc_id, rig};
use kaban_core::MemoryStore;
use kaban_ingest::index_status;
use std::fs;

const A: &str =
    "# One\n\nFirst paragraph.\n\n## Two\n\nSecond paragraph.\n\n## Three\n\nThird paragraph.\n";
const B: &str = "# B\n\nPlain text in b.\n";

fn new_rig(name: &str) -> Rig {
    rig("kaban-ingest-incr", name)
}

#[test]
fn editing_one_file_touches_only_that_documents_nodes() {
    // Arrange
    let rig = new_rig("one-file");
    fs::write(rig.root.join("a.md"), A).unwrap();
    fs::write(rig.root.join("b.md"), B).unwrap();
    rig.run();
    let seq = rig.last_seq();
    let b_before = rig.chunk_ids("b.md");

    // Act
    fs::write(
        rig.root.join("a.md"),
        A.replace("Third paragraph.", "Third, edited."),
    )
    .unwrap();
    let report = rig.run();

    // Assert
    assert_eq!(report.documents, 1, "{report:?}");
    assert_eq!(report.skipped, 1, "b.md is skipped by its content key");
    let touched = rig.touched_since(seq);
    let mut expected: Vec<String> = rig.chunk_ids("a.md");
    expected.extend(rig.section_ids("a.md"));
    expected.push(doc_id("a.md").to_string());
    for b in &b_before {
        assert!(!touched.contains(b), "b.md chunk {b} touched");
    }
    assert!(!touched.contains(&doc_id("b.md").to_string()));
    for id in &touched {
        // every touched node is a.md's (its document, a section or a chunk, or the
        // tombstoned old chunk)
        assert!(
            expected.contains(id) || !b_before.contains(id),
            "touched id {id} belongs to b.md"
        );
    }
    assert_eq!(rig.chunk_ids("b.md"), b_before);
}

#[test]
fn deleting_a_paragraph_tombstones_exactly_its_chunk_and_relinks_next() {
    // Arrange: one section with three paragraphs, each its own chunk (tiny target so they
    // never merge)
    let rig = new_rig("delete");
    let text =
        "# T\n\n## X\n\nAlpha paragraph.\n\n## Y\n\nBeta paragraph.\n\n## Z\n\nGamma paragraph.\n";
    fs::write(rig.root.join("a.md"), text).unwrap();
    rig.run();
    let before = rig.chunk_ids("a.md");
    assert_eq!(before.len(), 3, "three chunks: {before:?}");
    let sections_before = rig.section_ids("a.md");
    assert_eq!(sections_before.len(), 4, "T, X, Y, Z");

    // Act: remove the middle sibling section (heading and paragraph)
    fs::write(
        rig.root.join("a.md"),
        text.replace("## Y\n\nBeta paragraph.\n\n", ""),
    )
    .unwrap();
    rig.run();

    // Assert: exactly the middle chunk and its section are gone, the other ids survive
    let after = rig.chunk_ids("a.md");
    assert_eq!(after, vec![before[0].clone(), before[2].clone()]);
    let gone = kaban_core::NodeId::parse(&before[1]).unwrap();
    let node = rig
        .store
        .get(&gone, true)
        .unwrap()
        .expect("tombstoned row kept");
    assert_eq!(node.lifecycle, kaban_core::Lifecycle::Tombstoned);
    assert!(
        rig.store.get(&gone, false).unwrap().is_none(),
        "hidden from reads"
    );
    let sections_after = rig.section_ids("a.md");
    assert_eq!(
        sections_after,
        vec![
            sections_before[0].clone(),
            sections_before[1].clone(),
            sections_before[3].clone()
        ]
    );
    // NEXT between the surviving siblings X and Z is re-linked around the deleted Y
    let (_, edges) = rig.hop(&doc_id("a.md"), &["PART_OF", "NEXT"], 2);
    let next: Vec<(&str, &str)> = edges
        .iter()
        .filter(|e| e.rel == "NEXT")
        .map(|e| (e.src.as_str(), e.dst.as_str()))
        .collect();
    assert_eq!(
        next,
        vec![(sections_after[1].as_str(), sections_after[2].as_str())]
    );
}

#[test]
fn wikilinks_resolve_to_existing_documents_and_placeholders_upgrade_when_the_file_appears() {
    // Arrange
    let rig = new_rig("links");
    fs::write(
        rig.root.join("a.md"),
        "# A\n\nSee [[b]] and [[c#Sec|alias]].\n",
    )
    .unwrap();
    fs::write(rig.root.join("b.md"), B).unwrap();

    // Act
    let report = rig.run();

    // Assert: both bound (resolved: true); c missing => placeholder (status: missing)
    assert_eq!(report.documents, 2, "{report:?}");
    let (nodes, edges) = rig.hop(&doc_id("a.md"), &["LINKS_TO"], 1);
    let links: Vec<&kaban_core::Edge> = edges.iter().filter(|e| e.rel == "LINKS_TO").collect();
    assert_eq!(links.len(), 2, "{links:?}");
    let to_b = links
        .iter()
        .find(|e| e.dst == doc_id("b.md").to_string())
        .unwrap();
    assert_eq!(to_b.props["resolved"], true);
    assert_eq!(to_b.props["owner"], "kaban-ingest");
    let to_c = links
        .iter()
        .find(|e| e.dst == doc_id("c.md").to_string())
        .unwrap();
    assert_eq!(to_c.props["resolved"], true);
    assert_eq!(to_c.props["alias"], "alias");
    assert_eq!(to_c.props["anchor_kind"], "heading");
    let c = nodes
        .iter()
        .find(|n| n.id == doc_id("c.md").to_string())
        .unwrap();
    assert_eq!(c.props["status"], "missing");
    assert_eq!(c.props["path"], "c.md");
    // the placeholder is a Document row: `documents_by_status.missing` counts it (D2, DI2.4)
    let ws = kaban_core::WorkspaceId::default();
    let status = index_status(rig.store.as_ref(), &ws).unwrap();
    assert_eq!(status.documents_by_status["missing"], 1, "{status:?}");
    assert_eq!(status.documents_by_status["ok"], 2, "{status:?}");

    // Act again: the file appears
    fs::write(rig.root.join("c.md"), "# C\n\nNow here.\n").unwrap();
    rig.run();

    // Assert: same id, upgraded in place
    let c = rig.document("c.md").unwrap();
    assert_eq!(c.props["status"], "ok");
    assert_eq!(c.props["title"], "C");
    assert_eq!(rig.chunk_ids("c.md").len(), 1);
    // the upgraded placeholder leaves the `missing` count: the key is gone, three `ok`
    let status = index_status(rig.store.as_ref(), &ws).unwrap();
    assert!(
        !status.documents_by_status.contains_key("missing"),
        "{status:?}"
    );
    assert_eq!(status.documents_by_status["ok"], 3, "{status:?}");
}

#[test]
fn tags_yield_tagged_edges_to_shared_tag_nodes() {
    // Arrange
    let rig = new_rig("tags");
    fs::write(rig.root.join("a.md"), "# A\n\nTagged #a/b and #Other.\n").unwrap();
    fs::write(rig.root.join("b.md"), "# B\n\nAlso #A/B here.\n").unwrap();

    // Act
    rig.run();

    // Assert
    let (nodes, edges) = rig.hop(&doc_id("a.md"), &["TAGGED"], 1);
    let mut tags: Vec<&str> = nodes
        .iter()
        .filter(|n| n.kind == "Tag")
        .map(|n| n.text.as_str())
        .collect();
    tags.sort_unstable();
    assert_eq!(tags, vec!["a/b", "other"]);
    assert_eq!(edges.iter().filter(|e| e.rel == "TAGGED").count(), 2);
    // the case-folded tag of b.md is the same node
    let (nodes_b, _) = rig.hop(&doc_id("b.md"), &["TAGGED"], 1);
    let ab = nodes.iter().find(|n| n.text == "a/b").unwrap();
    assert!(
        nodes_b.iter().any(|n| n.id == ab.id),
        "a/b shared across documents"
    );
}

/// SR-6: prefix, middle and suffix edits re-id only the chunk whose text changed.
fn edited_ids(name: &str, before: &str, after: &str) -> (Vec<String>, Vec<String>) {
    let rig = new_rig(name);
    fs::write(rig.root.join("a.md"), before).unwrap();
    rig.run();
    let ids_before = rig.chunk_ids("a.md");
    fs::write(rig.root.join("a.md"), after).unwrap();
    rig.run();
    (ids_before, rig.chunk_ids("a.md"))
}

#[test]
fn a_prefix_edit_keeps_the_ids_of_the_later_chunks() {
    let (before, after) = edited_ids("prefix", A, &A.replace("First paragraph.", "First, new."));
    assert_eq!(before.len(), 3);
    assert_ne!(before[0], after[0]);
    assert_eq!(&before[1..], &after[1..]);
}

#[test]
fn a_middle_edit_keeps_the_ids_around_it() {
    let (before, after) = edited_ids("middle", A, &A.replace("Second paragraph.", "Second, new."));
    assert_eq!(before[0], after[0]);
    assert_ne!(before[1], after[1]);
    assert_eq!(before[2], after[2]);
}

#[test]
fn a_suffix_edit_keeps_the_ids_before_it() {
    let (before, after) = edited_ids("suffix", A, &A.replace("Third paragraph.", "Third, new."));
    assert_eq!(&before[..2], &after[..2]);
    assert_ne!(before[2], after[2]);
}

// ---- DI1.7 consolidation: what the DI1.5 tests left implicit ----

/// Every stored row of one document: the Document node, its sections, chunks, placeholder
/// targets and tags within three hops, and the edges between them, as JSON so a byte-level
/// comparison covers `rev`, `updated_at` and every prop.
fn rows_of(rig: &Rig, rel: &str) -> Vec<serde_json::Value> {
    let (mut nodes, mut edges) =
        rig.hop(&doc_id(rel), &["PART_OF", "NEXT", "LINKS_TO", "TAGGED"], 3);
    nodes.sort_by(|a, b| a.id.cmp(&b.id));
    edges.sort_by(|a, b| a.id.cmp(&b.id));
    let mut rows: Vec<serde_json::Value> = nodes
        .iter()
        .map(|n| serde_json::to_value(n).unwrap())
        .collect();
    rows.extend(edges.iter().map(|e| serde_json::to_value(e).unwrap()));
    rows
}

/// DI1.7: modifying one file leaves every other document's rows byte-identical. The row
/// snapshots of `b.md` (links to `headings.md` and carries a tag, so cross-document edges and
/// a shared Tag node are in the picture) and of the `headings.md` golden are taken before and
/// after an edit of `a.md`, and compared whole (`rev`, `updated_at`, props, edge ids).
#[test]
fn modifying_one_file_leaves_every_other_documents_rows_byte_identical() {
    // Arrange
    let rig = new_rig("isolation");
    fs::write(rig.root.join("a.md"), A).unwrap();
    fs::write(
        rig.root.join("b.md"),
        "# B\n\nLinks to [[headings]] and is tagged #shared.\n",
    )
    .unwrap();
    fs::copy(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/md/headings.md"),
        rig.root.join("headings.md"),
    )
    .unwrap();
    rig.run();
    let b_before = rows_of(&rig, "b.md");
    let h_before = rows_of(&rig, "headings.md");
    assert!(b_before.len() > 3 && h_before.len() > 10, "rows captured");

    // Act
    fs::write(
        rig.root.join("a.md"),
        A.replace("Second paragraph.", "Second, edited."),
    )
    .unwrap();
    let report = rig.run();

    // Assert
    assert_eq!((report.documents, report.skipped), (1, 2), "{report:?}");
    assert_eq!(rows_of(&rig, "b.md"), b_before, "b.md rows changed");
    assert_eq!(
        rows_of(&rig, "headings.md"),
        h_before,
        "headings.md rows changed"
    );
}

/// SR-6, the other half: a prefix, middle or suffix edit tombstones exactly the chunk whose
/// text changed (hidden from reads, kept as a row) and leaves the untouched chunks active under
/// their ids; the report counts one tombstone.
#[test]
fn each_edit_tombstones_exactly_the_changed_chunk_and_keeps_the_others_active() {
    use kaban_core::{Lifecycle, NodeId};
    let edits: [(&str, &str, usize); 3] = [
        ("tomb-prefix", "First paragraph.", 0),
        ("tomb-middle", "Second paragraph.", 1),
        ("tomb-suffix", "Third paragraph.", 2),
    ];
    for (name, old, changed) in edits {
        // Arrange
        let rig = new_rig(name);
        fs::write(rig.root.join("a.md"), A).unwrap();
        rig.run();
        let before = rig.chunk_ids("a.md");
        assert_eq!(before.len(), 3, "{name}: {before:?}");

        // Act
        fs::write(rig.root.join("a.md"), A.replace(old, "Edited text.")).unwrap();
        let report = rig.run();

        // Assert
        assert_eq!(report.chunks_tombstoned, 1, "{name}: {report:?}");
        let after = rig.chunk_ids("a.md");
        for (i, id) in before.iter().enumerate() {
            let node = rig
                .store
                .get(&NodeId::parse(id).unwrap(), true)
                .unwrap()
                .expect("row kept");
            if i == changed {
                assert_eq!(node.lifecycle, Lifecycle::Tombstoned, "{name}: chunk {i}");
                assert!(!after.contains(id), "{name}: old chunk {i} still listed");
            } else {
                assert_eq!(node.lifecycle, Lifecycle::Active, "{name}: chunk {i}");
                assert_eq!(after[i], *id, "{name}: chunk {i} re-identified");
            }
        }
    }
}

/// `forget --document` then re-ingest (docs-ingest-spec D10; `graph::forget_document`: "the
/// next `ingest` of the file re-creates the ids"): the tombstoned Document, sections and
/// chunks come back active under the same derived ids.
///
/// Observed red at DI1.7 against `651918b` (kept as a finding, `src/` not owned here): the
/// second run reports `documents: 0, skipped: 1`. `hash::skip_by_node` reads the Document with
/// `get(id, include_hidden = true)` and compares the skip key only, so the tombstoned row's
/// unchanged `content_hash` skips the file and nothing is restored until the file changes.
#[test]
fn forget_document_then_reingest_restores_the_same_ids() {
    use kaban_core::{Actor, Lifecycle, WriteCtx};
    use kaban_policy::Policy;
    // Arrange
    let rig = new_rig("forget");
    fs::write(rig.root.join("a.md"), A).unwrap();
    rig.run();
    let doc = doc_id("a.md");
    let chunks_before = rig.chunk_ids("a.md");
    let sections_before = rig.section_ids("a.md");

    // Act: forget, then ingest the unchanged file again
    let ctx = WriteCtx::new(
        Policy::empty().effective_hash(),
        kaban_core::WorkspaceId::default(),
        Actor::Agent,
    );
    let forgotten =
        kaban_ingest::forget_document(rig.store.as_ref(), &Policy::empty(), &ctx, "a.md").unwrap();
    let hidden = rig.store.get(&doc, false).unwrap();
    // DI1.8 (RD2-5): a directory ingest skips the forgotten document and counts it
    let skipped = rig.run();
    assert_eq!(
        (skipped.documents, skipped.skipped_forgotten),
        (0, 1),
        "{skipped:?}"
    );
    assert!(
        rig.store.get(&doc, false).unwrap().is_none(),
        "still hidden"
    );
    // naming the file explicitly reactivates it through the batch
    let mut explicit = rig.cfg.clone();
    explicit.roots = vec![rig.root.join("a.md")];
    let report = rig.run_with(&explicit);

    // Assert
    assert_eq!(
        forgotten.tombstoned,
        u32::try_from(1 + sections_before.len() + chunks_before.len()).unwrap(),
        "{forgotten:?}"
    );
    assert!(hidden.is_none(), "forgotten document hidden from reads");
    assert_eq!(report.documents, 1, "re-ingested, not skipped: {report:?}");
    let restored = rig
        .store
        .get(&doc, false)
        .unwrap()
        .expect("document active again");
    assert_eq!(restored.lifecycle, Lifecycle::Active);
    assert_eq!(rig.chunk_ids("a.md"), chunks_before);
    assert_eq!(rig.section_ids("a.md"), sections_before);
    for id in chunks_before.iter().chain(sections_before.iter()) {
        let node = rig
            .store
            .get(&kaban_core::NodeId::parse(id).unwrap(), false)
            .unwrap();
        assert!(node.is_some(), "{id} active again");
    }
}

/// DI1.8 (RD2-5): `--include-forgotten` reactivates a forgotten document from a directory
/// walk, and a chunk edited away then restored comes back under its old id, active.
#[test]
fn include_forgotten_and_edit_away_then_restore_reactivate_through_the_batch() {
    use kaban_core::{Actor, Lifecycle, NodeId, WriteCtx};
    use kaban_policy::Policy;
    let rig = new_rig("reactivate");
    fs::write(rig.root.join("a.md"), A).unwrap();
    rig.run();
    let before = rig.chunk_ids("a.md");
    // edit away, then restore
    fs::write(
        rig.root.join("a.md"),
        A.replace("Second paragraph.", "Edited paragraph."),
    )
    .unwrap();
    rig.run();
    fs::write(rig.root.join("a.md"), A).unwrap();
    let report = rig.run();
    assert_eq!(report.documents, 1, "{report:?}");
    assert_eq!(
        rig.chunk_ids("a.md"),
        before,
        "the restored chunk keeps its id"
    );
    let restored = rig
        .store
        .get(&NodeId::parse(&before[1]).unwrap(), false)
        .unwrap()
        .expect("restored chunk visible");
    assert_eq!(restored.lifecycle, Lifecycle::Active);
    assert_eq!(report.chunks_upserted, 0, "nothing new written: {report:?}");
    assert_eq!(report.chunks_unchanged, 3, "{report:?}");
    // forget, then `--include-forgotten` on the directory
    let ctx = WriteCtx::new(
        Policy::empty().effective_hash(),
        kaban_core::WorkspaceId::default(),
        Actor::Agent,
    );
    kaban_ingest::forget_document(rig.store.as_ref(), &Policy::empty(), &ctx, "a.md").unwrap();
    let mut cfg = rig.cfg.clone();
    cfg.include_forgotten = true;
    let report = rig.run_with(&cfg);
    assert_eq!(
        (report.documents, report.skipped_forgotten),
        (1, 0),
        "{report:?}"
    );
    assert!(rig.store.get(&doc_id("a.md"), false).unwrap().is_some());
}

/// DI1.8 (RD2-6; docs-ingest-spec D5 [A-3]): a human `link` from an unchanged chunk and an
/// `EVIDENCE_FOR` edge owned by another producer survive an edit of their document; the
/// pipeline's own stale `NEXT` edge is still removed.
#[test]
fn foreign_edges_from_an_unchanged_chunk_survive_a_document_edit() {
    use kaban_core::{Actor, NewNode, NodeId, WriteCtx};
    use kaban_policy::Policy;
    let rig = new_rig("foreign-edges");
    fs::write(rig.root.join("a.md"), A).unwrap();
    rig.run();
    let chunks = rig.chunk_ids("a.md");
    let first = NodeId::parse(&chunks[0]).unwrap();
    let ctx = WriteCtx::new(
        Policy::empty().effective_hash(),
        kaban_core::WorkspaceId::default(),
        Actor::Agent,
    );
    let fact = match rig
        .store
        .remember(
            NewNode {
                kind: "Fact".into(),
                text: "a human note".into(),
                ..Default::default()
            },
            &[],
            &ctx,
        )
        .unwrap()
    {
        kaban_core::RememberOutcome::Created(n) => n,
        other @ kaban_core::RememberOutcome::Existing { .. } => panic!("fresh store: {other:?}"),
    };
    let fact_id = NodeId::parse(&fact.id).unwrap();
    rig.store
        .link(&first, &fact_id, "RELATES_TO", None, None, &ctx)
        .unwrap();
    rig.store
        .link(
            &first,
            &fact_id,
            "EVIDENCE_FOR",
            None,
            Some(serde_json::json!({ "owner": "kaban-parse" })),
            &ctx,
        )
        .unwrap();
    // the edit touches the last paragraph only
    fs::write(
        rig.root.join("a.md"),
        A.replace("Third paragraph.", "Changed."),
    )
    .unwrap();
    rig.run();
    let (_, edges) = rig.hop(&first, &["RELATES_TO", "EVIDENCE_FOR"], 1);
    let mut rels: Vec<&str> = edges.iter().map(|e| e.rel.as_str()).collect();
    rels.sort_unstable();
    assert_eq!(rels, vec!["EVIDENCE_FOR", "RELATES_TO"], "{edges:?}");
}

/// `chmod 000` (the test runs unprivileged; root would read anyway and the case is moot).
fn make_readable(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o644)).unwrap();
}

fn make_unreadable(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o000)).unwrap();
    assert!(
        fs::read(path).is_err(),
        "the test needs an unprivileged user"
    );
}

/// DI1.8 (RD2-7): a failed refresh (the file becomes unreadable) tombstones the previously
/// indexed children and keeps the inventory on the Document, so a later changed success
/// and `forget --document` both find nothing stale.
#[test]
fn a_failed_refresh_tombstones_the_old_children_and_keeps_the_inventory() {
    use kaban_core::{Actor, Lifecycle, NodeId, WriteCtx};
    use kaban_policy::Policy;
    let rig = new_rig("failure");
    fs::write(rig.root.join("a.md"), A).unwrap();
    rig.run();
    let chunks = rig.chunk_ids("a.md");
    // Act: an unreadable file is a read error in the preparer (not a walk skip)
    fs::write(rig.root.join("a.md"), "changed so the key differs\n").unwrap();
    make_unreadable(&rig.root.join("a.md"));
    let failed = rig.run();
    assert_eq!(failed.failed, 1, "{failed:?}");
    assert_eq!(failed.chunks_tombstoned, 3, "{failed:?}");
    for id in &chunks {
        let n = rig
            .store
            .get(&NodeId::parse(id).unwrap(), true)
            .unwrap()
            .expect("row kept");
        assert_eq!(n.lifecycle, Lifecycle::Tombstoned, "{id}");
    }
    let doc = rig.document("a.md").expect("failed document stored");
    assert_eq!(doc.props["status"], "failed");
    assert_eq!(doc.props["chunk_ids"].as_array().map(Vec::len), Some(3));
    assert!(doc.props.get("error").is_some());
    // a changed success afterwards tombstones nothing twice and lists the new children
    make_readable(&rig.root.join("a.md"));
    fs::write(rig.root.join("a.md"), B).unwrap();
    let ok = rig.run();
    assert_eq!((ok.documents, ok.chunks_tombstoned), (1, 0), "{ok:?}");
    assert_eq!(rig.chunk_ids("a.md").len(), 1);
    // and `forget --document` after a failure still cleans everything
    fs::write(rig.root.join("a.md"), A).unwrap();
    rig.run();
    fs::write(rig.root.join("a.md"), "changed again\n").unwrap();
    make_unreadable(&rig.root.join("a.md"));
    rig.run();
    let ctx = WriteCtx::new(
        Policy::empty().effective_hash(),
        kaban_core::WorkspaceId::default(),
        Actor::Agent,
    );
    let forgotten =
        kaban_ingest::forget_document(rig.store.as_ref(), &Policy::empty(), &ctx, "a.md").unwrap();
    assert_eq!(
        forgotten.tombstoned, 1,
        "only the Document itself was still active"
    );
}

/// DI1.8 (RD2-8): report statistics come from typed commit outcomes: deleting one section with
/// one chunk reports one chunk tombstoned (the section is counted apart); editing one of
/// three chunks reports one upsert and two unchanged.
#[test]
fn report_counts_come_from_typed_commit_outcomes() {
    let rig = new_rig("counts");
    fs::write(rig.root.join("a.md"), A).unwrap();
    let first = rig.run();
    assert_eq!(
        (first.chunks_upserted, first.chunks_unchanged),
        (3, 0),
        "{first:?}"
    );
    fs::write(
        rig.root.join("a.md"),
        A.replace("Second paragraph.", "Edited paragraph."),
    )
    .unwrap();
    let edit = rig.run();
    assert_eq!(
        (
            edit.chunks_upserted,
            edit.chunks_unchanged,
            edit.chunks_tombstoned
        ),
        (1, 2, 1),
        "{edit:?}"
    );
    fs::write(
        rig.root.join("a.md"),
        A.replace("Second paragraph.", "Edited paragraph.")
            .replace("\n## Three\n\nThird paragraph.\n", ""),
    )
    .unwrap();
    let del = rig.run();
    assert_eq!(
        (
            del.chunks_tombstoned,
            del.sections_tombstoned,
            del.chunks_unchanged
        ),
        (1, 1, 2),
        "{del:?}"
    );
}

/// DI1.8b (RD2-4): a link-target-only edit (`https://example.test/a` to `/b`: title, sections
/// and chunk texts unchanged) is not "unchanged": the document is re-ingested and its
/// `unresolved_links` names the new target only.
#[test]
fn a_link_target_only_edit_reingests_and_updates_unresolved_links() {
    let rig = new_rig("link-only");
    let before = "# L\n\nSee [label](https://example.test/a) for more.\n";
    fs::write(rig.root.join("a.md"), before).unwrap();
    rig.run();
    let ids = rig.chunk_ids("a.md");

    fs::write(rig.root.join("a.md"), before.replace("/a)", "/b)")).unwrap();
    let report = rig.run();

    assert_eq!(report.documents, 1, "{report:?}");
    assert_eq!(report.skipped, 0, "{report:?}");
    let unresolved =
        serde_json::to_string(&rig.document("a.md").unwrap().props["unresolved_links"]).unwrap();
    assert!(
        unresolved.contains("https://example.test/b"),
        "{unresolved}"
    );
    assert!(
        !unresolved.contains("https://example.test/a"),
        "{unresolved}"
    );
    assert_eq!(rig.chunk_ids("a.md"), ids, "chunk texts did not change");
}

/// DI1.8b (RD2-4): a whitespace-only edit (extra blank lines before the second section) keeps
/// every id but moves the offsets; the keyed positional fingerprint makes the document fresh,
/// so the stored `byte_start` of the moved chunks is refreshed in place without a new id or a
/// tombstone, and the next run skips the document again.
#[test]
fn a_whitespace_only_edit_refreshes_offsets_without_new_ids() {
    use kaban_core::NodeId;
    let rig = new_rig("whitespace-only");
    fs::write(rig.root.join("a.md"), A).unwrap();
    rig.run();
    let ids = rig.chunk_ids("a.md");
    let offset = |rig: &Rig| {
        rig.store
            .get(&NodeId::parse(&ids[1]).unwrap(), true)
            .unwrap()
            .unwrap()
            .props["byte_start"]
            .as_u64()
            .unwrap()
    };
    let start_before = offset(&rig);

    fs::write(
        rig.root.join("a.md"),
        A.replace("\n\n## Two", "\n\n\n\n\n## Two"),
    )
    .unwrap();
    let report = rig.run();

    assert_eq!(report.documents, 1, "{report:?}");
    assert_eq!(rig.chunk_ids("a.md"), ids, "no new ids");
    assert_eq!(report.chunks_tombstoned, 0, "{report:?}");
    assert_eq!(
        report.chunks_upserted + report.chunks_unchanged,
        3,
        "the moved chunks are rewritten in place, the first is unchanged: {report:?}"
    );
    assert_eq!(offset(&rig), start_before + 3, "the offset was refreshed");
    assert_eq!(
        rig.run().skipped,
        1,
        "the refreshed document is unchanged again"
    );
}

/// DI1.8b (legacy stores): a tombstoned row without `props.owner` (written before DI1.8) whose
/// id re-appears is not a reactivation candidate: the batch succeeds and the legacy tombstone
/// stays hidden, instead of the store refusing the `Reactivate` and failing the document.
#[test]
fn a_legacy_tombstone_without_an_owner_stays_hidden_instead_of_failing_the_batch() {
    use kaban_core::{Actor, EdgeDiff, Lifecycle, NewNode, NodeId, Op, WriteBatch, WriteCtx};
    use kaban_policy::Policy;
    let rig = new_rig("legacy-tombstone");
    fs::write(rig.root.join("a.md"), A).unwrap();
    rig.run();
    let ids = rig.chunk_ids("a.md");
    // edit the second paragraph away: its chunk is tombstoned (with the owner mark)
    fs::write(
        rig.root.join("a.md"),
        A.replace("Second paragraph.", "Edited paragraph."),
    )
    .unwrap();
    rig.run();
    let gone = NodeId::parse(&ids[1]).unwrap();
    let stored = rig.store.get(&gone, true).unwrap().unwrap();
    assert_eq!(stored.lifecycle, Lifecycle::Tombstoned);
    // strip the owner mark, as a pre-DI1.8 row never had one (the upsert keeps the lifecycle)
    let mut props = stored.props.clone();
    props.as_object_mut().unwrap().remove("owner");
    let mut batch = WriteBatch::new(
        kaban_core::WorkspaceId::default(),
        vec![Op::UpsertNode {
            id: gone.clone(),
            node: NewNode {
                kind: stored.kind.clone(),
                text: stored.text.clone(),
                props: Some(props),
                ..NewNode::default()
            },
        }],
    );
    batch.edge_diff = EdgeDiff::None;
    let ctx = WriteCtx::new(
        Policy::empty().effective_hash(),
        kaban_core::WorkspaceId::default(),
        Actor::Agent,
    );
    kaban_core::IngestStore::apply_batch(rig.store.as_ref(), batch, &ctx).unwrap();
    let legacy = rig.store.get(&gone, true).unwrap().unwrap();
    assert_eq!(legacy.lifecycle, Lifecycle::Tombstoned);
    assert!(legacy.props.get("owner").is_none(), "{legacy:?}");

    // restore the paragraph: the id re-appears
    fs::write(rig.root.join("a.md"), A).unwrap();
    let report = rig.run();

    assert_eq!((report.documents, report.failed), (1, 0), "{report:?}");
    assert!(
        rig.store.get(&gone, false).unwrap().is_none(),
        "the legacy tombstone stays hidden"
    );
}
