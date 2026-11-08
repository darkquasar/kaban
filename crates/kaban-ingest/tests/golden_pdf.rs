//! PDF extraction through `pdf_oxide` (docs-ingest-plan.md DI2.1; docs-ingest-spec D1 pdf, D4,
//! D7, D8): `critical-tdd` slices over the public interface for the no-text-layer rule, the
//! page sectioning and the corrupted-PDF path, then `insta` goldens of [`Extracted`] over the
//! born-digital fixtures and one end-to-end ingest of the fixture directory.
//!
//! The fixtures under `tests/fixtures/pdf/` are written by `examples/gen_pdf_fixtures.rs`
//! (deterministic; see its module doc).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use kaban_core::{MemoryStore as _, NodeId, RecallQuery};
use kaban_ingest::extract::{Extracted, PDF_PAGE_CAP, PdfOxideExtractor, SectionDraft};
use kaban_ingest::{
    Block, ChunkConfig, ChunkDraft, ChunkKind, ExtractCtx, ExtractError, Extractor as _, Format,
    SectionKind, SourceId, chunk, extractor_for, gate_path, index_status,
};
use kaban_policy::Policy;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/pdf")
            .join(name),
    )
    .unwrap()
}

fn ctx(name: &str) -> ExtractCtx {
    let ws = kaban_core::WorkspaceId::default();
    let gated = gate_path(&Policy::empty(), &format!("docs/{name}")).unwrap();
    ExtractCtx::new(
        CancellationToken::new(),
        tokio::time::Instant::now() + Duration::from_secs(30),
        SourceId::new(&ws, &gated),
    )
}

/// D8, RD3-5: a PDF without a text layer is `partial` and `needs_ocr`, keeps its page
/// inventory (exactly one empty `Page` section per page, `page_count` the file's total) and
/// yields nothing to chunk.
#[test]
fn a_pdf_without_a_text_layer_is_partial_needs_ocr_and_has_no_chunks() {
    let extractor = extractor_for(Format::Pdf).expect("a PDF extractor is compiled in");
    let extracted = extractor
        .extract(&fixture("scanned.pdf"), &ctx("scanned.pdf"))
        .unwrap();
    assert!(extracted.needs_ocr, "{extracted:?}");
    assert!(extracted.partial, "{extracted:?}");
    assert_eq!(extracted.page_count, Some(1), "{extracted:?}");
    assert_eq!(extracted.sections.len(), 1, "{:?}", extracted.sections);
    let page = &extracted.sections[0];
    assert_eq!(page.kind, SectionKind::Page);
    assert_eq!(page.heading_path, vec!["p.1"]);
    assert_eq!(page.level, 1);
    assert_eq!(page.page, Some(1));
    assert!(page.blocks.is_empty(), "{page:?}");
    let chunks = chunk(&extracted, &ChunkConfig::default());
    assert!(chunks.iter().all(|(_, c)| c.is_empty()), "{chunks:?}");
}

/// D1/D4: one `Page` section per page rooted at `p.N`, the heading the export detects nested
/// under its page, and every paragraph chunk carrying its page.
#[test]
fn pages_become_sections_with_headings_nested_under_them_and_chunks_carry_the_page() {
    let extractor = extractor_for(Format::Pdf).unwrap();
    let extracted = extractor
        .extract(&fixture("born1.pdf"), &ctx("born1.pdf"))
        .unwrap();
    assert!(!extracted.partial && !extracted.needs_ocr, "{extracted:?}");
    let shape: Vec<(Vec<&str>, &str, Option<u32>, usize)> = extracted
        .sections
        .iter()
        .map(|s| {
            (
                s.heading_path.iter().map(String::as_str).collect(),
                match s.kind {
                    SectionKind::Page => "page",
                    SectionKind::Heading => "heading",
                    _ => "other",
                },
                s.page,
                s.blocks.len(),
            )
        })
        .collect();
    assert_eq!(
        shape,
        vec![
            (vec!["p.1"], "page", Some(1), 0),
            (vec!["p.1", "Born One"], "heading", Some(1), 2),
            (vec!["p.2"], "page", Some(2), 0),
            (vec!["p.2", "Second Page"], "heading", Some(2), 1),
        ]
    );
    let chunks = chunk(&extracted, &ChunkConfig::default());
    let pages: Vec<(Option<u32>, usize)> = chunks.iter().map(|(s, c)| (s.page, c.len())).collect();
    assert_eq!(
        pages,
        vec![(Some(1), 0), (Some(1), 1), (Some(2), 0), (Some(2), 1)]
    );
    for (_, drafts) in &chunks {
        for d in drafts {
            assert!(d.page.is_some(), "{d:?}");
            assert!(d.context_prefix.starts_with("p."), "{}", d.context_prefix);
        }
    }
}

