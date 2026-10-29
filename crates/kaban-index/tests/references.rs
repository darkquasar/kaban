//! Occurrences, `REFERENCES` and `TESTED_BY` over the committed fixtures (stage1-spec §4.3,
//! G1-F4, G1-F6, §8.2; stage1-plan CG3.3 slices `writer_golden_references_go_drivers`,
//! `tests_for_reference_form`, `tests_for_naming_form` and the TypeScript slices, which run for
//! real because the ts-drivers index exists, OWN-57).
//!
//! The expected `(E, S, weight)` rows were recorded from §8.2 before the first run (see the
//! GM3 report): `code_references(DatabaseDriver#)` lists the enclosing node `Wrapped#`,
//! `HttpDriver` absent. Observed: scip-go 0.2.7 emits no `enclosing_range` for type
//! declarations, so the field-type reference inside `Wrapped` attributes to the file node
//! `file:drivers/wrapped.go` (a documented producer behaviour, spec correction requested; the
//! golden fixes the exact set after the first run per §8.2).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use std::collections::BTreeSet;

use kaban_codegraph::reader::{MatchedBy, TestsTarget};
use kaban_codegraph::{CodeGraphReader, SqliteSnapshot};
use kaban_index::derive::test_file_for;

const GO: &str = "scip:scip-go gomod example.com/drivers 730da842221a844e8c2c82891dda9094f6428858 `example.com/drivers/drivers`/";
const TS: &str = "scip:scip-typescript npm @example/drivers 1.0.0 src/";

fn go_snapshot(dir: &common::Scratch) -> SqliteSnapshot {
    let policy = common::test_policy();
    let a = common::fixture_bytes("go-drivers/index.scip");
    let b = common::fixture_bytes("go-drivers/index-notest.scip");
    let p = common::publish(dir.path(), common::GO_DRIVERS_COMMIT, &a, Some(&b), &policy);
    SqliteSnapshot::open_ro(&p).unwrap()
}

fn ts_snapshot(dir: &common::Scratch) -> SqliteSnapshot {
    let policy = common::test_policy();
    let a = common::fixture_bytes("ts-drivers/index.scip");
    let p = common::publish(dir.path(), &common::commit(0xab), &a, None, &policy);
    SqliteSnapshot::open_ro(&p).unwrap()
}

