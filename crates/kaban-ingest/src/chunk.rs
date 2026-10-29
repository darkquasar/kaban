//! Chunker (docs-ingest-plan.md DI1.3; docs-ingest-spec D4): the section tree first, then per
//! section paragraphs and lists accumulate up to `target_tokens`, undersized successive text
//! chunks merge, a fenced code block is one `Code` chunk (never split, `oversize` above the hard
//! max), a table is one `Table` chunk with its header row repeated in `context_prefix`, and a
//! single paragraph group above `hard_max_tokens` is split by `text-splitter` in Markdown mode.
//! Token estimate: `chars / 4` (`TOKEN_ESTIMATOR = "chars4"`, plan §3).
//!
//! ```text
//!   Extracted.sections ==> for each section: blocks --+--> Paragraph/List: accumulate <= target
//!                                                      +--> Code:  flush, one Code chunk
//!                                                      +--> TableRow*: flush, one Table chunk
//!                          then merge undersized Text successors, then split groups > hard max
//!                          every emitted text also goes into the pre-chunk Stream (RD1-5)
//! ```
//!
//! [`chunk_with_stream`] also returns the [`Stream`]: every block's text in document order
//! with the paragraph boundaries the extractor produced (a blank line between blocks), one
//! [`ChunkMap`] per chunk into it and one [`StreamBlock`] per extractor block. The gate scans
//! that stream, so a credential whose rule depends on the whitespace between two paragraphs
//! (`Bearer` + newline + token) is found exactly as it stands in the document, and a hit
//! crossing a chunk span stubs every chunk it touches.
//!
//! The map is exact through every transform (RD2-2): a split piece is a trimmed slice of its
//! group, a merge appends the tail's segments after a synthetic `"\n\n"`, so every byte of a
//! chunk's text either maps to exactly one stream byte or is such a separator. The invariant is
//! checked at the seam ([`ChunkMap::check`]); a chunk whose map fails it is marked inexact and
//! the gate stubs it under any overlapping stream hit instead of trusting a projection.

use crate::extract::{Block, Extracted, SectionDraft};
use crate::model::ChunkKind;
use std::ops::Range;
use thiserror::Error;

/// Default delivery slice size (engine-spec EN-F37, spec D4 [A-2]): 32 KiB.
pub const DEFAULT_SLICE_BYTES: u32 = 32 * 1024;

/// Chunk size policy (D4). Build with [`ChunkConfig::builder`] or take [`ChunkConfig::default`]
/// (512 / 1024 / 64 tokens, 32 KiB slices).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkConfig {
    /// Paragraphs accumulate up to this many estimated tokens.
    pub target_tokens: u32,
    /// A paragraph group above this is split; a fence or table above it is `oversize`.
    pub hard_max_tokens: u32,
    /// A text chunk below this merges into its predecessor with the same section path.
    pub merge_min_tokens: u32,
    /// Delivery slice size for `slices` (spec D4 [A-2]; additive, plan §15 note 1).
    pub slice_bytes: u32,
}

impl Default for ChunkConfig {
    fn default() -> Self {
        Self {
            target_tokens: 512,
            hard_max_tokens: 1024,
            merge_min_tokens: 64,
            slice_bytes: DEFAULT_SLICE_BYTES,
        }
    }
}

impl ChunkConfig {
    /// Start from the defaults and override what you need (`api-builder-pattern`).
    pub fn builder() -> ChunkConfigBuilder {
        ChunkConfigBuilder::default()
    }

    /// `hard_max_tokens` in characters (`chars4`).
    fn hard_max_chars(self) -> usize {
        (self.hard_max_tokens as usize) * 4
    }
}

/// Why a [`ChunkConfigBuilder`] refused to build.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ChunkConfigError {
    /// `target_tokens` must be at least 1 and at most `hard_max_tokens`.
    #[error("target_tokens must be in 1..=hard_max_tokens")]
    Target,
    /// `merge_min_tokens` must not exceed `target_tokens`.
    #[error("merge_min_tokens must not exceed target_tokens")]
    MergeMin,
    /// `slice_bytes` must be at least 1.
    #[error("slice_bytes must be at least 1")]
    SliceBytes,
}

