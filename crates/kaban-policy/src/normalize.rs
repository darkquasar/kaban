//! Normalized text views with byte maps back to the original string.
//!
//! * `nfkc`: NFKC, ignorable code points stripped, case preserved. The keyword prefilter and
//!   the regex tier run on this view (core-spec S2).
//! * `sep` / `nosep`: NFKD + casefold, ignorables stripped, separators collapsed to one space or
//!   removed entirely. Literal (word/phrase) matching runs on these views.
//!
//! Every emitted byte maps back to the original char that produced it, so hits are reported as
//! raw byte offsets on char boundaries.

use crate::confusables::fold;
use unicode_normalization::UnicodeNormalization;

/// Normalized view of a string plus a map from normalized byte offset to original byte offset.
pub(crate) struct Normalized {
    pub(crate) text: String,
    /// `map[i]` = original byte offset where normalized byte i starts; `map[len]` = original len.
    pub(crate) map: Vec<usize>,
}

impl Normalized {
    /// Map a half-open byte span of the normalized text back to a half-open span of `raw`.
    /// The end is the end of the original char that produced the last normalized byte.
    pub(crate) fn raw_span(&self, raw: &str, start: usize, end: usize) -> (usize, usize) {
        let rs = self.map[start];
        if end <= start {
            return (rs, rs);
        }
        let last_orig = self.map[end - 1];
        let re = last_orig + raw[last_orig..].chars().next().map_or(1, char::len_utf8);
        (rs, re.min(raw.len()))
    }
}

/// All normalized views of one input string, built once per scan.
pub(crate) struct Views<'a> {
    pub(crate) raw: &'a str,
    pub(crate) nfkc: Normalized,
    pub(crate) sep: Normalized,
    pub(crate) nosep: Normalized,
}

impl<'a> Views<'a> {
    pub(crate) fn new(raw: &'a str) -> Self {
        Self {
            raw,
            nfkc: nfkc(raw),
            sep: normalize(raw, true),
            nosep: normalize(raw, false),
        }
    }
}

fn is_ignorable(c: char) -> bool {
    matches!(
        c,
        '\u{00AD}'
            | '\u{200B}'..='\u{200F}'
            | '\u{2028}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}'
            | '\u{FEFF}'
            | '\u{180E}'
            | '\u{034F}'
            | '\u{061C}'
    ) || ('\u{0300}'..='\u{036F}').contains(&c)
        || ('\u{FE00}'..='\u{FE0F}').contains(&c)
        || ('\u{20D0}'..='\u{20FF}').contains(&c)
}

fn is_separator(c: char) -> bool {
    c.is_whitespace()
        || matches!(
            c,
            '-' | '_'
                | '.'
                | '/'
                | '\\'
                | ':'
                | ','
                | ';'
                | '|'
                | '·'
                | '•'
                | '*'
                | '~'
                | '\''
                | '"'
                | '`'
                | '('
                | ')'
                | '['
                | ']'
                | '{'
                | '}'
        )
}

/// NFKC with ignorable code points stripped; case and separators preserved; a backslash that
/// escapes ASCII punctuation (markdown `api\_key`) is dropped. The regex tier and the keyword
/// prefilter run on this view.
pub(crate) fn nfkc(s: &str) -> Normalized {
    if s.is_ascii() {
        return nfkc_ascii(s);
    }
    nfkc_unicode(s)
}

/// [`nfkc`] for pure-ASCII input: NFKC is the identity and no ASCII char is ignorable, so only
/// the escaping backslashes are dropped. Byte-identical to [`nfkc_unicode`] (checked by the
/// `ascii_fast_paths_match_the_generic_ones` property); building the views is most of a scan's
/// cost (M2 review R2-6), and nearly every string the service sees is ASCII.
fn nfkc_ascii(s: &str) -> Normalized {
    let bytes = s.as_bytes();
    let mut text = String::with_capacity(s.len());
    let mut map = Vec::with_capacity(s.len() + 1);
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'\\' && bytes.get(i + 1).is_some_and(u8::is_ascii_punctuation) {
            continue;
        }
        text.push(b as char);
        map.push(i);
    }
    map.push(s.len());
    Normalized { text, map }
}

fn nfkc_unicode(s: &str) -> Normalized {
    let mut text = String::with_capacity(s.len());
    let mut map = Vec::with_capacity(s.len() + 1);
    let mut chars = s.char_indices().peekable();
    while let Some((orig_idx, ch)) = chars.next() {
        if ch == '\\' && chars.peek().is_some_and(|(_, n)| n.is_ascii_punctuation()) {
            continue;
        }
        for n in ch.nfkc() {
            if is_ignorable(n) {
                continue;
            }
            let start = text.len();
            text.push(n);
            for _ in start..text.len() {
                map.push(orig_idx);
            }
        }
    }
    map.push(s.len());
    Normalized { text, map }
}

/// NFKD + casefold + confusables fold + strip ignorables; separators either collapsed to one
/// space (`keep_sep`) or removed entirely. Every emitted byte maps back to the original char
/// that produced it.
pub(crate) fn normalize(s: &str, keep_sep: bool) -> Normalized {
    if s.is_ascii() {
        return normalize_ascii(s, keep_sep);
    }
    normalize_unicode(s, keep_sep)
}

