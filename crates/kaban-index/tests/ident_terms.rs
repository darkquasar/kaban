//! `symbols.ident_terms` and `symbols_fts` (cg-join-v1 §8, SD-7; stage1-plan CG3.3 slices
//! `ident_terms_table`, `ident_terms_is_gated_and_deterministic`, `symbols_fts_matches_ident_terms`).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use kaban_index::ident_terms;

#[test]
fn ident_terms_table() {
    let cases: &[((&str, &str, &str), &str)] = &[
        (("PostgresDriver", "", ""), "postgres driver"),
        (("Close", "open_db#Close().", ""), "close open db"),
        (("open_db", "open_db#Close().", ""), "open db close"),
        (("HTTPServer", "", ""), "http server"),
        (
            ("PostgresDriver", "PostgresDriver#", "Struct"),
            "postgres driver struct",
        ),
        (("driver.go", "", "File"), "driver go file"),
        (("dsn", "local 0", "Variable"), "dsn local 0 variable"),
        (("", "", ""), ""),
        (("__", "..", "//"), ""),
        (("parseURLToID", "", ""), "parse url to id"),
        (("snake_case_name", "", ""), "snake case name"),
        (("Ünïcode", "", ""), "ünïcode"),
        (("a", "a", "A"), "a"),
    ];
    for ((name, last, label), expected) in cases {
        assert_eq!(
            ident_terms::build(name, last, label),
            *expected,
            "build({name:?}, {last:?}, {label:?})"
        );
    }
    // total: arbitrary bytes never panic and the result is always lowercase
    let weird = "\u{0}\u{7f}🙂\u{301}A\u{301}b";
    let out = ident_terms::build(weird, weird, weird);
    assert_eq!(out, out.to_lowercase());
}

use kaban_codegraph::{CodeGraphReader, SqliteSnapshot};
use rusqlite::{Connection, OpenFlags};

/// A canary that exists in no single gated field and appears only once `name` and the key's
/// last descriptor are split and joined: the method `Canary(joined).` has `name` `Canary` and
/// the disambiguator `joined`, so the terms read `canary joined`, which the test policy's
/// `joined-canary` credential rule matches (G1-S1 step 3: the formed-string scan).
#[test]
fn ident_terms_is_gated_and_deterministic() {
    use scip::types::symbol_information::Kind;
    let policy = common::test_policy();
    let v = common::GO_DRIVERS_COMMIT;
    let sym = common::go_sym(v, "T#Canary(joined).");
    let ok = common::go_sym(v, "Plain#");
    let idx = common::index(vec![common::document(
        "drivers/c.go",
        vec![
            common::sym_info_kind(&sym, Kind::Method),
            common::sym_info_kind(&ok, Kind::Struct),
        ],
        vec![
            common::definition(&sym, &[1, 5, 11]),
            common::definition(&ok, &[3, 5, 10]),
        ],
    )]);
    // neither field alone hits the gate: the symbol row survives
    assert!(!policy.scan("Canary").rejected && !policy.scan(&sym).rejected);
    let bytes = common::bytes(&idx);
    let (d1, d2) = (common::Scratch::new(), common::Scratch::new());
    let p1 = common::publish(d1.path(), &common::commit(0x11), &bytes, None, &policy);
    let p2 = common::publish(d2.path(), &common::commit(0x11), &bytes, None, &policy);
    assert_eq!(
        common::read(&p1),
        common::read(&p2),
        "two runs, equal bytes"
    );
    let file_bytes = common::read(&p1);
    assert!(
        !file_bytes.windows(13).any(|w| w == b"canary joined"),
        "the canary is on no file"
    );
    let snap = SqliteSnapshot::open_ro(&p1).unwrap();
    let row = snap
        .node(&format!("scip:{sym}"))
        .unwrap()
        .expect("the row itself survives");
    assert_eq!(row.ident_terms, "", "the field is emptied");
    let plain = snap.node(&format!("scip:{ok}")).unwrap().unwrap();
    assert_eq!(plain.ident_terms, "plain struct");
    let meta = snap.meta().unwrap();
    assert_eq!(meta["counts:policy_dropped"], "1");
}

#[test]
fn symbols_fts_matches_ident_terms() {
    let policy = common::test_policy();
    let a = common::fixture_bytes("go-drivers/index.scip");
    let dir = common::Scratch::new();
    let p = common::publish(dir.path(), common::GO_DRIVERS_COMMIT, &a, None, &policy);
    let snap = SqliteSnapshot::open_ro(&p).unwrap();
    assert_eq!(snap.meta().unwrap()["symbols_fts_version"], "1");
    let pg = snap
        .by_label_name("Struct", "PostgresDriver")
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(pg.ident_terms, "postgres driver struct");
    drop(snap);
    // the FTS table answers over `ident_terms`, and its external content is consistent
    let conn = Connection::open_with_flags(&p, OpenFlags::SQLITE_OPEN_READ_WRITE).unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT name FROM symbols_fts WHERE symbols_fts MATCH 'postgres driver' ORDER BY name",
        )
        .unwrap();
    let names: Vec<String> = stmt
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(names, vec!["PostgresDriver".to_owned()]);
    drop(stmt);
    conn.execute_batch("INSERT INTO symbols_fts(symbols_fts, rank) VALUES('integrity-check', 1);")
        .expect("external content matches the index");
}
