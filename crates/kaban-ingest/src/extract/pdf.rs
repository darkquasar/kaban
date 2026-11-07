//! PDF extractor on `pdf_oxide` (docs-ingest-spec D1 pdf, D4, D7, D8; docs-ingest-plan.md
//! DI2.1; OWN-68 SC-07: the one PDF backend). One [`SectionDraft`] of kind `Page` per page with
//! `heading_path = ["p.N"]`, so the chunk card reads `file.pdf > p.N` (D7) and the section id is
//! stable per page; the headings `pdf_oxide`'s Markdown export detects on a page open sections
//! nested under that page (`["p.N", "Heading"]`); every block carries `page`.
//!
//! ```text
//!   bytes ==> PdfDocument::from_bytes ==> page_count (cap PDF_PAGE_CAP: partial beyond)
//!         ==> per page (ctx.check() first): has_text_layer ? to_markdown(page) : ""
//!         ==> text layer below MIN_CHARS_PER_PAGE on average ==> partial + needs_ocr,
//!             one empty Page section per page read, no blocks
//!         ==> else page Markdown lines -> headings (sections under the page), paragraphs,
//!             lists, fenced pipe tables (TableRow blocks -> one Table chunk), fenced code,
//!             runs of tabular-looking lines (one TableUnknown block -> one flagged text chunk)
//!   any pdf_oxide error ==> ExtractError::Format { class, source } ==> Document failed (D8)
//! ```
//!
//! Tables (OD-D6): on 0.3.78 `extract_tables` finds no table on the fixture, but the text
//! pipeline's inline table rendering emits the rows as a GFM pipe table inside a code fence of
//! the Markdown export; such a fence becomes [`Block::TableRow`]s, so the chunker produces the
//! same `kind: Table` chunk with the header repeated that the Markdown extractor yields. Rows
//! the export does not render as a pipe table stay plain paragraph text, except a run of two
//! or more consecutive lines that look tabular (RD3-4: a tab, a `|` or two or more spaces
//! between cells, the same number of gaps on every line): those become one
//! [`Block::TableUnknown`], chunked as text and flagged `table_unknown`, never claimed as a
//! table. On 0.3.78 the export collapses runs of spaces and tabs to one space, so the shipped
//! fixtures hold no such block (`born3.pdf` pins that shape).
//!
//! Offsets: every `byte_start`/`byte_end` indexes the concatenation of the per-page Markdown
//! exports joined by one newline, not the PDF bytes. An image-only document (`needs_ocr`)
//! keeps its page inventory: one empty `Page` section per page read, covering that page's
//! slice of the concatenation, with no blocks and so no chunks (RD3-5). `page_count` is the
//! total the file reports; the pages read are the `Page` sections, so a capped read shows
//! fewer of them than `page_count` beside `partial: true`.
//!
//! Error messages are fixed classes; the `pdf_oxide` error is the `source()` only
//! (`err-source-chain`), so no document content reaches a log or the store.

use super::{Block, Extracted, Extractor, SectionDraft};
use crate::cancel::{ExtractCtx, ExtractError};
use crate::model::{Format, SectionKind};
use pdf_oxide::PdfDocument;
use pdf_oxide::converters::ConversionOptions;
use serde_json::Value;
use std::sync::Arc;

/// Pages read at most (docs-ingest-plan.md DI2.1): pages beyond are not read and the document
/// is `partial`.
pub const PDF_PAGE_CAP: u32 = 2_000;

/// Non-whitespace characters per page, on average, below which the text layer counts as
/// absent (DI2.1 "near-empty text layer": `partial`, `needs_ocr`, no chunks).
pub const MIN_CHARS_PER_PAGE: usize = 20;

/// Fixed class of a document that does not open or whose page tree is unreadable.
const OPEN_ERROR: &str = "pdf parse error";
/// Fixed class of a page that fails to read or convert (a truncated file, D8).
const PAGE_ERROR: &str = "pdf page error";

/// `.pdf` through `pdf_oxide 0.3.78` (features `icc`, `legacy-crypto` only).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PdfOxideExtractor {
    page_cap: u32,
}

impl Default for PdfOxideExtractor {
    fn default() -> Self {
        Self::new()
    }
}

impl PdfOxideExtractor {
    /// The extractor with the plan's page cap ([`PDF_PAGE_CAP`]).
    pub const fn new() -> Self {
        Self {
            page_cap: PDF_PAGE_CAP,
        }
    }

    /// The extractor with another page cap (tests; the pipeline uses [`Self::new`]).
    pub const fn with_page_cap(page_cap: u32) -> Self {
        Self { page_cap }
    }

