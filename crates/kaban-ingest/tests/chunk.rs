//! Chunker (docs-ingest-plan.md DI1.3; docs-ingest-spec D4): section tree first, paragraphs
//! accumulate to the target, undersized successors merge, fences and tables are never split,
//! an oversized paragraph group splits at the hard max, order is stable.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

use kaban_core::WorkspaceId;
use kaban_ingest::{
    ChunkConfig, ChunkDraft, ChunkKind, ExtractCtx, Extracted, Extractor, MarkdownExtractor,
    SectionDraft, SourceId, chunk, estimate_tokens, gate_path,
};
use kaban_policy::Policy;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

fn ctx() -> ExtractCtx {
    let gated = gate_path(&Policy::empty(), "docs/fixture.md").unwrap();
    ExtractCtx::new(
        CancellationToken::new(),
        tokio::time::Instant::now() + Duration::from_secs(10),
        SourceId::new(&WorkspaceId::default(), &gated),
    )
}

fn extract(src: &str) -> Extracted {
    MarkdownExtractor.extract(src.as_bytes(), &ctx()).unwrap()
}

fn chunks_of(src: &str, cfg: &ChunkConfig) -> Vec<(SectionDraft, Vec<ChunkDraft>)> {
    chunk(&extract(src), cfg)
}

/// Plan slice 1: three short paragraphs under one heading accumulate into one text chunk
/// whose `context_prefix` is `"<title> > <h1>"`.
#[test]
fn three_short_paragraphs_under_one_heading_make_one_chunk() {
    let src = "# Title\n\nFirst paragraph.\n\nSecond paragraph.\n\nThird paragraph.\n";
    let out = chunks_of(src, &ChunkConfig::default());
    assert_eq!(out.len(), 1, "one section: {out:?}");
    let (section, chunks) = &out[0];
    assert_eq!(section.heading_path, vec!["Title".to_owned()]);
    assert_eq!(chunks.len(), 1, "{chunks:?}");
    let c = &chunks[0];
    assert_eq!(
        c.text,
        "First paragraph.\n\nSecond paragraph.\n\nThird paragraph."
    );
    assert_eq!(c.kind, ChunkKind::Text);
    assert_eq!(c.context_prefix, "Title");
    assert!(!c.oversize);
    assert_eq!(c.byte_start, 9);
    assert_eq!(c.byte_end, u64::try_from(src.len()).unwrap());
    assert_eq!(
        estimate_tokens(&c.text),
        u32::try_from(c.text.chars().count() / 4).unwrap()
    );
}

/// Plan slice 2: a 5 KB fence is one `Code { lang }` chunk, never split, flagged `oversize`
/// (above the 1024-token hard max); the prose around it stays in its own text chunks.
#[test]
fn a_five_kilobyte_code_fence_is_one_oversize_code_chunk() {
    let body: String = (0..100)
        .map(|i| format!("let value_{i:03} = compute_something_long({i}); // padding text"))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    assert!(body.len() > 5000, "{}", body.len());
    let src = format!("# Code\n\nBefore.\n\n```rust\n{body}```\n\nAfter.\n");
    let out = chunks_of(&src, &ChunkConfig::default());
    let chunks = &out[0].1;
    let kinds: Vec<&ChunkKind> = chunks.iter().map(|c| &c.kind).collect();
    assert_eq!(
        kinds,
        vec![
            &ChunkKind::Text,
            &ChunkKind::Code {
                lang: Some("rust".into())
            },
            &ChunkKind::Text
        ],
        "{chunks:?}"
    );
    let code = &chunks[1];
    assert!(code.oversize, "5 KB of code is above the hard max");
    assert_eq!(code.text.trim_end(), body.trim_end());
    assert!(estimate_tokens(&code.text) > 1024);
    assert!(!chunks[0].oversize && !chunks[2].oversize);
}

/// Plan slice 3: a table with 40 rows is one `Table` chunk, rows never split, with the header
/// row repeated at the end of `context_prefix`; every row's cells are in the chunk text.
#[test]
fn a_forty_row_table_is_one_table_chunk_with_the_header_in_the_prefix() {
    let mut src = String::from("# Data\n\n| Name | Value | Note |\n|---|---|---|\n");
    for i in 0..40 {
        use std::fmt::Write as _;
        writeln!(src, "| row{i} | {i} | note number {i} |").unwrap();
    }
    let out = chunks_of(&src, &ChunkConfig::default());
    let chunks = &out[0].1;
    assert_eq!(chunks.len(), 1, "{chunks:?}");
    let t = &chunks[0];
    assert_eq!(t.kind, ChunkKind::Table);
    assert_eq!(t.context_prefix, "Data > Name | Value | Note");
    assert!(
        t.text.lines().count() == 41,
        "header + 40 rows: {}",
        t.text.lines().count()
    );
    assert!(t.text.contains("| row39 | 39 | note number 39 |"));
    assert!(!t.oversize, "{} tokens", estimate_tokens(&t.text));
}

