//! Default-feature tests for the early pure judged-fixture seam.
#![allow(clippy::unwrap_used, dead_code, clippy::cast_precision_loss)]

#[path = "../src/bench/corpus.rs"]
#[allow(clippy::cast_possible_truncation)] // unchanged bounded corpus RNG arithmetic
mod corpus;
#[path = "../src/bench/recall_classes.rs"]
mod recall_classes;

use recall_classes::{
    EdgeJudgment, Fixtures, Judgment, Pair, QueryClass, edge_precision, ndcg_at_10,
};
use serde_json::{Value, json};
use std::collections::BTreeMap;

const SOURCES: &str = include_str!("../../../fixtures/judged/walkthrough.jsonl");
const QUERIES: &str = include_str!("../../../fixtures/judged/queries.jsonl");
const EDGES: &str = include_str!("../../../fixtures/judged/edges.jsonl");

#[test]
fn inspected_synthetic_sources_match_unchanged_sequential_corpus() {
    let fixtures = Fixtures::load(SOURCES, QUERIES, EDGES, 100).unwrap();
    let mut corpus = corpus::Corpus::new(corpus::SEED);
    for ordinal in 0..5 {
        let note = corpus.note(ordinal);
        assert!(
            !note.coderef,
            "initial sequence must not consume runner CodeRef draws"
        );
        if let Some(source) = fixtures
            .sources
            .iter()
            .find(|source| source.id == format!("corpus:{ordinal}"))
        {
            assert_eq!(source.text, note.text);
            assert_eq!(source.workspace, note.workspace);
        }
    }
}

