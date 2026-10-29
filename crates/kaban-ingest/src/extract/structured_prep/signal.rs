//! AI-F6 deterministic suggestions. Integer percentages are signal weights, not model
//! confidence or acceptance evidence. Glossary/checklist density uses minimal presence
//! (one matching line); only RFC 2119 has a normative numeric density cutoff.

use std::collections::BTreeMap;

use crate::{Format, GatedPath};

/// Counts from the caller's admitted sample (never retained on records).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DensityEvidence {
    /// Whitespace-delimited words.
    pub words: usize,
    /// Uppercase MUST or SHOULD tokens; MUST NOT counts once.
    pub rfc_keywords: usize,
    /// Definition-list or term/definition lines.
    pub definitions: usize,
    /// Unchecked Markdown checklist lines.
    pub checklists: usize,
}

/// Inspect sample evidence only; no record fields are extracted.
#[must_use]
pub fn density_evidence(sample: &str) -> DensityEvidence {
    let words = sample.split_whitespace().count();
    let rfc_keywords = sample
        .split_whitespace()
        .filter(|word| {
            matches!(
                word.trim_matches(|c: char| !c.is_ascii_alphabetic()),
                "MUST" | "SHOULD"
            )
        })
        .count();
    let mut evidence = DensityEvidence {
        words,
        rfc_keywords,
        ..DensityEvidence::default()
    };
    for line in sample.lines().map(str::trim) {
        if line.starts_with("- [ ]") {
            evidence.checklists += 1;
        }
        if line.starts_with(": ")
            || line.split_once(": ").is_some_and(|(term, definition)| {
                !term.is_empty()
                    && !definition.is_empty()
                    && !is_marker(line)
                    && !term.contains(['/', '$'])
            })
        {
            evidence.definitions += 1;
        }
    }
    evidence
}

/// Suggest `(source_type, integer_percent)` in descending score, then lexical type order.
/// Scores cap at 100 (postmortem at 90); only scores >= 30 survive. Code and any
/// `.kaban` path component produce no suggestions. Non-Markdown also suggests generic.
#[must_use]
pub fn suggest(
    path: &GatedPath,
    format: Format,
    headings: &[&str],
    sample: &str,
) -> Vec<(&'static str, u8)> {
    suggest_with_gazetteer(path, format, headings, sample, GazetteerEvidence::default())
}

/// Distinct current-policy-admitted, nonambiguous targets, not extracted record fields.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GazetteerEvidence {
    /// Targets matched by the symbol/path methods; repeat occurrences count once.
    pub symbol_path_targets: usize,
    /// Existing Failure targets matched by title, never aliases or ambiguous hits.
    pub failure_title_targets: usize,
}

impl GazetteerEvidence {
    /// Named AM11 evidence contributing at most ten integer percentage points per type.
    #[must_use]
    pub fn signals(self, source_type: &str) -> Vec<&'static str> {
        match source_type {
            "design_spec" | "runbook" if self.symbol_path_targets >= 2 => {
                vec!["gazetteer_symbol_path_targets"]
            }
            "postmortem" if self.failure_title_targets > 0 => {
                vec!["gazetteer_failure_title_target"]
            }
            _ => vec![],
        }
    }
}