/// Unsupported encryption must fail with a content-free class, never succeed as
/// an empty/scanned document. Reuse a valid small fixture with an unsupported
/// encryption dictionary in its trailer (object offsets and startxref stay valid).
#[test]
fn unsupported_pdf_encryption_fails_without_exposing_document_content() {
    let mut bytes = fixture("born1.pdf");
    let trailer = b"trailer\n<<";
    let at = bytes
        .windows(trailer.len())
        .position(|part| part == trailer)
        .unwrap()
        + trailer.len();
    bytes.splice(
        at..at,
        b"/Encrypt <</Filter /InventedM5Encryption>>"
            .iter()
            .copied(),
    );
    let err = PdfOxideExtractor::new()
        .extract(&bytes, &ctx("unsupported.pdf"))
        .unwrap_err();
    let ExtractError::Format { class, .. } = &err else {
        panic!("expected a fixed Format error, got {err:?}");
    };
    assert!(matches!(*class, "pdf parse error" | "pdf page error"));
    assert!(!err.to_string().contains("Born One"));
    assert!(!err.to_string().contains("InventedM5Encryption"));
}

/// D8: a truncated file is a fixed-class `Format` error whose text names no content, with the
/// parser's error behind `source()`.
#[test]
fn a_truncated_pdf_fails_with_a_fixed_class_and_a_source_chain() {
    let extractor = extractor_for(Format::Pdf).unwrap();
    let err = extractor
        .extract(&fixture("truncated.pdf"), &ctx("truncated.pdf"))
        .unwrap_err();
    let ExtractError::Format { class, .. } = &err else {
        panic!("expected a Format error, got {err:?}");
    };
    assert!(
        matches!(*class, "pdf parse error" | "pdf page error"),
        "{class}"
    );
    assert_eq!(err.to_string(), *class, "Display is the class only");
    assert!(
        std::error::Error::source(&err).is_some(),
        "the pdf_oxide error is the source"
    );
}

/// DI2.1 page cap: pages beyond the cap are not read and the document is `partial`.
#[test]
fn pages_beyond_the_cap_are_not_read_and_the_document_is_partial() {
    let capped = PdfOxideExtractor::with_page_cap(1);
    let extracted = capped
        .extract(&fixture("born1.pdf"), &ctx("born1.pdf"))
        .unwrap();
    assert!(extracted.partial, "{extracted:?}");
    assert!(!extracted.needs_ocr);
    assert_eq!(
        extracted.sections.iter().filter_map(|s| s.page).max(),
        Some(1)
    );
    // RD3-5: total vs processed: `page_count` is the file's total, the `Page` sections are the
    // pages read
    assert_eq!(extracted.page_count, Some(2), "{extracted:?}");
    assert_eq!(
        extracted
            .sections
            .iter()
            .filter(|s| s.kind == SectionKind::Page)
            .count(),
        1
    );
    let full = PdfOxideExtractor::new()
        .extract(&fixture("born1.pdf"), &ctx("born1.pdf"))
        .unwrap();
    assert_eq!(PdfOxideExtractor::new().page_cap(), PDF_PAGE_CAP);
    assert_eq!(full.sections.iter().filter_map(|s| s.page).max(), Some(2));
    assert_eq!(full.page_count, Some(2));
    assert!(!full.partial);
}

