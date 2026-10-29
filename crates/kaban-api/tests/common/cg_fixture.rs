//! The CG-JOIN v1 §10 fixture builder (engine-plan E2.6; SO-1): snapshot files written only
//! through `kaban_codegraph::write::{begin, insert_files, insert_symbols, insert_edges,
//! insert_occurrences, insert_meta, rebuild_fts, finish}` from Rust literals, into a temporary
//! `<tmp>/codegraph/<repo-id>/<commit40>.cg.db` (directory 0700, file 0600, final basename) so
//! that `validate_for_attach` and `FixedSnapshot` accept it unchanged. Never a committed `.cg.db`,
//! never a JSON description. Reused by `crates/kaban-store-sqlite/tests/attach.rs` through
//! `#[path]` (contract §10), so it depends on nothing else under `tests/common`.
//!
//! Variants: `afero` (the `MemMapFs#Create().` key of G1-F9), `afero-renamed` (that key absent, a
//! new one present), `afero-ambiguous` (two package versions normalising to one key, both
//! non-external), `afero-too-new` (`meta.schema_version` major + 1), `many(total, external,
//! local)` (the `snapshot_symbols` paging fixture). Parity with a real `kaban-index` output is
//! OD-P-4's residual.

#![allow(dead_code)] // each test binary uses a subset
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

use std::fs::OpenOptions;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use kaban_codegraph::write::{FileRow, SymbolRow};
use kaban_codegraph::{FileId, NodeId, NodeKind, Range, SCHEMA_VERSION, write};

/// The keyed directory name (`hmac_hex(..)[0..32]` in production): deliberately not the
/// process repository identity `ApiConfig.repo` the join compares (SD-5 identity test).
pub(crate) const REPO_ID: &str = "4d2a9f0c7b1e6a3d8c5f2e9b0a7d4c1f";
/// The `afero` fixture's commit (40 hex).
pub(crate) const AFERO_COMMIT: &str = "a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1";
/// A second commit for the switch tests.
pub(crate) const OTHER_COMMIT: &str = "b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2";
/// The G1-F9 key: `MemMapFs.Create` of `github.com/spf13/afero`, versions normalised to `.`.
pub(crate) const AFERO_KEY: &str = "scip-go gomod github.com/spf13/afero . MemMapFs#Create().";
/// The key the `afero-renamed` variant holds instead of [`AFERO_KEY`].
pub(crate) const AFERO_RENAMED_KEY: &str =
    "scip-go gomod github.com/spf13/afero . MemMapFs#CreateFile().";
/// The defining file of [`AFERO_KEY`] (`files.path`).
pub(crate) const AFERO_PATH: &str = "memmap.go";
/// The second file of the `afero` variants.
pub(crate) const AFERO_OTHER_PATH: &str = "mem/file.go";
/// The `many` variant's one file.
pub(crate) const MANY_PATH: &str = "many.go";
/// `meta.policy_version` unless the caller passes its own (the `policy: current | stale` tests).
pub(crate) const POLICY_VERSION: &str = "pv-e26";

/// Which rows the file holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Variant {
    /// [`AFERO_KEY`] once, non-external, defined in [`AFERO_PATH`]; a struct row per file and
    /// one external placeholder with another key.
    Afero,
    /// As [`Variant::Afero`] with [`AFERO_RENAMED_KEY`] in place of [`AFERO_KEY`].
    AferoRenamed,
    /// As [`Variant::Afero`] plus a second package version of the same method: two
    /// non-external `symbol` rows share [`AFERO_KEY`].
    AferoAmbiguous,
    /// As [`Variant::Afero`] with `meta.schema_version` one major above the supported one.
    AferoTooNew,
    /// `total` symbol rows in [`MANY_PATH`]: `local` of them `kind = 'local'`, `external` of the
    /// rest `external = 1`, every row with a non-empty `ident_terms`.
    Many {
        total: usize,
        external: usize,
        local: usize,
    },
}

/// What [`build`] wrote.
#[derive(Debug, Clone)]
pub(crate) struct Built {
    /// `<tmp>/codegraph/<REPO_ID>`.
    pub(crate) dir: PathBuf,
    /// `<dir>/<commit>.cg.db`.
    pub(crate) path: PathBuf,
    /// The 40-hex commit of the file name and `meta.commit`.
    pub(crate) commit: String,
}

