//! Snapshot admission slices (stage1-plan CG2.2; stage1-spec §4.1 reader rules, §7; cg-join-v1
//! §2, §11 `validate_for_attach_refuses`): `open_ro`, `validate_for_attach`, the write helpers'
//! order checks and byte-identical builds, through the public API and a builder over `write::*`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

#[path = "common/builder.rs"]
mod builder;

use std::os::unix::fs::PermissionsExt;

use builder::{Snapshot, SymbolSpec, commit, final_path, lines, scratch};
use kaban_codegraph::schema::{EdgeType, NodeId};
use kaban_codegraph::sqlite::{self, SqliteSnapshot, validate_for_attach};
use kaban_codegraph::{SCHEMA_VERSION, SnapshotError, WriteError};

#[test]
fn directory_identity_stays_bound_to_the_opened_object_after_replacement() {
    use kaban_codegraph::identity::{fs_identity, resolve_common_dir, walk};
    use std::os::unix::fs::MetadataExt;

    let scratch = scratch();
    let repo = scratch.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    std::fs::set_permissions(&repo, std::fs::Permissions::from_mode(0o700)).unwrap();
    let (handle, canonical) = resolve_common_dir(&repo, ".").unwrap();
    let before = fs_identity(&handle).unwrap();
    let metadata = std::fs::metadata(&repo).unwrap();
    #[cfg(target_os = "macos")]
    let device = metadata.dev() & 0xffff_ffff; // Darwin dev_t's 32-bit identifier.
    #[cfg(not(target_os = "macos"))]
    let device = metadata.dev();
    assert_eq!((before.dev, before.ino), (device, metadata.ino()));
    assert_eq!(
        handle.dir().owner_and_mode().unwrap(),
        (metadata.uid(), 0o700)
    );
    std::fs::rename(&repo, scratch.path().join("original")).unwrap();
    std::fs::create_dir(&repo).unwrap();
    assert_eq!(fs_identity(&handle).unwrap(), before);
    assert_ne!(
        walk(&canonical).unwrap().dev_ino().unwrap(),
        (before.dev, before.ino)
    );
}

#[test]
fn identity_opens_refuse_symlinks_and_nonprivate_modes() {
    use kaban_codegraph::identity::{IdentityError, PrivateFileError, open_private_file, walk};

    let scratch = scratch();
    let root = std::fs::canonicalize(scratch.path()).unwrap();
    let real = root.join("real");
    std::fs::create_dir(&real).unwrap();
    let linked = root.join("linked");
    std::os::unix::fs::symlink(&real, &linked).unwrap();
    assert_eq!(walk(&linked).unwrap_err(), IdentityError::SymlinkComponent);
    assert!(walk(&real).is_ok());
    let key = real.join("key");
    std::fs::write(&key, b"invented key").unwrap();
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert!(open_private_file(&key).is_ok());
    let key_link = root.join("key-link");
    std::os::unix::fs::symlink(&key, &key_link).unwrap();
    assert_eq!(
        open_private_file(&key_link).unwrap_err(),
        PrivateFileError::Symlink
    );
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(open_private_file(&key).unwrap_err(), PrivateFileError::Mode);
}

/// Two files, three symbols (one external), one edge: enough to exercise every table.
fn small_graph(commit: &str) -> Snapshot {
    let mut s = Snapshot::new(commit);
    let a = s.file("a.go", false);
    let b = s.file("b_test.go", true);
    s.symbol(SymbolSpec::file("file:a.go", "a.go", a));
    s.symbol(SymbolSpec::file("file:b_test.go", "b_test.go", b));
    let iface = s.symbol(
        SymbolSpec::symbol(
            "scip:s m p v x/Iface#",
            "Interface",
            "Iface",
            a,
            lines(1, 3),
        )
        .key("s m p . x/Iface#"),
    );
    let imp = s.symbol(
        SymbolSpec::symbol("scip:s m p v x/Impl#", "Struct", "Impl", a, lines(5, 9))
            .key("s m p . x/Impl#"),
    );
    s.symbol(SymbolSpec::symbol("scip:s m p v y/Ext#", "Struct", "Ext", a, lines(0, 0)).external());
    s.edge(imp, EdgeType::Implements, iface);
    s.occurrence(iface, b, lines(2, 2), 8, None);
    s
}

