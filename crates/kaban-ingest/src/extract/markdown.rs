//! Markdown extractor (docs-ingest-spec D1; docs-ingest-plan.md DI1.2): `pulldown-cmark` with
//! YAML metadata blocks, tables, footnotes, strikethrough and task lists, walked through
//! `into_offset_iter` so every block carries its byte range; headings open sections (the
//! heading path is the stack of enclosing headings), paragraphs, code blocks, table rows and
//! lists become [`Block`]s, standard links become `Url` links, and the Obsidian pre-pass
//! (`prepass`) adds wikilinks and tags found outside code, frontmatter, HTML and links.

use super::prepass::{self, Excluded};
use super::{
    AnchorKind, Block, CHECK_EVERY_BYTES, Extracted, Extractor, LinkDraft, SectionDraft, decode,
};
use crate::cancel::{ExtractCtx, ExtractError};
use crate::model::{Format, SectionKind};
use pulldown_cmark::{
    CodeBlockKind, Event, HeadingLevel, MetadataBlockKind, Options, Parser, Tag, TagEnd,
};
use serde_json::Value;
use std::ops::Range;

/// `.md` / `.markdown` through `pulldown-cmark 0.13.6`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MarkdownExtractor;

impl super::private::Sealed for MarkdownExtractor {}

impl Extractor for MarkdownExtractor {
    fn id(&self) -> &'static str {
        "pulldown-cmark@0.13.6"
    }

    fn formats(&self) -> &'static [Format] {
        &[Format::Markdown]
    }

    fn extract(&self, bytes: &[u8], ctx: &ExtractCtx) -> Result<Extracted, ExtractError> {
        ctx.check()?;
        let (text, decode_errors) = decode(bytes);
        let mut walk = Walk::new(&text);
        let options = Options::ENABLE_YAML_STYLE_METADATA_BLOCKS
            | Options::ENABLE_TABLES
            | Options::ENABLE_FOOTNOTES
            | Options::ENABLE_STRIKETHROUGH
            | Options::ENABLE_TASKLISTS;
        let mut next_check = CHECK_EVERY_BYTES;
        for (event, range) in Parser::new_ext(&text, options).into_offset_iter() {
            if range.start >= next_check {
                ctx.check()?;
                next_check = range.start + CHECK_EVERY_BYTES;
            }
            walk.event(event, range);
        }
        let mut extracted = walk.finish(ctx)?;
        extracted.partial |= decode_errors;
        extracted.detect_lang();
        Ok(extracted)
    }
}

/// Where inline text currently goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sink {
    /// Between blocks: text here is unexpected and starts an implicit paragraph.
    None,
    Paragraph,
    Heading,
    Code,
    Cell,
    Item,
}

/// The event walk's state (one per document).
struct Walk<'s> {
    source: &'s str,
    sections: Vec<SectionDraft>,
    current: SectionDraft,
    heading_stack: Vec<(u8, String)>,
    title: Option<String>,
    frontmatter: Value,
    frontmatter_source: Option<String>,
    partial: bool,
    links: Vec<LinkDraft>,
    excluded: Excluded,
    /// Inline text of the open block.
    buf: String,
    /// Start of the open block.
    block_start: usize,
    sink: Sink,
    /// Open heading level (while `sink == Heading`).
    heading_level: u8,
    /// Open code block's language.
    code_lang: Option<String>,
    /// Open table: current row's cells and whether it is the head row.
    cells: Vec<String>,
    in_head: bool,
    /// Open lists: items collected (nested lists flatten into the outermost).
    list_depth: usize,
    items: Vec<String>,
    list_start: usize,
    /// Open metadata block's text.
    meta: Option<String>,
}

impl<'s> Walk<'s> {
    fn new(source: &'s str) -> Self {
        Self {
            source,
            sections: Vec::new(),
            current: SectionDraft {
                heading_path: Vec::new(),
                level: 0,
                kind: SectionKind::Heading,
                byte_start: 0,
                byte_end: 0,
                page: None,
                blocks: Vec::new(),
            },
            heading_stack: Vec::new(),
            title: None,
            frontmatter: Value::Null,
            frontmatter_source: None,
            partial: false,
            links: Vec::new(),
            excluded: Excluded::default(),
            buf: String::new(),
            block_start: 0,
            sink: Sink::None,
            heading_level: 0,
            code_lang: None,
            cells: Vec::new(),
            in_head: false,
            list_depth: 0,
            items: Vec::new(),
            list_start: 0,
            meta: None,
        }
    }