/// `<tmp>/codegraph/<REPO_ID>`, both levels `0700` (the private-directory rule of G1-S7).
pub(crate) fn snapshot_dir(tmp: &Path) -> PathBuf {
    let cg = tmp.join("codegraph");
    let dir = cg.join(REPO_ID);
    std::fs::create_dir_all(&dir).unwrap();
    for p in [&cg, &dir] {
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    dir
}

/// Write `variant` as `<tmp>/codegraph/<REPO_ID>/<commit>.cg.db` with [`POLICY_VERSION`].
pub(crate) fn build(tmp: &Path, variant: Variant, commit: &str) -> Built {
    build_with_policy(tmp, variant, commit, POLICY_VERSION)
}

/// [`build`] with an explicit `meta.policy_version`. An existing file of that name is removed
/// first, so a rebuild is a new inode (a same-commit republication, RU3-2).
pub(crate) fn build_with_policy(
    tmp: &Path,
    variant: Variant,
    commit: &str,
    policy_version: &str,
) -> Built {
    let (files, symbols) = rows(variant);
    let schema = match variant {
        Variant::AferoTooNew => SCHEMA_VERSION + 1,
        _ => SCHEMA_VERSION,
    };
    build_rows(
        &snapshot_dir(tmp),
        REPO_ID,
        commit,
        policy_version,
        "go",
        schema,
        &files,
        &symbols,
    )
}

/// Publish caller-owned typed rows under the provider's actual keyed repository directory.
/// The walkthrough uses this seam without decoding its synthetic provenance `index.scip`.
#[allow(clippy::too_many_arguments)] // mirrors the independent snapshot metadata fields
pub(crate) fn build_rows(
    dir: &Path,
    repo_id: &str,
    commit: &str,
    policy_version: &str,
    languages: &str,
    schema_version: u32,
    files: &[FileRow],
    symbols: &[SymbolRow],
) -> Built {
    assert_eq!(commit.len(), 40, "a final name carries a 40-hex commit");
    let dir = dir.to_owned();
    let path = dir.join(format!("{commit}.cg.db"));
    let _ = std::fs::remove_file(&path);
    drop(
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .unwrap(),
    );
    let conn = rusqlite::Connection::open(&path).unwrap();
    write::begin(&conn).unwrap();
    write::insert_files(&conn, files).unwrap();
    write::insert_symbols(&conn, symbols).unwrap();
    write::insert_edges(&conn, &[]).unwrap();
    write::insert_occurrences(&conn, &[]).unwrap();
    write::insert_snippets(&conn, &[]).unwrap();
    write::insert_edge_types(&conn, &[]).unwrap();
    let schema = schema_version.to_string();
    let n_files = files.len().to_string();
    let n_symbols = symbols.len().to_string();
    // every key of cg-join-v1 §8 `meta`, sorted by key bytes (the writer's order rule)
    let mut meta: Vec<(&str, &str)> = vec![
        ("caches", ""),
        ("commit", commit),
        ("counts:files", n_files.as_str()),
        ("counts:symbols", n_symbols.as_str()),
        ("isolation", "none"),
        ("isolation_detail", ""),
        ("languages", languages),
        ("policy_version", policy_version),
        ("repo_id", repo_id),
        ("schema_version", schema.as_str()),
        ("scip_range_form", "end_exclusive"),
        ("symbols_fts_version", "1"),
    ];
    meta.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    write::insert_meta(&conn, &meta).unwrap();
    write::rebuild_fts(&conn).unwrap();
    write::finish(&conn).unwrap();
    drop(conn);
    Built {
        dir,
        path,
        commit: commit.to_owned(),
    }
}

fn file(id: i64, path: &str) -> FileRow {
    FileRow {
        id: FileId(id),
        path: path.into(),
        language: "go".into(),
        is_test: false,
        lines: 40,
    }
}

fn symbol(scip: &str, key: &str, label: &str, name: &str, file: Option<i64>) -> SymbolRow {
    SymbolRow {
        id: NodeId(0),
        kind: NodeKind::Symbol,
        r#ref: format!("scip:{scip}"),
        scip: Some(scip.to_owned()),
        key: Some(key.to_owned()),
        label: label.into(),
        name: name.into(),
        file_id: file.map(FileId),
        def: Some(Range {
            sl: 1,
            sc: 0,
            el: 1,
            ec: 4,
        }),
        enc: None,
        signature: None,
        doc: None,
        external: file.is_none(),
        is_test: false,
        ident_terms: name.to_lowercase(),
    }
}

/// The rows of a variant, sorted by their byte-order keys with ids as positions.
fn rows(variant: Variant) -> (Vec<FileRow>, Vec<SymbolRow>) {
    let mut symbols = match variant {
        Variant::Many {
            total,
            external,
            local,
        } => return many(total, external, local),
        Variant::Afero | Variant::AferoTooNew | Variant::AferoAmbiguous | Variant::AferoRenamed => {
            afero(variant)
        }
    };
    // `mem/file.go` < `memmap.go` in byte order (`/` before `m`)
    let files = vec![file(1, AFERO_OTHER_PATH), file(2, AFERO_PATH)];
    finish_ids(&mut symbols);
    (files, symbols)
}

fn afero(variant: Variant) -> Vec<SymbolRow> {
    const PKG: &str = "scip-go gomod github.com/spf13/afero";
    let (method, key) = match variant {
        Variant::AferoRenamed => ("MemMapFs#CreateFile().", AFERO_RENAMED_KEY),
        _ => ("MemMapFs#Create().", AFERO_KEY),
    };
    let name = if variant == Variant::AferoRenamed {
        "CreateFile"
    } else {
        "Create"
    };
    let mut rows = vec![
        symbol(
            &format!("{PKG} v1.11.0 MemMapFs#"),
            &format!("{PKG} . MemMapFs#"),
            "Struct",
            "MemMapFs",
            Some(2),
        ),
        symbol(
            &format!("{PKG} v1.11.0 {method}"),
            key,
            "Method",
            name,
            Some(2),
        ),
        symbol(
            &format!("{PKG} v1.11.0 mem/File#"),
            &format!("{PKG} . mem/File#"),
            "Struct",
            "File",
            Some(1),
        ),
        // an external placeholder: another package's method, no defining file
        symbol(
            "scip-go gomod github.com/other v1.0.0 Other#Create().",
            "scip-go gomod github.com/other . Other#Create().",
            "Method",
            "Create",
            None,
        ),
    ];
    if variant == Variant::AferoAmbiguous {
        rows.push(symbol(
            &format!("{PKG} v1.12.0 {method}"),
            key,
            "Method",
            name,
            Some(2),
        ));
    }
    rows
}

fn many(total: usize, external: usize, local: usize) -> (Vec<FileRow>, Vec<SymbolRow>) {
    assert!(local + external <= total);
    let mut rows = Vec::with_capacity(total);
    // `local:` sorts before `scip:`; zero-padded indexes keep byte order numeric
    for i in 0..local {
        let mut r = symbol("", "", "Local", &format!("l{i}"), Some(1));
        r.kind = NodeKind::Local;
        r.r#ref = format!("local:{i:05}");
        r.scip = None;
        r.key = None;
        rows.push(r);
    }
    let scip_rows = total - local;
    let first_external = scip_rows - external;
    for i in 0..scip_rows {
        let scip = format!("scip-go gomod example.com/many v1.0.0 p/S{i:05}#M().");
        let key = format!("scip-go gomod example.com/many . p/S{i:05}#M().");
        let mut r = symbol(&scip, &key, "Method", "M", Some(1));
        r.ident_terms = format!("m s{i:05}");
        if i >= first_external {
            r.external = true;
            r.file_id = None;
        }
        rows.push(r);
    }
    finish_ids(&mut rows);
    (vec![file(1, MANY_PATH)], rows)
}

/// Sort by `ref` bytes and assign the 1-based positions (the writer's id rule).
fn finish_ids(rows: &mut [SymbolRow]) {
    rows.sort_by(|a, b| a.r#ref.as_bytes().cmp(b.r#ref.as_bytes()));
    for (i, r) in rows.iter_mut().enumerate() {
        r.id = NodeId(i64::try_from(i + 1).unwrap());
    }
}
