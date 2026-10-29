//! What the index does not cover, in every answer (R-1; `indexer-quality-backlog.md` item 1):
//! the Rust producer counts what it skipped (items inside function bodies, feature-gated
//! targets, modules with an unresolved import) and `kaban-index` publishes the counters as the
//! snapshot's `coverage:<key>` meta rows (`crates/kaban-index/src/writer.rs`, `Coverage`). Every
//! `code_*` response carries the compact [`CoverageSummary`] projected from them, so that an
//! empty answer is read with its gaps in view, and the `kaban://code-index` resource the whole
//! table ([`CoverageIndex`]). A snapshot without the rows (Go, TypeScript, a producer older
//! than the sidecar) projects to zeros with no note and to no table: never an error.

use std::collections::BTreeMap;

use kaban_codegraph::{CodeGraphReader, Meta};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::CodeError;

/// The `meta` key prefix of the producer's counters.
const PREFIX: &str = "coverage:";

/// The counters that mean "not indexed" (`crates/kaban-index/src/writer.rs`, `Coverage::KEYS`
/// less the sizes `contexts`, `declarations`, `scip_bytes` and `block_item_lines`), in the
/// producer's order. `block_items_skipped` is the raw count of the distinct
/// `block_items_unique`: it stands in only when a producer older than the distinct count
/// wrote no `block_items_unique` row.
const GAP_KEYS: [&str; 13] = [
    "targets_skipped",
    "proc_macros_not_built",
    "block_items_skipped",
    "block_items_unique",
    "block_item_references",
    "shared_file_contexts_skipped",
    "incomplete_modules",
    "divergent_bodies",
    "cfg_divergent_declarations",
    "tokens_without_symbol",
    "positions_skipped",
    "derived_impls_generic_skipped",
    "nested_workspaces_not_indexed",
];

/// The compact coverage field of every `code_*` response: what the snapshot holds and what
/// the producer left out, with one plain sentence when anything is missing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CoverageSummary {
    /// Declarations the producer published (`coverage:declarations`); 0 when the snapshot
    /// carries no coverage rows.
    pub declarations: u64,
    /// Distinct items inside function bodies and macro expansions that have no identity in
    /// the graph (`coverage:block_items_unique`).
    pub block_items_unique: u64,
    /// Modules indexed with an unresolved import, module or macro, whose references are
    /// partial (`coverage:incomplete_modules`).
    pub incomplete_modules: u64,
    /// Cargo targets skipped for inactive `required-features` (`coverage:targets_skipped`).
    pub skipped_targets: u64,
    /// One sentence when any gap counter is non-zero: what is not indexed, and that an empty
    /// answer does not prove absence. Absent otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// One row of the producer's per-crate table (`coverage:per_crate`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CrateCoverage {
    /// The Cargo package name.
    #[serde(rename = "crate")]
    pub name: String,
    /// Published declarations of the package.
    pub declarations: u64,
    /// Distinct skipped block items in its files.
    pub block_items_unique: u64,
    /// Lines inside those items, each once.
    pub block_item_lines: u64,
}

/// The `coverage` object of `kaban://code-index`: every `coverage:<key>` counter by key and
/// the per-crate table.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CoverageIndex {
    /// The producer's counters (`targets_skipped`, `declarations`, `block_items_skipped`,
    /// `block_items_unique`, `block_item_lines`, `incomplete_modules`, ... as written).
    #[serde(flatten)]
    pub counters: BTreeMap<String, u64>,
    /// Declarations, distinct skipped block items and their lines by Cargo package.
    pub per_crate: Vec<CrateCoverage>,
}

/// The counters of `meta`: every `coverage:<key>` row with an integer value, `per_crate`
/// excluded.
fn counters(meta: &Meta) -> BTreeMap<String, u64> {
    meta.iter()
        .filter_map(|(k, v)| {
            let key = k.strip_prefix(PREFIX)?;
            if key == "per_crate" {
                return None;
            }
            Some((key.to_owned(), v.parse().ok()?))
        })
        .collect()
}

