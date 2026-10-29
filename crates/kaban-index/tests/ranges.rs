//! Typed and legacy SCIP ranges (stage1-spec §4.3 `[EN-amend]`, §21 note A1; stage1-plan CG3.3
//! slices `edges_from_typed_enclosing_range`, `edges_from_legacy_enclosing_range`,
//! `mixed_index_reports_mixed`) over the committed synthetic indexes.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use std::collections::BTreeSet;

use kaban_codegraph::{CodeGraphReader, Range, SqliteSnapshot};
use kaban_index::ranges::{self, RangeForm, RangeForms};

fn publish_synthetic(stem: &str, dir: &common::Scratch) -> SqliteSnapshot {
    let policy = common::test_policy();
    let bytes = common::synthetic_bytes(stem);
    let p = common::publish(dir.path(), &common::commit(0x44), &bytes, None, &policy);
    SqliteSnapshot::open_ro(&p).unwrap()
}

fn f_references_g(snap: &SqliteSnapshot) -> BTreeSet<(String, usize)> {
    let g = common::go_sym(common::GO_DRIVERS_COMMIT, "G().");
    let node = snap.node(&format!("scip:{g}")).unwrap().unwrap();
    snap.references(node.id, true)
        .unwrap()
        .into_iter()
        .map(|grp| (grp.enclosing.r#ref, grp.sites.len()))
        .collect()
}

fn expected_f() -> BTreeSet<(String, usize)> {
    let f = common::go_sym(common::GO_DRIVERS_COMMIT, "F().");
    BTreeSet::from([(format!("scip:{f}"), 1)])
}

#[test]
fn edges_from_typed_enclosing_range() {
    let dir = common::Scratch::new();
    let snap = publish_synthetic("typed-only", &dir);
    assert_eq!(f_references_g(&snap), expected_f());
    assert_eq!(snap.meta().unwrap()["scip_range_form"], "typed");
    let f = common::go_sym(common::GO_DRIVERS_COMMIT, "F().");
    let row = snap.node(&format!("scip:{f}")).unwrap().unwrap();
    assert_eq!(
        row.def,
        Some(Range {
            sl: 2,
            sc: 5,
            el: 2,
            ec: 6
        })
    );
    assert_eq!(
        row.enc,
        Some(Range {
            sl: 2,
            sc: 0,
            el: 5,
            ec: 1
        })
    );
}

#[test]
fn edges_from_legacy_enclosing_range() {
    let dir = common::Scratch::new();
    let snap = publish_synthetic("legacy-only", &dir);
    assert_eq!(f_references_g(&snap), expected_f());
    assert_eq!(snap.meta().unwrap()["scip_range_form"], "legacy");
}

#[test]
fn mixed_index_reports_mixed() {
    let dir = common::Scratch::new();
    let snap = publish_synthetic("mixed", &dir);
    assert_eq!(f_references_g(&snap), expected_f());
    assert_eq!(snap.meta().unwrap()["scip_range_form"], "mixed");
    // the three publications differ only in meta: same rows either way
    let d2 = common::Scratch::new();
    let typed = publish_synthetic("typed-only", &d2);
    assert_eq!(
        snap.nodes_in_file("drivers/forms.go").unwrap(),
        typed.nodes_in_file("drivers/forms.go").unwrap()
    );
}

#[test]
fn legacy_range_forms_and_folding() {
    assert_eq!(
        ranges::legacy(&[3, 1, 4]),
        Ok(Some(Range {
            sl: 3,
            sc: 1,
            el: 3,
            ec: 4
        }))
    );
    assert_eq!(
        ranges::legacy(&[3, 1, 5, 0]),
        Ok(Some(Range {
            sl: 3,
            sc: 1,
            el: 5,
            ec: 0
        }))
    );
    assert_eq!(ranges::legacy(&[]), Ok(None), "no range emitted");
    assert_eq!(
        ranges::legacy(&[1, 2]),
        Err(ranges::MalformedRange),
        "two integers are no range"
    );
    assert_eq!(
        ranges::legacy(&[-1, 0, 1]),
        Err(ranges::MalformedRange),
        "negative coordinates are no range"
    );
    assert_eq!(
        ranges::legacy(&[10, 0, 5, 0]),
        Err(ranges::MalformedRange),
        "an end line before the start line is no range (RU1-7)"
    );
    assert_eq!(
        ranges::legacy(&[3, 7, 3, 2]),
        Err(ranges::MalformedRange),
        "an end column before the start column on one line is no range (RU1-7)"
    );
    assert_eq!(ranges::MalformedRange.to_string(), "malformed_range");
    let outer = Range {
        sl: 2,
        sc: 0,
        el: 5,
        ec: 1,
    };
    assert!(ranges::contains(
        outer,
        Range {
            sl: 3,
            sc: 2,
            el: 3,
            ec: 3
        }
    ));
    assert!(ranges::contains(outer, outer));
    assert!(
        !ranges::contains(
            outer,
            Range {
                sl: 5,
                sc: 0,
                el: 5,
                ec: 2
            }
        ),
        "end past outer"
    );
    assert!(
        !ranges::contains(
            outer,
            Range {
                sl: 1,
                sc: 9,
                el: 2,
                ec: 0
            }
        ),
        "start before outer"
    );
    let fold = RangeForms::default();
    assert_eq!(
        fold.as_str(),
        "legacy",
        "nothing read yet reports the schema's historical form"
    );
    assert_eq!(fold.with(RangeForm::Typed), RangeForms::Typed);
    assert_eq!(fold.with(RangeForm::Legacy), RangeForms::Legacy);
    assert_eq!(
        fold.with(RangeForm::Typed).with(RangeForm::Legacy),
        RangeForms::Mixed
    );
    assert_eq!(
        fold.with(RangeForm::Legacy).with(RangeForm::Typed),
        RangeForms::Mixed
    );
    assert_eq!(RangeForms::Mixed.with(RangeForm::Typed), RangeForms::Mixed);
}

// ---------------------------------------------------------------------------------------------
// RU1-7: malformed producer ranges are counted skips, never a panic or a persisted span

/// The reviewer's case: `F` with the inverted enclosing range `[10,0,5,0]` and a site of `G` at
/// `[11,0,4,0]`, plus a reversed-column site `[3,7,3,2]`, in the legacy form. The rows stay
/// (without the inverted `enc`), the two sites are skipped and counted, nothing panics with
/// overflow checks on (the test profile).
#[test]
fn inverted_legacy_ranges_are_counted_skips() {
    use scip::types::symbol_information::Kind;
    let policy = common::test_policy();
    let v = common::GO_DRIVERS_COMMIT;
    let (f, g) = (common::go_sym(v, "F()."), common::go_sym(v, "G()."));
    let idx = common::index(vec![common::document(
        "drivers/inv.go",
        vec![
            common::sym_info_kind(&f, Kind::Function),
            common::sym_info_kind(&g, Kind::Function),
        ],
        vec![
            common::definition_enclosing(&f, &[10, 5, 6], &[10, 0, 5, 0]),
            common::definition(&g, &[20, 5, 6]),
            common::occurrence(&g, &[11, 0, 4, 0]),
            common::occurrence(&g, &[3, 7, 3, 2]),
            common::occurrence(&g, &[12, 1, 2]), // well-formed, outside every encloser
        ],
    )]);
    let dir = common::Scratch::new();
    let p = common::publish(
        dir.path(),
        &common::commit(0x77),
        &common::bytes(&idx),
        None,
        &policy,
    );
    let snap = SqliteSnapshot::open_ro(&p).unwrap();
    let f_row = snap.node(&format!("scip:{f}")).unwrap().unwrap();
    assert_eq!(
        f_row.def,
        Some(Range {
            sl: 10,
            sc: 5,
            el: 10,
            ec: 6
        })
    );
    assert_eq!(
        f_row.enc, None,
        "the inverted enclosing range is not persisted"
    );
    assert_eq!(
        f_references_g(&snap),
        BTreeSet::from([("file:drivers/inv.go".to_owned(), 1)]),
        "only the well-formed site, attributed to the file"
    );
    let meta = snap.meta().unwrap();
    assert_eq!(
        meta["counts:malformed_range_skipped"], "3",
        "the encloser and the two inverted sites"
    );
    assert_eq!(
        meta["counts:occurrences"], "3",
        "F def, G def, the one good site"
    );
}

/// The same in the typed form (`multi_line_range` / `multi_line_enclosing_range`): a malformed
/// typed range is refused, not silently replaced by a legacy one.
#[test]
fn inverted_typed_ranges_are_counted_skips() {
    use scip::types::symbol_information::Kind;
    let policy = common::test_policy();
    let v = common::GO_DRIVERS_COMMIT;
    let (f, g) = (common::go_sym(v, "F()."), common::go_sym(v, "G()."));
    let typed = common::RangeStyle::Typed;
    let idx = common::index(vec![common::document(
        "drivers/inv.go",
        vec![
            common::sym_info_kind(&f, Kind::Function),
            common::sym_info_kind(&g, Kind::Function),
        ],
        vec![
            common::styled(&f, 1, [10, 5, 10, 6], Some([10, 0, 5, 0]), typed),
            common::styled(&g, 1, [20, 5, 20, 6], None, typed),
            common::styled(&g, 0, [11, 0, 4, 0], None, typed),
            common::styled(&g, 0, [3, 7, 3, 2], None, typed),
        ],
    )]);
    let dir = common::Scratch::new();
    let p = common::publish(
        dir.path(),
        &common::commit(0x78),
        &common::bytes(&idx),
        None,
        &policy,
    );
    let snap = SqliteSnapshot::open_ro(&p).unwrap();
    let f_row = snap.node(&format!("scip:{f}")).unwrap().unwrap();
    assert_eq!(f_row.enc, None);
    let meta = snap.meta().unwrap();
    assert_eq!(meta["counts:malformed_range_skipped"], "3");
    assert_eq!(meta["counts:occurrences"], "2", "the two definitions only");
    assert_eq!(meta["scip_range_form"], "typed");
}
