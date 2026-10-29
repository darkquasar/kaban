//! Per-format extractors (docs-ingest-plan.md DI1.2; docs-ingest-spec D1): bytes in, an
//! [`Extracted`] document out (title, frontmatter, section tree of blocks, links, tags). Nothing
//! here is gated: every string an extractor returns is raw document content and passes the
//! denylist (DI1.4) before it is hashed for identity, logged or persisted (core-spec S1).
//!
//! ```text
//!   bytes ==> Extractor::extract(bytes, ctx) ==> Extracted { sections[blocks], links, tags }
//!             |  markdown: decode -> pulldown-cmark events (offsets) + wikilink/tag pre-pass
//!             |  text:     chardetng + encoding_rs -> one section, blank-line paragraphs
//!             |  pdf:      pdf_oxide per page -> Page section, headings under it (DI2.1)
//!             +-- ctx.check() every 64 KiB of input, or per page (cooperative, DI0.3)
//! ```

#[cfg(feature = "md")]
mod markdown;
#[cfg(feature = "pdf")]
mod pdf;
#[cfg(feature = "md")]
mod prepass;
pub mod structured_prep;
mod text;

use crate::cancel::{ExtractCtx, ExtractError};
use crate::model::{Format, SectionKind};
use serde::Serialize;
use serde_json::Value;
use std::borrow::Cow;

#[cfg(feature = "md")]
pub use markdown::MarkdownExtractor;
#[cfg(feature = "pdf")]
pub use pdf::{MIN_CHARS_PER_PAGE, PDF_PAGE_CAP, PdfOxideExtractor};
pub use text::TextExtractor;

/// Input bytes between two cooperative `ctx.check()` calls (docs-ingest-plan.md DI0.3: "markdown
/// per 64 KiB of events").
pub(crate) const CHECK_EVERY_BYTES: usize = 64 * 1024;

mod private {
    /// Seals [`super::Extractor`]: every extractor lives in this crate (`api-sealed-trait`), so
    /// the trait can grow methods for later formats without breaking downstream code.
    pub trait Sealed {}
}

/// A format extractor. Sealed: implementations are this crate's (`md`, `txt`, later `pdf`,
/// `html`, `sheets`, `docx`); callers use them through `&dyn Extractor`.
pub trait Extractor: private::Sealed + Send + Sync {
    /// `crate@version` of the parser (the `extractor` and `extractor_version` props).
    fn id(&self) -> &'static str;
    /// Formats this extractor handles.
    fn formats(&self) -> &'static [Format];
    /// Extract one document. Calls `ctx.check()` between units of work.
    ///
    /// # Errors
    /// `Cancelled`/`DeadlineExceeded` from the context, or a format error. Error messages name
    /// no document content; they are still gated before being stored (`Document.error`).
    fn extract(&self, bytes: &[u8], ctx: &ExtractCtx) -> Result<Extracted, ExtractError>;
}

/// What a wikilink or link points at (docs-ingest-spec D1, D3 `LINKS_TO.anchor_kind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AnchorKind {
    /// `[[Note]]`, `![[embed]]`.
    Note,
    /// `[[Note#Heading]]`.
    Heading,
    /// `[[Note^block]]`.
    Block,
    /// A standard Markdown link destination.
    Url,
}

/// One link found in the document (raw; resolution is DI1.5's).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LinkDraft {
    /// The target as written (`Note#Heading`, `Note^block`, a URL).
    pub raw_target: String,
    /// What the target addresses.
    pub anchor_kind: AnchorKind,
    /// `[[Note|alias]]` display text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    /// `![[embed]]`.
    pub embed: bool,
    /// Byte offset of the link in the extracted text.
    pub byte_offset: u64,
    /// End of the link's source span (exclusive; RD2-3). Not serialized: the DI1.2 snapshot
    /// form predates it and it is positional, not identity.
    #[serde(skip)]
    pub byte_end: u64,
}

impl LinkDraft {
    /// The link's source span.
    pub fn span(&self) -> std::ops::Range<u64> {
        self.byte_offset..self.byte_end.max(self.byte_offset)
    }
}

