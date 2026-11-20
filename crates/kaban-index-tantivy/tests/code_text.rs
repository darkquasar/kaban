//! Code-aware search through the public catalog snapshot interface.
#![allow(clippy::unwrap_used, clippy::panic)]

use kaban_core::RecallQuery;
use kaban_core::config::Recall;
use kaban_core::query_budget::QueryBudget;
use kaban_core::text_index::{
    IndexRow, MatchMode, Searched, SeedFilter, TextHit, TextIndex, TextQuery, TextSnapshot,
};
use kaban_index_tantivy::{TantivyIndex, segment};
use std::path::Path;

fn row(rid: i64, name: &str, owner: &str) -> IndexRow {
    IndexRow {
        id: rid.to_string(),
        rid,
        revision: 1,
        kind: "Method".into(),
        workspace_id: "default".into(),
        text: String::new(),
        summary: String::new(),
        stored_summary: None,
        tags: vec![],
        ident_values: vec![name.into(), format!("{owner}::{name}")],
        anchor_keys: vec![],
    }
}

fn find(pack: &[u8], text: &str, mode: MatchMode) -> Vec<i64> {
    let ontology = kaban_core::shipped::resolved().unwrap();
    let adapter = TantivyIndex::new(&ontology, &Recall::default());
    search(&adapter, pack, text, mode)
        .into_iter()
        .map(|h| h.rid)
        .collect()
}

fn search(adapter: &TantivyIndex, pack: &[u8], text: &str, mode: MatchMode) -> Vec<TextHit> {
    let budget = QueryBudget::new(u64::MAX, None);
    let q = RecallQuery::default();
    let query = TextQuery {
        text,
        mode,
        filter: SeedFilter::of(&q),
        limit: 10,
        sufficient: 10,
        skip: 0,
    };
    let at = TextSnapshot {
        generation: 1,
        kbx: Path::new("unused"),
        tiers: &[],
        sediment: &[pack],
    };
    let Searched::Hits { hits, .. } = adapter.search(&at, &query, &budget).unwrap() else {
        panic!("catalog search unavailable");
    };
    hits
}

fn s4_name(n: usize) -> String {
    let fixture = include_str!("fixtures/s4-parity-queries.jsonl");
    let case: serde_json::Value = serde_json::from_str(fixture.lines().nth(n).unwrap()).unwrap();
    case["selector"]["name"].as_str().unwrap().to_owned()
}

fn code_pack() -> Vec<u8> {
    let rows = [
        row(1, "snapshot_symbols", "CodeGraphReader"),
        row(2, "clone", "SnapshotSymbol"),
        row(3, "snapshot_status", "CodeGraphReader"),
        row(4, "snapshot_symbols", "OtherReader"),
        row(5, "configure_connection", "schema"),
    ];
    segment(&rows, &[], &QueryBudget::new(u64::MAX, None)).unwrap()
}

#[test]
fn underscore_prefix_finds_a_longer_name() {
    let pack = code_pack();
    assert_eq!(find(&pack, &s4_name(40), MatchMode::Lexical), [5]);
}

#[test]
fn qualified_operand_requires_every_part() {
    let pack = code_pack();
    assert_eq!(find(&pack, &s4_name(42), MatchMode::Lexical), [1]);
}

#[test]
fn camel_operand_requires_every_part_even_when_the_query_widens() {
    let pack = code_pack();
    let mut hits = find(&pack, &s4_name(43), MatchMode::Lexical);
    hits.sort_unstable();
    assert_eq!(hits, [1, 4]);
}

#[test]
fn misspelled_identifier_part_is_searchable() {
    let pack = code_pack();
    assert_eq!(find(&pack, "connecion", MatchMode::Fuzzy), [5]);
}

#[test]
fn quoted_code_keeps_its_word_order_and_a_misspelling_finds_a_camel_part() {
    let rows = [
        row(1, "parseHeaderLine", "Parser"),
        row(2, "lineHeaderParse", "Parser"),
    ];
    let pack = segment(&rows, &[], &QueryBudget::new(u64::MAX, None)).unwrap();
    assert_eq!(find(&pack, "\"header line\"", MatchMode::Lexical), [1]);
    let mut both = find(&pack, "headre", MatchMode::Fuzzy);
    both.sort_unstable();
    assert_eq!(both, [1, 2]);
}

#[test]
fn fuzzy_camel_operands_require_the_misspelled_part_too() {
    let rows = [
        row(1, "parse_header_line", "Parser"),
        row(2, "parseBody", "Parser"),
    ];
    let pack = segment(&rows, &[], &QueryBudget::new(u64::MAX, None)).unwrap();
    assert_eq!(find(&pack, "parseHeadrLine", MatchMode::Fuzzy), [1]);
}

