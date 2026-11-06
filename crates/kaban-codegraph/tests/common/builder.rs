//! A test snapshot builder over the public `write::*` helpers (stage1-plan CG2.2): rows are
//! Rust literals in the §4.1 order, the file is written under `<tmp>/<commit40>.cg.db` at 0600 so
//! that `open_ro` and `validate_for_attach` accept it unchanged (cg-join-v1 §10).

#![allow(
    dead_code,
    unreachable_pub,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic
)] // shared test support, included by `#[path]`

use std::fs::OpenOptions;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use kaban_codegraph::schema::{EdgeType, FileId, NodeId, NodeKind, Range, SCHEMA_VERSION};
use kaban_codegraph::write::{
    self, EdgeRow, EdgeTypeRow, FileRow, OccurrenceRow, SnippetRow, SymbolRow,
};
use rusqlite::Connection;

/// A deterministic 40-hex commit for fixtures: `n` repeated into the width.
pub fn commit(n: u8) -> String {
    format!("{n:02x}").repeat(20)
}

/// `<dir>/<commit40>.cg.db`.
pub fn final_path(dir: &Path, commit: &str) -> PathBuf {
    dir.join(format!("{commit}.cg.db"))
}

/// A scratch directory removed on drop (test-fixture-raii).
pub fn scratch() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("kaban-cg-")
        .tempdir()
        .expect("scratch directory")
}

/// Rows for one snapshot, in the §4.1 order; `meta` is sorted by key at write time.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub files: Vec<FileRow>,
    pub symbols: Vec<SymbolRow>,
    pub edges: Vec<EdgeRow>,
    pub occurrences: Vec<OccurrenceRow>,
    pub snippets: Vec<SnippetRow>,
    pub edge_types: Vec<EdgeTypeRow>,
    pub meta: Vec<(String, String)>,
}

impl Snapshot {
    /// An empty snapshot carrying the required meta keys for `commit`.
    pub fn new(commit: &str) -> Self {
        Self::default().with_meta(commit, &SCHEMA_VERSION.to_string())
    }

    pub fn with_meta(mut self, commit: &str, schema_version: &str) -> Self {
        self.meta = vec![
            ("commit".into(), commit.into()),
            ("isolation".into(), "none".into()),
            ("policy_version".into(), "pv1".into()),
            ("repo_id".into(), "0123456789abcdef0123456789abcdef".into()),
            ("schema_version".into(), schema_version.into()),
            ("symbols_fts_version".into(), "1".into()),
        ];
        self
    }

    pub fn set_meta(&mut self, key: &str, value: &str) {
        self.meta.retain(|(k, _)| k != key);
        self.meta.push((key.into(), value.into()));
        self.meta.sort();
    }

    /// Append a file row with the next id (callers keep `path` order).
    pub fn file(&mut self, path: &str, is_test: bool) -> FileId {
        let id = FileId(i64::try_from(self.files.len() + 1).unwrap());
        self.files.push(FileRow {
            id,
            path: path.into(),
            language: "go".into(),
            is_test,
            lines: 100,
        });
        id
    }