    /// Pages read at most.
    pub const fn page_cap(&self) -> u32 {
        self.page_cap
    }
}

impl super::private::Sealed for PdfOxideExtractor {}

fn format_error(class: &'static str, source: pdf_oxide::Error) -> ExtractError {
    ExtractError::Format {
        class,
        source: Arc::new(source),
    }
}

impl Extractor for PdfOxideExtractor {
    fn id(&self) -> &'static str {
        "pdf_oxide@0.3.78"
    }

    fn formats(&self) -> &'static [Format] {
        &[Format::Pdf]
    }

    fn extract(&self, bytes: &[u8], ctx: &ExtractCtx) -> Result<Extracted, ExtractError> {
        ctx.check()?;
        let doc =
            PdfDocument::from_bytes(bytes.to_vec()).map_err(|e| format_error(OPEN_ERROR, e))?;
        // The backend's permissive page conversion can turn decryption failures
        // into empty text. Refuse unreadable encryption before that can look like
        // a scanned document requiring OCR. Empty-password PDFs remain supported.
        if doc.is_encrypted()
            && !doc
                .authenticate(b"")
                .map_err(|e| format_error(OPEN_ERROR, e))?
        {
            return Err(format_error(
                OPEN_ERROR,
                pdf_oxide::Error::InvalidPdf("pdf password required".into()),
            ));
        }
        let total = doc.page_count().map_err(|e| format_error(OPEN_ERROR, e))?;
        let page_count = u32::try_from(total).ok();
        let cap = usize::try_from(self.page_cap).unwrap_or(usize::MAX);
        let pages = total.min(cap);
        let capped = total > cap;
        // detect_headings and extract_tables on, images off (the defaults of 0.3.78)
        let options = ConversionOptions::default();
        let mut markdown: Vec<String> = Vec::with_capacity(pages);
        let mut chars = 0usize;
        for index in 0..pages {
            ctx.check()?;
            let has_text = doc
                .has_text_layer(index)
                .map_err(|e| format_error(PAGE_ERROR, e))?;
            let md = if has_text {
                doc.to_markdown(index, &options)
                    .map_err(|e| format_error(PAGE_ERROR, e))?
            } else {
                String::new()
            };
            chars += md.chars().filter(|c| !c.is_whitespace()).count();
            markdown.push(md);
        }
        drop(doc);

        let needs_ocr = pages > 0 && chars < MIN_CHARS_PER_PAGE * pages;
        if needs_ocr {
            tracing::debug!(pages, chars, "pdf text layer below the OCR threshold");
            // the page inventory survives (RD3-5): one empty Page section per page read, no
            // blocks, so no chunks
            let mut extracted = empty(true, true, page_count);
            let mut base = 0u64;
            for (i, md) in markdown.iter().enumerate() {
                let page = u32::try_from(i + 1).unwrap_or(u32::MAX);
                extracted
                    .sections
                    .push(PageWalk::new(page, base).page_only(md.len()));
                base += md.len() as u64 + 1;
            }
            return Ok(extracted);
        }
        if capped {
            tracing::debug!(
                total,
                cap,
                "pdf page cap reached: pages beyond the cap were not read"
            );
        }
        let mut sections = Vec::new();
        let mut base = 0u64;
        for (i, md) in markdown.iter().enumerate() {
            let page = u32::try_from(i + 1).unwrap_or(u32::MAX);
            PageWalk::new(page, base).run(md, &mut sections);
            base += md.len() as u64 + 1;
        }
        let mut extracted = empty(capped, false, page_count);
        extracted.sections = sections;
        extracted.detect_lang();
        Ok(extracted)
    }
}

/// An [`Extracted`] with no sections (title and frontmatter are not read from a PDF) and the
/// total page count the file reports.
fn empty(partial: bool, needs_ocr: bool, page_count: Option<u32>) -> Extracted {
    Extracted {
        title: None,
        frontmatter: Value::Null,
        frontmatter_source: None,
        sections: Vec::new(),
        links: Vec::new(),
        tags: Vec::new(),
        partial,
        needs_ocr,
        page_count,
        lang: None,
    }
}

/// The `heading_path` root of page `page`: `p.N` (D7 chunk cards `file.pdf p.N`).
fn page_label(page: u32) -> String {
    format!("p.{page}")
}

