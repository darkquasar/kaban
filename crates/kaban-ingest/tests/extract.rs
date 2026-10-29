//! Markdown and text extractors (docs-ingest-plan.md DI1.2; docs-ingest-spec D1): the Obsidian
//! wikilink/tag pre-pass as behaviour slices, the event walk as `insta` snapshots of
//! `Extracted` over the four fixtures, and the text extractor's decoding.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

use kaban_core::WorkspaceId;
use kaban_ingest::{
    Admission, AnchorKind, ExtractCtx, Extracted, Extractor, MarkdownExtractor, SourceId,
    admit_frontmatter, gate_path,
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

/// Extract, then interpret the frontmatter the way the pipeline does once the gate admitted
/// its source (RD2-1: the extractor itself never parses the block).
fn extract_md(src: &str) -> Extracted {
    let mut ex = MarkdownExtractor.extract(src.as_bytes(), &ctx()).unwrap();
    if let Some(source) = ex.frontmatter_source.clone()
        && let Admission::Admitted(admitted) = admit_frontmatter(&Policy::empty(), &source)
    {
        ex.interpret_frontmatter(&admitted);
    }
    ex
}

/// Plan slice (pre-pass): `[[Note]]`, `[[Note#Heading]]`, `[[Note^block]]`, `[[Note|alias]]`
/// and `![[embed]]` yield one `LinkDraft` each with the anchor kind, alias and embed flag, and
/// the byte offset of the opening bracket.
#[test]
fn wikilinks_of_every_anchor_form_are_captured_with_alias_and_embed() {
    let src = "See [[Note]] and [[Note#Heading]] and [[Note^blk]] and [[Note|an alias]].\n\n![[image.png]]\n";
    let ex = extract_md(src);
    let links: Vec<(String, AnchorKind, Option<String>, bool, u64)> = ex
        .links
        .iter()
        .map(|l| {
            (
                l.raw_target.clone(),
                l.anchor_kind,
                l.alias.clone(),
                l.embed,
                l.byte_offset,
            )
        })
        .collect();
    assert_eq!(
        links,
        vec![
            ("Note".into(), AnchorKind::Note, None, false, 4),
            ("Note#Heading".into(), AnchorKind::Heading, None, false, 17),
            ("Note^blk".into(), AnchorKind::Block, None, false, 38),
            (
                "Note".into(),
                AnchorKind::Note,
                Some("an alias".into()),
                false,
                55
            ),
            (
                "image.png".into(),
                AnchorKind::Note,
                None,
                true,
                u64::try_from(src.find("![[").unwrap()).unwrap()
            ),
        ]
    );
}

/// Plan slice (pre-pass): `#tag` and nested `#a/b` are tags with the offset of their `#`; a
/// heading marker, an issue number and a URL fragment are not; nothing inside an inline code
/// span or a fenced block is a tag or a wikilink.
#[test]
fn tags_are_captured_but_never_inside_code_spans_or_fences() {
    let src = "# Heading\n\nA #tag and #project/alpha, issue #12, https://h/#frag.\n\nInline `#notatag [[NotALink]]` here.\n\n```sh\n#!/bin/sh\necho [[AlsoNot]] #nor-this\n```\n\nAfter the fence #real.\n";
    let ex = extract_md(src);
    let tags: Vec<(String, u64)> = ex
        .tags
        .iter()
        .map(|t| (t.tag.clone(), t.byte_offset))
        .collect();
    assert_eq!(
        tags,
        vec![
            (
                "tag".into(),
                u64::try_from(src.find("#tag").unwrap()).unwrap()
            ),
            (
                "project/alpha".into(),
                u64::try_from(src.find("#project").unwrap()).unwrap()
            ),
            (
                "real".into(),
                u64::try_from(src.find("#real").unwrap()).unwrap()
            ),
        ]
    );
    assert!(
        ex.links.is_empty(),
        "no wikilink inside code: {:?}",
        ex.links
    );
}

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/md")
            .join(name),
    )
    .unwrap()
}

fn snapshot(name: &str) {
    let ex = extract_md(std::str::from_utf8(&fixture(name)).unwrap());
    insta::assert_json_snapshot!(name, ex);
}

/// Focused postcheck (insta): the event walk over the four fixtures. Headings open sections
/// whose `heading_path` is the stack of enclosing headings; the preamble before the first
/// heading is dropped when empty.
#[test]
fn snapshot_headings_and_nesting() {
    snapshot("headings.md");
}

/// Focused postcheck (insta): frontmatter (title wins over the first heading), wikilinks of
/// every form, standard links and images as `url`, tags including nested ones, lists.
#[test]
fn snapshot_wikilinks_tags_and_frontmatter() {
    snapshot("links.md");
}