#[test]
fn quoted_compounds_keep_their_order_and_qualified_paths_require_both_sides() {
    let rows = [
        row(1, "parseHeaderLine", "source"),
        row(2, "lineHeaderParse", "other"),
        row(3, "parseHeaderLine", "other"),
    ];
    let pack = segment(&rows, &[], &QueryBudget::new(u64::MAX, None)).unwrap();
    let mut hits = find(&pack, "\"parseHeaderLine\"", MatchMode::Lexical);
    hits.sort_unstable();
    assert_eq!(hits, [1, 3]);
    assert_eq!(
        find(&pack, "source/parseHeaderLine", MatchMode::Lexical),
        [1]
    );
}

#[test]
fn exact_matches_only_whole_names_in_stable_row_order() {
    let mut first = row(1, "Open", "Parser");
    first
        .ident_values
        .extend((0..50).map(|n| format!("alias{n}")));
    let pack = segment(
        &[
            first,
            row(2, "OPEN", "Reader"),
            row(3, "openFile", "Reader"),
        ],
        &[],
        &QueryBudget::new(u64::MAX, None),
    )
    .unwrap();
    assert_eq!(find(&pack, "oPeN", MatchMode::Exact), [1, 2]);
    assert_eq!(find(&pack, "ope", MatchMode::Exact), [] as [i64; 0]);
}

#[test]
fn configured_identifier_weight_scales_compound_names_and_parts() {
    let mut document = row(2, "parseHeader", "Parser");
    document.kind = "Document".into();
    let rows = [row(1, "parseHeader", "Parser"), document];
    let pack = segment(&rows, &[], &QueryBudget::new(u64::MAX, None)).unwrap();

    let mut ontology = kaban_core::shipped::resolved().unwrap();
    let recall = Recall::default();
    let default = TantivyIndex::new(&ontology, &recall);
    let (_, kind) = ontology
        .kinds
        .iter_mut()
        .find(|(key, _)| kaban_core::stored_name(key) == "Document")
        .unwrap();
    kind.column_weights =
        Some(serde_json::from_value(serde_json::json!({"ident_terms": 4.0})).unwrap());
    let weighted = TantivyIndex::new(&ontology, &recall);

    for query in ["parseHeader", "header_parse", "Parser::parseHeader"] {
        let ordinary = search(&default, &pack, query, MatchMode::Lexical);
        assert_eq!(ordinary.iter().map(|h| h.rid).collect::<Vec<_>>(), [1, 2]);
        let hits = search(&weighted, &pack, query, MatchMode::Lexical);
        assert_eq!(
            hits.iter().map(|h| h.rid).collect::<Vec<_>>(),
            [2, 1],
            "{query}"
        );
        assert!((hits[1].score - ordinary[0].score).abs() < f64::from(f32::EPSILON));
        assert!(
            (hits[0].score - 2.0 * ordinary[1].score).abs() < f64::from(f32::EPSILON),
            "{query}"
        );
    }
}

#[test]
fn prefix_can_end_inside_a_whole_identifier_across_a_case_boundary() {
    let rows = [
        row(1, "PrefixedSqliteStore", "Database"),
        row(2, "SqliteStore", "Database"),
        row(3, "SqliteSnapshot", "Database"),
    ];
    let pack = segment(&rows, &[], &QueryBudget::new(u64::MAX, None)).unwrap();
    assert_eq!(find(&pack, "sqlitesto*", MatchMode::Lexical), [2]);
    assert_eq!(find(&pack, "sqlitesto", MatchMode::Lexical), [] as [i64; 0]);
    // A prefix of one part gives the same credit wherever the part occurs.
    assert_eq!(find(&pack, "sqlit*", MatchMode::Lexical), [1, 2, 3]);
}

#[test]
fn hyphenated_prose_matches_a_stemmed_phrase_and_code_still_requires_all_parts() {
    let prose = |rid, text: &str| IndexRow {
        text: text.into(),
        ..row(rid, "Guide", "Docs")
    };
    let rows = [
        prose(1, "Widely known issue"),
        prose(2, "Issues often become widely known"),
        prose(3, "Known defects and the issue tracker"),
        prose(4, "Known issues arise"),
        prose(5, "Issues occur in unknown places"),
    ];
    let pack = segment(&rows, &[], &QueryBudget::new(u64::MAX, None)).unwrap();
    let mut prose_hits = find(&pack, "known-issues", MatchMode::Lexical);
    prose_hits.sort_unstable();
    assert_eq!(prose_hits, [1, 4]);
    for compound in [
        "known.issues",
        "known::issues",
        "knownIssues",
        "known/issues",
    ] {
        let mut hits = find(&pack, compound, MatchMode::Lexical);
        hits.sort_unstable();
        assert_eq!(hits, [2, 4], "{compound}");
    }
}
