//! The synthetic bench corpus (stage1-plan §5 non-functional method, §18; CG7.3): a pure,
//! deterministic snapshot generator over [`crate::write`], modelled on the test publishers,
//! with no dependency on the API side (SD-9, OD-G1-6). `kaban bench cg gen` calls it; nothing
//! in a production build reaches it.
//!
//! Shape, per 20-symbol file (`symbols / 20` files, 50 files per package):
//!
//! ```text
//!   File node file:<path> (label File, name = basename; the indexer's `nodes_of_files` row,
//!       so the `path` selector form resolves; it owns no CONTAINS edge: the spec's parent is
//!       the symbol minus its last descriptor, never a file)
//!   Interface I<f>#  CONTAINS  I<f>#M0().  I<f>#M1().
//!   Struct S<f>_<j># (j < 3)  CONTAINS  S<f>_<j>#M0().  S<f>_<j>#M1().
//!       IMPLEMENTS one interface drawn Zipf-like over the files (file 0's collects the most)
//!       its methods IMPLEMENTS_MEMBER the interface's methods of the same index
//!   Function F<f>_<k>().  (k < 8)
//!   the last 15% of files are `_test.go` files: every symbol in them is a test symbol and
//!   each test Function is the TESTED_BY target of one non-test Function (Zipf-like, so one
//!   function collects the most tests)
//!   REFERENCES fill the budget up to 1.3 edges per symbol: the hub (`bench:hub` in meta)
//!   receives exactly 2,000 edges from distinct Functions, the rest a Zipf in-degree over the
//!   other symbols; one occurrence per edge inside the referencing Function's file
//! ```
//!
//! Everything derives from the seed through one xorshift64* stream, the ids are the §4.1
//! positions (`file:` refs sort before `scip:` refs, so the file nodes take `1..=files`) and
//! the file is `<dir>/<commit40>.cg.db` (0600) with the commit itself derived from the seed,
//! so two generations are byte-identical (`corpus_is_deterministic`). The bench targets are
//! published in `meta` under `bench:*`, so `kaban bench cg run` needs the snapshot alone;
//! `bench_targets_resolve` asserts each one resolves through the reader (CG7.3a: the first
//! corpus had no file node, so the `path` form of `code_members` was `unknown_symbol`).

#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)] // corpus arithmetic on counts, the 1-based ids and the Zipf draw

use std::collections::{BTreeMap, BTreeSet};
use std::fs::OpenOptions;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};
use thiserror::Error;

use crate::error::WriteError;
use crate::schema::{
    EdgeType, FileId, NodeId, NodeKind, Range, SCHEMA_VERSION, SYMBOLS_FTS_VERSION,
};
use crate::sqlite::FINAL_SUFFIX;
use crate::write::{self, EdgeRow, FileRow, OccurrenceRow, SymbolRow};

/// Symbols per file.
const PER_FILE: usize = 20;
/// Files per package directory.
const FILES_PER_PACKAGE: usize = 50;
/// Edges per symbol, times ten (1.3).
const EDGES_PER_SYMBOL_X10: usize = 13;
/// Test files per thousand (15%).
const TEST_FILES_PER_MILLE: usize = 150;
/// The hub's `REFERENCES` in-degree (§18: "Zipf in-degree with a 2,000 hub").
pub const HUB_IN_DEGREE: usize = 2_000;
/// `meta.repo_id` of every corpus (32 hex, like the fixtures).
const REPO_ID: &str = "b3bcb3bcb3bcb3bcb3bcb3bcb3bcb3bc";
/// SCIP `SymbolRole.ReadAccess`.
const READ_ACCESS: u32 = 8;

