//! The Go two-run union over the committed go-drivers runs (stage1-spec G1-F3; stage1-plan
//! CG3.2 slices `union_recovers_sibling_package_implementer`, `union_keeps_test_only_implementer`,
//! `union_is_order_independent`).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use std::collections::BTreeSet;

use kaban_codegraph::EdgeType;
use kaban_index::admit_index;
use kaban_index::union::{self, ImplementsSet};

const PKG: &str = "`example.com/drivers/drivers`/";
const SQLITE_PKG: &str = "`example.com/drivers/sqlite`/";

fn sym(pkg: &str, descriptors: &str) -> String {
    format!(
        "scip-go gomod example.com/drivers {} {pkg}{descriptors}",
        common::GO_DRIVERS_COMMIT
    )
}

/// `(src, dst)` pairs of one edge type whose destination is `dst`.
fn sources_of<'a>(set: &ImplementsSet<'a>, ty: EdgeType, dst: &str) -> BTreeSet<&'a str> {
    set.iter()
        .filter(|(_, d, t)| *t == ty && *d == dst)
        .map(|(s, _, _)| *s)
        .collect()
}

#[test]
fn union_recovers_sibling_package_implementer() {
    let policy = common::test_policy();
    let a_bytes = common::fixture_bytes("go-drivers/index.scip");
    let b_bytes = common::fixture_bytes("go-drivers/index-notest.scip");
    let (a, b) = (
        common::gated(&a_bytes, &policy),
        common::gated(&b_bytes, &policy),
    );
    let (a, b) = (admit_index(&a), admit_index(&b));
    let iface = sym(PKG, "DatabaseDriver#");

    // run A alone (tests compiled) misses the sibling package's implementer: the defect shape
    let a_only = union::implements(&a, None).set;
    let from_a = sources_of(&a_only, EdgeType::Implements, &iface);
    assert!(
        !from_a.contains(sym(SQLITE_PKG, "Driver#").as_str()),
        "{from_a:?}"
    );

    let both = union::implements(&a, Some(&b)).set;
    let expected: BTreeSet<String> = [
        sym(PKG, "MemDriver#"),
        sym(PKG, "PostgresDriver#"),
        sym(PKG, "Wrapped#"),
        sym(SQLITE_PKG, "Driver#"),
    ]
    .into_iter()
    .collect();
    let got: BTreeSet<String> = sources_of(&both, EdgeType::Implements, &iface)
        .into_iter()
        .map(str::to_owned)
        .collect();
    assert_eq!(got, expected, "HttpDriver absent, sqlite.Driver recovered");

    // the §8.2 member pairs: Open and Close for PostgresDriver, MemDriver, sqlite.Driver; none for Wrapped
    let members: BTreeSet<(String, String)> = both
        .iter()
        .filter(|(_, _, t)| *t == EdgeType::ImplementsMember)
        .filter(|(_, d, _)| d.contains("DatabaseDriver#"))
        .map(|(s, d, _)| ((*s).to_owned(), (*d).to_owned()))
        .collect();
    let mut expected_members = BTreeSet::new();
    for (pkg, ty) in [
        (PKG, "PostgresDriver"),
        (PKG, "MemDriver"),
        (SQLITE_PKG, "Driver"),
    ] {
        for m in ["Open", "Close"] {
            expected_members.insert((
                sym(pkg, &format!("{ty}#{m}().")),
                sym(PKG, &format!("DatabaseDriver#{m}.")),
            ));
        }
    }
    assert_eq!(members, expected_members);
    assert!(
        !both
            .iter()
            .any(|(s, _, _)| s.contains("Wrapped#") && s.ends_with("()."))
    );
    // self-loops are dropped (scip-go emits DatabaseDriver#Close. -> itself)
    assert!(both.iter().all(|(s, d, _)| s != d));
}

#[test]
fn union_keeps_test_only_implementer() {
    let policy = common::test_policy();
    let a_bytes = common::fixture_bytes("go-drivers/index.scip");
    let b_bytes = common::fixture_bytes("go-drivers/index-notest.scip");
    let (a, b) = (
        common::gated(&a_bytes, &policy),
        common::gated(&b_bytes, &policy),
    );
    let (a, b) = (admit_index(&a), admit_index(&b));
    let conn = sym(PKG, "Conn#");
    // the test binary links more of the standard library: `os.File` implements Conn in A only
    let os_file = "scip-go gomod github.com/golang/go/src go1.22 os/File#";
    let b_only = union::implements(&b, None).set;
    assert!(!sources_of(&b_only, EdgeType::Implements, &conn).contains(os_file));
    let a_only = union::implements(&a, None).set;
    assert!(sources_of(&a_only, EdgeType::Implements, &conn).contains(os_file));
    let both = union::implements(&a, Some(&b)).set;
    assert!(sources_of(&both, EdgeType::Implements, &conn).contains(os_file));
    assert!(both.is_superset(&a_only) && both.is_superset(&b_only));
}