/// Cooperative cancellation (PR-8): a cancelled context stops the extractor at a page boundary.
#[test]
fn a_cancelled_context_stops_the_extractor() {
    let token = CancellationToken::new();
    token.cancel();
    let ws = kaban_core::WorkspaceId::default();
    let gated = gate_path(&Policy::empty(), "docs/born1.pdf").unwrap();
    let cancelled = ExtractCtx::new(
        token,
        tokio::time::Instant::now() + Duration::from_secs(30),
        SourceId::new(&ws, &gated),
    );
    let err = extractor_for(Format::Pdf)
        .unwrap()
        .extract(&fixture("born1.pdf"), &cancelled)
        .unwrap_err();
    assert_eq!(err, ExtractError::Cancelled);
}

/// The goldens: `Extracted` over the four fixtures that open (`insta`, YAML).
#[test]
fn extracted_goldens_of_the_pdf_fixtures() {
    let extractor = extractor_for(Format::Pdf).unwrap();
    for name in ["born1.pdf", "born2.pdf", "born3.pdf", "scanned.pdf"] {
        let bytes = fixture(name);
        let first = extractor.extract(&bytes, &ctx(name)).unwrap();
        let second = extractor.extract(&bytes, &ctx(name)).unwrap();
        assert_eq!(first, second, "{name}: two extractions differ");
        insta::assert_yaml_snapshot!(name, first);
    }
}

/// OD-D6 on 0.3.78: the table fixture yields `TableRow` blocks (from the pipe table the
/// Markdown export renders), so the chunker emits one `Table` chunk with the header repeated.
#[test]
fn the_table_fixture_yields_one_table_chunk_with_its_header_repeated() {
    let extracted = extractor_for(Format::Pdf)
        .unwrap()
        .extract(&fixture("born2.pdf"), &ctx("born2.pdf"))
        .unwrap();
    let rows: Vec<&Block> = extracted
        .sections
        .iter()
        .flat_map(|s| s.blocks.iter())
        .filter(|b| matches!(b, Block::TableRow { .. }))
        .collect();
    assert_eq!(rows.len(), 3, "{rows:?}");
    let chunks = chunk(&extracted, &ChunkConfig::default());
    let tables: Vec<&ChunkDraft> = chunks
        .iter()
        .flat_map(|(_, c)| c.iter())
        .filter(|c| c.kind == ChunkKind::Table)
        .collect();
    assert_eq!(tables.len(), 1, "{tables:?}");
    assert!(
        tables[0].context_prefix.ends_with("Part | Qty | Unit"),
        "{}",
        tables[0].context_prefix
    );
    assert!(!tables[0].table_unknown);
}

