//! Public preparation signal weights, evidence and refusal coverage.
#![allow(clippy::unwrap_used)] // fixture failures are test failures
use kaban_ingest::extract::structured_prep::signal::{density_evidence, suggest};
use kaban_ingest::{Format, gate_path};
use kaban_policy::Policy;
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Case {
    path: String,
    headings: Vec<String>,
    sample: String,
    top: String,
    score: u8,
}

#[test]
fn signal_table_matches_weights_and_negative_near_misses() {
    for line in include_str!("fixtures/structured/signals.jsonl").lines() {
        let case: Case = serde_json::from_str(line).unwrap();
        let path = gate_path(&Policy::empty(), &case.path).unwrap();
        let headings: Vec<_> = case.headings.iter().map(String::as_str).collect();
        let candidates = suggest(&path, Format::Markdown, &headings, &case.sample);
        assert_eq!(
            candidates[0],
            (case.top.as_str(), case.score),
            "{}",
            case.path
        );
        assert!(
            candidates
                .iter()
                .all(|(_, score)| (30..=100).contains(score))
        );
        assert_eq!(
            candidates,
            suggest(&path, Format::Markdown, &headings, &case.sample)
        );
    }
}

#[test]
fn code_and_kaban_components_are_excluded_but_similar_names_are_not() {
    for name in [".kaban/notes.md", "nested/.kaban/notes.md"] {
        let path = gate_path(&Policy::empty(), name).unwrap();
        assert_eq!(
            suggest(&path, Format::Markdown, &["Status"], ""),
            Vec::new()
        );
    }
    let path = gate_path(&Policy::empty(), ".kaban-other/notes.md").unwrap();
    assert_eq!(suggest(&path, Format::Markdown, &[], ""), [("generic", 30)]);
    assert_eq!(suggest(&path, Format::Code, &["Status"], ""), Vec::new());
}

#[test]
fn fallback_formats_and_ties_are_stable() {
    let path = gate_path(&Policy::empty(), "README.md").unwrap();
    assert_eq!(
        suggest(&path, Format::Text, &[], ""),
        [("generic", 30), ("readme", 30)]
    );
    for format in [
        Format::Pdf,
        Format::Html,
        Format::Csv,
        Format::Sheet,
        Format::Docx,
    ] {
        assert_eq!(
            suggest(&path, format, &[], ""),
            [("generic", 30), ("readme", 30)]
        );
    }
    let path = gate_path(&Policy::empty(), "notes.txt").unwrap();
    assert_eq!(suggest(&path, Format::Markdown, &[], ""), [("generic", 30)]);
    let path = gate_path(&Policy::empty(), "notes.md").unwrap();
    assert_eq!(
        suggest(&path, Format::Markdown, &["Status", "Context"], ""),
        [("adr", 40)]
    );
}

#[test]
fn density_cutoff_counts_evidence_without_inventing_record_fields() {
    let path = gate_path(&Policy::empty(), "notes.md").unwrap();
    let at_cutoff = format!("MUST SHOULD MUST NOT {}", "word ".repeat(996));
    assert_eq!(density_evidence(&at_cutoff).words, 1000);
    assert_eq!(density_evidence(&at_cutoff).rfc_keywords, 3);
    assert_eq!(
        suggest(&path, Format::Markdown, &[], &at_cutoff),
        [("conventions", 50)]
    );
    let below = format!("{at_cutoff} word");
    assert_eq!(
        suggest(&path, Format::Markdown, &[], &below),
        [("generic", 30)]
    );
    assert_eq!(
        density_evidence("Term: definition\n- [ ] item").definitions,
        1
    );
    assert_eq!(
        density_evidence("Term: definition\n- [ ] item").checklists,
        1
    );
    assert_eq!(
        suggest(&path, Format::Markdown, &[], "MUSTARD should"),
        [("generic", 30)]
    );
}

#[test]
fn gazetteer_signals_are_named_bounded_distinct_and_keep_base_rules() {
    use kaban_ingest::extract::structured_prep::signal::{
        GazetteerEvidence, suggest_with_gazetteer,
    };
    let path = gate_path(&Policy::empty(), "notes.md").unwrap();
    let stats = GazetteerEvidence {
        symbol_path_targets: 2,
        failure_title_targets: 1,
    };
    assert_eq!(
        suggest_with_gazetteer(
            &path,
            Format::Markdown,
            &["Requirements"],
            "1. step\n$ command",
            stats
        ),
        [("runbook", 50), ("design_spec", 40)]
    );
    assert_eq!(
        suggest_with_gazetteer(&path, Format::Markdown, &["Timeline"], "", stats),
        [("postmortem", 90)]
    );
    assert_eq!(
        suggest_with_gazetteer(&path, Format::Markdown, &[], "", stats),
        [("generic", 30)],
        "bonuses alone do not cross threshold"
    );
    assert_eq!(
        suggest_with_gazetteer(
            &path,
            Format::Markdown,
            &["Requirements"],
            "",
            GazetteerEvidence {
                symbol_path_targets: 1,
                ..GazetteerEvidence::default()
            }
        ),
        [("design_spec", 30)]
    );
    assert_eq!(
        suggest_with_gazetteer(
            &path,
            Format::Markdown,
            &["Requirements"],
            "",
            GazetteerEvidence::default()
        ),
        suggest(&path, Format::Markdown, &["Requirements"], "")
    );
    assert_eq!(
        stats.signals("design_spec"),
        ["gazetteer_symbol_path_targets"]
    );
    assert_eq!(
        stats.signals("postmortem"),
        ["gazetteer_failure_title_target"]
    );
    assert_eq!(
        suggest_with_gazetteer(&path, Format::Code, &["Requirements"], "", stats),
        []
    );
}