/// Builder for [`ChunkConfig`]; every setter is optional, `build` validates the relations.
#[derive(Debug, Clone, Copy, Default)]
#[must_use = "builders do nothing unless you call build()"]
pub struct ChunkConfigBuilder {
    target_tokens: Option<u32>,
    hard_max_tokens: Option<u32>,
    merge_min_tokens: Option<u32>,
    slice_bytes: Option<u32>,
}

impl ChunkConfigBuilder {
    /// Accumulation target (default 512).
    pub fn target_tokens(mut self, n: u32) -> Self {
        self.target_tokens = Some(n);
        self
    }
    /// Split threshold (default 1024).
    pub fn hard_max_tokens(mut self, n: u32) -> Self {
        self.hard_max_tokens = Some(n);
        self
    }
    /// Merge threshold (default 64).
    pub fn merge_min_tokens(mut self, n: u32) -> Self {
        self.merge_min_tokens = Some(n);
        self
    }
    /// Delivery slice size in bytes (default 32 KiB).
    pub fn slice_bytes(mut self, n: u32) -> Self {
        self.slice_bytes = Some(n);
        self
    }
    /// Validate and build.
    ///
    /// # Errors
    /// [`ChunkConfigError`] when the relations do not hold.
    pub fn build(self) -> Result<ChunkConfig, ChunkConfigError> {
        let d = ChunkConfig::default();
        let cfg = ChunkConfig {
            target_tokens: self.target_tokens.unwrap_or(d.target_tokens),
            hard_max_tokens: self.hard_max_tokens.unwrap_or(d.hard_max_tokens),
            merge_min_tokens: self.merge_min_tokens.unwrap_or(d.merge_min_tokens),
            slice_bytes: self.slice_bytes.unwrap_or(d.slice_bytes),
        };
        if cfg.target_tokens == 0 || cfg.target_tokens > cfg.hard_max_tokens {
            return Err(ChunkConfigError::Target);
        }
        if cfg.merge_min_tokens > cfg.target_tokens {
            return Err(ChunkConfigError::MergeMin);
        }
        if cfg.slice_bytes == 0 {
            return Err(ChunkConfigError::SliceBytes);
        }
        Ok(cfg)
    }
}

/// A chunk before gating and identity: raw text, kind, positional props and its prefix. The
/// gate (DI1.4) masks or stubs `text`, then `text_hash`, `token_estimate`, `bytes` and
/// `slices` are measured on the sanitized text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkDraft {
    /// Raw chunk text.
    pub text: String,
    /// What the chunk holds.
    pub kind: ChunkKind,
    /// Byte range in the extracted text (positional, not identity).
    pub byte_start: u64,
    /// End of the byte range (exclusive).
    pub byte_end: u64,
    /// Page for paged formats.
    pub page: Option<u32>,
    /// `"<title> > <h1> > <h2>"`, plus `" > <header cells>"` for a table.
    pub context_prefix: String,
    /// Above `hard_max_tokens` and kept whole (fence or table, D4).
    pub oversize: bool,
    /// PDF rows that may be a table (DI2.1, OD-D6).
    pub table_unknown: bool,
}

/// `chars / 4`, saturating.
pub fn estimate_tokens(text: &str) -> u32 {
    u32::try_from(text.chars().count() / 4).unwrap_or(u32::MAX)
}

/// Estimate of `head + "\n\n" + tail` without building it: the characters of both parts and of
/// the separator are counted before the floor division, so two separately floored estimates
/// cannot understate the merged text (RD1-7).
fn estimate_joined(head: &str, tail: &str) -> u32 {
    let chars = head
        .chars()
        .count()
        .saturating_add(2)
        .saturating_add(tail.chars().count());
    u32::try_from(chars / 4).unwrap_or(u32::MAX)
}