    fn event(&mut self, event: Event<'_>, range: Range<usize>) {
        match event {
            Event::Start(tag) => self.start(tag, range),
            Event::End(tag) => self.end(tag, range),
            Event::Text(t) => {
                if let Some(meta) = &mut self.meta {
                    meta.push_str(&t);
                } else {
                    self.text(&t, range.start);
                }
            }
            Event::Code(t) => {
                self.excluded.push(range.clone());
                self.text(&t, range.start);
            }
            Event::SoftBreak | Event::HardBreak => self.text("\n", range.start),
            Event::Html(_) | Event::InlineHtml(_) => self.excluded.push(range),
            Event::TaskListMarker(done) => {
                self.text(if done { "[x] " } else { "[ ] " }, range.start);
            }
            Event::FootnoteReference(_)
            | Event::Rule
            | Event::InlineMath(_)
            | Event::DisplayMath(_) => {}
        }
    }

    fn text(&mut self, t: &str, at: usize) {
        if self.sink == Sink::None {
            // text outside any block (tight list items and table cells are handled by their
            // sinks; this is the defensive default)
            self.sink = Sink::Paragraph;
            self.block_start = at;
        }
        self.buf.push_str(t);
    }

    fn start(&mut self, tag: Tag<'_>, range: Range<usize>) {
        match tag {
            Tag::MetadataBlock(MetadataBlockKind::YamlStyle) => {
                self.excluded.push(range.clone());
                self.meta = Some(String::new());
                self.close_section(range.start);
                self.current = SectionDraft {
                    heading_path: Vec::new(),
                    level: 0,
                    kind: SectionKind::Frontmatter,
                    byte_start: range.start as u64,
                    byte_end: range.end as u64,
                    page: None,
                    blocks: Vec::new(),
                };
            }
            Tag::Heading { level, .. } => {
                self.close_section(range.start);
                self.sink = Sink::Heading;
                self.heading_level = heading_level(level);
                self.block_start = range.start;
                self.buf.clear();
            }
            Tag::Paragraph => {
                if self.sink == Sink::None {
                    self.sink = Sink::Paragraph;
                    self.block_start = range.start;
                    self.buf.clear();
                } else if self.sink == Sink::Item && !self.buf.is_empty() {
                    // a second paragraph inside a list item
                    self.buf.push('\n');
                }
            }
            Tag::CodeBlock(kind) => {
                self.excluded.push(range.clone());
                self.sink = Sink::Code;
                self.block_start = range.start;
                self.buf.clear();
                self.code_lang = match kind {
                    CodeBlockKind::Fenced(lang) => {
                        let lang = lang.split_whitespace().next().unwrap_or("").to_owned();
                        (!lang.is_empty()).then_some(lang)
                    }
                    CodeBlockKind::Indented => None,
                };
            }
            Tag::TableHead => {
                // the header row has no `TableRow` start of its own
                self.in_head = true;
                self.cells.clear();
                self.block_start = range.start;
            }
            Tag::TableRow => {
                self.cells.clear();
                self.block_start = range.start;
            }
            Tag::TableCell => {
                self.sink = Sink::Cell;
                self.buf.clear();
            }
            Tag::List(_) => {
                if self.list_depth == 0 {
                    self.items.clear();
                    self.list_start = range.start;
                }
                self.list_depth += 1;
            }
            Tag::Item => {
                if !self.buf.is_empty() && self.sink == Sink::Item {
                    // a nested list starts inside this item: flush the item's own text first
                    self.items
                        .push(std::mem::take(&mut self.buf).trim().to_owned());
                }
                self.sink = Sink::Item;
                self.buf.clear();
            }
            Tag::Link { dest_url, .. } => {
                self.excluded.push(range.clone());
                self.links.push(LinkDraft {
                    raw_target: dest_url.into_string(),
                    anchor_kind: AnchorKind::Url,
                    alias: None,
                    embed: false,
                    byte_offset: range.start as u64,
                    byte_end: range.end as u64,
                });
            }
            Tag::Image { dest_url, .. } => {
                self.excluded.push(range.clone());
                self.links.push(LinkDraft {
                    raw_target: dest_url.into_string(),
                    anchor_kind: AnchorKind::Url,
                    alias: None,
                    embed: true,
                    byte_offset: range.start as u64,
                    byte_end: range.end as u64,
                });
            }
            Tag::HtmlBlock => self.excluded.push(range),
            Tag::MetadataBlock(MetadataBlockKind::PlusesStyle)
            | Tag::Table(_)
            | Tag::BlockQuote(_)
            | Tag::FootnoteDefinition(_)
            | Tag::DefinitionList
            | Tag::DefinitionListTitle
            | Tag::DefinitionListDefinition
            | Tag::Emphasis
            | Tag::Strong
            | Tag::Strikethrough
            | Tag::Superscript
            | Tag::Subscript => {}
        }
    }