/// RD3-4 through the public chunker: a `TableUnknown` block becomes one `Text` chunk flagged
/// `table_unknown` beside the paragraphs (no merging), and the flag survives `merge_undersized`
/// when the short neighbours fold into one chunk (the default policy).
#[test]
fn a_table_unknown_block_yields_one_flagged_text_chunk_and_the_flag_survives_merging() {
    let rows = "Part\tQty\tUnit\nBolt      12       pcs";
    let extracted = Extracted {
        title: None,
        frontmatter: serde_json::Value::Null,
        frontmatter_source: None,
        sections: vec![SectionDraft {
            heading_path: vec!["p.1".to_owned()],
            level: 1,
            kind: SectionKind::Page,
            byte_start: 0,
            byte_end: 70,
            page: Some(1),
            blocks: vec![
                Block::Paragraph {
                    text: "Before the rows.".to_owned(),
                    byte_start: 0,
                    byte_end: 17,
                },
                Block::TableUnknown {
                    text: rows.to_owned(),
                    byte_start: 17,
                    byte_end: 54,
                },
                Block::Paragraph {
                    text: "After the rows.".to_owned(),
                    byte_start: 54,
                    byte_end: 70,
                },
            ],
        }],
        links: Vec::new(),
        tags: Vec::new(),
        partial: false,
        needs_ocr: false,
        page_count: Some(1),
        lang: None,
    };
    let no_merge = ChunkConfig {
        merge_min_tokens: 0,
        ..ChunkConfig::default()
    };
    let drafts: Vec<ChunkDraft> = chunk(&extracted, &no_merge)
        .into_iter()
        .flat_map(|(_, c)| c)
        .collect();
    let shape: Vec<(&str, bool)> = drafts
        .iter()
        .map(|d| (d.text.as_str(), d.table_unknown))
        .collect();
    assert_eq!(
        shape,
        vec![
            ("Before the rows.", false),
            (rows, true),
            ("After the rows.", false),
        ]
    );
    assert!(
        drafts.iter().all(|d| d.kind == ChunkKind::Text),
        "{drafts:?}"
    );
    assert_eq!((drafts[1].byte_start, drafts[1].byte_end), (17, 54));

    let merged: Vec<ChunkDraft> = chunk(&extracted, &ChunkConfig::default())
        .into_iter()
        .flat_map(|(_, c)| c)
        .collect();
    assert_eq!(merged.len(), 1, "{merged:?}");
    assert!(merged[0].table_unknown, "{merged:?}");
    assert_eq!(
        merged[0].text,
        format!("Before the rows.\n\n{rows}\n\nAfter the rows.")
    );
}

/// RD3-4, the fixture gap on 0.3.78: `born3.pdf` places three rows whose cells are separated
/// by runs of spaces, but the Markdown export collapses the runs and joins the rows into one
/// single-spaced line (`di24-probe.log`), so the conservative heuristic cannot fire: the rows
/// are one paragraph, no `table_unknown` chunk, and the stored document carries no
/// `table_unknown_chunks` key (zero is omitted) beside `page_count: 1`.
#[test]
fn the_space_separated_rows_fixture_is_flattened_by_the_export_and_stays_prose() {
    let extracted = extractor_for(Format::Pdf)
        .unwrap()
        .extract(&fixture("born3.pdf"), &ctx("born3.pdf"))
        .unwrap();
    assert_eq!(extracted.page_count, Some(1));
    let blocks: Vec<&Block> = extracted
        .sections
        .iter()
        .flat_map(|s| s.blocks.iter())
        .collect();
    assert!(
        blocks.iter().all(|b| matches!(b, Block::Paragraph { .. })),
        "{blocks:?}"
    );
    assert!(
        blocks
            .iter()
            .any(|b| matches!(b, Block::Paragraph { text, .. } if text.contains("Bolt 12 pcs"))),
        "{blocks:?}"
    );
    let drafts: Vec<ChunkDraft> = chunk(&extracted, &ChunkConfig::default())
        .into_iter()
        .flat_map(|(_, c)| c)
        .collect();
    assert!(drafts.iter().all(|d| !d.table_unknown), "{drafts:?}");

    let rig = common::real::rig("kaban-di24", "born3");
    std::fs::create_dir_all(rig.root.join("docs")).unwrap();
    std::fs::write(rig.root.join("docs/born3.pdf"), fixture("born3.pdf")).unwrap();
    let report = rig.run();
    assert_eq!(
        (report.documents, report.partial, report.failed),
        (1, 0, 0),
        "{report:?}"
    );
    let doc = rig.document("docs/born3.pdf").unwrap();
    assert_eq!(doc.props["status"], "ok");
    assert_eq!(doc.props["page_count"], 1);
    assert!(
        doc.props.get("table_unknown_chunks").is_none(),
        "{:?}",
        doc.props
    );
}