/// `(enclosing ref, site count)` groups of the references of `ref`, tests included.
fn reference_groups(snap: &SqliteSnapshot, r#ref: &str) -> BTreeSet<(String, usize)> {
    let node = snap
        .node(r#ref)
        .unwrap()
        .unwrap_or_else(|| panic!("node {ref}"));
    snap.references(node.id, true)
        .unwrap()
        .into_iter()
        .map(|g| (g.enclosing.r#ref, g.sites.len()))
        .collect()
}

#[test]
fn writer_golden_references_go_drivers() {
    let dir = common::Scratch::new();
    let snap = go_snapshot(&dir);
    let iface = format!("{GO}DatabaseDriver#");
    // expected per §8.2: {Wrapped#}; observed: the file node of wrapped.go (see the module doc)
    assert_eq!(
        reference_groups(&snap, &iface),
        BTreeSet::from([("file:drivers/wrapped.go".to_owned(), 1)])
    );
    assert!(
        !reference_groups(&snap, &iface)
            .iter()
            .any(|(e, _)| e.contains("HttpDriver")),
        "HttpDriver absent"
    );
    // the test function references PostgresDriver# and PostgresDriver#Open(). once each
    let test_fn = format!("{GO}TestPostgresOpen().");
    assert_eq!(
        reference_groups(&snap, &format!("{GO}PostgresDriver#Open().")),
        BTreeSet::from([(test_fn.clone(), 1)])
    );
    assert!(reference_groups(&snap, &format!("{GO}PostgresDriver#")).contains(&(test_fn, 1)));
    // Conn# is referenced from the bodies of the Open methods (inside their enclosing ranges)
    let conn_groups = reference_groups(&snap, &format!("{GO}Conn#"));
    assert!(
        conn_groups.contains(&(format!("{GO}PostgresDriver#Open()."), 1)),
        "{conn_groups:?}"
    );
    assert!(conn_groups.contains(&(format!("{GO}MemDriver#Open()."), 1)));
    // weights are site counts: an occurrence outside every enclosing range goes to the file node
    let meta = snap.meta().unwrap();
    assert_eq!(
        meta["scip_range_form"], "legacy",
        "scip-go 0.2.7 emits legacy ranges only"
    );
    assert_eq!(
        meta["counts:occurrences"], "82",
        "every site of the seven admitted documents"
    );
}

#[test]
fn tests_for_reference_form() {
    let dir = common::Scratch::new();
    let snap = go_snapshot(&dir);
    let open = snap
        .node(&format!("{GO}PostgresDriver#Open()."))
        .unwrap()
        .unwrap();
    let links = snap.tests_for(TestsTarget::Symbol(open.id)).unwrap();
    let got: Vec<(String, MatchedBy, bool)> = links
        .iter()
        .map(|l| (l.test.r#ref.clone(), l.matched_by, l.heuristic))
        .collect();
    assert_eq!(
        got,
        vec![(
            format!("{GO}TestPostgresOpen()."),
            MatchedBy::Reference,
            false
        )]
    );
    // a symbol no test references has no reference-form link
    let http = snap
        .node(&format!("{GO}HttpDriver#Open()."))
        .unwrap()
        .unwrap();
    assert_eq!(
        snap.tests_for(TestsTarget::Symbol(http.id)).unwrap().len(),
        0
    );
}

#[test]
fn tests_for_naming_form() {
    let dir = common::Scratch::new();
    let snap = go_snapshot(&dir);
    let file = snap.node("file:drivers/postgres.go").unwrap().unwrap();
    let links = snap
        .tests_for(TestsTarget::File(file.file_id.unwrap()))
        .unwrap();
    let got: Vec<(String, MatchedBy, bool)> = links
        .iter()
        .map(|l| (l.test.r#ref.clone(), l.matched_by, l.heuristic))
        .collect();
    assert_eq!(
        got,
        vec![(
            "file:drivers/postgres_test.go".to_owned(),
            MatchedBy::Naming,
            true
        )]
    );
    let other = snap.node("file:drivers/http.go").unwrap().unwrap();
    assert_eq!(
        snap.tests_for(TestsTarget::File(other.file_id.unwrap()))
            .unwrap()
            .len(),
        0
    );
}

// ---------------------------------------------------------------------------------------------
// TypeScript (RV-10; the fixture exists, OWN-57)

#[test]
fn writer_golden_references_ts_drivers() {
    let dir = common::Scratch::new();
    let snap = ts_snapshot(&dir);
    let iface = format!("{TS}`driver.ts`/DatabaseDriver#");
    // scip-typescript emits an enclosing range for classes and a whole-file module symbol
    // (`src/`mem.ts`/`), so the `implements` clause attributes to the class and the import to
    // the module symbol; MockDriver never names the interface
    let groups = reference_groups(&snap, &iface);
    assert_eq!(
        groups,
        BTreeSet::from([
            (format!("{TS}`mem.ts`/"), 1),
            (format!("{TS}`mem.ts`/MemDriver#"), 1),
            (format!("{TS}`postgres.ts`/"), 1),
            (format!("{TS}`postgres.ts`/PostgresDriver#"), 1),
        ]),
        "{groups:?}"
    );
    assert!(!groups.iter().any(|(e, _)| e.contains("mock")));
    let meta = snap.meta().unwrap();
    assert_eq!(
        meta["scip_range_form"], "legacy",
        "scip-typescript 0.4.0 emits legacy ranges"
    );
    assert_eq!(meta["languages"], "typescript");
    assert_eq!(meta["counts:language_from_extension"], "5");
}

#[test]
fn tests_for_reference_form_ts() {
    let dir = common::Scratch::new();
    let snap = ts_snapshot(&dir);
    let open = snap
        .node(&format!("{TS}`postgres.ts`/PostgresDriver#open()."))
        .unwrap()
        .unwrap();
    // expected per §8.2: the test function of src/postgres.test.ts by reference; observed: the
    // fixture's `test` is an ambient `declare function` and the call sits at module level, so
    // the reference is enclosed by scip-typescript's whole-file module symbol (a Namespace
    // descriptor, neither Function nor Method) and no reference-form link exists (spec
    // correction requested); the site itself is recorded against the module symbol
    let links = snap.tests_for(TestsTarget::Symbol(open.id)).unwrap();
    assert_eq!(links.len(), 0, "{links:?}");
    assert_eq!(
        reference_groups(&snap, &open.r#ref),
        BTreeSet::from([(format!("{TS}`postgres.test.ts`/"), 1)])
    );
}

#[test]
fn tests_for_naming_form_ts() {
    let dir = common::Scratch::new();
    let snap = ts_snapshot(&dir);
    let file = snap.node("file:src/postgres.ts").unwrap().unwrap();
    let links = snap
        .tests_for(TestsTarget::File(file.file_id.unwrap()))
        .unwrap();
    let got: Vec<(String, MatchedBy, bool)> = links
        .iter()
        .map(|l| (l.test.r#ref.clone(), l.matched_by, l.heuristic))
        .collect();
    assert_eq!(
        got,
        vec![(
            "file:src/postgres.test.ts".to_owned(),
            MatchedBy::Naming,
            true
        )]
    );
    // the three TypeScript naming rules on invented paths
    assert_eq!(
        test_file_for("src/x.ts", "typescript"),
        vec!["src/x.test.ts", "src/x.spec.ts", "src/__tests__/x.ts"]
    );
    assert_eq!(
        test_file_for("x.tsx", "typescript"),
        vec!["x.test.tsx", "x.spec.tsx", "__tests__/x.tsx"]
    );
    assert_eq!(test_file_for("pkg/x.go", "go"), vec!["pkg/x_test.go"]);
    assert_eq!(test_file_for("pkg/x_test.go", "go"), Vec::<String>::new());
    assert_eq!(test_file_for("README.md", "markdown"), Vec::<String>::new());
}

#[test]
fn determinism_index_twice_ts_drivers() {
    let policy = common::test_policy();
    let a = common::fixture_bytes("ts-drivers/index.scip");
    let (d1, d2) = (common::Scratch::new(), common::Scratch::new());
    let p1 = common::publish(d1.path(), &common::commit(0xab), &a, None, &policy);
    let p2 = common::publish(d2.path(), &common::commit(0xab), &a, None, &policy);
    assert_eq!(common::read(&p1), common::read(&p2));
}

// ---------------------------------------------------------------------------------------------
// negative paths

#[test]
fn occurrence_outside_every_enclosing_range_attributes_to_the_file_node() {
    use scip::types::symbol_information::Kind;
    let policy = common::test_policy();
    let v = common::GO_DRIVERS_COMMIT;
    let (f, g) = (common::go_sym(v, "F()."), common::go_sym(v, "G()."));
    let idx = common::index(vec![common::document(
        "drivers/a.go",
        vec![
            common::sym_info_kind(&f, Kind::Function),
            common::sym_info_kind(&g, Kind::Function),
        ],
        vec![
            common::definition_enclosing(&f, &[2, 5, 6], &[2, 0, 4, 1]),
            common::definition(&g, &[6, 5, 6]),
            common::occurrence(&g, &[3, 2, 3]), // inside F
            common::occurrence(&g, &[9, 0, 1]), // outside every range
        ],
    )]);
    let dir = common::Scratch::new();
    let p = common::publish(
        dir.path(),
        &common::commit(7),
        &common::bytes(&idx),
        None,
        &policy,
    );
    let snap = SqliteSnapshot::open_ro(&p).unwrap();
    assert_eq!(
        reference_groups(&snap, &format!("scip:{g}")),
        BTreeSet::from([
            (format!("scip:{f}"), 1),
            ("file:drivers/a.go".to_owned(), 1)
        ])
    );
}

#[test]
fn occurrence_of_a_gate_skipped_symbol_is_dropped() {
    use scip::types::symbol_information::Kind;
    let policy = common::test_policy();
    let v = common::GO_DRIVERS_COMMIT;
    let (f, bad) = (common::go_sym(v, "F()."), common::go_sym(v, "CANARY_SYM#"));
    let idx = common::index(vec![common::document(
        "drivers/a.go",
        vec![
            common::sym_info_kind(&f, Kind::Function),
            common::sym_info_kind(&bad, Kind::Struct),
        ],
        vec![
            common::definition_enclosing(&f, &[2, 5, 6], &[2, 0, 4, 1]),
            common::definition(&bad, &[6, 5, 14]),
            common::occurrence(&bad, &[3, 2, 12]), // inside F: would be a REFERENCES edge
        ],
    )]);
    let dir = common::Scratch::new();
    let p = common::publish(
        dir.path(),
        &common::commit(8),
        &common::bytes(&idx),
        None,
        &policy,
    );
    let bytes = common::read(&p);
    assert!(!bytes.windows(10).any(|w| w == b"CANARY_SYM"));
    let snap = SqliteSnapshot::open_ro(&p).unwrap();
    let f_node = snap.node(&format!("scip:{f}")).unwrap().unwrap();
    let graph = snap
        .neighbours(
            f_node.id,
            kaban_codegraph::Depth::new(1, snap.limits()).unwrap(),
            &[],
        )
        .unwrap();
    assert!(
        graph
            .edges
            .iter()
            .all(|e| e.r#type != kaban_codegraph::EdgeType::References),
        "{graph:?}"
    );
    let meta = snap.meta().unwrap();
    assert_eq!(
        meta["counts:policy_skipped_row"], "3",
        "row, its definition, its reference"
    );
    assert_eq!(
        meta["counts:occurrences"], "1",
        "only F's definition site remains"
    );
}

/// RU1-6: a producer record that repeats a site adds no weight. Two identical non-definition
/// occurrences of `G` inside `F` are one `occurrences` row, one grouped site for the reader and
/// a `REFERENCES` weight of one; a distinct third site makes two.
#[test]
fn references_weight_equals_the_deduplicated_site_count() {
    use scip::types::symbol_information::Kind;
    let policy = common::test_policy();
    let v = common::GO_DRIVERS_COMMIT;
    let (f, g) = (common::go_sym(v, "F()."), common::go_sym(v, "G()."));
    let idx = common::index(vec![common::document(
        "drivers/dup.go",
        vec![
            common::sym_info_kind(&f, Kind::Function),
            common::sym_info_kind(&g, Kind::Function),
        ],
        vec![
            common::definition_enclosing(&f, &[2, 5, 6], &[2, 0, 6, 1]),
            common::definition(&g, &[8, 5, 6]),
            common::occurrence(&g, &[3, 2, 3]),
            common::occurrence(&g, &[3, 2, 3]), // the same site again
            common::occurrence(&g, &[4, 2, 3]),
        ],
    )]);
    let dir = common::Scratch::new();
    let p = common::publish(
        dir.path(),
        &common::commit(0x66),
        &common::bytes(&idx),
        None,
        &policy,
    );
    let snap = SqliteSnapshot::open_ro(&p).unwrap();
    let groups = reference_groups(&snap, &format!("scip:{g}"));
    assert_eq!(groups, BTreeSet::from([(format!("scip:{f}"), 2)]));
    let conn =
        rusqlite::Connection::open_with_flags(&p, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let weight: i64 = conn
        .query_row(
            "SELECT weight FROM edges WHERE type = 'REFERENCES'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(weight, 2, "weight = distinct sites, not producer records");
    let occurrences: i64 = conn
        .query_row("SELECT count(*) FROM occurrences", [], |r| r.get(0))
        .unwrap();
    assert_eq!(occurrences, 4, "F def, G def, two distinct sites");
    assert_eq!(snap.meta().unwrap()["counts:occurrences"], "4");
}