/// Line walker over one page's Markdown export: headings open sections under the page,
/// blank lines close paragraphs, list items group, fences become table rows or code.
struct PageWalk {
    page: u32,
    base: u64,
    /// Enclosing headings on this page: `(markdown level, text)`.
    headings: Vec<(u8, String)>,
    current: SectionDraft,
    /// Open paragraph lines and the byte range they cover.
    para: Vec<String>,
    para_range: (u64, u64),
    /// Open list items and their range.
    list: Vec<String>,
    list_range: (u64, u64),
    /// Open fence: info string, lines, start offset.
    fence: Option<(String, Vec<String>, u64)>,
    /// Open run of tabular-looking lines (RD3-4), their range and their cell-gap count.
    tabular: Vec<String>,
    tabular_range: (u64, u64),
    tabular_gaps: usize,
    /// Closed sections of this page, in order.
    done: Vec<SectionDraft>,
}

impl PageWalk {
    fn new(page: u32, base: u64) -> Self {
        Self {
            page,
            base,
            headings: Vec::new(),
            current: SectionDraft {
                heading_path: vec![page_label(page)],
                level: 1,
                kind: SectionKind::Page,
                byte_start: base,
                byte_end: base,
                page: Some(page),
                blocks: Vec::new(),
            },
            para: Vec::new(),
            para_range: (base, base),
            list: Vec::new(),
            list_range: (base, base),
            fence: None,
            tabular: Vec::new(),
            tabular_range: (base, base),
            tabular_gaps: 0,
            done: Vec::new(),
        }
    }

    /// One empty `Page` section covering this page's slice of the concatenated export (`len`
    /// bytes from `base`): the page inventory of an image-only document (RD3-5).
    fn page_only(mut self, len: usize) -> SectionDraft {
        self.current.byte_end = self.base + len as u64;
        self.current
    }

    fn run(mut self, md: &str, out: &mut Vec<SectionDraft>) {
        let mut at = 0usize;
        for raw in md.split_inclusive('\n') {
            let start = self.base + at as u64;
            at += raw.len();
            let end = self.base + at as u64;
            let line = raw.trim_end_matches(['\n', '\r']);
            self.line(line, start, end);
        }
        let end = self.base + md.len() as u64;
        self.flush_blocks();
        self.close_section(end);
        out.append(&mut self.done);
    }

    fn line(&mut self, line: &str, start: u64, end: u64) {
        if let Some((info, lines, fence_start)) = self.fence.as_mut() {
            if line.trim_start().starts_with("```") {
                let (info, lines, fence_start) =
                    (std::mem::take(info), std::mem::take(lines), *fence_start);
                self.fence = None;
                self.close_fence(&info, &lines, fence_start, end);
            } else {
                lines.push(line.to_owned());
            }
            return;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            self.flush_blocks();
            return;
        }
        if let Some(rest) = trimmed.strip_prefix("```") {
            self.flush_blocks();
            self.fence = Some((rest.trim().to_owned(), Vec::new(), start));
            return;
        }
        if let Some((level, text)) = heading(trimmed) {
            self.flush_blocks();
            self.heading(level, text, start);
            return;
        }
        if let Some(item) = list_item(trimmed) {
            self.flush_tabular();
            self.flush_para();
            if self.list.is_empty() {
                self.list_range.0 = start;
            }
            self.list.push(item.to_owned());
            self.list_range.1 = end;
            return;
        }
        let gaps = cell_gaps(trimmed);
        if gaps >= 2 {
            if !self.tabular.is_empty() && gaps != self.tabular_gaps {
                self.flush_tabular();
            }
            self.flush_list();
            if self.tabular.is_empty() {
                self.tabular_range.0 = start;
                self.tabular_gaps = gaps;
            }
            self.tabular.push(trimmed.to_owned());
            self.tabular_range.1 = end;
            return;
        }
        self.flush_tabular();
        self.flush_list();
        if self.para.is_empty() {
            self.para_range.0 = start;
        }
        self.para.push(trimmed.to_owned());
        self.para_range.1 = end;
    }

    fn heading(&mut self, level: u8, text: &str, at: u64) {
        self.close_section(at);
        while self.headings.last().is_some_and(|(l, _)| *l >= level) {
            self.headings.pop();
        }
        self.headings.push((level, text.to_owned()));
        let mut heading_path = Vec::with_capacity(self.headings.len() + 1);
        heading_path.push(page_label(self.page));
        heading_path.extend(self.headings.iter().map(|(_, t)| t.clone()));
        self.current = SectionDraft {
            heading_path,
            level: level.saturating_add(1),
            kind: SectionKind::Heading,
            byte_start: at,
            byte_end: at,
            page: Some(self.page),
            blocks: Vec::new(),
        };
    }

    fn close_section(&mut self, end: u64) {
        self.current.byte_end = end.max(self.current.byte_start);
        let done = std::mem::replace(
            &mut self.current,
            SectionDraft {
                heading_path: Vec::new(),
                level: 0,
                kind: SectionKind::Heading,
                byte_start: end,
                byte_end: end,
                page: Some(self.page),
                blocks: Vec::new(),
            },
        );
        self.done.push(done);
    }

