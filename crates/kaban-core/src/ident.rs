//! Deterministic identifier vocabulary shared by ingestion and lexical retrieval.

use std::collections::HashSet;

/// Version of the deterministic derived vocabulary, persisted in `index_meta`.
pub const IDENT_TERMS_VERSION: &str = "2";

/// The lexical shape of a query, not an inference about its meaning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryClass {
    /// An identifier, path or explicit phrase.
    Identifier,
    /// Ordinary prose.
    NaturalLanguage,
}

/// Classify explicit identifier separators, camel boundaries and quoted queries. A dot counts
/// only between two identifier characters, so sentence periods and ellipses stay prose.
#[must_use]
pub fn classify(query: &str) -> QueryClass {
    if query.contains(['_', '/', '"'])
        || query.contains("::")
        || internal_dot(query)
        || camel_boundary(query)
    {
        QueryClass::Identifier
    } else {
        QueryClass::NaturalLanguage
    }
}

fn internal_dot(text: &str) -> bool {
    let word = |c: &u8| c.is_ascii_alphanumeric() || *c == b'_';
    text.as_bytes()
        .windows(3)
        .any(|part| part[1] == b'.' && word(&part[0]) && word(&part[2]))
}

fn camel_boundary(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes
        .windows(2)
        .any(|pair| pair[0].is_ascii_lowercase() && pair[1].is_ascii_uppercase())
        || bytes.windows(3).any(|part| {
            part[0].is_ascii_uppercase()
                && part[1].is_ascii_uppercase()
                && part[2].is_ascii_lowercase()
        })
}

/// The one identifier splitter: the alphanumeric runs of `s` (Unicode; every other character,
/// `_` `:` `.` `/` `-` included, separates), each further split on case boundaries: before an
/// uppercase letter that follows a lowercase letter or a digit (`parseURL` -> `parse`, `URL`),
/// and before the last uppercase letter of an uppercase run that is followed by a lowercase
/// letter (`HTTPServer` -> `HTTP`, `Server`). [`ident_terms`] and the code analyzer share it.
#[must_use]
pub fn split_terms(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    for run in s.split(|c: char| !c.is_alphanumeric()) {
        if run.is_empty() {
            continue;
        }
        let chars: Vec<(usize, char)> = run.char_indices().collect();
        let mut start = 0;
        for i in 1..chars.len() {
            let (at, c) = chars[i];
            let prev = chars[i - 1].1;
            let next = chars.get(i + 1).map(|&(_, n)| n);
            let boundary = c.is_uppercase()
                && (prev.is_lowercase()
                    || prev.is_numeric()
                    || (prev.is_uppercase() && next.is_some_and(char::is_lowercase)));
            if boundary {
                out.push(&run[start..at]);
                start = at;
            }
        }
        out.push(&run[start..]);
    }
    out
}

/// Derive whole identifiers, separator/camel parts and lowercase copies, in first-seen order.
/// Plain prose and short tokens do not inflate the derived column. Inputs are already gated.
#[must_use]
pub fn ident_terms(
    text: &str,
    summary: Option<&str>,
    tags: &[String],
    ident_fields: &[&str],
    anchors: &[&str],
) -> String {
    let mut terms = Vec::new();
    let mut seen = HashSet::new();
    let mut push = |term: &str| {
        if !term.is_empty() && seen.insert(term.to_owned()) {
            terms.push(term.to_owned());
        }
    };
    for input in std::iter::once(text)
        .chain(summary)
        .chain(tags.iter().map(String::as_str))
        .chain(ident_fields.iter().copied())
        .chain(anchors.iter().copied())
    {
        for token in input.split(|c: char| !(c.is_ascii_alphanumeric() || "_:./-".contains(c))) {
            // a sentence period or ellipsis closes the token, it is not part of it
            let token = token.trim_end_matches('.');
            if token.len() < 3
                || !token
                    .as_bytes()
                    .first()
                    .is_some_and(|c| c.is_ascii_alphabetic() || *c == b'_')
                || classify(token) != QueryClass::Identifier
            {
                continue;
            }
            push(token);
            for segment in token.split([':', '.', '/', '-']).filter(|s| !s.is_empty()) {
                push(segment);
                for part in split_terms(segment) {
                    push(part);
                }
            }
        }
    }
    let originals = terms.clone();
    for term in originals {
        let lower = term.to_ascii_lowercase();
        if seen.insert(lower.clone()) {
            terms.push(lower);
        }
    }
    terms.join(" ")
}