/// Delivery slices of `text` at `slice_bytes` per slice (spec D4 [A-2], engine-spec EN-F37):
/// every slice ends on a char boundary, cut back from `slice_bytes` to the last boundary at or
/// before it, so the count is measured on the sanitized string itself, not on its byte length
/// (RD1-8). A limit narrower than one scalar's UTF-8 width delivers that scalar whole, one per
/// slice (a slice never splits a scalar and never is empty). 0 for empty text.
pub fn slices_for(text: &str, slice_bytes: u32) -> u16 {
    let per = (slice_bytes.max(1)) as usize;
    u16::try_from(slice_ends(text, per).count()).unwrap_or(u16::MAX)
}

/// End offsets of the delivery slices of `text`: the boundary algorithm shared by the count and
/// by a delivery (`per` bytes, cut back to a char boundary, at least one scalar per slice).
fn slice_ends(text: &str, per: usize) -> impl Iterator<Item = usize> + '_ {
    let mut pos = 0usize;
    std::iter::from_fn(move || {
        if pos >= text.len() {
            return None;
        }
        let mut end = pos.saturating_add(per).min(text.len());
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        if end == pos {
            // the limit is narrower than this scalar: deliver it whole
            end = text[pos..]
                .chars()
                .next()
                .map_or(text.len(), |c| pos + c.len_utf8());
        }
        pos = end;
        Some(end)
    })
}

/// `"<title> > <h1> > <h2>"`: the title (when it is not already the first heading) followed by
/// the heading path.
fn context_prefix(title: Option<&str>, heading_path: &[String]) -> String {
    let mut parts: Vec<&str> = Vec::with_capacity(heading_path.len() + 1);
    if let Some(t) = title
        && heading_path.first().is_none_or(|h| h != t)
    {
        parts.push(t);
    }
    parts.extend(heading_path.iter().map(String::as_str));
    parts.join(" > ")
}

/// One run of chunk text copied verbatim from the stream: `text[output] == stream[source]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    /// Byte range in the stream text.
    pub source: Range<usize>,
    /// Byte range in the chunk text.
    pub output: Range<usize>,
}

/// The exact source-to-output mapping of one chunk (RD2-2). Segments are in order in both
/// coordinates and never overlap; the output gaps between consecutive segments are the
/// `"\n\n"` separators a merge inserted and map to no stream byte; every other byte of the
/// chunk text maps to exactly one stream byte.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChunkMap {
    segments: Vec<Segment>,
    /// `false` when [`ChunkMap::check`] failed at the seam: the segments then hold the hull
    /// only and the gate must not trust a projection through them.
    exact: bool,
}

impl ChunkMap {
    /// A chunk whose text is `stream[source]` verbatim.
    fn single(source: Range<usize>) -> Self {
        Self {
            segments: vec![Segment {
                output: 0..source.len(),
                source,
            }],
            exact: true,
        }
    }

    /// The segments, in order.
    pub fn segments(&self) -> &[Segment] {
        &self.segments
    }

    /// Whether the mapping passed the seam invariant (see the module doc).
    pub fn is_exact(&self) -> bool {
        self.exact
    }

    /// The hull of the chunk in the stream (first segment start to last segment end).
    pub fn source_span(&self) -> Range<usize> {
        match (self.segments.first(), self.segments.last()) {
            (Some(a), Some(b)) => a.source.start..b.source.end,
            _ => 0..0,
        }
    }

    /// Append `tail`'s segments after a separator of `separator` bytes written at output
    /// offset `at` (the length of the text before the separator).
    fn append(&mut self, tail: &Self, at: usize, separator: usize) {
        let shift = at + separator;
        self.segments.extend(tail.segments.iter().map(|s| Segment {
            source: s.source.clone(),
            output: s.output.start + shift..s.output.end + shift,
        }));
        self.exact &= tail.exact;
    }