/// Why a corpus was not written.
#[derive(Debug, Error)]
pub enum CorpusError {
    /// Fewer than one file's worth of symbols were asked for.
    #[error("a corpus needs at least {PER_FILE} symbols")]
    TooSmall,
    /// The output directory or the snapshot file could not be created (the final name
    /// already exists, the directory is missing or not writable).
    #[error("corpus file not created ({0})")]
    Io(std::io::Error),
    /// A write helper refused or SQLite failed.
    #[error(transparent)]
    Write(#[from] WriteError),
}

impl From<std::io::Error> for CorpusError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<rusqlite::Error> for CorpusError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Write(WriteError::from(e))
    }
}

/// What one generation produced: the file, its counts and the bench targets (also published
/// in `meta` as `bench:*`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CorpusStats {
    /// `<dir>/<commit40>.cg.db`.
    pub path: PathBuf,
    /// The 40-hex commit derived from the seed.
    pub commit: String,
    /// `files` rows.
    pub files: usize,
    /// Symbol rows (20 per file; the `--symbols` figure).
    pub symbols: usize,
    /// Every `symbols` table row: the symbols plus one file node per file (`counts:symbols`,
    /// the figure the indexer writes; the bench's `n_nodes`).
    pub nodes: usize,
    /// `edges` rows.
    pub edges: usize,
    /// `occurrences` rows.
    pub occurrences: usize,
    /// The `REFERENCES` hub's ref.
    pub hub: String,
    /// Its in-degree (2,000 when the corpus has enough Functions, else every other Function).
    pub hub_in_degree: usize,
    /// The interface with the most implementers.
    pub interface: String,
    /// Its implementer count.
    pub interface_implementers: usize,
    /// The function with the most `TESTED_BY` targets.
    pub tested: String,
    /// Its test count.
    pub tested_count: usize,
    /// The path of the hub's file (the `code_members` target through its file node).
    pub members_path: String,
}

/// xorshift64* (the bench corpus of the memory engine uses the same family).
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(if seed == 0 {
            0x9E37_79B9_7F4A_7C15
        } else {
            seed
        })
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// `0..n` (`n > 0`).
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    /// `[0, 1)`.
    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// A rank in `lo..=hi` with probability about `1 / rank` (Zipf exponent 1, by the inverse
    /// of the continuous harmonic CDF).
    fn zipf(&mut self, lo: usize, hi: usize) -> usize {
        if hi <= lo {
            return lo;
        }
        let a = (lo as f64).ln();
        let b = (hi as f64 + 1.0).ln();
        let r = (a + self.unit() * (b - a)).exp().floor() as usize;
        r.clamp(lo, hi)
    }
}

/// One symbol before ids are assigned.
struct Sym {
    r#ref: String,
    scip: String,
    key: String,
    label: &'static str,
    name: String,
    file: usize,
    line: u32,
    is_test: bool,
}