#[test]
fn open_of_a_builder_snapshot_succeeds_and_quick_check_runs_once() {
    let dir = scratch();
    let commit = commit(1);
    let path = small_graph(&commit).write_final(dir.path(), &commit);
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "the builder writes 0600");

    let before = sqlite::testing::quick_checks_run();
    let snap = SqliteSnapshot::open_ro(&path).expect("admitted");
    assert_eq!(snap.path(), path);
    assert_eq!(snap.schema_version(), SCHEMA_VERSION);
    assert_eq!(
        sqlite::testing::quick_checks_run(),
        before + 1,
        "first open checks"
    );

    let again = SqliteSnapshot::open_ro(&path).expect("admitted again");
    assert_eq!(again.schema_version(), SCHEMA_VERSION);
    assert_eq!(
        sqlite::testing::quick_checks_run(),
        before + 1,
        "quick_check_once_per_process: a second open of the same (dev, ino, size) runs none"
    );
}

#[test]
fn files_modes_and_symlink_refusal() {
    let dir = scratch();
    let commit = commit(2);
    let real = small_graph(&commit).write_final(dir.path(), &commit);

    // a pre-placed symlink at a final name: refused on open, the target never opened
    let link_commit = commit.replace("02", "03");
    let link = final_path(dir.path(), &link_commit);
    std::os::unix::fs::symlink(&real, &link).unwrap();
    assert_eq!(
        SqliteSnapshot::open_ro(&link).unwrap_err(),
        SnapshotError::SymlinkRefused
    );
    // a dangling symlink too (symlink_metadata, never a follow)
    let dangling = final_path(dir.path(), &commit.replace("02", "04"));
    std::os::unix::fs::symlink(dir.path().join("nowhere.cg.db"), &dangling).unwrap();
    assert_eq!(
        SqliteSnapshot::open_ro(&dangling).unwrap_err(),
        SnapshotError::SymlinkRefused
    );
    // a directory at a final name
    let as_dir = final_path(dir.path(), &commit.replace("02", "05"));
    std::fs::create_dir(&as_dir).unwrap();
    assert_eq!(
        SqliteSnapshot::open_ro(&as_dir).unwrap_err(),
        SnapshotError::NotRegularFile
    );
    // a 0640 file: refused; back at 0600: admitted
    std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o640)).unwrap();
    assert_eq!(
        SqliteSnapshot::open_ro(&real).unwrap_err(),
        SnapshotError::ModeRefused
    );
    std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert!(SqliteSnapshot::open_ro(&real).is_ok());
    // a missing file is an io class, not a panic
    assert_eq!(
        SqliteSnapshot::open_ro(&final_path(dir.path(), &commit.replace("02", "06"))).unwrap_err(),
        SnapshotError::Io { class: "not_found" }
    );
}

#[test]
fn a_newer_schema_major_is_snapshot_too_new_with_the_fixed_text() {
    let dir = scratch();
    let commit = commit(7);
    let mut s = small_graph(&commit);
    let newer = SCHEMA_VERSION + 1;
    s.set_meta("schema_version", &format!("{newer}.4"));
    let path = s.write_final(dir.path(), &commit);
    let err = SqliteSnapshot::open_ro(&path).unwrap_err();
    assert_eq!(
        err,
        SnapshotError::SnapshotTooNew {
            major: newer,
            supported: SCHEMA_VERSION
        }
    );
    assert_eq!(
        err.to_string(),
        format!("snapshot too new: schema major {newer} is above the supported {SCHEMA_VERSION}")
    );
    assert!(!err.to_string().contains(&commit), "no path in the text");

    // an older minor within the major is readable
    let mut s = small_graph(&commit);
    s.set_meta("schema_version", &format!("{SCHEMA_VERSION}.0"));
    let older_commit = commit.replace("07", "08");
    let path = s.write_final(dir.path(), &older_commit);
    assert_eq!(
        SqliteSnapshot::open_ro(&path).unwrap().schema_version(),
        SCHEMA_VERSION
    );
}

