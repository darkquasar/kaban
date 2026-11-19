//! Code-aware search through the public catalog snapshot interface.
#![allow(clippy::unwrap_used, clippy::panic)]

use kaban_core::RecallQuery;
use kaban_core::config::Recall;
use kaban_core::query_budget::QueryBudget;
use kaban_core::text_index::{
    IndexRow, MatchMode, Searched, SeedFilter, TextIndex, TextQuery, TextSnapshot,
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
    let budget = QueryBudget::new(u64::MAX, None);
    let ontology = kaban_core::shipped::resolved().unwrap();
    let adapter = TantivyIndex::new(&ontology, &Recall::default());
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
    hits.into_iter().map(|h| h.rid).collect()
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