    /// Append a symbol row with the next id (callers keep `ref` order).
    pub fn symbol(&mut self, row: SymbolSpec<'_>) -> NodeId {
        let id = NodeId(i64::try_from(self.symbols.len() + 1).unwrap());
        let kind = row.kind;
        self.symbols.push(SymbolRow {
            id,
            kind,
            r#ref: row.r#ref.into(),
            // `scip:<scip>`; a local is `local:<path>:<scip>` and its scip has no colon
            scip: match kind {
                NodeKind::File => None,
                NodeKind::Local => Some(row.r#ref.rsplit_once(':').unwrap().1.into()),
                NodeKind::Symbol => Some(row.r#ref.split_once(':').unwrap().1.into()),
            },
            key: row.key.map(Into::into),
            label: row.label.into(),
            name: row.name.into(),
            file_id: row.file_id,
            def: row.def,
            enc: row.enc,
            signature: None,
            doc: None,
            external: row.external,
            is_test: row.is_test,
            ident_terms: row.ident_terms.into(),
        });
        id
    }

    pub fn edge(&mut self, src: NodeId, r#type: EdgeType, dst: NodeId) {
        self.edges.push(EdgeRow {
            src,
            dst,
            r#type,
            weight: 1,
        });
    }

    pub fn occurrence(
        &mut self,
        symbol_id: NodeId,
        file_id: FileId,
        range: Range,
        roles: u32,
        enclosing_id: Option<NodeId>,
    ) {
        self.occurrences.push(OccurrenceRow {
            symbol_id,
            file_id,
            range,
            roles,
            enclosing_id,
        });
    }

    /// Write the snapshot through the public helpers to `path` (created 0600, never clobbered).
    pub fn write_to(&self, path: &Path) -> Result<(), kaban_codegraph::WriteError> {
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .expect("create snapshot file");
        drop(file);
        let conn = Connection::open(path)?;
        self.write_with(&conn)
    }

    /// Every `scip` column value parses under the crate's own grammar and is canonical: the
    /// fixtures are relational stand-ins for indexer output, so they must at least be symbols the
    /// reader side would accept (`GM2a` residual).
    pub fn assert_symbols_grammar_valid(&self) {
        for row in &self.symbols {
            if let Some(scip) = &row.scip {
                let parsed = kaban_codegraph::symbol::parse(scip).unwrap_or_else(|e| {
                    panic!("fixture symbol {scip:?} is not grammar-valid: {e}")
                });
                assert_eq!(kaban_codegraph::symbol::format(&parsed), *scip);
            }
        }
    }

    /// The helper sequence over an open connection (after the grammar check of the fixture).
    pub fn write_with(&self, conn: &Connection) -> Result<(), kaban_codegraph::WriteError> {
        self.assert_symbols_grammar_valid();
        write::begin(conn)?;
        write::insert_files(conn, &self.files)?;
        write::insert_symbols(conn, &self.symbols)?;
        write::insert_edges(conn, &self.edges)?;
        write::insert_occurrences(conn, &self.occurrences)?;
        write::insert_snippets(conn, &self.snippets)?;
        write::insert_edge_types(conn, &self.edge_types)?;
        let mut meta = self.meta.clone();
        meta.sort();
        let meta: Vec<(&str, &str)> = meta.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        write::insert_meta(conn, &meta)?;
        write::rebuild_fts(conn)?;
        write::finish(conn)
    }

    /// Write under `dir` with the final name of `commit` and return the path.
    pub fn write_final(&self, dir: &Path, commit: &str) -> PathBuf {
        let path = final_path(dir, commit);
        self.write_to(&path).expect("snapshot written");
        path
    }
}

/// The caller-chosen columns of a symbol row.
#[derive(Debug, Clone, Copy)]
pub struct SymbolSpec<'a> {
    pub kind: NodeKind,
    pub r#ref: &'a str,
    pub key: Option<&'a str>,
    pub label: &'a str,
    pub name: &'a str,
    pub file_id: Option<FileId>,
    pub def: Option<Range>,
    pub enc: Option<Range>,
    pub external: bool,
    pub is_test: bool,
    pub ident_terms: &'a str,
}

impl<'a> SymbolSpec<'a> {
    /// A non-external global symbol defined in `file` at `def`.
    pub fn symbol(r#ref: &'a str, label: &'a str, name: &'a str, file: FileId, def: Range) -> Self {
        Self {
            kind: NodeKind::Symbol,
            r#ref,
            key: None,
            label,
            name,
            file_id: Some(file),
            def: Some(def),
            enc: Some(def),
            external: false,
            is_test: false,
            ident_terms: "",
        }
    }

    /// A file node for `file` at `path`.
    pub fn file(r#ref: &'a str, name: &'a str, file: FileId) -> Self {
        Self {
            kind: NodeKind::File,
            r#ref,
            key: None,
            label: "File",
            name,
            file_id: Some(file),
            def: None,
            enc: None,
            external: false,
            is_test: false,
            ident_terms: "",
        }
    }

    pub fn key(mut self, key: &'a str) -> Self {
        self.key = Some(key);
        self
    }

    pub fn external(mut self) -> Self {
        self.external = true;
        self.file_id = None;
        self.def = None;
        self.enc = None;
        self
    }

    pub fn is_test(mut self) -> Self {
        self.is_test = true;
        self
    }
}

/// A line range `[sl, el]` at columns 0..1.
pub fn lines(sl: u32, el: u32) -> Range {
    Range {
        sl,
        sc: 0,
        el,
        ec: 1,
    }
}

/// Connection-lifetime observer: inspect existing descriptors, never reopen SQLite paths.
pub fn open_fds_to(path: &Path) -> usize {
    #[cfg(target_os = "linux")]
    {
        let target = std::fs::canonicalize(path).unwrap();
        std::fs::read_dir("/proc/self/fd")
            .unwrap()
            .map(Result::unwrap)
            .filter(|e| std::fs::read_link(e.path()).is_ok_and(|l| l == target))
            .count()
    }
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::fs::MetadataExt;
        let target = std::fs::metadata(path).unwrap();
        std::fs::read_dir("/dev/fd")
            .unwrap()
            .map(Result::unwrap)
            .filter(|entry| {
                let fd = entry
                    .file_name()
                    .to_string_lossy()
                    .parse::<i32>()
                    .expect("descriptor number");
                match nix::sys::stat::fstat(fd) {
                    Ok(st) => {
                        u64::from(u32::from_ne_bytes(st.st_dev.to_ne_bytes())) == target.dev()
                            && st.st_ino == target.ino()
                    }
                    Err(nix::errno::Errno::EBADF) => false,
                    Err(e) => panic!("descriptor observation failed: {e}"),
                }
            })
            .count()
    }
}