/// Focused postcheck (insta): fenced and indented code, a table as header and rows, a block
/// quote, task-list markers; nothing inside the code span or fence is a link or tag.
#[test]
fn snapshot_code_fences_and_tables() {
    snapshot("code-tables.md");
}

/// Focused postcheck (insta): a CRLF file yields the same block texts as its LF twin (soft
/// breaks become `\n`), with offsets into the CRLF source.
#[test]
fn snapshot_crlf_file() {
    snapshot("crlf.md");
    let crlf = extract_md(std::str::from_utf8(&fixture("crlf.md")).unwrap());
    let lf = extract_md(
        &String::from_utf8(fixture("crlf.md"))
            .unwrap()
            .replace("\r\n", "\n"),
    );
    let texts = |e: &Extracted| -> Vec<String> {
        e.sections
            .iter()
            .flat_map(|s| s.blocks.iter())
            .map(|b| {
                format!("{b:?}")
                    .split(", byte_start")
                    .next()
                    .unwrap()
                    .to_owned()
            })
            .collect()
    };
    assert_eq!(texts(&crlf), texts(&lf));
    let names = |e: &Extracted| -> Vec<String> { e.tags.iter().map(|t| t.tag.clone()).collect() };
    assert_eq!(names(&crlf), names(&lf));
    assert_eq!(names(&crlf), vec!["tag".to_owned()]);
    let targets =
        |e: &Extracted| -> Vec<String> { e.links.iter().map(|l| l.raw_target.clone()).collect() };
    assert_eq!(targets(&crlf), targets(&lf));
    assert_eq!(targets(&crlf), vec!["Link".to_owned()]);
}

/// The text extractor decodes non-UTF-8 bytes through `chardetng`/`encoding_rs`, splits
/// paragraphs on blank lines and detects the language of enough prose; a UTF-8 BOM is dropped.
#[test]
fn text_extractor_decodes_legacy_bytes_and_splits_paragraphs() {
    use kaban_ingest::{Block, TextExtractor};
    // windows-1252: "café" with 0xE9, two paragraphs
    let bytes: Vec<u8> = b"caf\xe9 au lait\r\n\r\nsecond paragraph here".to_vec();
    let ex = TextExtractor.extract(&bytes, &ctx()).unwrap();
    assert_eq!(ex.sections.len(), 1);
    let texts: Vec<&str> = ex.sections[0]
        .blocks
        .iter()
        .map(|b| match b {
            Block::Paragraph { text, .. } => text.as_str(),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(texts, vec!["café au lait", "second paragraph here"]);
    assert!(!ex.partial, "a clean legacy decode is not partial");

    let english = "The quick brown fox jumps over the lazy dog. This sentence is long enough for the language detector to be confident about English text.\n\nAnother paragraph follows here with more ordinary English words in it.";
    let ex = TextExtractor
        .extract(format!("\u{feff}{english}").as_bytes(), &ctx())
        .unwrap();
    assert_eq!(ex.lang.as_deref(), Some("eng"));
    assert_eq!(ex.sections[0].blocks.len(), 2);
    assert!(
        matches!(&ex.sections[0].blocks[0], Block::Paragraph { text, .. } if text.starts_with("The quick")),
        "BOM dropped"
    );
}

/// RD1-9: 30,000 interleaved code spans and tags (`` `x` #t `` per line, 240 KB, under the
/// file-size cap) are cancelled while the pre-pass runs: the extractor returns `Cancelled`
/// promptly instead of finishing a quadratic exclusion check after the token was cancelled.
#[test]
fn the_wikilink_and_tag_prepass_honours_cancellation_with_many_interleaved_spans() {
    use kaban_ingest::ExtractError;
    let src = "`x` #t\n".repeat(30_000);
    let token = CancellationToken::new();
    let gated = gate_path(&Policy::empty(), "docs/fixture.md").unwrap();
    let ctx = ExtractCtx::new(
        token.clone(),
        tokio::time::Instant::now() + Duration::from_secs(60),
        SourceId::new(&WorkspaceId::default(), &gated),
    );
    let canceller = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(40));
        token.cancel();
    });
    let started = std::time::Instant::now();
    let result = MarkdownExtractor.extract(src.as_bytes(), &ctx);
    let elapsed = started.elapsed();
    canceller.join().unwrap();
    assert!(
        matches!(result, Err(ExtractError::Cancelled)),
        "cancelled extraction must not complete: {:?}",
        result.map(|e| e.tags.len())
    );
    assert!(elapsed < Duration::from_secs(2), "took {elapsed:?}");
}