/// Existing deterministic table plus the bounded AM11 evidence; no model or field inference.
#[must_use]
pub fn suggest_with_gazetteer(
    path: &GatedPath,
    format: Format,
    headings: &[&str],
    sample: &str,
    gazetteer: GazetteerEvidence,
) -> Vec<(&'static str, u8)> {
    let path = path.relative_path();
    if format == Format::Code || path.split('/').any(|part| part == ".kaban") {
        return Vec::new();
    }
    let filename = path.rsplit('/').next().unwrap_or(path);
    let mut scores = BTreeMap::<&'static str, u8>::new();
    let mut add = |kind, weight: u8| {
        let score = scores.entry(kind).or_default();
        *score = score
            .saturating_add(weight)
            .min(if kind == "postmortem" { 90 } else { 100 });
    };
    let under = |prefixes: &[&str]| prefixes.iter().any(|prefix| path.starts_with(prefix));
    let heading = |names: &[&str]| {
        headings
            .iter()
            .any(|h| names.iter().any(|n| h.trim().eq_ignore_ascii_case(n)))
    };
    if under(&["docs/adr/", "doc/adr/", "adr/", "docs/decisions/"]) {
        add("adr", 40);
    }
    if filename
        .split_once('-')
        .is_some_and(|(n, _)| (3..=4).contains(&n.len()) && n.bytes().all(|b| b.is_ascii_digit()))
    {
        add("adr", 20);
    }
    if heading(&[
        "Status",
        "Context",
        "Decision",
        "Consequences",
        "Context and Problem Statement",
        "Decision Outcome",
        "Considered Options",
    ]) {
        add("adr", 40);
    }
    if under(&["docs/specs/", "rfcs/"]) {
        add("design_spec", 30);
    }
    if heading(&["Requirements", "Non-goals", "Decisions"]) {
        add("design_spec", 30);
    }
    if matches!(filename, "CONTRIBUTING.md" | "AGENTS.md" | "CLAUDE.md")
        || (filename.starts_with("STYLE") && filename.strip_suffix(".md").is_some())
    {
        add("conventions", 80);
    }
    let density = density_evidence(sample);
    if density.words > 0
        && density.rfc_keywords.saturating_mul(1000) >= density.words.saturating_mul(3)
    {
        add("conventions", 50);
    }
    if under(&["postmortems/", "incidents/"]) {
        add("postmortem", 50);
    }
    if heading(&[
        "Timeline",
        "Root cause",
        "Impact",
        "Action items",
        "Lessons",
    ]) {
        add("postmortem", 80);
    }
    if filename == "LESSONS.md"
        || heading(&["Lesson"])
        || sample.lines().any(|line| {
            line.split('|')
                .any(|column| column.trim().eq_ignore_ascii_case("Lesson"))
        })
    {
        add("lessons", 70);
    }
    if filename == "CHANGELOG.md"
        || heading(&["Unreleased"])
        || headings.iter().any(|h| version_heading(h))
    {
        add("changelog", 80);
    }
    if iso_date(filename) || under(&["journal/", "meetings/"]) || heading(&["Attendees", "Agenda"])
    {
        add("journal", 60);
    }
    if density.definitions > 0 {
        add("glossary", 50);
    }
    if matches!(filename, "TODO.md" | "ROADMAP.md") || density.checklists > 0 {
        add("todo", 60);
    }
    if sample.lines().any(numbered_step) && sample.lines().any(|line| line.starts_with("$ ")) {
        add("runbook", 40);
    }
    if filename == "README.md" {
        add("readme", 30);
    }
    if format != Format::Markdown || filename.strip_suffix(".txt").is_some() {
        add("generic", 30);
    }
    if sample.lines().any(is_marker) {
        // A marker raises one existing ADR/design suggestion. ADR wins a tie; without
        // either, its 20-point marker signal alone cannot cross the candidate threshold.
        let kind = if scores.get("design_spec").copied().unwrap_or(0)
            > scores.get("adr").copied().unwrap_or(0)
        {
            "design_spec"
        } else {
            "adr"
        };
        let score = scores.entry(kind).or_default();
        *score = score.saturating_add(20).min(100);
    }
    for kind in ["design_spec", "runbook", "postmortem"] {
        if !gazetteer.signals(kind).is_empty() {
            let score = scores.entry(kind).or_default();
            *score = score
                .saturating_add(10)
                .min(if kind == "postmortem" { 90 } else { 100 });
        }
    }
    let mut candidates: Vec<_> = scores
        .into_iter()
        .filter(|(_, score)| *score >= 30)
        .collect();
    if candidates.is_empty() {
        candidates.push(("generic", 30));
    }
    candidates.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    candidates
}

fn is_marker(line: &str) -> bool {
    ["WHY:", "DECISION:", "TRADEOFF:", "REJECTED:"]
        .iter()
        .any(|marker| line.starts_with(marker))
}

fn numbered_step(line: &str) -> bool {
    line.trim_start()
        .split_once(". ")
        .is_some_and(|(number, _)| !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit()))
}

fn version_heading(heading: &str) -> bool {
    heading
        .trim()
        .strip_prefix('[')
        .and_then(|h| h.split_once(']'))
        .is_some_and(|(version, _)| {
            let pieces: Vec<_> = version.split('.').collect();
            pieces.len() == 3
                && pieces
                    .iter()
                    .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
        })
}

fn iso_date(filename: &str) -> bool {
    filename.as_bytes().windows(10).any(|date| {
        date[4] == b'-'
            && date[7] == b'-'
            && date
                .iter()
                .enumerate()
                .all(|(i, b)| i == 4 || i == 7 || b.is_ascii_digit())
            && &date[5..7] >= b"01".as_slice()
            && &date[5..7] <= b"12".as_slice()
            && &date[8..10] >= b"01".as_slice()
            && &date[8..10] <= b"31".as_slice()
    })
}