/// [`normalize`] for pure-ASCII input: NFKD is the identity, no ASCII char is ignorable or in
/// the confusables table, and Unicode lowercasing of ASCII is ASCII lowercasing. Byte-identical
/// to [`normalize_unicode`] (property-tested).
fn normalize_ascii(s: &str, keep_sep: bool) -> Normalized {
    let mut text = String::with_capacity(s.len());
    let mut map = Vec::with_capacity(s.len() + 1);
    let mut last_sep = true; // suppress leading separator
    for (i, &b) in s.as_bytes().iter().enumerate() {
        if is_separator(b as char) {
            if keep_sep && !last_sep {
                text.push(' ');
                map.push(i);
                last_sep = true;
            }
            continue;
        }
        last_sep = false;
        text.push(b.to_ascii_lowercase() as char);
        map.push(i);
    }
    if keep_sep && text.ends_with(' ') {
        text.pop();
        map.pop();
    }
    map.push(s.len());
    Normalized { text, map }
}

fn normalize_unicode(s: &str, keep_sep: bool) -> Normalized {
    let mut text = String::with_capacity(s.len());
    let mut map = Vec::with_capacity(s.len() + 1);
    let mut last_sep = true; // suppress leading separator
    for (orig_idx, ch) in s.char_indices() {
        for n in ch.nfkd() {
            if is_ignorable(n) {
                continue;
            }
            if is_separator(n) {
                if keep_sep && !last_sep {
                    text.push(' ');
                    map.push(orig_idx);
                    last_sep = true;
                }
                continue;
            }
            last_sep = false;
            let start = text.len();
            // fold before casefolding: confusables are case-specific (Greek Ν is N, ν is v)
            match fold(n) {
                Some(k) => text.extend(k.chars().map(|c| c.to_ascii_lowercase())),
                None => text.extend(n.to_lowercase()),
            }
            for _ in start..text.len() {
                map.push(orig_idx);
            }
        }
    }
    if keep_sep && text.ends_with(' ') {
        text.pop();
        map.pop();
    }
    map.push(s.len());
    Normalized { text, map }
}

/// The normalized view of a term as text: NFKD, casefold, confusables fold, ignorables
/// stripped; separators collapsed to one space (`keep_sep`) or removed. The same view the
/// literal matcher uses, exported for identity keys built from titles (F1 `dup_key`, M3-3).
pub fn norm_term(t: &str, keep_sep: bool) -> String {
    normalize(t, keep_sep).text
}

/// A policy-normalized view bound to its original UTF-8 input.
pub struct NormalizedText<'a> {
    raw: &'a str,
    view: Normalized,
}

impl NormalizedText<'_> {
    /// NFKD/casefold/confusable/ignorable-folded text.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.view.text
    }

    /// Map valid normalized byte boundaries to the original UTF-8 span.
    #[must_use]
    pub fn raw_span(&self, span: std::ops::Range<usize>) -> Option<std::ops::Range<usize>> {
        if span.start > span.end
            || span.end > self.view.text.len()
            || !self.view.text.is_char_boundary(span.start)
            || !self.view.text.is_char_boundary(span.end)
        {
            return None;
        }
        let (start, end) = self.view.raw_span(self.raw, span.start, span.end);
        Some(start..end)
    }
}

/// Normalize with the exact policy pipeline and retain safe original-byte offsets.
#[must_use]
pub fn norm_with_offsets(raw: &str, keep_sep: bool) -> NormalizedText<'_> {
    NormalizedText {
        raw,
        view: normalize(raw, keep_sep),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion
mod tests {
    use super::*;

    fn same(a: &Normalized, b: &Normalized) -> bool {
        a.text == b.text && a.map == b.map
    }

    proptest::proptest! {
        /// The ASCII fast paths (R2-6) produce exactly the views the generic Unicode paths do:
        /// text and byte map alike, with and without separators, including escaping backslashes.
        #[test]
        fn ascii_fast_paths_match_the_generic_ones(s in "[ -~]{0,64}") {
            proptest::prop_assert!(same(&nfkc_ascii(&s), &nfkc_unicode(&s)), "nfkc: {s:?}");
            proptest::prop_assert!(same(&normalize_ascii(&s, true), &normalize_unicode(&s, true)), "sep: {s:?}");
            proptest::prop_assert!(same(&normalize_ascii(&s, false), &normalize_unicode(&s, false)), "nosep: {s:?}");
        }
    }

    #[test]
    fn the_hand_picked_edge_cases_match_too() {
        for s in [
            "",
            "\\",
            "\\_",
            "api\\_key = x",
            "--leading and trailing--",
            "MiXeD Case/With.Separators_and\ttabs",
            "a\\\\b",
        ] {
            assert!(same(&nfkc_ascii(s), &nfkc_unicode(s)), "nfkc {s:?}");
            assert!(
                same(&normalize_ascii(s, true), &normalize_unicode(s, true)),
                "sep {s:?}"
            );
            assert!(
                same(&normalize_ascii(s, false), &normalize_unicode(s, false)),
                "nosep {s:?}"
            );
        }
    }
}