    /// Project a stream range onto the chunk text: one output range per segment it touches.
    pub fn project<'a>(&'a self, hit: &'a Range<usize>) -> impl Iterator<Item = Range<usize>> + 'a {
        self.segments
            .iter()
            .filter(move |s| hit.start < s.source.end && hit.end > s.source.start)
            .map(move |s| {
                let from = hit.start.max(s.source.start) - s.source.start;
                let to = hit.end.min(s.source.end) - s.source.start;
                s.output.start + from..s.output.start + to
            })
    }

    /// The inverse: a chunk-text range back onto the stream, one range per segment it touches
    /// (separator bytes map to nothing).
    pub fn to_source<'a>(
        &'a self,
        out: &'a Range<usize>,
    ) -> impl Iterator<Item = Range<usize>> + 'a {
        self.segments
            .iter()
            .filter(move |s| out.start < s.output.end && out.end > s.output.start)
            .map(move |s| {
                let from = out.start.max(s.output.start) - s.output.start;
                let to = out.end.min(s.output.end) - s.output.start;
                s.source.start + from..s.source.start + to
            })
    }

    /// The seam invariant: every segment is a verbatim slice, segments are ordered and
    /// disjoint in both coordinates, output gaps are exactly `"\n\n"` and the segments cover
    /// the chunk text to its end.
    pub fn check(&self, text: &str, stream: &str) -> bool {
        let mut out_pos = 0usize;
        let mut src_pos = 0usize;
        for s in &self.segments {
            let ordered = s.output.start >= out_pos && s.source.start >= src_pos;
            let gap_ok =
                s.output.start == out_pos || text.get(out_pos..s.output.start) == Some("\n\n");
            let verbatim = text
                .get(s.output.clone())
                .is_some_and(|t| stream.get(s.source.clone()) == Some(t));
            if !(ordered && gap_ok && verbatim) {
                return false;
            }
            out_pos = s.output.end;
            src_pos = s.source.end;
        }
        out_pos == text.len()
    }

    /// Keep an exact map; replace a failed one by its inexact hull (fail closed at the gate).
    fn verified(self, text: &str, stream: &str) -> Self {
        if self.check(text, stream) {
            return self;
        }
        debug_assert!(false, "chunk map invariant violated at the chunker seam");
        let hull = self.source_span();
        Self {
            segments: vec![Segment {
                output: 0..text.len(),
                source: hull,
            }],
            exact: false,
        }
    }
}

/// One extractor block as it sits in the stream, with its byte range in the extracted source
/// text (RD2-3: derived links and tags are located by source span).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamBlock {
    /// Byte range in the stream text.
    pub stream: Range<usize>,
    /// Byte range in the extracted source text.
    pub source: Range<u64>,
}

/// The pre-chunk extracted stream the gate scans (RD1-5): every chunk's source text in document
/// order, separated by the blank line the extractor saw between blocks, one [`ChunkMap`] per
/// chunk (flattened in document order, one per [`ChunkDraft`]) and one [`StreamBlock`] per
/// extractor block. Pieces of one split paragraph group are contiguous in the stream, as in
/// the group.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Stream {
    /// The stream text.
    pub text: String,
    /// The i-th chunk's mapping into `text` (before gating), for every chunk in order.
    pub maps: Vec<ChunkMap>,
    /// Every extractor block in order, with its source range.
    pub blocks: Vec<StreamBlock>,
}

impl Stream {
    /// Append one emitted text after a paragraph boundary; returns its span.
    fn push(&mut self, text: &str) -> Range<usize> {
        if !self.text.is_empty() {
            self.text.push_str("\n\n");
        }
        let start = self.text.len();
        self.text.push_str(text);
        start..self.text.len()
    }

    /// Record one extractor block at `stream` with its source range.
    fn block(&mut self, stream: Range<usize>, source: (u64, u64)) {
        self.blocks.push(StreamBlock {
            stream,
            source: source.0..source.1,
        });
    }

    /// A contiguous layout over already-built chunk texts: the i-th map is where the i-th text
    /// sits in their separator-less concatenation (the gate's legacy input, see
    /// `gate::stream_text`). No blocks: no source ranges are known.
    pub(crate) fn contiguous(texts: impl IntoIterator<Item = impl AsRef<str>>) -> Self {
        let mut out = Self::default();
        for t in texts {
            let start = out.text.len();
            out.text.push_str(t.as_ref());
            out.maps.push(ChunkMap::single(start..out.text.len()));
        }
        out
    }
}

