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
                for piece in segment.split('_').filter(|s| !s.is_empty()) {
                    let bytes = piece.as_bytes();
                    let mut start = 0;
                    for i in 1..bytes.len() {
                        let boundary = bytes[i].is_ascii_uppercase()
                            && (bytes[i - 1].is_ascii_lowercase()
                                || (bytes[i - 1].is_ascii_uppercase()
                                    && bytes.get(i + 1).is_some_and(u8::is_ascii_lowercase)));
                        if boundary {
                            push(&piece[start..i]);
                            start = i;
                        }
                    }
                    push(&piece[start..]);
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