/// `(scip, key, ref)` of a descriptor string in package `pkg`.
fn triple(pkg: usize, descriptors: &str) -> (String, String, String) {
    let scip =
        format!("scip-go gomod example.com/bench v1 `example.com/bench/pkg{pkg:03}`/{descriptors}");
    let key =
        format!("scip-go gomod example.com/bench . `example.com/bench/pkg{pkg:03}`/{descriptors}");
    let r#ref = format!("scip:{scip}");
    (scip, key, r#ref)
}

/// 40 lowercase hex digits from the seed's own stream.
fn commit_of(seed: u64) -> String {
    use std::fmt::Write as _;
    let mut rng = Rng::new(seed ^ 0xC0DE_1DE5_0000_0000);
    let mut commit = String::with_capacity(40);
    for _ in 0..5 {
        // writing to a `String` cannot fail
        let _ = write!(commit, "{:08x}", rng.next() as u32);
    }
    commit
}

/// The symbols of the corpus in generation order, with the per-file index of each group.
struct Layout {
    syms: Vec<Sym>,
    paths: Vec<String>,
    /// Per file: the interface, its two methods, the three structs with their two methods,
    /// the eight functions (indices into `syms`).
    ifaces: Vec<usize>,
    iface_methods: Vec<[usize; 2]>,
    structs: Vec<[usize; 3]>,
    struct_methods: Vec<[[usize; 2]; 3]>,
    functions: Vec<usize>,
    test_functions: Vec<usize>,
    nontest_functions: Vec<usize>,
}

fn layout(files: usize) -> Layout {
    let test_from = files - files * TEST_FILES_PER_MILLE / 1000;
    let mut l = Layout {
        syms: Vec::with_capacity(files * PER_FILE),
        paths: Vec::with_capacity(files),
        ifaces: Vec::with_capacity(files),
        iface_methods: Vec::with_capacity(files),
        structs: Vec::with_capacity(files),
        struct_methods: Vec::with_capacity(files),
        functions: Vec::with_capacity(files * 8),
        test_functions: Vec::new(),
        nontest_functions: Vec::new(),
    };
    for f in 0..files {
        let is_test = f >= test_from;
        let pkg = f / FILES_PER_PACKAGE;
        let path = if is_test {
            format!("pkg{pkg:03}/f{f:05}_test.go")
        } else {
            format!("pkg{pkg:03}/f{f:05}.go")
        };
        l.paths.push(path);
        let mut line = 0u32;
        let mut push = |l: &mut Layout, desc: String, label: &'static str, name: String| {
            let (scip, key, r#ref) = triple(pkg, &desc);
            l.syms.push(Sym {
                r#ref,
                scip,
                key,
                label,
                name,
                file: f,
                line,
                is_test,
            });
            line += 5;
            l.syms.len() - 1
        };
        let iface = push(&mut l, format!("I{f}#"), "Interface", format!("I{f}"));
        let im = [
            push(&mut l, format!("I{f}#M0()."), "Method", "M0".to_owned()),
            push(&mut l, format!("I{f}#M1()."), "Method", "M1".to_owned()),
        ];
        let mut structs = [0usize; 3];
        let mut methods = [[0usize; 2]; 3];
        for j in 0..3 {
            structs[j] = push(&mut l, format!("S{f}_{j}#"), "Struct", format!("S{f}_{j}"));
            methods[j] = [
                push(&mut l, format!("S{f}_{j}#M0()."), "Method", "M0".to_owned()),
                push(&mut l, format!("S{f}_{j}#M1()."), "Method", "M1".to_owned()),
            ];
        }
        for k in 0..8 {
            let name = if is_test {
                format!("TestF{f}_{k}")
            } else {
                format!("F{f}_{k}")
            };
            let idx = push(&mut l, format!("{name}()."), "Function", name);
            l.functions.push(idx);
            if is_test {
                l.test_functions.push(idx);
            } else {
                l.nontest_functions.push(idx);
            }
        }
        l.ifaces.push(iface);
        l.iface_methods.push(im);
        l.structs.push(structs);
        l.struct_methods.push(methods);
    }
    l
}

/// Generate `<out>/<commit40>.cg.db` with about `symbols` symbols (rounded down to whole
/// files) from `seed`.
///
/// # Errors
/// [`CorpusError`]: too few symbols, the file could not be created, a write helper refused.
pub fn generate(out: &Path, symbols: usize, seed: u64) -> Result<CorpusStats, CorpusError> {
    let files = symbols / PER_FILE;
    if files == 0 {
        return Err(CorpusError::TooSmall);
    }
    let mut rng = Rng::new(seed);
    let l = layout(files);
    let n = l.syms.len();

    // edges by generation index, de-duplicated by (src, type, dst)
    let mut edges: BTreeSet<(usize, EdgeType, usize)> = BTreeSet::new();
    let mut implementers = vec![0usize; files];
    for f in 0..files {
        edges.insert((l.ifaces[f], EdgeType::Contains, l.iface_methods[f][0]));
        edges.insert((l.ifaces[f], EdgeType::Contains, l.iface_methods[f][1]));
        for j in 0..3 {
            let s = l.structs[f][j];
            edges.insert((s, EdgeType::Contains, l.struct_methods[f][j][0]));
            edges.insert((s, EdgeType::Contains, l.struct_methods[f][j][1]));
            let target = rng.zipf(1, files) - 1;
            edges.insert((s, EdgeType::Implements, l.ifaces[target]));
            implementers[target] += 1;
            for m in 0..2 {
                edges.insert((
                    l.struct_methods[f][j][m],
                    EdgeType::ImplementsMember,
                    l.iface_methods[target][m],
                ));
            }
        }
    }
    let mut tests = vec![0usize; l.nontest_functions.len().max(1)];
    for &t in &l.test_functions {
        if l.nontest_functions.is_empty() {
            break;
        }
        let target = rng.zipf(1, l.nontest_functions.len()) - 1;
        if edges.insert((l.nontest_functions[target], EdgeType::TestedBy, t)) {
            tests[target] += 1;
        }
    }

    // REFERENCES: the hub first, then the Zipf tail; one occurrence per edge
    let hub = l.functions[0];
    let mut occurrences: Vec<(usize, usize)> = Vec::new(); // (src, dst)
    let mut hub_in_degree = 0usize;
    let n_funcs = l.functions.len();
    let mut i = 0usize;
    // a prime stride walks every Function once before repeating; the bound closes the loop
    // when the corpus has fewer Functions than the hub needs
    while hub_in_degree < HUB_IN_DEGREE && i < n_funcs * 2 {
        let src = l.functions[(i * 4099 + 1) % n_funcs];
        i += 1;
        if src != hub && edges.insert((src, EdgeType::References, hub)) {
            occurrences.push((src, hub));
            hub_in_degree += 1;
        }
    }
    let target_edges = n * EDGES_PER_SYMBOL_X10 / 10;
    let mut draws = 0usize;
    while edges.len() < target_edges && draws < target_edges * 8 {
        draws += 1;
        let src = l.functions[rng.below(n_funcs)];
        let dst = (rng.zipf(2, n) * 7_919) % n;
        if dst == hub || dst == src {
            continue;
        }
        if edges.insert((src, EdgeType::References, dst)) {
            occurrences.push((src, dst));
        }
    }

    // ids: files by path, symbols by ref (§4.1)
    let mut file_order: Vec<usize> = (0..files).collect();
    file_order.sort_by(|a, b| l.paths[*a].as_bytes().cmp(l.paths[*b].as_bytes()));
    let mut file_id = vec![FileId(0); files];
    let file_rows: Vec<FileRow> = file_order
        .iter()
        .enumerate()
        .map(|(pos, &f)| {
            let id = FileId(pos as i64 + 1);
            file_id[f] = id;
            FileRow {
                id,
                path: l.paths[f].clone(),
                language: "go".to_owned(),
                is_test: f >= files - files * TEST_FILES_PER_MILLE / 1000,
                lines: (PER_FILE as u32) * 5 + 20,
            }
        })
        .collect();
    // the file nodes (the indexer's `nodes_of_files` rows): `file:<path>` sorts before every
    // `scip:` ref, and path order is ref order under the fixed prefix, so their ids are the
    // file ids and the symbols follow
    let file_node_rows: Vec<SymbolRow> = file_rows
        .iter()
        .map(|f| SymbolRow {
            id: NodeId(f.id.0),
            kind: NodeKind::File,
            r#ref: format!("file:{}", f.path),
            scip: None,
            key: None,
            label: "File".to_owned(),
            name: f.path.rsplit('/').next().unwrap_or(&f.path).to_owned(),
            file_id: Some(f.id),
            def: None,
            enc: None,
            signature: None,
            doc: None,
            external: false,
            is_test: f.is_test,
            ident_terms: String::new(),
        })
        .collect();
    let mut sym_order: Vec<usize> = (0..n).collect();
    sym_order.sort_by(|a, b| l.syms[*a].r#ref.as_bytes().cmp(l.syms[*b].r#ref.as_bytes()));
    let mut node_id = vec![NodeId(0); n];
    let symbol_rows: Vec<SymbolRow> = sym_order
        .iter()
        .enumerate()
        .map(|(pos, &s)| {
            let id = NodeId((files + pos) as i64 + 1);
            node_id[s] = id;
            let sym = &l.syms[s];
            SymbolRow {
                id,
                kind: NodeKind::Symbol,
                r#ref: sym.r#ref.clone(),
                scip: Some(sym.scip.clone()),
                key: Some(sym.key.clone()),
                label: sym.label.to_owned(),
                name: sym.name.clone(),
                file_id: Some(file_id[sym.file]),
                def: Some(Range {
                    sl: sym.line,
                    sc: 0,
                    el: sym.line,
                    ec: sym.name.len() as u32,
                }),
                enc: Some(Range {
                    sl: sym.line,
                    sc: 0,
                    el: sym.line + 4,
                    ec: 1,
                }),
                signature: None,
                doc: None,
                external: false,
                is_test: sym.is_test,
                ident_terms: String::new(),
            }
        })
        .collect();
    let symbols = symbol_rows.len();
    let node_rows: Vec<SymbolRow> = file_node_rows.into_iter().chain(symbol_rows).collect();
    let mut edge_rows: Vec<EdgeRow> = edges
        .iter()
        .map(|&(src, t, dst)| EdgeRow {
            src: node_id[src],
            dst: node_id[dst],
            r#type: t,
            weight: 1,
        })
        .collect();
    edge_rows
        .sort_by(|a, b| (a.src, a.r#type.as_str(), a.dst).cmp(&(b.src, b.r#type.as_str(), b.dst)));
    let mut per_src: BTreeMap<usize, u32> = BTreeMap::new();
    let mut occ_rows: Vec<OccurrenceRow> = occurrences
        .iter()
        .map(|&(src, dst)| {
            let k = per_src.entry(src).or_insert(0);
            let col = *k * 4;
            *k += 1;
            let line = l.syms[src].line + 1;
            OccurrenceRow {
                symbol_id: node_id[dst],
                file_id: file_id[l.syms[src].file],
                range: Range {
                    sl: line,
                    sc: col,
                    el: line,
                    ec: col + 3,
                },
                roles: READ_ACCESS,
                enclosing_id: Some(node_id[src]),
            }
        })
        .collect();
    occ_rows.sort_by_key(|o| {
        (
            o.file_id,
            o.range.sl,
            o.range.sc,
            o.range.el,
            o.range.ec,
            o.symbol_id,
            o.roles,
        )
    });

    // the bench targets
    let (top_iface, top_impl) = implementers
        .iter()
        .enumerate()
        .max_by_key(|(f, c)| (**c, usize::MAX - *f))
        .map_or((0, 0), |(f, c)| (f, *c));
    let (top_tested, top_tests) = tests
        .iter()
        .enumerate()
        .max_by_key(|(f, c)| (**c, usize::MAX - *f))
        .map_or((0, 0), |(f, c)| (f, *c));
    let tested_ref = l
        .nontest_functions
        .get(top_tested)
        .map_or_else(|| l.syms[hub].r#ref.clone(), |&s| l.syms[s].r#ref.clone());

    let commit = commit_of(seed);
    let path = out.join(format!("{commit}{FINAL_SUFFIX}"));
    drop(
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?,
    );
    let schema = SCHEMA_VERSION.to_string();
    let fts = SYMBOLS_FTS_VERSION.to_string();
    let seed_text = format!("{seed:#x}");
    let counts = [
        ("counts:edges", edge_rows.len().to_string()),
        ("counts:files", file_rows.len().to_string()),
        ("counts:occurrences", occ_rows.len().to_string()),
        ("counts:symbols", node_rows.len().to_string()),
    ];
    let bench = [
        ("bench:hub", l.syms[hub].r#ref.clone()),
        ("bench:hub_in_degree", hub_in_degree.to_string()),
        ("bench:interface", l.syms[l.ifaces[top_iface]].r#ref.clone()),
        ("bench:interface_implementers", top_impl.to_string()),
        ("bench:members_path", l.paths[l.syms[hub].file].clone()),
        ("bench:seed", seed_text.clone()),
        ("bench:tested", tested_ref.clone()),
        ("bench:tested_count", top_tests.to_string()),
    ];
    let indexer = format!("kaban-codegraph testing::corpus (synthetic, seed {seed_text})");
    let mut meta: BTreeMap<&str, &str> = BTreeMap::from([
        ("caches", "dedicated"),
        ("commit", commit.as_str()),
        ("indexer:go", indexer.as_str()),
        ("isolation", "none"),
        ("isolation_detail", "synthetic corpus, no indexer ran"),
        ("languages", "go"),
        ("policy_version", "bench"),
        ("repo_id", REPO_ID),
        ("schema_version", schema.as_str()),
        ("symbols_fts_version", fts.as_str()),
    ]);
    for (k, v) in counts.iter().chain(bench.iter()) {
        meta.insert(k, v.as_str());
    }
    let meta_rows: Vec<(&str, &str)> = meta.into_iter().collect();

    let conn = Connection::open_with_flags(
        &path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    write::begin(&conn)?;
    write::insert_files(&conn, &file_rows)?;
    write::insert_symbols(&conn, &node_rows)?;
    write::insert_edges(&conn, &edge_rows)?;
    write::insert_occurrences(&conn, &occ_rows)?;
    write::insert_snippets(&conn, &[])?;
    write::insert_edge_types(&conn, &[])?;
    write::insert_meta(&conn, &meta_rows)?;
    write::rebuild_fts(&conn)?;
    write::finish(&conn)?;
    drop(conn);

    Ok(CorpusStats {
        path,
        commit,
        files: file_rows.len(),
        symbols,
        nodes: node_rows.len(),
        edges: edge_rows.len(),
        occurrences: occ_rows.len(),
        hub: l.syms[hub].r#ref.clone(),
        hub_in_degree,
        interface: l.syms[l.ifaces[top_iface]].r#ref.clone(),
        interface_implementers: top_impl,
        tested: tested_ref,
        tested_count: top_tests,
        members_path: l.paths[l.syms[hub].file].clone(),
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    fn sha256(path: &Path) -> String {
        format!("{:x}", Sha256::digest(std::fs::read(path).unwrap()))
    }

    /// Two generations from one seed are byte-identical; the shape holds at 2k symbols.
    #[test]
    fn corpus_is_deterministic() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let sa = generate(a.path(), 2_000, 0x5eed).unwrap();
        let sb = generate(b.path(), 2_000, 0x5eed).unwrap();
        assert_eq!(sa, sb.clone_with_path(sa.path.clone()));
        assert_eq!(sha256(&sa.path), sha256(&sb.path));
        assert_eq!(sa.symbols, 2_000);
        assert_eq!(sa.files, 100);
        assert_eq!(
            sa.nodes, 2_100,
            "the symbols plus one file node per file (CG7.3a)"
        );
        // 1.3 edges per symbol is the floor: at 2k symbols the structural edges (1,700), the
        // 120 TESTED_BY and the hub's 799 sources already pass 2,600, so the Zipf tail adds
        // nothing; at 100k the same shape leaves about 37k edges for the tail and the budget
        // is exact (`counts:edges` of the bench corpus)
        assert!(sa.edges >= 2_600, "{sa:?}");
        assert_eq!(sa.hub_in_degree, 799, "every other Function (800 only)");
        assert!(sa.interface_implementers >= 3, "{sa:?}");
        assert!(sa.tested_count >= 1, "{sa:?}");
        assert_eq!(sa.commit.len(), 40);
        let mode = std::fs::metadata(&sa.path).unwrap().permissions();
        assert_eq!(
            std::os::unix::fs::PermissionsExt::mode(&mode) & 0o777,
            0o600
        );
        // another seed is another file
        let c = tempfile::tempdir().unwrap();
        let sc = generate(c.path(), 2_000, 0x5eee).unwrap();
        assert_ne!(sc.commit, sa.commit);
        assert_ne!(sha256(&sc.path), sha256(&sa.path));
    }

    /// Every `bench:*` target of a generated corpus resolves through the reader the way the
    /// bench's selectors do (CG7.3a regression: the first corpus had no file node, so the
    /// `path` form was `unknown_symbol` and the 100k run stopped at `tool_members`).
    #[test]
    fn bench_targets_resolve() {
        use crate::reader::CodeGraphReader;
        use crate::r#ref::{check_repo_relative_path, parse_ref};
        use crate::sqlite::SqliteSnapshot;

        let dir = tempfile::tempdir().unwrap();
        let stats = generate(dir.path(), 2_000, 0x9e37_79b9_7f4a_7c15).unwrap();
        let snap = SqliteSnapshot::open_ro(&stats.path).unwrap();
        let meta = snap.meta().unwrap();
        let target = |k: &str| meta.get(k).cloned().unwrap_or_else(|| panic!("meta {k}"));
        assert_eq!(target("counts:symbols"), stats.nodes.to_string());

        // the `ref` form: `parse_ref` then `node(ref)`, each a non-external symbol row
        for (key, label) in [
            ("bench:hub", "Function"),
            ("bench:interface", "Interface"),
            ("bench:tested", "Function"),
        ] {
            let r#ref = target(key);
            parse_ref(&r#ref).unwrap_or_else(|e| panic!("{key}: {e:?}"));
            let row = snap
                .node(&r#ref)
                .unwrap()
                .unwrap_or_else(|| panic!("{key} {ref:?} has no row"));
            assert_eq!(row.kind, NodeKind::Symbol, "{key}");
            assert_eq!(row.label, label, "{key}");
            assert!(!row.external, "{key}");
        }
        // the `path` form: the repo-relative rule, then `node("file:<path>")` is the file node,
        // and `{path, line}` reaches the file's first symbol
        let path = target("bench:members_path");
        check_repo_relative_path(&path).unwrap();
        let file = snap
            .node(&format!("file:{path}"))
            .unwrap()
            .unwrap_or_else(|| panic!("no file node for {path:?}"));
        assert_eq!(file.kind, NodeKind::File);
        assert_eq!(file.label, "File");
        assert_eq!(file.name, "f00000.go");
        assert_eq!(file.path.as_deref(), Some(path.as_str()));
        assert_eq!(file.def, None);
        let at_line_0 = snap.by_path_line(&path, 0).unwrap();
        assert_eq!(at_line_0.len(), 1, "{at_line_0:?}");
        assert_eq!(at_line_0[0].label, "Interface");
        // the bench's `tool_members` target: the interface's two methods; a file node owns no
        // CONTAINS edge (stage1-spec: parent = the symbol minus its last descriptor)
        let iface = snap.node(&target("bench:interface")).unwrap().unwrap();
        assert_eq!(snap.members(iface.id, &[]).unwrap().len(), 2);
        assert_eq!(snap.members(file.id, &[]).unwrap().len(), 0);
        assert_eq!(
            snap.implementers(iface.id, false).unwrap().len(),
            target("bench:interface_implementers")
                .parse::<usize>()
                .unwrap()
        );
        // the file nodes take the first ids (`file:` < `scip:`) and never collide with a symbol
        assert_eq!(file.id, NodeId(1));
        assert_eq!(
            snap.node_by_id(NodeId(stats.files as i64 + 1))
                .unwrap()
                .unwrap()
                .kind,
            NodeKind::Symbol
        );
    }

    impl CorpusStats {
        fn clone_with_path(&self, path: PathBuf) -> Self {
            Self {
                path,
                ..self.clone()
            }
        }
    }
}