/// Sections with their chunks, plus the stream the chunks were cut from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunked {
    /// Sections in order, each with its chunks in document order.
    pub sections: Vec<(SectionDraft, Vec<ChunkDraft>)>,
    /// The pre-chunk stream with one span per chunk.
    pub stream: Stream,
}

/// Chunk every section of `extracted` (D4). Sections keep their order; each section's chunks
/// are in document order.
pub fn chunk(extracted: &Extracted, cfg: &ChunkConfig) -> Vec<(SectionDraft, Vec<ChunkDraft>)> {
    chunk_with_stream(extracted, cfg).sections
}

/// [`chunk`] plus the [`Stream`] the gate scans.
pub fn chunk_with_stream(extracted: &Extracted, cfg: &ChunkConfig) -> Chunked {
    let mut stream = Stream::default();
    let sections = extracted
        .sections
        .iter()
        .map(|section| {
            let prefix = context_prefix(extracted.title.as_deref(), &section.heading_path);
            let chunks = chunk_section(section, &prefix, cfg, &mut stream);
            (section.clone(), chunks)
        })
        .collect();
    Chunked { sections, stream }
}

/// A chunk and its mapping into the stream, while a section is being chunked.
struct Emitted {
    draft: ChunkDraft,
    map: ChunkMap,
}

/// Paragraphs and lists accumulating into one text chunk.
#[derive(Default)]
struct Group {
    text: String,
    start: u64,
    end: u64,
    /// Each block's range in `text` with its source range (the stream's [`StreamBlock`]s).
    blocks: Vec<(Range<usize>, (u64, u64))>,
    /// The group holds a [`Block::TableUnknown`] (flushed alone, RD3-4): its drafts carry the
    /// flag.
    table_unknown: bool,
}

impl Group {
    fn push(&mut self, text: &str, range: (u64, u64)) {
        if self.text.is_empty() {
            self.start = range.0;
        } else {
            self.text.push_str("\n\n");
        }
        let at = self.text.len();
        self.text.push_str(text);
        self.blocks.push((at..self.text.len(), range));
        self.end = range.1;
    }

    /// Emit the group as one text chunk, or, above `hard_max_tokens`, as the pieces
    /// `text-splitter` (Markdown mode) cuts at `hard_max_chars` (D4). Every piece keeps the
    /// group's byte range: positional only, not identity. Every piece is a slice of the group
    /// (trimmed at both ends), so its map is the one segment at that slice of the stream.
    fn flush(
        &mut self,
        prefix: &str,
        section: &SectionDraft,
        cfg: &ChunkConfig,
        stream: &mut Stream,
        out: &mut Vec<Emitted>,
    ) {
        if self.text.is_empty() {
            return;
        }
        let text = std::mem::take(&mut self.text);
        let table_unknown = std::mem::take(&mut self.table_unknown);
        let group = stream.push(&text);
        for (range, source) in std::mem::take(&mut self.blocks) {
            stream.block(group.start + range.start..group.start + range.end, source);
        }
        let whole = 0..text.len();
        let pieces: Vec<Range<usize>> = if estimate_tokens(&text) > cfg.hard_max_tokens {
            split_markdown(&text, cfg.hard_max_chars())
        } else {
            std::iter::once(whole).collect()
        };
        for piece in pieces {
            let span = group.start + piece.start..group.start + piece.end;
            out.push(Emitted {
                draft: ChunkDraft {
                    text: text[piece].to_owned(),
                    kind: ChunkKind::Text,
                    byte_start: self.start,
                    byte_end: self.end,
                    page: section.page,
                    context_prefix: prefix.to_owned(),
                    oversize: false,
                    table_unknown,
                },
                map: ChunkMap::single(span),
            });
        }
    }
}