/// End to end (D7, D8): the fixture directory through the real `Ingestor`: two documents
/// `ok`, the scanned one `partial` with `needs_ocr`, the truncated one `failed` with the fixed
/// class, the batch complete; page sections and `file.pdf > p.N` chunk cards in the store.
#[test]
fn ingesting_the_fixture_directory_records_ok_partial_and_failed_and_continues() {
    let rig = common::real::rig("kaban-di21", "fixture-dir");
    std::fs::create_dir_all(rig.root.join("docs")).unwrap();
    for name in ["born1.pdf", "born2.pdf", "scanned.pdf", "truncated.pdf"] {
        std::fs::write(rig.root.join("docs").join(name), fixture(name)).unwrap();
    }
    let report = rig.run();
    assert_eq!(report.documents, 3, "{report:?}");
    assert_eq!(report.partial, 1, "{report:?}");
    assert_eq!(report.failed, 1, "{report:?}");
    assert_eq!(report.blocked, 0);
    assert!(!report.cancelled);

    let status = |rel: &str| -> String {
        rig.document(rel).unwrap().props["status"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    assert_eq!(status("docs/born1.pdf"), "ok");
    assert_eq!(status("docs/born2.pdf"), "ok");
    assert_eq!(status("docs/scanned.pdf"), "partial");
    assert_eq!(status("docs/truncated.pdf"), "failed");
    let scanned = rig.document("docs/scanned.pdf").unwrap();
    assert_eq!(scanned.props["needs_ocr"], serde_json::Value::Bool(true));
    assert_eq!(rig.chunk_ids("docs/scanned.pdf"), Vec::<String>::new());
    // RD3-5: the page inventory is stored (one Page section, no chunk) and `page_count` is the
    // file's total, for the image-only and the born-digital document alike
    assert_eq!(rig.section_ids("docs/scanned.pdf").len(), 1);
    assert_eq!(scanned.props["page_count"], 1);
    assert_eq!(
        rig.document("docs/born1.pdf").unwrap().props["page_count"],
        2
    );
    let truncated = rig.document("docs/truncated.pdf").unwrap();
    let error = truncated.props["error"].as_str().unwrap();
    assert!(
        matches!(error, "pdf parse error" | "pdf page error"),
        "{error}"
    );

    // the graph shape of the two-page fixture: four sections, two chunks
    assert_eq!(rig.section_ids("docs/born1.pdf").len(), 4);
    assert_eq!(rig.chunk_ids("docs/born1.pdf").len(), 2);
    let born2_chunks = rig.chunk_ids("docs/born2.pdf");
    // the two undersized paragraphs merge into one chunk (D4), plus the table chunk
    assert_eq!(born2_chunks.len(), 2, "{born2_chunks:?}");
    let kinds: Vec<String> = born2_chunks
        .iter()
        .map(|id| {
            let n = rig
                .store
                .get(&NodeId::parse(id).unwrap(), true)
                .unwrap()
                .unwrap();
            n.props["kind"].to_string()
        })
        .collect();
    assert!(kinds.iter().any(|k| k == "\"table\""), "{kinds:?}");

    // the chunk card (D7): `path > p.N > heading` through the store's recall
    let hits = rig
        .store
        .recall(&RecallQuery {
            query: "second page heading".to_owned(),
            kinds: vec!["Chunk".to_owned()],
            limit: 5,
            ..RecallQuery::default()
        })
        .unwrap();
    let cards: Vec<&str> = hits
        .iter()
        .filter_map(|h| h.node.summary.as_deref())
        .collect();
    assert!(
        cards.contains(&"docs/born1.pdf > p.2 > Second Page"),
        "{cards:?}"
    );

    // `index_status` lists the PDF extractor and counts the statuses
    let ws = kaban_core::WorkspaceId::default();
    let status = index_status(rig.store.as_ref(), &ws).unwrap();
    assert!(
        status.extractors.iter().any(|e| e == "pdf_oxide@0.3.78"),
        "{:?}",
        status.extractors
    );
    assert_eq!(status.documents_by_status["failed"], 1);
    assert_eq!(status.documents_by_status["partial"], 1);
    assert_eq!(status.documents_by_status["ok"], 2);
    assert_eq!(status.failed.len(), 1);
    assert_eq!(status.failed[0].path, "docs/truncated.pdf");
}