/// Plan slice 4: a single 10 KB paragraph (above the hard max) is split by `text-splitter` into
/// text chunks of at most 1024 tokens each, all under the same section, in document order and
/// identical across two runs; the pieces together carry the whole paragraph.
#[test]
fn a_ten_kilobyte_paragraph_splits_at_the_hard_max_with_stable_order() {
    let sentence = "The quick brown fox jumps over the lazy dog near the river bank. ";
    let para: String = sentence.repeat(160);
    assert!(para.len() > 10_000);
    let src = format!("# Long\n\n## Part\n\n{para}\n");
    let cfg = ChunkConfig::default();
    let first = chunks_of(&src, &cfg);
    let second = chunks_of(&src, &cfg);
    assert_eq!(first, second, "deterministic");
    let (section, chunks) = &first[1];
    assert_eq!(
        section.heading_path,
        vec!["Long".to_owned(), "Part".to_owned()]
    );
    assert!(chunks.len() >= 3, "{} chunks", chunks.len());
    for c in chunks {
        assert!(
            estimate_tokens(&c.text) <= cfg.hard_max_tokens,
            "{} tokens",
            estimate_tokens(&c.text)
        );
        assert_eq!(c.kind, ChunkKind::Text);
        assert_eq!(c.context_prefix, "Long > Part");
        assert!(!c.oversize, "a split group is not oversize");
    }
    let rejoined: String = chunks
        .iter()
        .map(|c| c.text.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    assert_eq!(
        rejoined.split_whitespace().collect::<Vec<_>>(),
        para.split_whitespace().collect::<Vec<_>>(),
        "no words lost or duplicated"
    );
}

/// D4 merge rule: with a tiny target, each paragraph would be its own chunk; an undersized
/// successor (below `merge_min_tokens`) merges into the preceding text chunk of the same
/// section, while a code chunk between them is never merged into.
#[test]
fn undersized_successive_text_chunks_in_one_section_merge() {
    let big = "word ".repeat(64); // 320 chars = 80 tokens: exactly the target, so the tail
    // cannot accumulate into it and is flushed as its own 2-token chunk
    let src = format!("# S\n\n{big}\n\ntiny tail.\n\n```\ncode\n```\n\ntiny after code.\n");
    let cfg = ChunkConfig::builder()
        .target_tokens(80)
        .hard_max_tokens(200)
        .merge_min_tokens(16)
        .build()
        .unwrap();
    let chunks = &chunks_of(&src, &cfg)[0].1;
    let kinds: Vec<&ChunkKind> = chunks.iter().map(|c| &c.kind).collect();
    assert_eq!(
        kinds,
        vec![
            &ChunkKind::Text,
            &ChunkKind::Code { lang: None },
            &ChunkKind::Text
        ],
        "{chunks:?}"
    );
    assert!(
        chunks[0].text.ends_with("tiny tail."),
        "the tiny tail merged into its predecessor: {:?}",
        chunks[0].text
    );
    assert_eq!(
        chunks[2].text, "tiny after code.",
        "nothing merges across a code chunk"
    );
}

/// Spec D4 [A-2] (plan §15 note 1): a 200 KiB fence stays one oversize code chunk and needs 7
/// delivery slices of 32 KiB; `slices_for` is 0 for empty text and 1 up to one slice.
#[test]
fn a_two_hundred_kilobyte_fence_is_one_oversize_chunk_with_seven_slices() {
    use kaban_ingest::{DEFAULT_SLICE_BYTES, slices_for};
    let line = "x".repeat(99) + "\n";
    let body = line.repeat(2048); // 204,800 bytes
    let src = format!("# Big\n\n```\n{body}```\n");
    let chunks = &chunks_of(&src, &ChunkConfig::default())[0].1;
    assert_eq!(chunks.len(), 1);
    assert!(chunks[0].oversize);
    assert_eq!(slices_for(&chunks[0].text, DEFAULT_SLICE_BYTES), 7);
    assert_eq!(slices_for("", DEFAULT_SLICE_BYTES), 0);
    assert_eq!(slices_for("x", DEFAULT_SLICE_BYTES), 1);
    assert_eq!(slices_for(&"x".repeat(32 * 1024), DEFAULT_SLICE_BYTES), 1);
    assert_eq!(
        slices_for(&"x".repeat(32 * 1024 + 1), DEFAULT_SLICE_BYTES),
        2
    );
}

/// The builder refuses inconsistent relations and the defaults are 512 / 1024 / 64.
#[test]
fn chunk_config_builder_validates_relations_and_defaults_match_the_plan() {
    use kaban_ingest::ChunkConfigError;
    let d = ChunkConfig::default();
    assert_eq!(
        (d.target_tokens, d.hard_max_tokens, d.merge_min_tokens),
        (512, 1024, 64)
    );
    assert_eq!(ChunkConfig::builder().build().unwrap(), d);
    assert_eq!(
        ChunkConfig::builder().target_tokens(2000).build(),
        Err(ChunkConfigError::Target)
    );
    assert_eq!(
        ChunkConfig::builder().merge_min_tokens(600).build(),
        Err(ChunkConfigError::MergeMin)
    );
    assert_eq!(
        ChunkConfig::builder().slice_bytes(0).build(),
        Err(ChunkConfigError::SliceBytes)
    );
}

/// RD1-7: a 4,095-character paragraph (1,023 tokens) followed by a 7-character one (1 token)
/// must not merge: the joined text with its `"\n\n"` separator is 4,104 characters, 1,026
/// tokens, above the hard maximum. Every chunk that is not `oversize` stays within the hard
/// maximum measured on its actual text.
#[test]
fn merging_undersized_text_counts_the_separator_and_never_exceeds_the_hard_max() {
    let cfg = ChunkConfig::default();
    let big = "a".repeat(4095);
    let src = format!("# H\n\n{big}\n\nseven77\n");
    let chunks = &chunks_of(&src, &cfg)[0].1;
    for c in chunks {
        assert!(
            c.oversize || estimate_tokens(&c.text) <= cfg.hard_max_tokens,
            "{} tokens in a chunk that is not oversize",
            estimate_tokens(&c.text)
        );
    }
    assert_eq!(chunks.len(), 2, "the second paragraph stays its own chunk");
}

/// RD1-8: `"€".repeat(21845)` is 65,535 bytes; a 32 KiB delivery slice cut back to a char
/// boundary holds 10,922 three-byte scalars (32,766 bytes), so the delivery needs three
/// slices, not `ceil(65535 / 32768) = 2`. A limit narrower than one scalar delivers that
/// scalar whole, one per slice.
#[test]
fn slice_counts_follow_char_boundaries_of_the_sanitized_text() {
    use kaban_ingest::{DEFAULT_SLICE_BYTES, slices_for};
    let euros = "€".repeat(21845);
    assert_eq!(euros.len(), 65_535);
    assert_eq!(slices_for(&euros, DEFAULT_SLICE_BYTES), 3);
    assert_eq!(
        slices_for("€€", 1),
        2,
        "sub-scalar limit: one scalar per slice"
    );
    assert_eq!(slices_for("€a", 3), 2);
}

/// DI1.8b (RD2-2): the chunker's seam invariant. Through trimming (a split paragraph), tail
/// merging (undersized successors) and multibyte text, every chunk's mapping lists segments
/// whose output text is a verbatim slice of the stream at the source range, the segments cover
/// the whole chunk text apart from the `"\n\n"` separators a merge inserts, and the segments
/// are in order in both coordinates.
#[test]
fn every_chunk_text_maps_segment_by_segment_onto_the_stream() {
    use kaban_ingest::chunk_with_stream;
    let cfg = ChunkConfig::builder()
        .target_tokens(40)
        .hard_max_tokens(60)
        .merge_min_tokens(8)
        .build()
        .unwrap();
    // 290 chars > the 240-char hard max: the cut lands in the gap, the head is trimmed and
    // the 60-char tail is its own piece until the two short paragraphs merge into it
    let long = format!("{}{}{}", "é".repeat(200), " ".repeat(30), "Ω".repeat(60));
    let src = format!("# S\n\n{long}\n\nshort one.\n\nshort two.\n\n```\ncode\n```\n\ntail.\n");
    let chunked = chunk_with_stream(&extract(&src), &cfg);
    let drafts: Vec<&ChunkDraft> = chunked.sections.iter().flat_map(|(_, c)| c).collect();
    assert_eq!(chunked.stream.maps.len(), drafts.len());
    let mut merged_seen = false;
    for (draft, map) in drafts.iter().zip(&chunked.stream.maps) {
        let segments = map.segments();
        assert!(!segments.is_empty(), "{:?}", draft.text);
        let mut out_pos = 0usize;
        let mut src_pos = 0usize;
        for seg in segments {
            assert!(seg.output.start >= out_pos && seg.source.start >= src_pos);
            if seg.output.start > out_pos {
                assert_eq!(&draft.text[out_pos..seg.output.start], "\n\n");
                merged_seen = true;
            }
            assert_eq!(
                &draft.text[seg.output.clone()],
                &chunked.stream.text[seg.source.clone()],
                "a segment is not a verbatim slice"
            );
            out_pos = seg.output.end;
            src_pos = seg.source.end;
        }
        assert_eq!(
            out_pos,
            draft.text.len(),
            "the mapping covers the chunk text"
        );
    }
    assert!(merged_seen, "the layout exercised a merge");
    assert_eq!(drafts.len(), 4, "head, tail+shorts, code, tail: {drafts:?}");
    assert!(drafts[1].text.starts_with('Ω') && drafts[1].text.ends_with("short two."));
}