/// Split one paragraph group above the hard max into pieces of at most `max_chars` characters
/// along Markdown semantic boundaries (`text-splitter`, opt-in feature `md-splitter`: its
/// `icu_segmenter` data costs about 4.5 MB of binary, DI1.6 N1 gate). Pieces are byte ranges
/// of `text` (the splitter yields slices of its input).
#[cfg(feature = "md-splitter")]
fn split_markdown(text: &str, max_chars: usize) -> Vec<Range<usize>> {
    let splitter = text_splitter::MarkdownSplitter::new(max_chars.max(1));
    splitter
        .chunk_indices(text)
        .map(|(at, piece)| at..at + piece.len())
        .collect()
}

/// The default cut: at the nearest whitespace before `max_chars` characters (the plan's
/// fallback; `md-splitter` replaces it with the Markdown-aware splitter). Pieces are byte
/// ranges of `text`, trimmed at both ends; a piece that is whitespace only is dropped.
#[cfg(not(feature = "md-splitter"))]
fn split_markdown(text: &str, max_chars: usize) -> Vec<Range<usize>> {
    let max_chars = max_chars.max(1);
    let mut out = Vec::new();
    let mut base = 0usize; // byte offset of `rest` in `text`
    let mut rest = text;
    while rest.chars().count() > max_chars {
        let limit: usize = rest
            .char_indices()
            .nth(max_chars)
            .map_or(rest.len(), |(i, _)| i);
        let cut = rest[..limit]
            .rfind(char::is_whitespace)
            .unwrap_or(limit)
            .max(1);
        let (head, tail) = rest.split_at(cut);
        push_trimmed(&mut out, head, base);
        let skipped = tail.len() - tail.trim_start().len();
        base += cut + skipped;
        rest = tail.trim_start();
    }
    push_trimmed(&mut out, rest, base);
    out
}

/// The trimmed range of `piece` (which starts at byte `base` of the group text), unless empty.
#[cfg(not(feature = "md-splitter"))]
fn push_trimmed(out: &mut Vec<Range<usize>>, piece: &str, base: usize) {
    let trimmed = piece.trim();
    if trimmed.is_empty() {
        return;
    }
    let start = base + (piece.len() - piece.trim_start().len());
    out.push(start..start + trimmed.len());
}

/// Consecutive table rows accumulating into one `Table` chunk.
#[derive(Default)]
struct Table {
    header: Vec<String>,
    lines: Vec<String>,
    start: u64,
    end: u64,
}

impl Table {
    fn push(&mut self, cells: &[String], header: bool, range: (u64, u64)) {
        if self.lines.is_empty() {
            self.start = range.0;
        }
        if header && self.header.is_empty() {
            self.header = cells.to_vec();
        }
        self.lines.push(format!("| {} |", cells.join(" | ")));
        self.end = range.1;
    }

    fn flush(
        &mut self,
        prefix: &str,
        section: &SectionDraft,
        cfg: &ChunkConfig,
        stream: &mut Stream,
        out: &mut Vec<Emitted>,
    ) {
        if self.lines.is_empty() {
            return;
        }
        let text = std::mem::take(&mut self.lines).join("\n");
        let span = stream.push(&text);
        stream.block(span.clone(), (self.start, self.end));
        let header = std::mem::take(&mut self.header);
        let context_prefix = if header.is_empty() {
            prefix.to_owned()
        } else if prefix.is_empty() {
            header.join(" | ")
        } else {
            format!("{prefix} > {}", header.join(" | "))
        };
        out.push(Emitted {
            draft: ChunkDraft {
                oversize: estimate_tokens(&text) > cfg.hard_max_tokens,
                text,
                kind: ChunkKind::Table,
                byte_start: self.start,
                byte_end: self.end,
                page: section.page,
                context_prefix,
                table_unknown: false,
            },
            map: ChunkMap::single(span),
        });
    }
}