/// The compact summary of `meta`: zeros and no note without the rows.
#[must_use]
pub(super) fn summary(meta: &Meta) -> CoverageSummary {
    let counters = counters(meta);
    let count = |k: &str| counters.get(k).copied().unwrap_or(0);
    CoverageSummary {
        declarations: count("declarations"),
        block_items_unique: count("block_items_unique"),
        incomplete_modules: count("incomplete_modules"),
        skipped_targets: count("targets_skipped"),
        note: note(&counters),
    }
}

/// One sentence naming every non-zero gap counter (r1-1, r1-4: each tested on its own, never
/// summed, so an accepted maximum cannot overflow and no gap is silent), `None` when there is
/// no gap.
fn note(counters: &BTreeMap<String, u64>) -> Option<String> {
    let gaps: Vec<String> = GAP_KEYS
        .iter()
        .filter(|&&key| {
            key != "block_items_skipped" || !counters.contains_key("block_items_unique")
        })
        .filter_map(|&key| {
            let count = *counters.get(key)?;
            (count != 0).then(|| format!("{key} {count}"))
        })
        .collect();
    if gaps.is_empty() {
        return None;
    }
    Some(format!(
        "not everything is indexed ({}; the whole table is kaban://code-index); an empty answer does not prove absence",
        gaps.join(", ")
    ))
}

/// The whole table of `meta`, `None` when the snapshot carries no coverage row. A
/// `coverage:per_crate` value that does not parse (a masked one) is an empty table.
#[must_use]
pub(super) fn index(meta: &Meta) -> Option<CoverageIndex> {
    let counters = counters(meta);
    let table = meta.get("coverage:per_crate");
    if counters.is_empty() && table.is_none() {
        return None;
    }
    let per_crate = table
        .and_then(|text| serde_json::from_str(text).ok())
        .unwrap_or_default();
    Some(CoverageIndex {
        counters,
        per_crate,
    })
}

/// The compact coverage of the resolved snapshot, from its `meta`.
///
/// # Errors
/// The reader's failure, by class.
pub(super) fn summary_of(reader: &dyn CodeGraphReader) -> Result<CoverageSummary, CodeError> {
    let meta = reader.meta()?;
    Ok(summary(&meta))
}

#[cfg(test)]
#[allow(clippy::expect_used)] // test code: failures are the assertion
mod tests {
    use super::*;

