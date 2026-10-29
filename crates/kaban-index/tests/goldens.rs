//! Goldens, the §8.2 row-level expectations through the CG2.3 queries, and the writer half of
//! the canary sweep (stage1-spec §8.2, §8.3 step 7, G1-F1 verify; stage1-plan CG3.4,
//! `focused-postcheck` with the golden review rule: the goldens were written from the first run
//! by `regen_goldens` and reviewed against §8.2 in the GM3 report; a mismatch with §8.2 is a
//! spec correction request, never a golden edit).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use kaban_codegraph::{CodeGraphReader, EdgeType, SqliteSnapshot};
use kaban_index::admit_index;
use kaban_index::writer::Rows;
use rusqlite::{Connection, OpenFlags};

const GO: &str = "scip:scip-go gomod example.com/drivers 730da842221a844e8c2c82891dda9094f6428858 `example.com/drivers/drivers`/";
const GO_SQLITE: &str = "scip:scip-go gomod example.com/drivers 730da842221a844e8c2c82891dda9094f6428858 `example.com/drivers/sqlite`/";
const TS: &str = "scip:scip-typescript npm @example/drivers 1.0.0 src/";

/// The published go-drivers snapshot (run A + run B) under the canary policy.
fn publish_go(dir: &Path) -> PathBuf {
    let a = common::fixture_bytes("go-drivers/index.scip");
    let b = common::fixture_bytes("go-drivers/index-notest.scip");
    common::publish(
        dir,
        common::GO_DRIVERS_COMMIT,
        &a,
        Some(&b),
        &common::canary_policy(),
    )
}

fn publish_ts(dir: &Path) -> PathBuf {
    let a = common::fixture_bytes("ts-drivers/index.scip");
    common::publish(
        dir,
        &common::commit(0xab),
        &a,
        None,
        &common::canary_policy(),
    )
}

// ---------------------------------------------------------------------------------------------
// sorted row dumps