fn chunk_section(
    section: &SectionDraft,
    prefix: &str,
    cfg: &ChunkConfig,
    stream: &mut Stream,
) -> Vec<ChunkDraft> {
    let mut out = Vec::new();
    let mut group = Group::default();
    let mut table = Table::default();
    for block in &section.blocks {
        if !matches!(block, Block::TableRow { .. }) {
            table.flush(prefix, section, cfg, stream, &mut out);
        }
        match block {
            Block::Paragraph { text, .. } => {
                if !group.text.is_empty() && estimate_joined(&group.text, text) > cfg.target_tokens
                {
                    group.flush(prefix, section, cfg, stream, &mut out);
                }
                group.push(text, block.range());
            }
            Block::List { items, .. } => {
                let text = items
                    .iter()
                    .map(|i| format!("- {i}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                if !group.text.is_empty() && estimate_joined(&group.text, &text) > cfg.target_tokens
                {
                    group.flush(prefix, section, cfg, stream, &mut out);
                }
                group.push(&text, block.range());
            }
            Block::Code { lang, text, .. } => {
                group.flush(prefix, section, cfg, stream, &mut out);
                let (byte_start, byte_end) = block.range();
                let span = stream.push(text);
                stream.block(span.clone(), (byte_start, byte_end));
                out.push(Emitted {
                    draft: ChunkDraft {
                        text: text.clone(),
                        kind: ChunkKind::Code { lang: lang.clone() },
                        byte_start,
                        byte_end,
                        page: section.page,
                        context_prefix: prefix.to_owned(),
                        oversize: estimate_tokens(text) > cfg.hard_max_tokens,
                        table_unknown: false,
                    },
                    map: ChunkMap::single(span),
                });
            }
            Block::TableRow { cells, header, .. } => {
                group.flush(prefix, section, cfg, stream, &mut out);
                table.push(cells, *header, block.range());
            }
            // RD3-4: the rows alone as one text chunk flagged `table_unknown`, through the same
            // `stream.push`/`stream.block` path as a paragraph (the RD2-2 seam invariant holds)
            Block::TableUnknown { text, .. } => {
                group.flush(prefix, section, cfg, stream, &mut out);
                group.table_unknown = true;
                group.push(text, block.range());
                group.flush(prefix, section, cfg, stream, &mut out);
            }
        }
    }
    table.flush(prefix, section, cfg, stream, &mut out);
    group.flush(prefix, section, cfg, stream, &mut out);
    let merged = merge_undersized(out, cfg);
    let mut drafts = Vec::with_capacity(merged.len());
    for e in merged {
        // the seam invariant (RD2-2): checked for every chunk, exact or marked inexact
        stream
            .maps
            .push(e.map.verified(&e.draft.text, &stream.text));
        drafts.push(e.draft);
    }
    drafts
}

/// D4: a text chunk below `merge_min_tokens` merges into the text chunk right before it (same
/// section, so the same section path) when the merged text, separator included, stays within
/// the hard max (RD1-7); code and table chunks are never merged into or across.
fn merge_undersized(chunks: Vec<Emitted>, cfg: &ChunkConfig) -> Vec<Emitted> {
    let mut out: Vec<Emitted> = Vec::with_capacity(chunks.len());
    for c in chunks {
        if c.draft.kind == ChunkKind::Text
            && estimate_tokens(&c.draft.text) < cfg.merge_min_tokens
            && let Some(prev) = out.last_mut()
            && prev.draft.kind == ChunkKind::Text
            && estimate_joined(&prev.draft.text, &c.draft.text) <= cfg.hard_max_tokens
        {
            // the tail's segments follow the synthetic separator; the trimmed whitespace between
            // the pieces in the stream maps to nothing (RD2-2)
            prev.map.append(&c.map, prev.draft.text.len(), 2);
            prev.draft.text.push_str("\n\n");
            prev.draft.text.push_str(&c.draft.text);
            prev.draft.byte_end = c.draft.byte_end;
            // the uncertainty survives the merge (RD3-4): a chunk holding tabular rows stays
            // flagged whichever side they came from
            prev.draft.table_unknown |= c.draft.table_unknown;
            continue;
        }
        out.push(c);
    }
    out
}