    fn flush_blocks(&mut self) {
        self.flush_tabular();
        self.flush_para();
        self.flush_list();
    }

    /// Close an open tabular run: two or more lines become one [`Block::TableUnknown`] after
    /// the paragraph that precedes them; a single line is prose and joins the open paragraph.
    fn flush_tabular(&mut self) {
        if self.tabular.is_empty() {
            return;
        }
        let lines = std::mem::take(&mut self.tabular);
        let (start, end) = self.tabular_range;
        self.tabular_gaps = 0;
        if lines.len() >= 2 {
            self.flush_para();
            self.current.blocks.push(Block::TableUnknown {
                text: lines.join("\n"),
                byte_start: start,
                byte_end: end,
            });
            return;
        }
        if self.para.is_empty() {
            self.para_range.0 = start;
        }
        self.para.extend(lines);
        self.para_range.1 = end;
    }

    fn flush_para(&mut self) {
        if self.para.is_empty() {
            return;
        }
        let text = std::mem::take(&mut self.para).join("\n");
        self.current.blocks.push(Block::Paragraph {
            text,
            byte_start: self.para_range.0,
            byte_end: self.para_range.1,
        });
    }

    fn flush_list(&mut self) {
        if self.list.is_empty() {
            return;
        }
        self.current.blocks.push(Block::List {
            items: std::mem::take(&mut self.list),
            byte_start: self.list_range.0,
            byte_end: self.list_range.1,
        });
    }

    /// A closed fence: a GFM pipe table (every non-blank line `| ... |`, at least a header
    /// and one row) becomes table rows, anything else one code block.
    fn close_fence(&mut self, info: &str, lines: &[String], start: u64, end: u64) {
        let rows: Vec<&str> = lines
            .iter()
            .map(|l| l.trim())
            .filter(|l| !l.is_empty())
            .collect();
        let is_table = rows.len() >= 2
            && rows
                .iter()
                .all(|l| l.starts_with('|') && l.ends_with('|') && l.len() >= 2);
        if is_table {
            let mut header = true;
            for row in rows {
                let cells: Vec<String> = row[1..row.len() - 1]
                    .split('|')
                    .map(|c| c.trim().to_owned())
                    .collect();
                if cells.iter().all(|c| is_separator(c)) {
                    continue;
                }
                self.current.blocks.push(Block::TableRow {
                    cells,
                    header,
                    byte_start: start,
                    byte_end: end,
                });
                header = false;
            }
            return;
        }
        self.current.blocks.push(Block::Code {
            lang: (!info.is_empty()).then(|| info.to_owned()),
            text: lines.join("\n"),
            byte_start: start,
            byte_end: end,
        });
    }
}

/// `#`..`######` followed by a space: the level and the heading text.
fn heading(line: &str) -> Option<(u8, &str)> {
    let hashes = line.bytes().take_while(|b| *b == b'#').count();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    let rest = line[hashes..].strip_prefix(' ')?;
    let text = rest.trim().trim_end_matches('#').trim();
    (!text.is_empty()).then(|| (u8::try_from(hashes).unwrap_or(6), text))
}

/// `- item`, `* item`, `+ item` or `1. item`: the item text.
fn list_item(line: &str) -> Option<&str> {
    if let Some(rest) = line
        .strip_prefix("- ")
        .or_else(|| line.strip_prefix("* "))
        .or_else(|| line.strip_prefix("+ "))
    {
        return Some(rest.trim());
    }
    let digits = line.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return None;
    }
    line[digits..].strip_prefix(". ").map(str::trim)
}

/// A GFM alignment cell: `---`, `:---`, `---:`, `:---:`.
fn is_separator(cell: &str) -> bool {
    let inner = cell.trim_start_matches(':').trim_end_matches(':');
    !inner.is_empty() && inner.bytes().all(|b| b == b'-')
}

/// Cell gaps of a line outside a fence (RD3-4): runs of whitespace or `|` between non-gap
/// characters that hold a tab, a `|` or two or more spaces. Single-spaced prose has none; a
/// line with two or more is a candidate table row.
fn cell_gaps(line: &str) -> usize {
    let is_gap = |c: char| c.is_whitespace() || c == '|';
    let mut gaps = 0usize;
    // the open run: (spaces so far, a tab or pipe seen)
    let mut run: Option<(usize, bool)> = None;
    for c in line.trim_matches(is_gap).chars() {
        if is_gap(c) {
            let (spaces, strong) = run.get_or_insert((0, false));
            if c == '\t' || c == '|' {
                *strong = true;
            } else {
                *spaces += 1;
            }
        } else if let Some((spaces, strong)) = run.take()
            && (strong || spaces >= 2)
        {
            gaps += 1;
        }
    }
    gaps
}