/// `(name, text)` dumps of every table in the §4.1 order, one row per line, tab separated.
fn dumps(path: &Path) -> Vec<(&'static str, String)> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let dump = |sql: &str| {
        let mut stmt = conn.prepare(sql).unwrap();
        let cols = stmt.column_count();
        let mut out = String::new();
        let rows = stmt
            .query_map([], |r| {
                (0..cols)
                    .map(|i| {
                        r.get_ref(i).map(|v| match v {
                            rusqlite::types::ValueRef::Null => "NULL".to_owned(),
                            rusqlite::types::ValueRef::Integer(n) => n.to_string(),
                            rusqlite::types::ValueRef::Real(f) => f.to_string(),
                            rusqlite::types::ValueRef::Text(t) => {
                                String::from_utf8_lossy(t).replace(['\t', '\n'], " ")
                            }
                            rusqlite::types::ValueRef::Blob(b) => format!("<{} bytes>", b.len()),
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()
            })
            .unwrap();
        for r in rows {
            let _ = writeln!(out, "{}", r.unwrap().join("\t"));
        }
        out
    };
    vec![
        (
            "files",
            dump("SELECT id, path, language, is_test, lines FROM files ORDER BY id"),
        ),
        (
            "symbols",
            dump(
                "SELECT id, kind, ref, key, label, name, file_id, def_sl, def_sc, def_el, def_ec, \
                 enc_sl, enc_sc, enc_el, enc_ec, external, is_test, ident_terms, \
                 length(signature), length(doc) FROM symbols ORDER BY id",
            ),
        ),
        (
            "edges",
            dump(
                "SELECT (SELECT ref FROM symbols WHERE id = e.src), e.type, \
                 (SELECT ref FROM symbols WHERE id = e.dst), e.weight FROM edges e \
                 ORDER BY e.src, e.type, e.dst",
            ),
        ),
        (
            "occurrences",
            dump(
                "SELECT f.path, o.sl, o.sc, o.el, o.ec, (SELECT ref FROM symbols WHERE id = o.symbol_id), \
                 o.roles, (SELECT ref FROM symbols WHERE id = o.enclosing_id) \
                 FROM occurrences o JOIN files f ON f.id = o.file_id \
                 ORDER BY o.file_id, o.sl, o.sc, o.el, o.ec, o.symbol_id, o.roles",
            ),
        ),
        (
            "edge_types",
            dump("SELECT type, source, confidence FROM edge_types ORDER BY type"),
        ),
        ("meta", dump("SELECT key, value FROM meta ORDER BY key")),
        // G1-N3: the snapshot size is reported per run; deterministic bytes, deterministic size
        (
            "snapshot_bytes",
            format!("{}\n", std::fs::metadata(path).unwrap().len()),
        ),
    ]
}

fn goldens_dir(fixture: &str) -> PathBuf {
    common::fixtures_dir().join(fixture).join("goldens")
}

fn assert_goldens(fixture: &str, path: &Path) {
    for (name, text) in dumps(path) {
        let golden = goldens_dir(fixture).join(format!("{name}.txt"));
        let committed = std::fs::read(&golden).unwrap_or_else(|e| {
            panic!(
                "{} missing ({e}); run the ignored regen_goldens test",
                golden.display()
            )
        });
        assert!(
            committed == text.as_bytes(),
            "{} differs from the published rows (a defect or a producer change; never edit the \
             golden by hand, see the GM3 report)\n--- committed\n{}\n--- observed\n{text}",
            golden.display(),
            String::from_utf8_lossy(&committed)
        );
    }
}

#[test]
fn writer_golden_symbols_go_drivers() {
    let dir = common::Scratch::new();
    let p = publish_go(dir.path());
    assert_goldens("go-drivers", &p);
}

#[test]
fn writer_golden_symbols_ts_drivers() {
    let dir = common::Scratch::new();
    let p = publish_ts(dir.path());
    assert_goldens("ts-drivers", &p);
}

/// Writes the goldens of both fixtures from a fresh publication (committed; stage1-plan CG3.4).
/// Run on purpose only: `-- --ignored regen_goldens`; review the diff against §8.2.
#[test]
#[ignore = "regenerates committed golden bytes"]
fn regen_goldens() {
    for (fixture, publish) in [
        ("go-drivers", publish_go as fn(&Path) -> PathBuf),
        ("ts-drivers", publish_ts as fn(&Path) -> PathBuf),
    ] {
        let dir = common::Scratch::new();
        let p = publish(dir.path());
        let out = goldens_dir(fixture);
        std::fs::create_dir_all(&out).unwrap();
        for (name, text) in dumps(&p) {
            std::fs::write(out.join(format!("{name}.txt")), text).unwrap();
        }
    }
}

// ---------------------------------------------------------------------------------------------
// §8.2 at the row level, through the CG2.3 queries

fn refs(rows: &[kaban_codegraph::NodeRow]) -> Vec<String> {
    rows.iter().map(|r| r.r#ref.clone()).collect()
}

#[test]
fn go_drivers_expected_rows_of_section_8_2() {
    let dir = common::Scratch::new();
    let snap = SqliteSnapshot::open_ro(&publish_go(dir.path())).unwrap();
    // code_implementers({label: Interface, name: DatabaseDriver}), include_external: false
    let iface = snap
        .by_label_name("Interface", "DatabaseDriver")
        .unwrap()
        .pop()
        .expect("one Interface named DatabaseDriver");
    let implementers = snap.implementers(iface.id, false).unwrap();
    assert_eq!(
        refs(&implementers),
        vec![
            format!("{GO}MemDriver#"),
            format!("{GO}PostgresDriver#"),
            format!("{GO}Wrapped#"),
            format!("{GO_SQLITE}Driver#"),
        ],
        "4 rows ordered by path then ref; HttpDriver absent"
    );
    // code_members(PostgresDriver#, labels: [Method]): Open(). and Close(). (2 rows)
    let pg = snap.node(&format!("{GO}PostgresDriver#")).unwrap().unwrap();
    let members = snap.members(pg.id, &["Method"]).unwrap();
    assert_eq!(
        refs(&members),
        vec![
            format!("{GO}PostgresDriver#Close()."),
            format!("{GO}PostgresDriver#Open().")
        ]
    );
    // code_members(Wrapped#): one row, the embedded field DatabaseDriver with label Field
    let wrapped = snap.node(&format!("{GO}Wrapped#")).unwrap().unwrap();
    let members = snap.members(wrapped.id, &[]).unwrap();
    assert_eq!(members.len(), 1);
    assert_eq!(members[0].label, "Field");
    assert_eq!(members[0].name, "DatabaseDriver");
    // IMPLEMENTS_MEMBER: Open and Close of PostgresDriver, MemDriver, sqlite.Driver towards the
    // DatabaseDriver members (6 edges); 0 for Wrapped
    let mut member_edges = BTreeSet::new();
    for m in ["Open", "Close"] {
        let spec = snap
            .node(&format!("{GO}DatabaseDriver#{m}."))
            .unwrap()
            .unwrap();
        let g = snap
            .neighbours(
                spec.id,
                kaban_codegraph::Depth::new(1, snap.limits()).unwrap(),
                &[EdgeType::ImplementsMember],
            )
            .unwrap();
        for e in g.edges {
            if e.to == spec.r#ref {
                member_edges.insert(e.from);
            }
        }
    }
    let expected: BTreeSet<String> = [
        format!("{GO}PostgresDriver#Open()."),
        format!("{GO}PostgresDriver#Close()."),
        format!("{GO}MemDriver#Open()."),
        format!("{GO}MemDriver#Close()."),
        format!("{GO_SQLITE}Driver#Open()."),
        format!("{GO_SQLITE}Driver#Close()."),
    ]
    .into_iter()
    .collect();
    assert_eq!(
        member_edges, expected,
        "6 IMPLEMENTS_MEMBER edges, none from Wrapped"
    );
    assert!(!member_edges.iter().any(|r| r.contains("Wrapped")));
}

#[test]
fn ts_drivers_expected_rows_of_section_8_2() {
    let dir = common::Scratch::new();
    let snap = SqliteSnapshot::open_ro(&publish_ts(dir.path())).unwrap();
    // expected per §8.2: selection by {label: Interface, name: DatabaseDriver}; observed:
    // scip-typescript 0.4.0 emits UnspecifiedKind for every symbol, so the by-label lookup is
    // empty and the row is selected by key (spec correction requested)
    assert_eq!(
        snap.by_label_name("Interface", "DatabaseDriver")
            .unwrap()
            .len(),
        0
    );
    let iface = snap
        .symbol_unique("scip-typescript npm @example/drivers . src/`driver.ts`/DatabaseDriver#")
        .unwrap()
        .expect("DatabaseDriver by key");
    assert_eq!(iface.label, "UnspecifiedKind");
    let implementers = snap.implementers(iface.id, false).unwrap();
    assert_eq!(
        refs(&implementers),
        vec![
            format!("{TS}`mem.ts`/MemDriver#"),
            format!("{TS}`postgres.ts`/PostgresDriver#")
        ],
        "exactly PostgresDriver and MemDriver; MockDriver absent (no `implements`)"
    );
    // code_members(PostgresDriver#, labels: [Method]) expected open(). and close().; observed by
    // label: nothing (UnspecifiedKind); without the label filter: the two methods
    let pg = snap
        .node(&format!("{TS}`postgres.ts`/PostgresDriver#"))
        .unwrap()
        .unwrap();
    assert_eq!(snap.members(pg.id, &["Method"]).unwrap().len(), 0);
    assert_eq!(
        refs(&snap.members(pg.id, &[]).unwrap()),
        vec![
            format!("{TS}`postgres.ts`/PostgresDriver#close()."),
            format!("{TS}`postgres.ts`/PostgresDriver#open().")
        ]
    );
    // IMPLEMENTS_MEMBER: 4 edges (open/close of PostgresDriver and MemDriver), through the
    // descriptor fallback of `effective_label` (counted in meta)
    let mut member_edges = BTreeSet::new();
    for m in ["open", "close"] {
        let spec = snap
            .node(&format!("{TS}`driver.ts`/DatabaseDriver#{m}()."))
            .unwrap()
            .unwrap();
        let g = snap
            .neighbours(
                spec.id,
                kaban_codegraph::Depth::new(1, snap.limits()).unwrap(),
                &[EdgeType::ImplementsMember],
            )
            .unwrap();
        for e in g.edges {
            if e.to == spec.r#ref {
                member_edges.insert(e.from);
            }
        }
    }
    assert_eq!(member_edges.len(), 4, "{member_edges:?}");
    let meta = snap.meta().unwrap();
    assert!(meta["counts:label_from_descriptor"].parse::<u32>().unwrap() >= 10);
}

// ---------------------------------------------------------------------------------------------
// the writer half of the canary sweep (§8.3 step 7)

/// The JSON report the writer half can produce before CG4.6: `meta` and the warning classes.
fn writer_report(rows: &Rows, commit: &str, policy: &kaban_policy::Policy) -> String {
    let meta: std::collections::BTreeMap<String, String> = rows
        .meta_rows(&common::meta(commit), policy)
        .rows
        .into_iter()
        .collect();
    serde_json::json!({
        "meta": meta,
        "warnings": rows.warnings(),
        "counts": rows.counts().entries(),
    })
    .to_string()
}

fn contains(hay: &[u8], needle: &str) -> bool {
    hay.windows(needle.len()).any(|w| w == needle.as_bytes())
}

#[test]
fn canary_sweep_writer_half_go_drivers() {
    let policy = common::canary_policy();
    let a = common::fixture_bytes("go-drivers/index.scip");
    let b = common::fixture_bytes("go-drivers/index-notest.scip");
    // the canaries are in the producer's output (CANARY_DOC in a doc comment); CANARY_SNIPPET
    // is a string literal in a body and never reaches a SCIP index
    assert!(
        contains(&a, "CANARY_DOC"),
        "the fixture carries the doc canary"
    );
    let dir = common::Scratch::new();
    let p = publish_go(dir.path());
    let bytes = common::read(&p);
    for canary in ["CANARY_DOC", "CANARY_SNIPPET", "CANARY_SYM"] {
        assert!(!contains(&bytes, canary), "{canary} in the snapshot bytes");
    }
    let (ga, gb) = (common::gated(&a, &policy), common::gated(&b, &policy));
    let (aa, ab) = (admit_index(&ga), admit_index(&gb));
    let rows = Rows::derive(&aa, Some(&ab), &policy);
    let report = writer_report(&rows, common::GO_DRIVERS_COMMIT, &policy);
    for canary in ["CANARY_DOC", "CANARY_SNIPPET", "CANARY_SYM"] {
        assert!(!report.contains(canary), "{canary} in the JSON report");
    }
    assert!(
        report.contains("\"policy_dropped\""),
        "the doc drop is reported as a class"
    );
    let snap = SqliteSnapshot::open_ro(&p).unwrap();
    let iface = snap.node(&format!("{GO}DatabaseDriver#")).unwrap().unwrap();
    assert_eq!(iface.doc, None, "the doc field is dropped, the row stays");
    assert_eq!(snap.meta().unwrap()["counts:policy_dropped"], "1");
}

#[test]
fn writer_structural_hit_skips_row() {
    let policy = common::canary_policy();
    let bytes = common::synthetic_bytes("canary-sym");
    let dir = common::Scratch::new();
    let p = common::publish(dir.path(), &common::commit(0x51), &bytes, None, &policy);
    let file = common::read(&p);
    assert!(
        !contains(&file, "CANARY_SYM"),
        "the identifier is on no byte of the snapshot"
    );
    let g = common::gated(&bytes, &policy);
    let admitted = admit_index(&g);
    let rows = Rows::derive(&admitted, None, &policy);
    let report = writer_report(&rows, &common::commit(0x51), &policy);
    assert!(!report.contains("CANARY_SYM"), "nor of the report");
    assert!(report.contains("\"policy_skipped_row\""));
    let snap = SqliteSnapshot::open_ro(&p).unwrap();
    let meta = snap.meta().unwrap();
    // the struct row, its method row and their two definition sites: 4 under the class
    assert_eq!(meta["counts:policy_skipped_row"], "4");
    assert_eq!(
        meta["counts:symbols"], "3",
        "Plain#, its package placeholder and the file node"
    );
    assert_eq!(snap.by_label_name("Struct", "Plain").unwrap().len(), 1);
    assert!(
        snap.nodes_in_file("drivers/variant.go")
            .unwrap()
            .iter()
            .all(|n| !n.name.contains("CANARY"))
    );
}