    fn end(&mut self, tag: TagEnd, range: Range<usize>) {
        match tag {
            TagEnd::MetadataBlock(MetadataBlockKind::YamlStyle) => {
                // kept as written for the gate to admit; never interpreted here (RD2-1)
                self.frontmatter_source = self.meta.take();
                self.close_section(range.end);
            }
            TagEnd::Heading(_) => {
                let text = std::mem::take(&mut self.buf).trim().to_owned();
                let level = self.heading_level;
                self.heading_stack.retain(|(l, _)| *l < level);
                self.heading_stack.push((level, text.clone()));
                if self.title.is_none() {
                    self.title = Some(text);
                }
                self.current = SectionDraft {
                    heading_path: self.heading_stack.iter().map(|(_, t)| t.clone()).collect(),
                    level,
                    kind: SectionKind::Heading,
                    byte_start: range.start as u64,
                    byte_end: range.end as u64,
                    page: None,
                    blocks: Vec::new(),
                };
                self.sink = Sink::None;
            }
            TagEnd::Paragraph => {
                if self.sink == Sink::Paragraph {
                    let text = std::mem::take(&mut self.buf).trim().to_owned();
                    if !text.is_empty() {
                        self.current.blocks.push(Block::Paragraph {
                            text,
                            byte_start: self.block_start as u64,
                            byte_end: range.end as u64,
                        });
                    }
                    self.sink = Sink::None;
                }
            }
            TagEnd::CodeBlock => {
                let text = std::mem::take(&mut self.buf);
                self.current.blocks.push(Block::Code {
                    lang: self.code_lang.take(),
                    text: text.trim_end_matches('\n').to_owned(),
                    byte_start: self.block_start as u64,
                    byte_end: range.end as u64,
                });
                self.sink = Sink::None;
            }
            TagEnd::TableCell => {
                self.cells
                    .push(std::mem::take(&mut self.buf).trim().to_owned());
                self.sink = Sink::None;
            }
            TagEnd::TableHead | TagEnd::TableRow => {
                let header = self.in_head;
                self.in_head = false;
                self.current.blocks.push(Block::TableRow {
                    cells: std::mem::take(&mut self.cells),
                    header,
                    byte_start: self.block_start as u64,
                    byte_end: range.end as u64,
                });
            }
            TagEnd::Item => {
                let text = std::mem::take(&mut self.buf).trim().to_owned();
                if !text.is_empty() {
                    self.items.push(text);
                }
                self.sink = if self.list_depth > 1 {
                    Sink::Item
                } else {
                    Sink::None
                };
            }
            TagEnd::List(_) => {
                self.list_depth = self.list_depth.saturating_sub(1);
                if self.list_depth == 0 {
                    let items = std::mem::take(&mut self.items);
                    if !items.is_empty() {
                        self.current.blocks.push(Block::List {
                            items,
                            byte_start: self.list_start as u64,
                            byte_end: range.end as u64,
                        });
                    }
                    self.sink = Sink::None;
                }
            }
            TagEnd::Table
            | TagEnd::Link
            | TagEnd::Image
            | TagEnd::HtmlBlock
            | TagEnd::BlockQuote(_)
            | TagEnd::FootnoteDefinition
            | TagEnd::DefinitionList
            | TagEnd::DefinitionListTitle
            | TagEnd::DefinitionListDefinition
            | TagEnd::Emphasis
            | TagEnd::Strong
            | TagEnd::Strikethrough
            | TagEnd::Superscript
            | TagEnd::Subscript
            | TagEnd::MetadataBlock(MetadataBlockKind::PlusesStyle) => {}
        }
    }

    /// Push the open section (unless it is an empty preamble) and note where it ended.
    fn close_section(&mut self, end: usize) {
        let is_empty_preamble = self.current.level == 0
            && self.current.kind == SectionKind::Heading
            && self.current.blocks.is_empty();
        if is_empty_preamble {
            self.current.byte_start = end as u64;
            return;
        }
        self.current.byte_end = (end as u64).max(self.current.byte_start);
        let done = std::mem::replace(
            &mut self.current,
            SectionDraft {
                heading_path: Vec::new(),
                level: 0,
                kind: SectionKind::Heading,
                byte_start: end as u64,
                byte_end: end as u64,
                page: None,
                blocks: Vec::new(),
            },
        );
        self.sections.push(done);
    }

    /// Close the last section and run the pre-pass (which checks `ctx` per bounded batch).
    fn finish(mut self, ctx: &ExtractCtx) -> Result<Extracted, ExtractError> {
        self.close_section(self.source.len());
        let excluded = std::mem::take(&mut self.excluded).sorted();
        let (wikilinks, tags) = prepass::scan(self.source, &excluded, ctx)?;
        let mut links = self.links;
        links.extend(wikilinks);
        links.sort_by_key(|l| l.byte_offset);
        Ok(Extracted {
            title: self.title,
            frontmatter: self.frontmatter,
            frontmatter_source: self.frontmatter_source,
            sections: self.sections,
            links,
            tags,
            partial: self.partial,
            needs_ocr: false,
            page_count: None,
            lang: None,
        })
    }
}

fn heading_level(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}
