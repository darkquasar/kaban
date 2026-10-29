//! `symbols.ident_terms` (cg-join-v1 §8, SD-7, RV-3; stage1-plan CG3.3): the deterministic
//! identifier split the EN-F25 dictionary and the `code_*` selectors read through
//! `symbols_fts` instead of scanning. [`build`] is pure and total: it splits `name`, the last
//! descriptor of `key` and `label` on case boundaries and on `_` `.` `#` `/` (every other
//! non-alphanumeric character separates too), lowercases, de-duplicates preserving the first
//! occurrence and space-joins. The writer applies it to gated values and re-scans the result as
//! a formed string (G1-S1 step 3): a hit empties the field and counts `policy_dropped`.

use std::borrow::Cow;

/// The space-joined, lowercased, de-duplicated terms of the three inputs, in order.
#[must_use]
pub fn build(name: &str, key_last_descriptor: &str, label: &str) -> String {
    let mut seen: Vec<Cow<'_, str>> = Vec::new();
    for input in [name, key_last_descriptor, label] {
        for term in split_terms(input) {
            let lower = lowercase(term);
            if !seen.contains(&lower) {
                seen.push(lower);
            }
        }
    }
    seen.join(" ")
}

/// Lowercase without allocating when the term already is (own-cow-conditional).
fn lowercase(term: &str) -> Cow<'_, str> {
    if term.chars().any(char::is_uppercase) {
        Cow::Owned(term.to_lowercase())
    } else {
        Cow::Borrowed(term)
    }
}

/// The alphanumeric runs of `s`, further split on case boundaries: before an uppercase letter
/// that follows a lowercase letter or a digit (`parseURL` -> `parse`, `URL`), and before the
/// last uppercase letter of an uppercase run that is followed by a lowercase letter
/// (`HTTPServer` -> `HTTP`, `Server`).
fn split_terms(s: &str) -> Vec<&str> {
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