fn mutate_first(input: &str, edit: impl FnOnce(&mut Value)) -> String {
    let mut rows: Vec<Value> = input
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    edit(&mut rows[0]);
    rows.iter()
        .map(|row| serde_json::to_string(row).unwrap())
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn checked_in_labels_have_required_counts_classes_and_auditable_evidence() {
    let fixtures = Fixtures::load(SOURCES, QUERIES, EDGES, 100).unwrap();
    assert_eq!(fixtures.queries.len(), 100);
    assert_eq!(fixtures.edges.len(), 100);
    let mut counts = BTreeMap::new();
    for query in &fixtures.queries {
        *counts.entry(query.class).or_insert(0) += 1;
        assert!(query.provenance.contains("agent-authored"));
        assert!(
            query
                .judgments
                .iter()
                .all(|label| !label.evidence.trim().is_empty())
        );
    }
    for class in [
        QueryClass::ExactId,
        QueryClass::ErrorString,
        QueryClass::NaturalLanguage,
        QueryClass::Identifier,
    ] {
        assert_eq!(counts[&class], 25);
    }
    assert!(fixtures.edges.iter().any(|edge| edge.correct));
    assert!(fixtures.edges.iter().any(|edge| !edge.correct));
    assert!(fixtures.edges.iter().all(|edge| edge.rel == "MENTIONS"));
    assert!(
        fixtures
            .sources
            .iter()
            .any(|source| source.id == "corpus:1" && source.text.contains("Err9b10"))
    );
    let rubric = include_str!("../../../fixtures/judged/README.md");
    for required in [
        "Relevance rubric",
        "ambiguous-case",
        "agent-authored",
        "zero-based",
        "not external",
        "Unknown returned hits",
    ] {
        assert!(rubric.contains(required), "missing rubric topic {required}");
    }
}

#[test]
fn loader_rejects_malformed_unknown_duplicate_missing_and_out_of_range_labels() {
    assert!(Fixtures::load(SOURCES, "{", EDGES, 100).is_err());
    for (key, value) in [
        ("class", json!("similarity")),
        ("id", json!("")),
        ("provenance", json!("")),
        ("unexpected", json!(true)),
    ] {
        let bad = mutate_first(QUERIES, |row| row[key] = value);
        assert!(Fixtures::load(SOURCES, &bad, EDGES, 100).is_err());
    }
    for (key, value) in [
        ("target", json!("absent")),
        ("grade", json!(3)),
        ("evidence", json!("")),
    ] {
        let bad = mutate_first(QUERIES, |row| row["judgments"][0][key] = value);
        assert!(Fixtures::load(SOURCES, &bad, EDGES, 100).is_err());
    }
    let rows: Vec<_> = QUERIES.lines().collect();
    let duplicate = format!("{}\n{}", QUERIES, rows[0]);
    assert!(Fixtures::load(SOURCES, &duplicate, EDGES, 100).is_err());
    assert!(Fixtures::load(SOURCES, &rows[..99].join("\n"), EDGES, 100).is_err());
    assert!(Fixtures::load(SOURCES, QUERIES, EDGES, 101).is_err());
    let no_class = QUERIES.replace("natural_language", "identifier");
    assert!(Fixtures::load(SOURCES, &no_class, EDGES, 100).is_err());
    let wrong_workspace = mutate_first(QUERIES, |row| row["workspace"] = json!("foreign"));
    assert!(Fixtures::load(SOURCES, &wrong_workspace, EDGES, 100).is_err());
}

#[test]
fn loader_rejects_invalid_edge_populations_and_source_catalogs() {
    for (key, value) in [
        ("from", json!("absent")),
        ("to", json!("absent")),
        ("rel", json!("SIMILAR_TO")),
        ("workspace", json!("foreign")),
        ("evidence", json!("")),
    ] {
        let bad = mutate_first(EDGES, |row| row[key] = value);
        assert!(Fixtures::load(SOURCES, QUERIES, &bad, 100).is_err());
    }
    let duplicate = format!("{}\n{}", EDGES, EDGES.lines().next().unwrap());
    assert!(Fixtures::load(SOURCES, QUERIES, &duplicate, 100).is_err());
    let short = EDGES.lines().take(99).collect::<Vec<_>>().join("\n");
    assert!(Fixtures::load(SOURCES, QUERIES, &short, 100).is_err());
    let duplicate = format!("{}\n{}", SOURCES, SOURCES.lines().next().unwrap());
    assert!(Fixtures::load(&duplicate, QUERIES, EDGES, 100).is_err());
    let bad = mutate_first(SOURCES, |row| row["text"] = json!(""));
    assert!(Fixtures::load(&bad, QUERIES, EDGES, 100).is_err());
}

fn pool() -> Vec<Judgment> {
    [("direct", 2), ("partial", 1), ("irrelevant", 0)]
        .into_iter()
        .map(|(target, grade)| Judgment {
            target: target.into(),
            grade,
            evidence: "invented hand-checked metric fixture".into(),
        })
        .collect()
}

fn hits(ids: &[&str]) -> Vec<String> {
    ids.iter().map(|id| (*id).into()).collect()
}

#[test]
fn ndcg_matches_hand_calculation_and_degrades_when_direct_answer_is_second() {
    let perfect = ndcg_at_10(&pool(), &hits(&["direct", "partial", "irrelevant"])).unwrap();
    assert_eq!(perfect.score, Some(1.0));
    let degraded = ndcg_at_10(&pool(), &hits(&["partial", "direct", "irrelevant"])).unwrap();
    let expected = (1.0 + 3.0 / 3.0_f64.log2()) / (3.0 + 1.0 / 3.0_f64.log2());
    assert!((degraded.score.unwrap() - expected).abs() < 1e-12);
    assert!(degraded.score.unwrap() < 1.0);
    assert_eq!(ndcg_at_10(&pool(), &[]).unwrap().score, Some(0.0));
}

#[test]
fn ndcg_deduplicates_marks_unknowns_and_handles_zero_ideal_without_nan() {
    let duplicated = ndcg_at_10(&pool(), &hits(&["direct", "direct", "partial"])).unwrap();
    assert_eq!(duplicated.score, Some(1.0));
    assert_eq!(duplicated.duplicate_hits, 1);
    let unknown = ndcg_at_10(&pool(), &hits(&["unknown", "direct"])).unwrap();
    assert_eq!(unknown.score, None);
    assert_eq!((unknown.judged_hits, unknown.unjudged_hits), (1, 1));
    let zero = ndcg_at_10(
        &[Judgment {
            target: "zero".into(),
            grade: 0,
            evidence: "irrelevant".into(),
        }],
        &hits(&["zero"]),
    )
    .unwrap();
    assert!(zero.zero_ideal);
    assert_eq!(zero.score, Some(0.0));
    let mut invalid = pool();
    invalid[0].grade = 3;
    assert!(ndcg_at_10(&invalid, &[]).is_err());
    let mut duplicate = pool();
    duplicate.push(pool()[0].clone());
    assert!(ndcg_at_10(&duplicate, &[]).is_err());
}

fn edge(to: &str, correct: bool) -> EdgeJudgment {
    EdgeJudgment {
        id: to.into(),
        from: "chunk".into(),
        to: to.into(),
        workspace: "ws1".into(),
        rel: "MENTIONS".into(),
        correct,
        evidence: "invented inspected target".into(),
        provenance: "test".into(),
    }
}

fn pair(to: &str) -> Pair {
    Pair {
        workspace: "ws1".into(),
        from: "chunk".into(),
        to: to.into(),
    }
}

#[test]
fn edge_precision_reports_judged_correct_unjudged_coverage_and_duplicates() {
    let labels = [
        edge("right", true),
        edge("wrong", false),
        edge("unpredicted", true),
    ];
    let result = edge_precision(
        &labels,
        &[pair("right"), pair("wrong"), pair("unknown"), pair("right")],
    )
    .unwrap();
    assert_eq!(
        (
            result.judged,
            result.predicted,
            result.judged_predicted,
            result.correct,
            result.unjudged,
            result.duplicates
        ),
        (3, 3, 2, 1, 1, 1)
    );
    assert_eq!(result.precision, Some(0.5));
    assert_eq!(result.coverage, Some(2.0 / 3.0));
    let mut foreign = pair("right");
    foreign.workspace = "other".into();
    let unknown = edge_precision(&labels, &[foreign]).unwrap();
    assert_eq!(unknown.precision, None);
    assert_eq!(unknown.coverage, Some(0.0));
    let empty = edge_precision(&labels, &[]).unwrap();
    assert_eq!((empty.precision, empty.coverage), (None, None));
    assert!(edge_precision(&[edge("right", true), edge("right", false)], &[]).is_err());
}