/// One `#tag` (nested `a/b` kept as written).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TagDraft {
    /// The tag without its `#`.
    pub tag: String,
    /// Byte offset of the `#` in the extracted text.
    pub byte_offset: u64,
    /// End of the tag's source span (exclusive; RD2-3). Not serialized (see [`LinkDraft`]).
    #[serde(skip)]
    pub byte_end: u64,
}

impl TagDraft {
    /// The tag's source span (`#` included).
    pub fn span(&self) -> std::ops::Range<u64> {
        self.byte_offset..self.byte_end.max(self.byte_offset)
    }
}

/// A block of a section (the chunker's input, DI1.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "block")]
pub enum Block {
    /// Prose (a paragraph, a heading-less run of text, a block quote).
    Paragraph {
        /// The text.
        text: String,
        /// Byte range in the extracted text.
        byte_start: u64,
        /// End of the byte range (exclusive).
        byte_end: u64,
    },
    /// A fenced or indented code block (never split by the chunker, D4).
    Code {
        /// Fence language tag, when present.
        #[serde(skip_serializing_if = "Option::is_none")]
        lang: Option<String>,
        /// The code.
        text: String,
        /// Byte range in the extracted text.
        byte_start: u64,
        /// End of the byte range (exclusive).
        byte_end: u64,
    },
    /// One table row; consecutive rows form one table (the first is the header).
    TableRow {
        /// Cell texts.
        cells: Vec<String>,
        /// Header row.
        header: bool,
        /// Byte range in the extracted text.
        byte_start: u64,
        /// End of the byte range (exclusive).
        byte_end: u64,
    },
    /// A list (items flattened to their text).
    List {
        /// Item texts.
        items: Vec<String>,
        /// Byte range in the extracted text.
        byte_start: u64,
        /// End of the byte range (exclusive).
        byte_end: u64,
    },
    /// A run of PDF text lines that look tabular (cells separated by tabs, runs of spaces or
    /// pipes, the same count on every line) but whose table structure the export did not
    /// recover (OD-D6, RD3-4): chunked as text and flagged `table_unknown`, never claimed as a
    /// table.
    TableUnknown {
        /// The lines as exported, joined by newlines.
        text: String,
        /// Byte range in the extracted text.
        byte_start: u64,
        /// End of the byte range (exclusive).
        byte_end: u64,
    },
}

impl Block {
    /// Byte range of the block.
    pub fn range(&self) -> (u64, u64) {
        match self {
            Self::Paragraph {
                byte_start,
                byte_end,
                ..
            }
            | Self::Code {
                byte_start,
                byte_end,
                ..
            }
            | Self::TableRow {
                byte_start,
                byte_end,
                ..
            }
            | Self::List {
                byte_start,
                byte_end,
                ..
            }
            | Self::TableUnknown {
                byte_start,
                byte_end,
                ..
            } => (*byte_start, *byte_end),
        }
    }
}

/// One section of the tree (a heading and what follows it until the next heading of the same
/// or a higher level; the preamble before the first heading; a frontmatter block; a page).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SectionDraft {
    /// Heading texts from the top down to this section (empty for the preamble).
    pub heading_path: Vec<String>,
    /// Heading level (`0` for the preamble and frontmatter).
    pub level: u8,
    /// What the section is.
    pub kind: SectionKind,
    /// Byte range in the extracted text.
    pub byte_start: u64,
    /// End of the byte range (exclusive).
    pub byte_end: u64,
    /// Page for paged formats.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub page: Option<u32>,
    /// The section's blocks in document order.
    pub blocks: Vec<Block>,
}

/// The extractor's output for one document: raw (ungated) content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Extracted {
    /// Frontmatter `title` once the block was interpreted, else the first heading.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Frontmatter as JSON: `Value::Null` until [`Extracted::interpret_frontmatter`] ran on
    /// the admitted source (RD2-1: an extractor never interprets the block itself).
    pub frontmatter: Value,
    /// The frontmatter block's source text (the YAML as written), for the metadata gate to
    /// admit before any interpretation (RD1-3, RD2-1). Not part of the serialized form: raw
    /// content.
    #[serde(skip)]
    pub frontmatter_source: Option<String>,
    /// The section tree in document order.
    pub sections: Vec<SectionDraft>,
    /// Links in document order.
    pub links: Vec<LinkDraft>,
    /// Tags in document order.
    pub tags: Vec<TagDraft>,
    /// Some content could not be extracted (decoding errors, malformed frontmatter).
    pub partial: bool,
    /// A paged document without a text layer (PDF, DI2.1).
    pub needs_ocr: bool,
    /// Total pages the file reports (paged formats only; RD3-5). The pages processed are the
    /// `Page` sections, one per page read: fewer than `page_count` beside `partial: true` means
    /// the page cap stopped the read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub page_count: Option<u32>,
    /// ISO 639-3 code of the detected language, when `whatlang` is confident.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lang: Option<String>,
}