    fn meta(rows: &[(&str, &str)]) -> Meta {
        rows.iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn without_rows_the_summary_is_zero_with_no_note_and_there_is_no_table() {
        let m = meta(&[("commit", "ab"), ("counts:symbols", "3")]);
        assert_eq!(summary(&m), CoverageSummary::default());
        assert_eq!(index(&m), None);
    }

    #[test]
    fn gaps_make_one_sentence_and_the_table_keeps_every_counter() {
        let m = meta(&[
            ("coverage:declarations", "61569"),
            ("coverage:block_items_skipped", "28242"),
            ("coverage:block_items_unique", "9000"),
            ("coverage:incomplete_modules", "9"),
            ("coverage:targets_skipped", "3"),
            ("coverage:odd", "many"),
            (
                "coverage:per_crate",
                r#"[{"crate":"kaban-api","declarations":1,"block_items_unique":2,"block_item_lines":3}]"#,
            ),
        ]);
        let s = summary(&m);
        assert_eq!((s.declarations, s.block_items_unique), (61569, 9000));
        assert_eq!((s.incomplete_modules, s.skipped_targets), (9, 3));
        assert_eq!(
            s.note.as_deref(),
            Some(
                "not everything is indexed (targets_skipped 3, block_items_unique 9000, incomplete_modules 9; the whole table is kaban://code-index); an empty answer does not prove absence"
            )
        );
        let i = index(&m).expect("rows present");
        assert_eq!(i.counters.get("block_items_skipped"), Some(&28242));
        assert!(
            !i.counters.contains_key("odd"),
            "non-numeric values are left out"
        );
        assert!(!i.counters.contains_key("per_crate"));
        assert_eq!(i.per_crate.len(), 1);
        assert_eq!(i.per_crate[0].name, "kaban-api");
        assert_eq!(
            serde_json::to_value(&i).expect("json")["block_items_unique"],
            9000,
            "the counters are flattened onto the object"
        );
    }

    /// A masked `coverage:per_crate` (the policy hit a crate name, or the table was longer than
    /// the policy's field limit) is an empty table beside intact counters, never an error.
    #[test]
    fn a_masked_table_is_empty_and_the_counters_stay() {
        let m = meta(&[
            ("coverage:declarations", "40"),
            ("coverage:per_crate", "[policy:canary-cred]"),
        ]);
        let i = index(&m).expect("rows present");
        assert_eq!(i.counters.get("declarations"), Some(&40));
        assert_eq!(i.per_crate, Vec::<CrateCoverage>::new());
        assert_eq!(summary(&m).declarations, 40);
    }

    /// r1-1: the note is decided counter by counter, never by a sum; the accepted maximum of
    /// one counter beside another non-zero one is a note, not an overflow.
    #[test]
    fn the_maximum_counter_beside_another_gap_is_a_note_not_an_overflow() {
        let m = meta(&[
            ("coverage:block_items_unique", "18446744073709551615"),
            ("coverage:incomplete_modules", "1"),
            ("coverage:targets_skipped", "0"),
        ]);
        let s = summary(&m);
        assert_eq!(s.block_items_unique, u64::MAX);
        let note = s.note.expect("a gap is a note");
        assert!(note.contains("18446744073709551615"), "{note}");
        assert!(note.contains("does not prove absence"), "{note}");
    }

    /// r1-4: every counter that means "not indexed" carries the note on its own, the legacy
    /// `block_items_skipped` included when the producer wrote no `block_items_unique`.
    #[test]
    fn each_gap_counter_alone_makes_the_note() {
        for key in [
            "targets_skipped",
            "proc_macros_not_built",
            "block_items_unique",
            "block_items_skipped",
            "block_item_references",
            "shared_file_contexts_skipped",
            "incomplete_modules",
            "divergent_bodies",
            "cfg_divergent_declarations",
            "tokens_without_symbol",
            "positions_skipped",
        ] {
            let row = format!("coverage:{key}");
            let m = meta(&[("coverage:declarations", "40"), (&row, "1")]);
            let note = summary(&m).note;
            assert!(note.is_some(), "{key} = 1 carries no note");
            assert!(
                note.as_deref().is_some_and(|n| n.contains(key)),
                "{key}: the note names the gap: {note:?}"
            );
            let m = meta(&[("coverage:declarations", "40"), (&row, "0")]);
            assert_eq!(summary(&m).note, None, "{key} = 0 is no gap");
        }
        // the raw count is superseded by the distinct one when the producer wrote both
        let m = meta(&[
            ("coverage:block_items_skipped", "28242"),
            ("coverage:block_items_unique", "0"),
        ]);
        assert_eq!(summary(&m).note, None, "unique present and zero: no gap");
        // the counters that are not gaps carry none
        let m = meta(&[
            ("coverage:declarations", "40"),
            ("coverage:contexts", "277"),
            ("coverage:scip_bytes", "95184043"),
            ("coverage:block_item_lines", "31000"),
        ]);
        assert_eq!(summary(&m).note, None);
    }

    #[test]
    fn published_declarations_alone_carry_no_note() {
        let m = meta(&[("coverage:declarations", "40")]);
        let s = summary(&m);
        assert_eq!(s.declarations, 40);
        assert_eq!(s.note, None);
        assert!(index(&m).is_some());
    }
}
