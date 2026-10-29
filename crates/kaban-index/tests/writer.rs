//! Symbol and file rows, structural edges and deterministic publication (stage1-plan CG3.2
//! slices; stage1-spec G1-F1, F2, F5, G1-N5, G1-S1 inventory, §4.1 file protocol).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use kaban_codegraph::EdgeType;
use kaban_index::RelationshipFlags;
use kaban_index::derive::derive_edge_type;
use protobuf::Enum;
use scip::types::symbol_information::Kind;

/// Every `Kind` of the pinned schema, from the enum descriptor (the schema, not a hand list).
fn every_kind() -> Vec<Kind> {
    use protobuf::EnumFull;
    Kind::enum_descriptor()
        .values()
        .map(|v| Kind::from_i32(v.value()).unwrap())
        .collect()
}

fn is_member(k: Kind) -> bool {
    is_method_like(k) || k == Kind::Field
}

fn is_method_like(k: Kind) -> bool {
    matches!(
        k,
        Kind::Method | Kind::MethodSpecification | Kind::TraitMethod | Kind::StaticMethod
    )
}

#[test]
fn derive_edge_type_table() {
    let kinds = every_kind();
    assert!(kinds.len() > 80, "the pinned schema has many kinds");
    let mut implements = 0;
    let mut implements_member = 0;
    let mut type_of = 0;
    for &src in &kinds {
        for &dst in &kinds {
            for bits in 0u8..16 {
                let flags = RelationshipFlags::from_bits(bits);
                let got = derive_edge_type(src, dst, flags);
                let expected = if flags.is_implementation() {
                    if is_method_like(src) && is_method_like(dst) {
                        Some(EdgeType::ImplementsMember)
                    } else if is_member(src) && is_member(dst) {
                        None
                    } else {
                        Some(EdgeType::Implements)
                    }
                } else if flags.is_type_definition() {
                    Some(EdgeType::TypeOf)
                } else {
                    None
                };
                assert_eq!(got, expected, "({src:?}, {dst:?}, {flags:?})");
                match got {
                    Some(EdgeType::Implements) => implements += 1,
                    Some(EdgeType::ImplementsMember) => implements_member += 1,
                    Some(EdgeType::TypeOf) => type_of += 1,
                    _ => {}
                }
            }
        }
    }
    // the table never yields the edge types that are not relationship-derived
    assert!(implements > 0 && implements_member > 0 && type_of > 0);
    // no EXTENDS, no OVERRIDES (OD-4): the only outputs are the three above
    for &src in &kinds {
        for &dst in &kinds {
            for bits in 0u8..16 {
                let got = derive_edge_type(src, dst, RelationshipFlags::from_bits(bits));
                assert!(
                    matches!(
                        got,
                        None | Some(
                            EdgeType::Implements | EdgeType::ImplementsMember | EdgeType::TypeOf
                        )
                    ),
                    "unexpected {got:?}"
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// publication over the committed go-drivers fixture (G1-N5, §4.1 file protocol)

use std::collections::BTreeSet;
use std::os::unix::fs::PermissionsExt;

use kaban_codegraph::{CodeGraphReader, SqliteSnapshot};
use kaban_index::admit_index;
use kaban_index::derive::{self, ProjectRoot, check_project_root};
use kaban_index::writer::{self, PublishError, Rows};
use rusqlite::{Connection, OpenFlags};

fn go_bytes() -> (Vec<u8>, Vec<u8>) {
    (
        common::fixture_bytes("go-drivers/index.scip"),
        common::fixture_bytes("go-drivers/index-notest.scip"),
    )
}

fn open_raw(path: &std::path::Path) -> Connection {
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap()
}

fn sql_strings(conn: &Connection, sql: &str) -> Vec<String> {
    let mut stmt = conn.prepare(sql).unwrap();
    let rows = stmt.query_map([], |r| r.get::<_, String>(0)).unwrap();
    rows.map(Result::unwrap).collect()
}

fn count(conn: &Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |r| r.get(0)).unwrap()
}

#[test]
fn determinism_index_twice_go_drivers() {
    let policy = common::test_policy();
    let (a, b) = go_bytes();
    let first = common::Scratch::new();
    let second = common::Scratch::new();
    let p1 = common::publish(
        first.path(),
        common::GO_DRIVERS_COMMIT,
        &a,
        Some(&b),
        &policy,
    );
    let p2 = common::publish(
        second.path(),
        common::GO_DRIVERS_COMMIT,
        &a,
        Some(&b),
        &policy,
    );
    let bytes1 = common::read(&p1);
    let bytes2 = common::read(&p2);
    assert_eq!(
        bytes1, bytes2,
        "two publications from the same inputs differ"
    );
    assert_eq!(
        p1.file_name().unwrap().to_str().unwrap(),
        format!("{}.cg.db", common::GO_DRIVERS_COMMIT)
    );
    // mode 0600, no temp file left behind
    let mode = std::fs::metadata(&p1).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    let leftovers: Vec<_> = std::fs::read_dir(first.path()).unwrap().collect();
    assert_eq!(leftovers.len(), 1, "only the final file remains");
    // the meta dump has no timestamp-like key and the §4.1 required keys
    let snap = SqliteSnapshot::open_ro(&p1).expect("reader admits the published file");
    let meta = snap.meta().unwrap();
    for key in meta.keys() {
        let k = key.to_ascii_lowercase();
        assert!(
            !(k.contains("time") || k.contains("date") || k.contains("stamp")),
            "timestamp-like meta key {key}"
        );
    }
    for required in [
        "schema_version",
        "commit",
        "repo_id",
        "languages",
        "indexer:go",
        "kaban_index_version",
        "scip_version",
        "policy_version",
        "isolation",
        "isolation_detail",
        "resources",
        "scip_range_form",
        "symbols_fts_version",
        "union_reason",
        "counts:files",
        "counts:symbols",
        "counts:edges",
    ] {
        assert!(meta.contains_key(required), "meta lacks {required}");
    }
    assert_eq!(meta["commit"], common::GO_DRIVERS_COMMIT);
    assert_eq!(meta["languages"], "go");
    assert!(meta["indexer:go"].starts_with("scip-go 0.2.7 index"));
    assert!(!meta.contains_key("container_image_digest"));
    // fixed page size (G1-N5)
    let raw = open_raw(&p1);
    assert_eq!(count(&raw, "PRAGMA page_size"), 4096);
    assert_eq!(count(&raw, "SELECT count(*) FROM snippets"), 0);
    assert_eq!(count(&raw, "SELECT count(*) FROM edge_types"), 7);
}

#[test]
fn publish_refuses_existing_name() {
    let policy = common::test_policy();
    let (a, _) = go_bytes();
    let dir = common::Scratch::new();
    let first = common::publish(dir.path(), common::GO_DRIVERS_COMMIT, &a, None, &policy);
    let before = common::read(&first);
    let gated = common::gated(&a, &policy);
    let admitted = admit_index(&gated);
    let rows = Rows::derive(&admitted, None, &policy);
    let err = writer::publish(
        dir.path(),
        &rows,
        &common::meta(common::GO_DRIVERS_COMMIT),
        &policy,
    )
    .expect_err("a second publication under the same name");
    assert!(matches!(err, PublishError::Exists), "{err:?}");
    assert_eq!(
        common::read(&first),
        before,
        "the published file is untouched"
    );
    let names: BTreeSet<String> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        names,
        BTreeSet::from([format!("{}.cg.db", common::GO_DRIVERS_COMMIT)]),
        "the temp file was removed"
    );
    // a symlink at the temp name is refused too (O_EXCL|O_NOFOLLOW), nothing written through it
    let dir2 = common::Scratch::new();
    let target = dir2.path().join("elsewhere");
    std::fs::write(&target, b"keep").unwrap();
    let tmp = dir2.path().join(format!(
        "{}.cg.db.tmp-{}",
        common::GO_DRIVERS_COMMIT,
        std::process::id()
    ));
    std::os::unix::fs::symlink(&target, &tmp).unwrap();
    let err = writer::publish(
        dir2.path(),
        &rows,
        &common::meta(common::GO_DRIVERS_COMMIT),
        &policy,
    )
    .expect_err("temp name occupied by a symlink");
    assert!(matches!(err, PublishError::TempExists), "{err:?}");
    assert_eq!(std::fs::read(&target).unwrap(), b"keep");
}

#[test]
fn writer_duplicates_counted() {
    let policy = common::test_policy();
    let dir = common::Scratch::new();
    let bytes = common::synthetic_bytes("duplicate");
    let path = common::publish(dir.path(), &common::commit(1), &bytes, None, &policy);
    let raw = open_raw(&path);
    let labels = sql_strings(&raw, "SELECT label FROM symbols WHERE name = 'Dup'");
    assert_eq!(
        labels,
        vec!["Struct".to_owned()],
        "the first in document order stays"
    );
    let snap = SqliteSnapshot::open_ro(&path).unwrap();
    assert_eq!(snap.meta().unwrap()["counts:duplicates_skipped"], "1");
}

#[test]
fn writer_grammar_failure_skips_row() {
    let policy = common::test_policy();
    let dir = common::Scratch::new();
    let bytes = common::synthetic_bytes("malformed");
    let path = common::publish(dir.path(), &common::commit(2), &bytes, None, &policy);
    let bytes = common::read(&path);
    assert!(
        !bytes
            .windows(b"unterminated".len())
            .any(|w| w == b"unterminated"),
        "nothing of the malformed value is in the file"
    );
    let raw = open_raw(&path);
    assert_eq!(
        count(&raw, "SELECT count(*) FROM symbols WHERE name = 'Bad'"),
        0
    );
    assert_eq!(
        count(
            &raw,
            "SELECT count(*) FROM occurrences o JOIN symbols s ON s.id = o.symbol_id WHERE s.name = 'Bad'"
        ),
        0,
        "the skipped row's definition and reference sites are absent"
    );
    assert_eq!(
        count(&raw, "SELECT count(*) FROM occurrences"),
        1,
        "Good's definition site stays"
    );
    let edges = count(
        &raw,
        "SELECT count(*) FROM edges WHERE type IN ('IMPLEMENTS', 'IMPLEMENTS_MEMBER')",
    );
    assert_eq!(
        edges, 0,
        "the relationship of a skipped row creates no edge"
    );
    let snap = SqliteSnapshot::open_ro(&path).unwrap();
    let meta = snap.meta().unwrap();
    assert_eq!(meta["counts:grammar_skipped_rows"], "1");
    for v in meta.values() {
        assert!(
            !v.contains("unterminated"),
            "the value is not echoed in meta"
        );
    }
}

#[test]
fn writer_signature_doc_bounds() {
    let policy = common::test_policy();
    let dir = common::Scratch::new();
    let bytes = common::synthetic_bytes("big-doc");
    let path = common::publish(dir.path(), &common::commit(3), &bytes, None, &policy);
    let raw = open_raw(&path);
    let (sig, doc): (String, String) = raw
        .query_row(
            "SELECT signature, doc FROM symbols WHERE name = 'Big'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(sig.len(), derive::MAX_SIGNATURE_BYTES);
    assert!(sig.ends_with(derive::TRUNCATION_MARKER));
    assert_eq!(doc.len(), derive::MAX_DOC_BYTES);
    assert!(doc.ends_with(derive::TRUNCATION_MARKER));
}

/// Writes every synthetic index of `common::synthetic_index` into `fixtures/codegraph/synthetic/`
/// (committed; stage1-plan CG3.2). Run on purpose only: `-- --ignored regen_synthetic_fixtures`.
#[test]
#[ignore = "regenerates committed fixture bytes"]
fn regen_synthetic_fixtures() {
    let dir = common::fixtures_dir().join("synthetic");
    for stem in common::SYNTHETIC_STEMS {
        let bytes = common::bytes(&common::synthetic_index(stem));
        std::fs::write(dir.join(format!("{stem}.scip")), bytes).unwrap();
    }
}

/// The G1-S1 write inventory: every TEXT column of the published schema is either content the
/// gate covers or a closed control vocabulary.
#[test]
fn writer_inventory_complete() {
    const INVENTORY: &[&str] = &[
        "files.path",
        "symbols.ref",
        "symbols.scip",
        "symbols.key",
        "symbols.name",
        "symbols.signature",
        "symbols.doc",
        "snippets.text",
        "meta.key",
        "meta.value",
        "symbols.ident_terms", // formed from gated name/key/label and re-scanned (CG3.3)
    ];
    const CONTROL: &[&str] = &[
        "files.language", // closed set: kaban-index's detection
        "symbols.kind",   // symbol | local | file
        "symbols.label",  // scip.proto Kind names, File
        "edges.type",     // the seven edge types
        "edge_types.type",
        "edge_types.source",     // fixed texts of §4.3
        "edge_types.confidence", // measured | rule | heuristic
    ];
    let policy = common::test_policy();
    let (a, _) = go_bytes();
    let dir = common::Scratch::new();
    let path = common::publish(dir.path(), common::GO_DRIVERS_COMMIT, &a, None, &policy);
    let raw = open_raw(&path);
    let tables = sql_strings(
        &raw,
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' \
         AND name NOT LIKE 'symbols_fts%' ORDER BY name",
    );
    assert!(tables.len() >= 6, "{tables:?}");
    let mut text_columns = BTreeSet::new();
    for t in &tables {
        let mut stmt = raw.prepare(&format!("PRAGMA table_info({t})")).unwrap();
        let cols = stmt
            .query_map([], |r| Ok((r.get::<_, String>(1)?, r.get::<_, String>(2)?)))
            .unwrap();
        for c in cols {
            let (name, ty) = c.unwrap();
            if ty.to_ascii_uppercase().contains("TEXT") {
                text_columns.insert(format!("{t}.{name}"));
            }
        }
    }
    let covered: BTreeSet<String> = INVENTORY
        .iter()
        .chain(CONTROL.iter())
        .map(|s| (*s).to_owned())
        .collect();
    let uncovered: Vec<_> = text_columns.difference(&covered).collect();
    assert!(
        uncovered.is_empty(),
        "TEXT columns outside the inventory: {uncovered:?}"
    );
    let stale: Vec<_> = covered.difference(&text_columns).collect();
    assert!(
        stale.is_empty(),
        "inventory names columns the schema lacks: {stale:?}"
    );
}

// ---------------------------------------------------------------------------------------------
// negative paths

/// `Rows` cannot be mutated outside the crate any more (RU1-2), so the dangling check is
/// exercised on its own over the derived rows plus one invented edge; `publish` runs the same
/// check first (see `writer.rs`), and the derivation debug-asserts it.
#[test]
fn dangling_edge_endpoint_is_detected() {
    let policy = common::test_policy();
    let (a, _) = go_bytes();
    let gated = common::gated(&a, &policy);
    let admitted = admit_index(&gated);
    let rows = Rows::derive(&admitted, None, &policy);
    assert!(derive::check_no_dangling(rows.symbols(), rows.edges()).is_ok());
    let beyond = kaban_codegraph::NodeId(i64::try_from(rows.symbols().len()).unwrap() + 1);
    let mut edges = rows.edges().to_vec();
    edges.push(kaban_codegraph::write::EdgeRow {
        src: beyond,
        dst: kaban_codegraph::NodeId(1),
        r#type: kaban_codegraph::EdgeType::Contains,
        weight: 1,
    });
    let check = derive::check_no_dangling(rows.symbols(), &edges);
    assert!(matches!(check, Err(derive::Dangling { id, .. }) if id == beyond));
}

#[test]
fn project_root_outside_repository_is_project_root_mismatch() {
    let repo = std::path::Path::new("/tmp/kaban-fixture-go-drivers/go-drivers");
    assert!(
        check_project_root(
            &ProjectRoot::parse("file:///tmp/kaban-fixture-go-drivers/go-drivers"),
            repo
        )
        .is_ok()
    );
    assert!(
        check_project_root(
            &ProjectRoot::parse("file:///tmp/kaban-fixture-go-drivers/go-drivers/"),
            repo
        )
        .is_ok()
    );
    assert!(
        check_project_root(
            &ProjectRoot::parse("/tmp/kaban-fixture-go-drivers/go-drivers"),
            repo
        )
        .is_ok()
    );
    let err = check_project_root(&ProjectRoot::parse("file:///work"), repo).unwrap_err();
    assert_eq!(err.to_string(), "project_root_mismatch");
    assert!(
        check_project_root(&ProjectRoot::parse("file:///tmp/other/go-drivers-2"), repo).is_err()
    );
    assert!(check_project_root(&ProjectRoot::parse(""), repo).is_err());
}

/// The nested Cargo roots the run did not index (backlog 17, NEST-1) land in `meta` as the
/// counter `coverage:nested_workspaces_not_indexed` and the JSON list
/// `coverage:nested_workspace_paths`, next to the producer's coverage rows; a run that named
/// none publishes neither row.
#[test]
fn nested_workspaces_land_as_coverage_meta_rows_and_are_absent_without_them() {
    let policy = common::test_policy();
    let (a, _) = go_bytes();
    let gated = common::gated(&a, &policy);
    let admitted = admit_index(&gated);
    let meta = common::meta(&common::commit(0x71));

    let plain = Rows::derive(&admitted, None, &policy);
    let rows = plain.meta_rows(&meta, &policy).rows;
    assert!(
        !rows.iter().any(|(k, _)| k.starts_with("coverage:nested")),
        "no nested rows unless set: {rows:?}"
    );

    let mut named = Rows::derive(&admitted, None, &policy);
    named.set_nested_workspaces(vec![
        "fixtures/rust/minimal".to_owned(),
        "tools/producer".to_owned(),
    ]);
    let dir = common::Scratch::new();
    let p = writer::publish(dir.path(), &named, &meta, &policy).unwrap();
    let snap = SqliteSnapshot::open_ro(&p).unwrap();
    let meta_rows = snap.meta().unwrap();
    assert_eq!(meta_rows["coverage:nested_workspaces_not_indexed"], "2");
    assert_eq!(
        meta_rows["coverage:nested_workspace_paths"],
        r#"["fixtures/rust/minimal","tools/producer"]"#
    );
}