impl Extracted {
    /// Interpret the frontmatter block (core OD-1 YAML) from its admitted source
    /// (RD2-1: the gate's [`crate::gate::AdmittedFrontmatter`] is the only way in): a block
    /// that parses sets `frontmatter` and lets its `title` win over the first heading; one that
    /// does not parse, or whose keys are not strings, marks the document `partial` and leaves
    /// no frontmatter.
    pub fn interpret_frontmatter(&mut self, admitted: &crate::gate::AdmittedFrontmatter<'_>) {
        match yaml_serde::from_str::<yaml_serde::Value>(admitted.source())
            .ok()
            .and_then(|v| serde_json::to_value(v).ok())
        {
            Some(value) => {
                if let Some(title) = value.get("title").and_then(Value::as_str) {
                    self.title = Some(title.to_owned());
                }
                self.frontmatter = value;
            }
            None => self.partial = true,
        }
    }

    /// The prose of every section, for language detection.
    fn prose(&self) -> String {
        let mut out = String::new();
        for s in &self.sections {
            for b in &s.blocks {
                if let Block::Paragraph { text, .. } = b {
                    out.push_str(text);
                    out.push('\n');
                }
            }
        }
        out
    }

    /// Fill `lang` from the prose (`whatlang`, reliable detections only).
    fn detect_lang(&mut self) {
        self.lang = detect_lang(&self.prose());
    }
}

/// ISO 639-3 code of `text`'s language when `whatlang` is confident, else `None` (short or
/// mixed text stays unlabelled rather than mislabelled).
pub fn detect_lang(text: &str) -> Option<String> {
    let sample: String = text.chars().take(4096).collect();
    whatlang::detect(&sample)
        .filter(whatlang::Info::is_reliable)
        .map(|info| info.lang().code().to_owned())
}

/// Decode document bytes: UTF-8 when valid (borrowed, `own-cow-conditional`), otherwise the
/// `chardetng` guess decoded by `encoding_rs`. The flag says whether the decoder replaced
/// malformed sequences (the document is then `partial`). The derived-metadata gate decodes
/// again to slice link and tag spans out of the same text the extractor walked (RD2-3).
pub(crate) fn decode(bytes: &[u8]) -> (Cow<'_, str>, bool) {
    if let Ok(s) = std::str::from_utf8(bytes) {
        return (
            Cow::Borrowed(s.strip_prefix('\u{feff}').unwrap_or(s)),
            false,
        );
    }
    let mut detector = chardetng::EncodingDetector::new(chardetng::Iso2022JpDetection::Deny);
    detector.feed(bytes, true);
    let encoding = detector.guess(None, chardetng::Utf8Detection::Allow);
    let (text, _, had_errors) = encoding.decode(bytes);
    (Cow::Owned(text.into_owned()), had_errors)
}

/// Pick the extractor for a format (`Code` is ingested as text, spec §7; the walker only
/// yields it with `include_code`).
pub fn extractor_for(format: Format) -> Option<&'static dyn Extractor> {
    static TEXT: TextExtractor = TextExtractor;
    #[cfg(feature = "md")]
    static MARKDOWN: MarkdownExtractor = MarkdownExtractor;
    #[cfg(feature = "pdf")]
    static PDF: PdfOxideExtractor = PdfOxideExtractor::new();
    match format {
        #[cfg(feature = "md")]
        Format::Markdown => Some(&MARKDOWN),
        #[cfg(feature = "pdf")]
        Format::Pdf => Some(&PDF),
        Format::Text | Format::Code => Some(&TEXT),
        _ => None,
    }
}