#[test]
fn a_damaged_file_is_corrupt_and_stays_corrupt_without_a_second_check() {
    let dir = scratch();
    let commit = commit(9);
    let path = small_graph(&commit).write_final(dir.path(), &commit);
    let bytes = std::fs::read(&path).unwrap();
    assert!(bytes.len() > 8192, "more than two pages");
    // zero the last page: the header and `meta` survive, `quick_check` reports the damage
    let truncated = final_path(dir.path(), &commit.replace("09", "0a"));
    let mut damaged = bytes.clone();
    let last = damaged.len() - 4096;
    damaged[last..].fill(0);
    std::fs::write(&truncated, &damaged).unwrap();
    std::fs::set_permissions(&truncated, std::fs::Permissions::from_mode(0o600)).unwrap();
    let before = sqlite::testing::quick_checks_run();
    assert_eq!(
        SqliteSnapshot::open_ro(&truncated).unwrap_err(),
        SnapshotError::Corrupt
    );
    let after_first = sqlite::testing::quick_checks_run();
    assert_eq!(after_first, before + 1, "the verdict came from quick_check");
    assert_eq!(
        SqliteSnapshot::open_ro(&truncated).unwrap_err(),
        SnapshotError::Corrupt
    );
    assert_eq!(
        sqlite::testing::quick_checks_run(),
        after_first,
        "the remembered verdict is reused"
    );
    // not a database at all
    let garbage = final_path(dir.path(), &commit.replace("09", "0b"));
    std::fs::write(&garbage, b"not a database").unwrap();
    std::fs::set_permissions(&garbage, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(
        SqliteSnapshot::open_ro(&garbage).unwrap_err(),
        SnapshotError::Corrupt
    );
}

/// cg-join-v1 §11: symlink, mode, temp name, too-new, corrupt, and a `?`/`#` path whose URI is
/// byte-exact and opens through `ATTACH` (no `query_only`: the engine attaches on a dedicated
/// read connection, contract §2 revision 2).
#[test]
fn validate_for_attach_refuses() {
    let dir = scratch();
    let commit = commit(0x10);
    let real = small_graph(&commit).write_final(dir.path(), &commit);

    let link = final_path(dir.path(), &commit.replace("10", "11"));
    std::os::unix::fs::symlink(&real, &link).unwrap();
    assert_eq!(
        validate_for_attach(&link).unwrap_err(),
        SnapshotError::SymlinkRefused
    );

    std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o640)).unwrap();
    assert_eq!(
        validate_for_attach(&real).unwrap_err(),
        SnapshotError::ModeRefused
    );
    std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o600)).unwrap();

    let temp = dir.path().join(format!("{commit}.cg.db.tmp-1"));
    std::fs::copy(&real, &temp).unwrap();
    std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(
        validate_for_attach(&temp).unwrap_err(),
        SnapshotError::TempNameRefused
    );
    let short = dir.path().join("abc.cg.db");
    std::fs::copy(&real, &short).unwrap();
    std::fs::set_permissions(&short, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(
        validate_for_attach(&short).unwrap_err(),
        SnapshotError::TempNameRefused
    );
    let upper = final_path(dir.path(), &commit.replace("10", "AB"));
    std::fs::copy(&real, &upper).unwrap();
    std::fs::set_permissions(&upper, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(
        validate_for_attach(&upper).unwrap_err(),
        SnapshotError::TempNameRefused
    );

    let mut too_new = small_graph(&commit);
    too_new.set_meta("schema_version", &(SCHEMA_VERSION + 7).to_string());
    let too_new = too_new.write_final(dir.path(), &commit.replace("10", "12"));
    assert_eq!(
        validate_for_attach(&too_new).unwrap_err(),
        SnapshotError::SnapshotTooNew {
            major: SCHEMA_VERSION + 7,
            supported: SCHEMA_VERSION
        }
    );

    let corrupt = final_path(dir.path(), &commit.replace("10", "13"));
    std::fs::write(&corrupt, b"SQLite format 3\0 but nothing else").unwrap();
    std::fs::set_permissions(&corrupt, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(
        validate_for_attach(&corrupt).unwrap_err(),
        SnapshotError::Corrupt
    );

    // the encoded URI, byte-exact, for a directory named with `?`, `#`, `%`, a space and UTF-8
    let odd_dir = dir.path().join("q?f#p%25 s\u{e9}");
    std::fs::create_dir(&odd_dir).unwrap();
    let odd_commit = commit.replace("10", "14");
    let odd = small_graph(&odd_commit).write_final(&odd_dir, &odd_commit);
    let uri = validate_for_attach(&odd).expect("admitted");
    let expected = format!(
        "file:{}/q%3Ff%23p%2525%20s%C3%A9/{odd_commit}.cg.db?mode=ro&immutable=1",
        dir.path().to_str().unwrap()
    );
    assert_eq!(uri.as_sql_param(), expected);
    assert_eq!(uri.path(), odd);
    assert_eq!(uri.schema_version(), SCHEMA_VERSION);

    // SQLite opens it through ATTACH on a URI-enabled connection, read-only
    let conn = rusqlite::Connection::open_with_flags(
        "file::memory:",
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
            | rusqlite::OpenFlags::SQLITE_OPEN_CREATE
            | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )
    .unwrap();
    conn.execute("ATTACH DATABASE ?1 AS cg", [uri.as_sql_param()])
        .unwrap();
    let count: i64 = conn
        .query_row("SELECT count(*) FROM cg.meta", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 6);
    let commit_read: String = conn
        .query_row("SELECT value FROM cg.meta WHERE key = 'commit'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(commit_read, odd_commit);
    assert!(
        conn.execute("INSERT INTO cg.meta(key, value) VALUES ('x', 'y')", [])
            .is_err(),
        "mode=ro&immutable=1: the attached snapshot is not writable"
    );
}

#[test]
fn symbols_fts_external_content_rebuilds() {
    let dir = scratch();
    let empty_commit = commit(0x20);
    let empty = Snapshot::new(&empty_commit).write_final(dir.path(), &empty_commit);
    let conn =
        rusqlite::Connection::open_with_flags(&empty, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let n: i64 = conn
        .query_row("SELECT count(*) FROM symbols_fts", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 0);
    let has_ident_terms: i64 = conn
        .query_row(
            "SELECT count(*) FROM pragma_table_info('symbols') WHERE name = 'ident_terms'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(has_ident_terms, 1, "the ident_terms column is real");
    let page: i64 = conn
        .query_row("PRAGMA page_size", [], |r| r.get(0))
        .unwrap();
    assert_eq!(page, 4096);

    let full_commit = commit(0x21);
    let mut s = small_graph(&full_commit);
    s.symbols[2].ident_terms = "iface mem map fs".into();
    let full = s.write_final(dir.path(), &full_commit);
    let conn =
        rusqlite::Connection::open_with_flags(&full, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let hit: String = conn
        .query_row(
            "SELECT s.ref FROM symbols_fts f JOIN symbols s ON s.id = f.rowid WHERE symbols_fts MATCH 'ident_terms:map'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(hit, "scip:s m p v x/Iface#");
    let by_name: i64 = conn
        .query_row(
            "SELECT count(*) FROM symbols_fts WHERE symbols_fts MATCH 'name:Impl'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(by_name, 1);
}

#[test]
fn write_helpers_refuse_unsorted_input_and_builds_are_byte_identical() {
    use sha2::{Digest, Sha256};
    let dir = scratch();
    let commit = commit(0x30);

    // unsorted files: refused before any row is written
    let mut unsorted = small_graph(&commit);
    unsorted.files.swap(0, 1);
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    assert!(matches!(
        unsorted.write_with(&conn),
        Err(WriteError::Unsorted)
    ));
    let n: i64 = conn
        .query_row("SELECT count(*) FROM files", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 0, "nothing written");

    // ids that are not positions, unsorted symbols, edges, occurrences, meta
    let mut bad_ids = small_graph(&commit);
    bad_ids.files[1].id = kaban_codegraph::FileId(7);
    assert!(matches!(
        bad_ids.write_with(&rusqlite::Connection::open_in_memory().unwrap()),
        Err(WriteError::Ids)
    ));
    let mut dup_symbol = small_graph(&commit);
    dup_symbol.symbols.swap(2, 3);
    assert!(matches!(
        dup_symbol.write_with(&rusqlite::Connection::open_in_memory().unwrap()),
        Err(WriteError::Unsorted)
    ));
    let mut edges = small_graph(&commit);
    edges.edge(NodeId(1), EdgeType::Contains, NodeId(2));
    assert!(matches!(
        edges.write_with(&rusqlite::Connection::open_in_memory().unwrap()),
        Err(WriteError::Unsorted)
    ));
    let mut occ = small_graph(&commit);
    occ.occurrence(NodeId(1), kaban_codegraph::FileId(1), lines(0, 0), 1, None);
    assert!(matches!(
        occ.write_with(&rusqlite::Connection::open_in_memory().unwrap()),
        Err(WriteError::Unsorted)
    ));
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    kaban_codegraph::write::begin(&conn).unwrap();
    assert!(matches!(
        kaban_codegraph::write::insert_meta(&conn, &[("b", "1"), ("a", "2")]),
        Err(WriteError::Unsorted)
    ));

    // two builds from the same rows are byte-identical
    let first = small_graph(&commit).write_final(dir.path(), &commit);
    let second_dir = scratch();
    let second = small_graph(&commit).write_final(second_dir.path(), &commit);
    let digest = |p: &std::path::Path| format!("{:x}", Sha256::digest(std::fs::read(p).unwrap()));
    assert_eq!(digest(&first), digest(&second));
    assert!(std::fs::metadata(&first).unwrap().len() > 0);

    // a write helper on a read-only connection fails
    let ro =
        rusqlite::Connection::open_with_flags(&first, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    assert!(matches!(
        kaban_codegraph::write::insert_meta(&ro, &[("z", "1")]),
        Err(WriteError::Sql(_))
    ));
    assert!(matches!(
        kaban_codegraph::write::rebuild_fts(&ro),
        Err(WriteError::Sql(_))
    ));
}

/// `GM2a` RG2-2: the §4.1 edge order is by the stored `type` text, so `TESTED_BY` sorts before
/// `TYPE_OF` (byte order), whatever the enum's declaration order; the same rule holds for
/// `edge_types`.
#[test]
fn edge_order_is_by_stored_type_text() {
    use kaban_codegraph::schema::Confidence;
    use kaban_codegraph::write::{self, EdgeRow, EdgeTypeRow};
    let commit = commit(0x31);
    let mut s = small_graph(&commit);
    s.edges.clear();
    let one = NodeId(3);
    let contract_order = [
        EdgeRow {
            src: one,
            dst: NodeId(4),
            r#type: EdgeType::TestedBy,
            weight: 1,
        },
        EdgeRow {
            src: one,
            dst: NodeId(4),
            r#type: EdgeType::TypeOf,
            weight: 1,
        },
    ];
    s.edges.extend(contract_order);
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    s.write_with(&conn)
        .expect("TESTED_BY before TYPE_OF from one source is the contract order");
    let stored: Vec<String> = conn
        .prepare("SELECT type FROM edges ORDER BY src, type, dst")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(stored, ["TESTED_BY", "TYPE_OF"]);

    let mut inverse = small_graph(&commit);
    inverse.edges.clear();
    inverse.edges.push(contract_order[1]);
    inverse.edges.push(contract_order[0]);
    assert!(
        matches!(
            inverse.write_with(&rusqlite::Connection::open_in_memory().unwrap()),
            Err(WriteError::Unsorted)
        ),
        "TYPE_OF before TESTED_BY is out of order"
    );

    let row = |t: EdgeType| EdgeTypeRow {
        r#type: t,
        source: "x".into(),
        confidence: Confidence::Rule,
    };
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    write::begin(&conn).unwrap();
    write::insert_edge_types(&conn, &[row(EdgeType::TestedBy), row(EdgeType::TypeOf)])
        .expect("edge_types in text order");
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    write::begin(&conn).unwrap();
    assert!(matches!(
        write::insert_edge_types(&conn, &[row(EdgeType::TypeOf), row(EdgeType::TestedBy)]),
        Err(WriteError::Unsorted)
    ));

    // a producer that sorts with the type's own `Ord` produces accepted input: the enum order
    // agrees with the stored text order
    let mut by_ord = EdgeType::ALL;
    by_ord.sort_unstable();
    let mut by_text = EdgeType::ALL;
    by_text.sort_unstable_by_key(|t| t.as_str());
    assert_eq!(by_ord, by_text, "EdgeType's Ord is its stored text order");
    assert!(EdgeType::TestedBy < EdgeType::TypeOf);
}

/// Rewrite `path` in place with `bytes` (same size or not) and give it a modification time
/// distinct from the current one, as a real republish would (the file system's own clock may
/// not have moved between two writes in the same test).
fn rewrite_with_new_mtime(path: &std::path::Path, bytes: &[u8]) {
    use std::time::{Duration, SystemTime};
    let before = std::fs::metadata(path).unwrap().modified().unwrap();
    std::fs::write(path, bytes).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
    f.set_modified(before + Duration::from_secs(7)).unwrap();
    drop(f);
    assert_ne!(
        std::fs::metadata(path).unwrap().modified().unwrap(),
        before,
        "the generation changed"
    );
    let _ = SystemTime::UNIX_EPOCH;
}

/// `GM2a` RG2-9: the `quick_check` memo is keyed by the file generation `(dev, ino, size,
/// mtime)`, for a pass and for a fail: a same-size rewrite is re-checked, whichever way the
/// verdict goes, and a replacement file under the same name is a new generation.
#[test]
fn quick_check_memo_is_keyed_by_generation_not_only_by_identity_and_size() {
    let dir = scratch();
    let commit = commit(0x0c);
    let path = small_graph(&commit).write_final(dir.path(), &commit);
    let good = std::fs::read(&path).unwrap();
    let mut damaged = good.clone();
    let last = damaged.len() - 4096;
    damaged[last..].fill(0);
    assert_eq!(good.len(), damaged.len(), "same size, different bytes");

    // pass, then same-size damage: re-checked and refused
    let before = sqlite::testing::quick_checks_run();
    SqliteSnapshot::open_ro(&path).expect("admitted");
    assert_eq!(sqlite::testing::quick_checks_run(), before + 1);
    rewrite_with_new_mtime(&path, &damaged);
    assert_eq!(
        SqliteSnapshot::open_ro(&path).unwrap_err(),
        SnapshotError::Corrupt,
        "the remembered pass does not cover the damaged generation"
    );
    assert_eq!(
        sqlite::testing::quick_checks_run(),
        before + 2,
        "re-checked"
    );

    // fail, then same-size repair: re-checked and admitted
    rewrite_with_new_mtime(&path, &good);
    SqliteSnapshot::open_ro(&path).expect("the repaired generation is admitted");
    assert_eq!(
        sqlite::testing::quick_checks_run(),
        before + 3,
        "re-checked"
    );
    // unchanged: the verdict is reused
    SqliteSnapshot::open_ro(&path).expect("admitted again");
    assert_eq!(
        sqlite::testing::quick_checks_run(),
        before + 3,
        "one check per generation"
    );

    // a replacement file under the same name (unlink and recreate, same bytes and size) is a
    // new generation: checked once more, whatever inode the file system hands out
    std::fs::remove_file(&path).unwrap();
    std::fs::write(&path, &good).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
    f.set_modified(
        std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_793_004_407),
    )
    .unwrap();
    drop(f);
    SqliteSnapshot::open_ro(&path).expect("the replacement is admitted");
    assert_eq!(
        sqlite::testing::quick_checks_run(),
        before + 4,
        "a new generation is checked"
    );
}

/// `GM2a` residual: `meta.schema_version` is `major` or `major.minor`, both decimal; anything
/// else is `Corrupt`, never a lenient prefix parse.
#[test]
fn schema_version_parse_is_strict() {
    let dir = scratch();
    let base = commit(0x0d);
    for (i, bad) in ["1.invalid", "1.2.3", "", ".1", "1.", "x1", "1 ", "+1"]
        .iter()
        .enumerate()
    {
        let c = base.replace("0d", &format!("{:02x}", 0xd0 + i));
        let mut s = small_graph(&c);
        s.set_meta("schema_version", bad);
        let path = s.write_final(dir.path(), &c);
        assert_eq!(
            SqliteSnapshot::open_ro(&path).unwrap_err(),
            SnapshotError::Corrupt,
            "{bad:?} is malformed"
        );
    }
    for (i, good) in [
        format!("{SCHEMA_VERSION}"),
        format!("{SCHEMA_VERSION}.0"),
        format!("{SCHEMA_VERSION}.17"),
    ]
    .iter()
    .enumerate()
    {
        let c = base.replace("0d", &format!("{:02x}", 0xe0 + i));
        let mut s = small_graph(&c);
        s.set_meta("schema_version", good);
        let path = s.write_final(dir.path(), &c);
        assert_eq!(
            SqliteSnapshot::open_ro(&path).unwrap().schema_version(),
            SCHEMA_VERSION,
            "{good:?} is well formed"
        );
    }
}

// ---------------------------------------------------------------------------------------------
// RU1-3: the writer-side opener admits the publisher's temp-file shape only

#[test]
fn open_existing_refuses_a_published_final_name() {
    use kaban_codegraph::write;
    let dir = scratch();
    let c = commit(0x3a);
    let path = small_graph(&c).write_final(dir.path(), &c);
    let before = std::fs::read(&path).unwrap();
    // a final `<commit40>.cg.db` is refused with the fixed class before any write
    let err = write::open_existing(&path).expect_err("a final snapshot name is not writable");
    assert!(matches!(err, WriteError::NotTempFile), "{err:?}");
    assert_eq!(err.to_string(), "not a temporary snapshot file name");
    assert!(!err.to_string().contains(&c), "no name in the text");
    assert_eq!(std::fs::read(&path).unwrap(), before, "bytes unchanged");
    // and so is every other shape that is not `<commit40>.cg.db.tmp-<pid>`
    for name in [
        "snapshot.db",
        format!("{c}.cg.db.tmp-").as_str(),
        format!("{c}.cg.db.tmp-12x").as_str(),
        format!("{}.cg.db.tmp-1", &c[..39]).as_str(),
        format!("{}.cg.db.tmp-1", c.to_uppercase()).as_str(),
    ] {
        let p = dir.path().join(name);
        std::fs::write(&p, b"").unwrap();
        let err = write::open_existing(&p).expect_err(name);
        assert!(matches!(err, WriteError::NotTempFile), "{name}: {err:?}");
    }
    // the temp shape itself is admitted and `insert_meta` works through it
    let tmp = dir
        .path()
        .join(format!("{c}.cg.db.tmp-{}", std::process::id()));
    std::fs::write(&tmp, b"").unwrap();
    let conn = write::open_existing(&tmp).expect("the temp shape is admitted");
    write::begin(&conn).unwrap();
    write::insert_meta(&conn, &[("a", "1")]).unwrap();
    // reopening the published snapshot for a write is impossible through the helper: the
    // only way to `insert_meta` into it would be an opener this crate does not provide
    drop(conn);
    assert_eq!(std::fs::read(&path).unwrap(), before);
}
