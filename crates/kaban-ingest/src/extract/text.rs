//! Plain-text extractor (docs-ingest-spec D1 `.txt`; spec §7 source files with
//! `--include-code`): `chardetng` detection and `encoding_rs` decoding when the bytes are not
//! UTF-8, one section, paragraphs split on blank lines (CRLF tolerated).

use super::{Block, CHECK_EVERY_BYTES, Extracted, Extractor, SectionDraft, decode};
use crate::cancel::{ExtractCtx, ExtractError};
use crate::model::{Format, SectionKind};
use serde_json::Value;

/// `.txt` (and `Code` with `--include-code`) as one section of blank-line paragraphs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TextExtractor;

impl super::private::Sealed for TextExtractor {}

impl Extractor for TextExtractor {
    fn id(&self) -> &'static str {
        "chardetng@1.0.0+encoding_rs@0.8.42"
    }

    fn formats(&self) -> &'static [Format] {
        &[Format::Text, Format::Code]
    }

    fn extract(&self, bytes: &[u8], ctx: &ExtractCtx) -> Result<Extracted, ExtractError> {
        ctx.check()?;
        let (text, decode_errors) = decode(bytes);
        let mut blocks = Vec::new();
        let mut next_check = CHECK_EVERY_BYTES;
        for (start, end) in paragraph_ranges(&text) {
            if start >= next_check {
                ctx.check()?;
                next_check = start + CHECK_EVERY_BYTES;
            }
            let para = text[start..end].trim();
            if para.is_empty() {
                continue;
            }
            blocks.push(Block::Paragraph {
                text: para.replace("\r\n", "\n"),
                byte_start: start as u64,
                byte_end: end as u64,
            });
        }
        let mut extracted = Extracted {
            title: None,
            frontmatter: Value::Null,
            frontmatter_source: None,
            sections: vec![SectionDraft {
                heading_path: Vec::new(),
                level: 0,
                kind: SectionKind::Heading,
                byte_start: 0,
                byte_end: text.len() as u64,
                page: None,
                blocks,
            }],
            links: Vec::new(),
            tags: Vec::new(),
            partial: decode_errors,
            needs_ocr: false,
            page_count: None,
            lang: None,
        };
        extracted.detect_lang();
        Ok(extracted)
    }
}

/// Byte ranges of the runs separated by blank lines (a line with only whitespace), LF or CRLF.
fn paragraph_ranges(text: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut start: Option<usize> = None;
    let mut at = 0usize;
    for line in text.split_inclusive('\n') {
        let blank = line.trim().is_empty();
        match (blank, start) {
            (true, Some(s)) => {
                out.push((s, at));
                start = None;
            }
            (false, None) => start = Some(at),
            _ => {}
        }
        at += line.len();
    }
    if let Some(s) = start {
        out.push((s, text.len()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paragraphs_split_on_blank_lines_with_lf_and_crlf() {
        let t = "one\r\nstill one\r\n\r\ntwo\n\n\n  \nthree";
        let ranges = paragraph_ranges(t);
        let paras: Vec<&str> = ranges.iter().map(|(s, e)| t[*s..*e].trim()).collect();
        assert_eq!(paras, vec!["one\r\nstill one", "two", "three"]);
    }
}
