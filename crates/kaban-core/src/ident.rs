//! Deterministic identifier vocabulary shared by ingestion and lexical retrieval.

/// Format of the projected text and identifier analyzers. A change rebuilds persisted text tiers.
pub const TEXT_ANALYZER_VERSION: &str = "3";

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

/// The alphanumeric runs of `s`, further split on case boundaries: before an uppercase letter
/// that follows a lowercase letter or a digit (`parseURL` -> `parse`, `URL`), and before the
/// last uppercase letter of an uppercase run that is followed by a lowercase letter
/// (`HTTPServer` -> `HTTP`, `Server`).
#[must_use]
pub fn split_terms(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    for run in s.split(|c: char| !c.is_alphanumeric()) {
        let mut chars = run.char_indices().peekable();
        let Some((_, mut prev)) = chars.next() else {
            continue;
        };
        let mut start = 0;
        while let Some((at, c)) = chars.next() {
            let boundary = c.is_uppercase()
                && (prev.is_lowercase()
                    || prev.is_numeric()
                    || (prev.is_uppercase()
                        && chars.peek().is_some_and(|(_, next)| next.is_lowercase())));
            prev = c;
            if boundary {
                out.push(&run[start..at]);
                start = at;
            }
        }
        out.push(&run[start..]);
    }
    out
}