#[test]
fn union_is_order_independent() {
    let policy = common::test_policy();
    let a_bytes = common::fixture_bytes("go-drivers/index.scip");
    let b_bytes = common::fixture_bytes("go-drivers/index-notest.scip");
    let (a, b) = (
        common::gated(&a_bytes, &policy),
        common::gated(&b_bytes, &policy),
    );
    let (a, b) = (admit_index(&a), admit_index(&b));
    let ab = union::implements(&a, Some(&b)).set;
    let ba = union::implements(&b, Some(&a)).set;
    assert_eq!(ab, ba);
    let dump = |s: &ImplementsSet<'_>| {
        s.iter().fold(String::new(), |mut out, (x, y, t)| {
            use std::fmt::Write as _;
            let _ = writeln!(out, "{x}\t{t}\t{y}");
            out
        })
    };
    assert_eq!(
        dump(&ab),
        dump(&ba),
        "the serialised set is byte-identical either way"
    );
}

// ---------------------------------------------------------------------------------------------
// RU1-4 and RU1-5: rejected identities and run-aware counts through the published snapshot

use kaban_codegraph::{CodeGraphReader, SqliteSnapshot};
use scip::types::symbol_information::Kind;

fn edges_of_type(snap_path: &std::path::Path, ty: &str) -> i64 {
    let conn = rusqlite::Connection::open_with_flags(
        snap_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    conn.query_row("SELECT count(*) FROM edges WHERE type = ?1", [ty], |r| {
        r.get(0)
    })
    .unwrap()
}

/// A: `S` is policy-skipped (a canary in `display_name`), `I` is clean. B: `S` is clean and `I`
/// implements it. The union must not resurrect `S` as a placeholder or emit the edge (RU1-4);
/// the dropped triple is counted beside A's skipped row.
#[test]
fn rejected_identity_of_run_a_never_returns_through_run_b() {
    let policy = common::test_policy();
    let v = common::GO_DRIVERS_COMMIT;
    let (iface, implementer) = (common::go_sym(v, "S#"), common::go_sym(v, "I#"));
    let mut s_hit = common::sym_info_kind(&iface, Kind::Interface);
    s_hit.display_name = "S CANARY_DISPLAY".to_owned();
    let run_a = common::index(vec![common::document(
        "drivers/u.go",
        vec![s_hit, common::sym_info_kind(&implementer, Kind::Struct)],
        vec![
            common::definition(&iface, &[1, 5, 6]),
            common::definition(&implementer, &[3, 5, 6]),
        ],
    )]);
    let mut i_impl = common::sym_info_kind(&implementer, Kind::Struct);
    i_impl.relationships.push(common::implementation(&iface));
    let run_b = common::index(vec![common::document(
        "drivers/u.go",
        vec![common::sym_info_kind(&iface, Kind::Interface), i_impl],
        vec![
            common::definition(&iface, &[1, 5, 6]),
            common::definition(&implementer, &[3, 5, 6]),
        ],
    )]);
    let dir = common::Scratch::new();
    let path = common::publish(
        dir.path(),
        &common::commit(0x14),
        &common::bytes(&run_a),
        Some(&common::bytes(&run_b)),
        &policy,
    );
    let snap = SqliteSnapshot::open_ro(&path).unwrap();
    assert!(
        snap.node(&format!("scip:{iface}")).unwrap().is_none(),
        "no placeholder for the rejected identity"
    );
    assert!(snap.node(&format!("scip:{implementer}")).unwrap().is_some());
    assert_eq!(edges_of_type(&path, "IMPLEMENTS"), 0);
    let meta = snap.meta().unwrap();
    assert_eq!(
        meta["counts:policy_skipped_row"], "3",
        "A: the row and its definition site; the union: the dropped triple"
    );
    assert_eq!(meta["counts:b_policy_skipped_row"], "0");
    assert_eq!(
        meta["counts:placeholder_rows"], "1",
        "the package placeholder only"
    );
}

/// B carries a documentation canary that A does not: B's counter is reported under its own key
/// and A's stays zero (RU1-5).
#[test]
fn run_b_counters_are_reported_under_their_own_keys() {
    let policy = common::test_policy();
    let v = common::GO_DRIVERS_COMMIT;
    let (iface, implementer) = (common::go_sym(v, "S#"), common::go_sym(v, "I#"));
    let mut i_impl = common::sym_info_kind(&implementer, Kind::Struct);
    i_impl.relationships.push(common::implementation(&iface));
    let run_a = common::index(vec![common::document(
        "drivers/u.go",
        vec![
            common::sym_info_kind(&iface, Kind::Interface),
            i_impl.clone(),
        ],
        vec![common::definition(&iface, &[1, 5, 6])],
    )]);
    let mut s_doc = common::sym_info_kind(&iface, Kind::Interface);
    s_doc.documentation = vec!["token CANARY_B_ONLY".to_owned()];
    let run_b = common::index(vec![common::document(
        "drivers/u.go",
        vec![s_doc, i_impl],
        vec![common::definition(&iface, &[1, 5, 6])],
    )]);
    let (ga, gb) = (
        common::gated(&common::bytes(&run_a), &policy),
        common::gated(&common::bytes(&run_b), &policy),
    );
    let (aa, ab) = (admit_index(&ga), admit_index(&gb));
    let rows = kaban_index::writer::Rows::derive(&aa, Some(&ab), &policy);
    assert_eq!(
        rows.warnings(),
        vec![kaban_index::WarningClass::PolicyDropped],
        "B's drop is a warning of the run"
    );
    let dir = common::Scratch::new();
    let path = kaban_index::writer::publish(
        dir.path(),
        &rows,
        &common::meta(&common::commit(0x15)),
        &policy,
    )
    .unwrap();
    let snap = SqliteSnapshot::open_ro(&path).unwrap();
    let meta = snap.meta().unwrap();
    assert_eq!(meta["counts:policy_dropped"], "0", "A dropped nothing");
    assert_eq!(meta["counts:b_policy_dropped"], "1");
    assert_eq!(meta["counts:b_policy_skipped_row"], "0");
    assert_eq!(
        edges_of_type(&path, "IMPLEMENTS"),
        1,
        "the clean relationship stays"
    );
    assert!(
        !common::read(&path)
            .windows(13)
            .any(|w| w == b"CANARY_B_ONLY")
    );
}

/// Grammar failures outside `SymbolInformation` rows are counted (RU1-5): an occurrence-only
/// symbol and a relationship target that fail the grammar.
#[test]
fn grammar_failures_of_occurrence_only_and_relationship_target_symbols_are_counted() {
    let policy = common::test_policy();
    let v = common::GO_DRIVERS_COMMIT;
    let good = common::go_sym(v, "Good#");
    let bad_occ = "scip-go gomod example.com/drivers 1.0.0 `unterminated/Occ#";
    let bad_target = "scip-go gomod example.com/drivers 1.0.0 `unterminated/Target#";
    let mut info = common::sym_info_kind(&good, Kind::Struct);
    info.relationships.push(common::implementation(bad_target));
    let run_a = common::index(vec![common::document(
        "drivers/g.go",
        vec![info],
        vec![
            common::definition(&good, &[1, 5, 9]),
            common::occurrence(bad_occ, &[3, 1, 4]),
        ],
    )]);
    let dir = common::Scratch::new();
    let path = common::publish(
        dir.path(),
        &common::commit(0x16),
        &common::bytes(&run_a),
        None,
        &policy,
    );
    let snap = SqliteSnapshot::open_ro(&path).unwrap();
    let meta = snap.meta().unwrap();
    assert_eq!(
        meta["counts:grammar_skipped_rows"], "2",
        "the occurrence-only symbol and the relationship target: one missing placeholder each"
    );
    assert_eq!(
        meta["counts:grammar_skipped_relationships"], "1",
        "the implementation towards the ungrammatical target"
    );
    assert_eq!(meta["counts:occurrences"], "1", "Good's definition only");
    assert_eq!(edges_of_type(&path, "IMPLEMENTS"), 0);
    assert!(
        !common::read(&path)
            .windows(12)
            .any(|w| w == b"unterminated")
    );
}