#[cfg(test)]
mod tests {
    #![allow(clippy::panic)] // test code: failures are the assertion
    use super::*;

    #[test]
    fn headings_and_list_items_are_recognised() {
        assert_eq!(heading("## Born One"), Some((2, "Born One")));
        assert_eq!(heading("#NoSpace"), None);
        assert_eq!(heading("####### seven"), None);
        assert_eq!(list_item("- a"), Some("a"));
        assert_eq!(list_item("12. twelve"), Some("twelve"));
        assert_eq!(list_item("12.no"), None);
        assert!(is_separator(":----"));
        assert!(!is_separator("Part"));
    }

    #[test]
    fn cell_gaps_count_tabs_pipes_and_runs_of_spaces_but_never_single_spaces() {
        assert_eq!(cell_gaps("Bolt      12       pcs"), 2);
        assert_eq!(cell_gaps("Part\tQty\tUnit"), 2);
        assert_eq!(cell_gaps("| Bolt | 12 | pcs |"), 2);
        assert_eq!(cell_gaps("Bolt 12 pcs"), 0);
        assert_eq!(cell_gaps("two  words"), 1);
        assert_eq!(cell_gaps("single"), 0);
    }

    /// RD3-4 at the walker level (the 0.3.78 export collapses runs of spaces, so no fixture
    /// reaches this branch): two or more consecutive tabular lines with the same gap count
    /// become one `TableUnknown` block in order between the paragraphs around them.
    #[test]
    fn a_run_of_tabular_lines_becomes_one_table_unknown_block_between_the_paragraphs() {
        let md = "## Parts\n\nIntro line.\nPart\tQty\tUnit\nBolt      12       pcs\nNut       30       pcs\n\nAfter the rows.\n";
        let mut out = Vec::new();
        PageWalk::new(1, 0).run(md, &mut out);
        assert_eq!(out.len(), 2, "{out:?}");
        assert_eq!(out[1].heading_path, vec!["p.1", "Parts"]);
        let blocks = &out[1].blocks;
        assert_eq!(blocks.len(), 3, "{blocks:?}");
        assert!(
            matches!(&blocks[0], Block::Paragraph { text, .. } if text == "Intro line."),
            "{blocks:?}"
        );
        match &blocks[1] {
            Block::TableUnknown { text, .. } => {
                assert_eq!(
                    text,
                    "Part\tQty\tUnit\nBolt      12       pcs\nNut       30       pcs"
                );
            }
            other => panic!("expected a table_unknown block, got {other:?}"),
        }
        assert_eq!(blocks[1].range(), (22, 82));
        assert!(
            matches!(&blocks[2], Block::Paragraph { text, .. } if text == "After the rows."),
            "{blocks:?}"
        );
    }

    /// A lone tabular line, or lines whose gap counts differ, are prose: they join the
    /// paragraph in reading order and no `TableUnknown` block appears.
    #[test]
    fn a_single_tabular_line_or_unequal_gap_counts_stay_prose() {
        let md = "Lead in.\nPart\tQty\tUnit\nplain prose line\n\na  b  c\nd  e  f  g\n";
        let mut out = Vec::new();
        PageWalk::new(1, 0).run(md, &mut out);
        assert_eq!(out.len(), 1, "{out:?}");
        let texts: Vec<&str> = out[0]
            .blocks
            .iter()
            .map(|b| match b {
                Block::Paragraph { text, .. } => text.as_str(),
                other => panic!("unexpected block {other:?}"),
            })
            .collect();
        assert_eq!(
            texts,
            vec![
                "Lead in.\nPart\tQty\tUnit\nplain prose line",
                "a  b  c\nd  e  f  g"
            ]
        );
        assert_eq!(out[0].blocks[0].range(), (0, 40));
    }

    /// RD3-5: the page inventory of an image-only document is one empty `Page` section per
    /// page, covering that page's slice of the concatenated export.
    #[test]
    fn page_only_covers_the_pages_slice_and_holds_no_block() {
        let section = PageWalk::new(3, 40).page_only(12);
        assert_eq!(section.heading_path, vec!["p.3"]);
        assert_eq!(section.kind, SectionKind::Page);
        assert_eq!((section.byte_start, section.byte_end), (40, 52));
        assert_eq!(section.page, Some(3));
        assert_eq!(section.blocks, Vec::new());
    }
}
