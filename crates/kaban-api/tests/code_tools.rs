//! The typed `code_*` tools through `KabanApi` (stage1-spec G1-F12, G1-S2, G1-S8, §8.2;
//! stage1-plan CG7.1): the selector rule, the goldens over the go-drivers and ts-drivers
//! fixture snapshots (published here through `kaban-index` exactly as its own goldens are),
//! the list limits and the order-before-cut rule over synthetic snapshots, head resolution
//! against a scratch repository, and the content-gate and error-text boundaries (the `[red]`
//! slices). Self-contained: no shared harness.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

use std::collections::{BTreeMap, BTreeSet};
use std::fs::OpenOptions;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use kaban_api::control::{
    Direction, EDGE_TYPE_VALUES, MATCHED_BY_VALUES, REGISTRY, ROLE_VALUES, append_index,
    append_key, tool_schemas,
};
use kaban_api::{
    ApiConfig, ApiError, CodeDefinitionRequest, CodeImplementersRequest, CodeListResponse,
    CodeMembersRequest, CodeNeighboursRequest, CodeReferencesRequest, CodeReferencesResponse,
    CodeTestsForRequest, CodeTestsForResponse, FollowUp, GitRunner, KabanApi, SymbolSelector,
    TargetNote, TestTier,
};
use kaban_codegraph::testing::FixedSnapshot;
use kaban_codegraph::{
    EdgeType, FileId, HeadInput, IsolationTier, NoSnapshot, NodeId, NodeKind, Range, Resolved,
    SCHEMA_VERSION, SnapshotProvider, write,
};
use kaban_index::writer::{self, Commit, Meta, Provenance, Rows};
use kaban_index::{admit_index, decode_and_gate};
use kaban_policy::Policy;
use kaban_store_sqlite::SqliteStore;
use serde_json::Value;

/// A value shaped like an AWS access key id: hits the builtin credential rule
/// `aws-access-key-id` (always reject). Assembled at run time so the test source carries no
/// credential-shaped literal.
fn canary() -> String {
    format!("{}{}", "AKIA", "QWERTYUIOPASDFGH")
}

/// `<repo-id>` of every fixture snapshot.
const REPO_ID: &str = "0123456789abcdef0123456789abcdef";

/// The go-drivers fixture's indexed commit (`fixtures/codegraph/go-drivers`).
const GO_COMMIT: &str = "730da842221a844e8c2c82891dda9094f6428858";

/// The ts-drivers fixture's commit (`kaban-index/tests/goldens.rs` publishes it under `0xab`).
const TS_COMMIT: &str = "abababababababababababababababababababab";

/// Ref prefixes of the two Go packages and the TypeScript package (versioned, as emitted).
const GO: &str = "scip:scip-go gomod example.com/drivers 730da842221a844e8c2c82891dda9094f6428858 `example.com/drivers/drivers`/";
const GO_SQLITE: &str = "scip:scip-go gomod example.com/drivers 730da842221a844e8c2c82891dda9094f6428858 `example.com/drivers/sqlite`/";
const TS: &str = "scip:scip-typescript npm @example/drivers 1.0.0 src/";

/// Normalised key prefixes (version `.`).
const GO_KEY: &str = "scip-go gomod example.com/drivers . `example.com/drivers/drivers`/";
const TS_KEY: &str = "scip-typescript npm @example/drivers . src/";

// ---------------------------------------------------------------------------------------------
// scratch, repository, fixtures

/// A private 0700 scratch directory removed on drop (test-fixture-raii).
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let d = std::env::temp_dir().join(format!("kaban-api-code-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o700)).unwrap();
        Self(d)
    }

    /// `<scratch>/codegraph/<repo-id>`, 0700, created.
    fn codegraph_dir(&self) -> PathBuf {
        let cg = self.0.join("codegraph");
        let d = cg.join(REPO_ID);
        std::fs::create_dir_all(&d).unwrap();
        for p in [&cg, &d] {
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        d
    }

    /// `<scratch>/repo`: a git repository with one commit, so `rev-parse HEAD` answers.
    fn repo(&self) -> PathBuf {
        let root = self.0.join("repo");
        std::fs::create_dir_all(&root).unwrap();
        git(&root, &["init", "-q", "-b", "main"]);
        git(&root, &["config", "user.email", "fixture@example.invalid"]);
        git(&root, &["config", "user.name", "fixture"]);
        std::fs::write(root.join("README.md"), "# fixture\n").unwrap();
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-q", "-m", "initial"]);
        root
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn git(root: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_AUTHOR_DATE", "2026-10-26T08:16:02Z")
        .env("GIT_COMMITTER_DATE", "2026-10-26T08:16:02Z")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/codegraph")
}

fn fixture_bytes(rel: &str) -> Vec<u8> {
    let path = fixtures_dir().join(rel);
    std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// `examples/policy-test-canaries.yaml`: the §8.1 canaries as credential rules.
fn canary_policy() -> Policy {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/policy-test-canaries.yaml");
    Policy::load(&path).expect("the canary policy loads")
}

/// Decode, gate, admit, derive and publish `<dir>/<commit>.cg.db` from the index bytes, as
/// `kaban-index/tests/goldens.rs:24-45` does, with `policy_version` in `meta`.
fn publish(
    dir: &Path,
    commit: &str,
    a_bytes: &[u8],
    b_bytes: Option<&[u8]>,
    policy_version: &str,
) -> PathBuf {
    let policy = canary_policy();
    let a = decode_and_gate(a_bytes, &policy).expect("decode and gate A");
    let b = b_bytes.map(|b| decode_and_gate(b, &policy).expect("decode and gate B"));
    let admitted_a = admit_index(&a);
    let admitted_b = b.as_ref().map(|b| admit_index(b));
    let rows = Rows::derive(&admitted_a, admitted_b.as_ref(), &policy);
    let meta = Meta {
        commit: Commit::parse(commit).expect("40-hex commit"),
        repo_id: REPO_ID.to_owned(),
        policy_version: policy_version.to_owned(),
        provenance: Provenance {
            isolation: IsolationTier::None,
            isolation_detail: "test".to_owned(),
            resources: Vec::new(),
            container_image_digest: None,
        },
    };
    writer::publish(dir, &rows, &meta, &policy).expect("publish")
}

fn publish_go(dir: &Path, policy_version: &str) -> PathBuf {
    let a = fixture_bytes("go-drivers/index.scip");
    let b = fixture_bytes("go-drivers/index-notest.scip");
    publish(dir, GO_COMMIT, &a, Some(&b), policy_version)
}

fn publish_ts(dir: &Path, policy_version: &str) -> PathBuf {
    let a = fixture_bytes("ts-drivers/index.scip");
    publish(dir, TS_COMMIT, &a, None, policy_version)
}

// ---------------------------------------------------------------------------------------------
// a synthetic snapshot through the public write helpers

/// One symbol of a synthetic snapshot before ids are assigned.
struct Sym {
    kind: NodeKind,
    r#ref: String,
    scip: String,
    key: String,
    label: &'static str,
    name: String,
    path: String,
    def: Range,
    is_test: bool,
    /// An external placeholder: no defining file, no range, `UnspecifiedKind`.
    external: bool,
}

/// One reference site of a synthetic snapshot before ids are assigned: the referenced symbol,
/// the document, the range, the SCIP role bits and the enclosing symbol.
struct Occ {
    symbol: String,
    path: String,
    range: Range,
    roles: u32,
    enclosing: String,
}

/// A synthetic snapshot: rows by ref, ids assigned at publish (`write::*` refuses anything
/// else), edges by ref pair, occurrences by ref, snippets by ref, extra `meta` rows beside the
/// fixed six.
#[derive(Default)]
struct Synth {
    files: Vec<String>,
    symbols: Vec<Sym>,
    edges: Vec<(String, EdgeType, String)>,
    occurrences: Vec<Occ>,
    snippets: Vec<(String, String)>,
    meta: Vec<(String, String)>,
}

impl Synth {
    fn file(&mut self, path: &str) -> &mut Self {
        self.files.push(path.to_owned());
        self
    }

    /// The stored snippet of `symbol` (CG8.2; the fixture snapshots carry none).
    fn snippet(&mut self, symbol: &str, text: &str) -> &mut Self {
        self.snippets.push((symbol.to_owned(), text.to_owned()));
        self
    }

    /// One more `meta` row (the keys `kaban-index` writes beside the fixed six: `languages`,
    /// `indexer:<lang>`, `counts:*`, ...; CG7.3).
    fn meta(&mut self, key: &str, value: &str) -> &mut Self {
        self.meta.push((key.to_owned(), value.to_owned()));
        self
    }

    /// A non-external global symbol of the synthetic package defined in `path`.
    fn symbol(&mut self, descriptors: &str, label: &'static str, name: &str, path: &str) -> String {
        let scip =
            format!("scip-go gomod example.com/synth v1 `example.com/synth/pkg`/{descriptors}");
        let key =
            format!("scip-go gomod example.com/synth . `example.com/synth/pkg`/{descriptors}");
        let r#ref = format!("scip:{scip}");
        let line = u32::try_from(self.symbols.len()).unwrap();
        self.symbols.push(Sym {
            kind: NodeKind::Symbol,
            r#ref: r#ref.clone(),
            scip,
            key,
            label,
            name: name.to_owned(),
            path: path.to_owned(),
            def: Range {
                sl: line,
                sc: 0,
                el: line,
                ec: 4,
            },
            is_test: false,
            external: false,
        });
        r#ref
    }

    /// The file node of `path` (`file:<path>`; the `path` selector form resolves to it).
    fn file_node(&mut self, path: &str) -> String {
        let r#ref = format!("file:{path}");
        self.symbols.push(Sym {
            kind: NodeKind::File,
            r#ref: r#ref.clone(),
            scip: String::new(),
            key: String::new(),
            label: "File",
            name: path.rsplit('/').next().unwrap_or(path).to_owned(),
            path: path.to_owned(),
            def: Range {
                sl: 0,
                sc: 0,
                el: 0,
                ec: 0,
            },
            is_test: false,
            external: false,
        });
        r#ref
    }

    /// A document-local symbol `local <n>` of `path` as scip-go and rust-analyzer publish
    /// them (`ref = local:<path>:local <n>`, named by the id).
    fn local(&mut self, path: &str, n: u32) -> String {
        let scip = format!("local {n}");
        let r#ref = format!("local:{path}:{scip}");
        let line = u32::try_from(self.symbols.len()).unwrap();
        self.symbols.push(Sym {
            kind: NodeKind::Local,
            r#ref: r#ref.clone(),
            scip,
            key: String::new(),
            label: "Variable",
            name: n.to_string(),
            path: path.to_owned(),
            def: Range {
                sl: line,
                sc: 0,
                el: line,
                ec: 4,
            },
            is_test: false,
            external: false,
        });
        r#ref
    }

    /// An external placeholder row as `kaban-index` writes the endpoint of an edge into a
    /// dependency (R-3): the raw `scip` as given, label `UnspecifiedKind`, no file, no range.
    fn placeholder(&mut self, scip: &str, name: &str) -> String {
        let r#ref = format!("scip:{scip}");
        self.symbols.push(Sym {
            kind: NodeKind::Symbol,
            r#ref: r#ref.clone(),
            scip: scip.to_owned(),
            key: scip.to_owned(),
            label: "UnspecifiedKind",
            name: name.to_owned(),
            path: String::new(),
            def: Range {
                sl: 0,
                sc: 0,
                el: 0,
                ec: 0,
            },
            is_test: false,
            external: true,
        });
        r#ref
    }

    fn edge(&mut self, src: &str, r#type: EdgeType, dst: &str) -> &mut Self {
        self.edges.push((src.to_owned(), r#type, dst.to_owned()));
        self
    }

    /// A reference site of `symbol` at `(line, col)` in `path`, enclosed by `enclosing`.
    fn occurrence(
        &mut self,
        symbol: &str,
        path: &str,
        line: u32,
        col: u32,
        roles: u32,
        enclosing: &str,
    ) -> &mut Self {
        self.occurrences.push(Occ {
            symbol: symbol.to_owned(),
            path: path.to_owned(),
            range: Range {
                sl: line,
                sc: col,
                el: line,
                ec: col + 3,
            },
            roles,
            enclosing: enclosing.to_owned(),
        });
        self
    }

    /// Write `<dir>/<commit>.cg.db` (0600) with `schema_version` and `policy_version` in meta.
    fn publish(
        &self,
        dir: &Path,
        commit: &str,
        schema_version: u32,
        policy_version: &str,
    ) -> PathBuf {
        let path = dir.join(format!("{commit}.cg.db"));
        drop(
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
                .unwrap(),
        );
        let mut files = self.files.clone();
        files.sort();
        let file_rows: Vec<write::FileRow> = files
            .iter()
            .enumerate()
            .map(|(i, p)| write::FileRow {
                id: FileId(i64::try_from(i + 1).unwrap()),
                path: p.clone(),
                language: "go".to_owned(),
                is_test: false,
                lines: 100,
            })
            .collect();
        let file_id = |p: &str| file_rows.iter().find(|f| f.path == p).unwrap().id;
        let mut order: Vec<&Sym> = self.symbols.iter().collect();
        order.sort_by(|a, b| a.r#ref.cmp(&b.r#ref));
        let id_of = |r: &str| {
            NodeId(i64::try_from(order.iter().position(|s| s.r#ref == r).unwrap() + 1).unwrap())
        };
        let symbol_rows: Vec<write::SymbolRow> = order
            .iter()
            .map(|s| write::SymbolRow {
                id: id_of(&s.r#ref),
                kind: s.kind,
                r#ref: s.r#ref.clone(),
                scip: (s.kind != NodeKind::File).then(|| s.scip.clone()),
                key: (s.kind == NodeKind::Symbol).then(|| s.key.clone()),
                label: s.label.to_owned(),
                name: s.name.clone(),
                file_id: (!s.external).then(|| file_id(&s.path)),
                def: (s.kind != NodeKind::File && !s.external).then_some(s.def),
                enc: None,
                signature: None,
                doc: None,
                external: s.external,
                is_test: s.is_test,
                ident_terms: String::new(),
            })
            .collect();
        let mut edge_rows: Vec<write::EdgeRow> = self
            .edges
            .iter()
            .map(|(src, t, dst)| write::EdgeRow {
                src: id_of(src),
                dst: id_of(dst),
                r#type: *t,
                weight: 1,
            })
            .collect();
        edge_rows.sort_by_key(|e| (e.src, e.r#type, e.dst));
        let mut occ_rows: Vec<write::OccurrenceRow> = self
            .occurrences
            .iter()
            .map(|o| write::OccurrenceRow {
                symbol_id: id_of(&o.symbol),
                file_id: file_id(&o.path),
                range: o.range,
                roles: o.roles,
                enclosing_id: Some(id_of(&o.enclosing)),
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

        let conn = rusqlite::Connection::open(&path).unwrap();
        write::begin(&conn).unwrap();
        write::insert_files(&conn, &file_rows).unwrap();
        write::insert_symbols(&conn, &symbol_rows).unwrap();
        write::insert_edges(&conn, &edge_rows).unwrap();
        write::insert_occurrences(&conn, &occ_rows).unwrap();
        let mut snippet_rows: Vec<write::SnippetRow> = self
            .snippets
            .iter()
            .map(|(symbol, text)| write::SnippetRow {
                symbol_id: id_of(symbol),
                text: text.clone(),
            })
            .collect();
        snippet_rows.sort_by_key(|r| r.symbol_id);
        write::insert_snippets(&conn, &snippet_rows).unwrap();
        write::insert_edge_types(&conn, &[]).unwrap();
        let schema = schema_version.to_string();
        // the fixed six plus the extras, by key (`insert_meta` refuses an unsorted list)
        let mut meta: BTreeMap<&str, &str> = BTreeMap::from([
            ("commit", commit),
            ("isolation", "none"),
            ("policy_version", policy_version),
            ("repo_id", REPO_ID),
            ("schema_version", schema.as_str()),
            ("symbols_fts_version", "1"),
        ]);
        for (k, v) in &self.meta {
            meta.insert(k.as_str(), v.as_str());
        }
        let meta_rows: Vec<(&str, &str)> = meta.into_iter().collect();
        write::insert_meta(&conn, &meta_rows).unwrap();
        write::rebuild_fts(&conn).unwrap();
        write::finish(&conn).unwrap();
        path
    }
}

/// `Iface#` with `n` implementers `Impl<i>#`, `Impl<i>` defined in `p<n-1-i>.go` so that the
/// `(path, ref)` order differs from the ref order.
fn implementers_snapshot(n: usize) -> Synth {
    let mut s = Synth::default();
    s.file("iface.go");
    let iface = s.symbol("Iface#", "Interface", "Iface", "iface.go");
    for i in 0..n {
        let path = format!("p{:03}.go", n - 1 - i);
        s.file(&path);
        let imp = s.symbol(
            &format!("Impl{i:03}#"),
            "Struct",
            &format!("Impl{i:03}"),
            &path,
        );
        s.edge(&imp, EdgeType::Implements, &iface);
    }
    s
}

// ---------------------------------------------------------------------------------------------
// the service

/// A git runner with a test-sized deadline (the default 200 ms is for serving, not for a
/// loaded test box).
fn patient_git() -> GitRunner {
    GitRunner {
        program: PathBuf::from("git"),
        timeout: Duration::from_secs(10),
    }
}

/// A service bound to `root` under the builtin policy (so the AWS canary rejects), without a
/// provider.
fn api_without_provider(s: &Scratch, root: &Path) -> KabanApi {
    let policy = Policy::from_file(Policy::parse(b"").unwrap(), "test".into()).unwrap();
    api_under_policy(s, root, policy)
}

/// A service bound to `root` under `policy`, without a provider.
fn api_under_policy(s: &Scratch, root: &Path, policy: Policy) -> KabanApi {
    let store = Arc::new(SqliteStore::open(&s.0.join("kaban.db"), &s.0.join("keys")).unwrap());
    KabanApi::with_config(
        store.clone(),
        policy,
        store.signer(),
        None,
        ApiConfig {
            repo_root: Some(root.to_path_buf()),
            repo: Some("fixture".to_owned()),
            ..Default::default()
        },
    )
    .with_git(patient_git())
}

/// A service over the go-drivers snapshot (published under the live policy version, so
/// `policy: current`).
fn go_api(s: &Scratch) -> KabanApi {
    let root = s.repo();
    let api = api_without_provider(s, &root);
    let dir = s.codegraph_dir();
    let path = publish_go(&dir, &api.policy_version());
    api.with_snapshot_provider(dir, Arc::new(FixedSnapshot::new(path)))
}

/// A service over the ts-drivers snapshot (published under another policy version, so
/// `policy: stale`).
fn ts_api(s: &Scratch) -> KabanApi {
    let root = s.repo();
    let api = api_without_provider(s, &root);
    let dir = s.codegraph_dir();
    let path = publish_ts(&dir, "pv-other");
    api.with_snapshot_provider(dir, Arc::new(FixedSnapshot::new(path)))
}

/// A service over a synthetic snapshot.
fn synth_api(s: &Scratch, synth: &Synth) -> KabanApi {
    let root = s.repo();
    let api = api_without_provider(s, &root);
    let dir = s.codegraph_dir();
    let path = synth.publish(&dir, &"5".repeat(40), SCHEMA_VERSION, "pv-synth");
    api.with_snapshot_provider(dir, Arc::new(FixedSnapshot::new(path)))
}

fn by_symbol(key: &str) -> SymbolSelector {
    SymbolSelector {
        symbol: Some(key.to_owned()),
        ..Default::default()
    }
}

fn by_ref(r: &str) -> SymbolSelector {
    SymbolSelector {
        r#ref: Some(r.to_owned()),
        ..Default::default()
    }
}

fn by_label_name(label: &str, name: &str) -> SymbolSelector {
    SymbolSelector {
        label: Some(label.to_owned()),
        name: Some(name.to_owned()),
        ..Default::default()
    }
}

fn by_path(path: &str) -> SymbolSelector {
    SymbolSelector {
        path: Some(path.to_owned()),
        ..Default::default()
    }
}

fn implementers(target: SymbolSelector) -> CodeImplementersRequest {
    CodeImplementersRequest {
        target,
        ..Default::default()
    }
}

fn members(target: SymbolSelector, labels: &[&str]) -> CodeMembersRequest {
    CodeMembersRequest {
        target,
        labels: labels.iter().map(|l| (*l).to_owned()).collect(),
        ..Default::default()
    }
}

fn definition(target: SymbolSelector) -> CodeDefinitionRequest {
    CodeDefinitionRequest {
        target,
        current_path: None,
    }
}

fn references(target: SymbolSelector) -> CodeReferencesRequest {
    CodeReferencesRequest {
        target,
        ..Default::default()
    }
}

fn tests_for(target: SymbolSelector) -> CodeTestsForRequest {
    CodeTestsForRequest {
        target,
        ..Default::default()
    }
}

fn neighbours(target: SymbolSelector, depth: u8, edge_types: &[&str]) -> CodeNeighboursRequest {
    CodeNeighboursRequest {
        target,
        depth,
        edge_types: edge_types.iter().map(|t| (*t).to_owned()).collect(),
        ..Default::default()
    }
}

fn refs(r: &CodeListResponse) -> Vec<String> {
    r.results.iter().map(|c| c.r#ref.clone()).collect()
}

/// The enclosing refs of a references response, in order.
fn group_refs(r: &CodeReferencesResponse) -> Vec<String> {
    r.by_enclosing
        .iter()
        .map(|g| g.enclosing.r#ref.clone())
        .collect()
}

/// `(test ref, matched_by, heuristic)` of every link, in order.
fn link_rows(r: &CodeTestsForResponse) -> Vec<(String, String, bool)> {
    r.tests
        .iter()
        .map(|l| (l.test.r#ref.clone(), l.matched_by.clone(), l.heuristic))
        .collect()
}

fn range4(r: Option<kaban_api::CodeRange>) -> Option<(u32, u32, u32, u32)> {
    r.map(|r| (r.start_line, r.start_col, r.end_line, r.end_col))
}

/// The `Invalid` text of an error, which must start with `class:`.
fn class_text(e: ApiError, class: &str) -> String {
    match e {
        ApiError::Invalid(text) => {
            assert!(
                text.starts_with(&format!("{class}:")),
                "expected class {class}, got {text}"
            );
            text
        }
        other => panic!("expected Invalid({class}), got {other:?}"),
    }
}

// ---------------------------------------------------------------------------------------------
// task 1: the selector, limits, ordering, head resolution, the boundaries

#[test]
fn selector_one_of_rules() {
    let s = Scratch::new("one-of");
    let api = go_api(&s);
    let sym = format!("{GO_KEY}PostgresDriver#");
    let r#ref = format!("{GO}PostgresDriver#");
    let invalid: Vec<(&str, SymbolSelector)> = vec![
        ("empty", SymbolSelector::default()),
        (
            "symbol+ref",
            SymbolSelector {
                symbol: Some(sym.clone()),
                r#ref: Some(r#ref.clone()),
                ..Default::default()
            },
        ),
        (
            "symbol+name",
            SymbolSelector {
                symbol: Some(sym.clone()),
                name: Some("PostgresDriver".into()),
                ..Default::default()
            },
        ),
        (
            "symbol+path",
            SymbolSelector {
                symbol: Some(sym.clone()),
                path: Some("drivers/postgres.go".into()),
                ..Default::default()
            },
        ),
        (
            "ref+label",
            SymbolSelector {
                r#ref: Some(r#ref.clone()),
                label: Some("Struct".into()),
                ..Default::default()
            },
        ),
        (
            "label alone",
            SymbolSelector {
                label: Some("Struct".into()),
                ..Default::default()
            },
        ),
        (
            "name alone",
            SymbolSelector {
                name: Some("PostgresDriver".into()),
                ..Default::default()
            },
        ),
        (
            "line alone",
            SymbolSelector {
                line: Some(3),
                ..Default::default()
            },
        ),
        (
            "label+name+path+line",
            SymbolSelector {
                label: Some("Struct".into()),
                name: Some("PostgresDriver".into()),
                path: Some("drivers/postgres.go".into()),
                line: Some(3),
                ..Default::default()
            },
        ),
        (
            "path+line+symbol",
            SymbolSelector {
                symbol: Some(sym.clone()),
                path: Some("drivers/postgres.go".into()),
                line: Some(3),
                ..Default::default()
            },
        ),
        (
            "label+line",
            SymbolSelector {
                label: Some("Struct".into()),
                line: Some(3),
                ..Default::default()
            },
        ),
        // a label outside the closed set
        (
            "unknown label",
            by_label_name("Interfacee", "DatabaseDriver"),
        ),
        // the path rule: a parent component
        ("path with ..", by_path("../drivers/postgres.go")),
        (
            "path+line with leading slash",
            SymbolSelector {
                path: Some("/drivers/postgres.go".into()),
                line: Some(3),
                ..Default::default()
            },
        ),
    ];
    for (what, target) in invalid {
        let invalid = |e: ApiError| {
            let text = e.to_string();
            assert!(text.starts_with("selector_invalid:"), "{what}: {text}");
        };
        invalid(api.code_members(members(target.clone(), &[])).unwrap_err());
        invalid(api.code_definition(definition(target.clone())).unwrap_err());
        invalid(api.code_implementers(implementers(target)).unwrap_err());
    }
    // the bare `path` form is a file target: allowed by members and definition, refused by
    // implementers (a file implements nothing)
    let file = by_path("drivers/postgres.go");
    let def = api.code_definition(definition(file.clone())).unwrap();
    assert_eq!(def.definition.kind, "file");
    assert_eq!(def.definition.r#ref, "file:drivers/postgres.go");
    assert_eq!(def.definition.symbol, None, "a file node has no key");
    assert!(api.code_members(members(file.clone(), &[])).is_ok());
    let e = api.code_implementers(implementers(file)).unwrap_err();
    class_text(e, "selector_invalid");
    // `{label, name, path}` is the name form narrowed to one file (Q-2), and its path obeys
    // the repository-relative rule
    let narrowed = SymbolSelector {
        path: Some("drivers/postgres.go".into()),
        ..by_label_name("Struct", "PostgresDriver")
    };
    assert_eq!(
        api.code_definition(definition(narrowed))
            .unwrap()
            .definition
            .r#ref,
        r#ref
    );
    let e = api
        .code_definition(definition(SymbolSelector {
            path: Some("../drivers/postgres.go".into()),
            ..by_label_name("Struct", "PostgresDriver")
        }))
        .unwrap_err();
    class_text(e, "selector_invalid");
    // every valid form resolves
    assert_eq!(
        api.code_definition(definition(by_symbol(&sym)))
            .unwrap()
            .definition
            .r#ref,
        r#ref
    );
    assert_eq!(
        api.code_definition(definition(by_ref(&r#ref)))
            .unwrap()
            .definition
            .name,
        "PostgresDriver"
    );
    assert_eq!(
        api.code_definition(definition(by_label_name("Struct", "PostgresDriver")))
            .unwrap()
            .definition
            .r#ref,
        r#ref
    );
    // `{path, line}`: line 3 of postgres.go defines `PostgresDriver` (def 3:5-3:19)
    let at = api
        .code_definition(definition(SymbolSelector {
            path: Some("drivers/postgres.go".into()),
            line: Some(3),
            ..Default::default()
        }))
        .unwrap();
    assert_eq!(at.definition.r#ref, r#ref);
    // exact, case-sensitive names: no match is `unknown_symbol`
    let e = api
        .code_definition(definition(by_label_name("Struct", "postgresdriver")))
        .unwrap_err();
    class_text(e, "unknown_symbol");
}

/// CG8.2 (G1-F17): a list tool omits the stored snippet by default and returns it with
/// `include_snippets: true`; `code_definition` returns it always; a symbol without a stored
/// snippet stays `None` under the flag.
#[test]
fn include_snippets_flag() {
    let mut synth = Synth::default();
    synth.file("pg.go");
    let pg = synth.symbol("Pg#", "Struct", "Pg", "pg.go");
    let open = synth.symbol("Pg#Open().", "Method", "Open", "pg.go");
    let close = synth.symbol("Pg#Close().", "Method", "Close", "pg.go");
    synth
        .edge(&pg, EdgeType::Contains, &open)
        .edge(&pg, EdgeType::Contains, &close)
        .snippet(
            &open,
            "func (p *Pg) Open(dsn string) (Conn, error) {\n\treturn nil, nil\n}",
        );
    let s = Scratch::new("include-snippets");
    let api = synth_api(&s, &synth);
    let card_of = |r: &CodeListResponse, name: &str| {
        r.results
            .iter()
            .find(|c| c.name == name)
            .cloned()
            .unwrap_or_else(|| panic!("{name} listed: {r:?}"))
    };

    // negative: the default omits every snippet
    let plain = api
        .code_members(members(by_label_name("Struct", "Pg"), &[]))
        .unwrap();
    assert_eq!(plain.count, 2);
    assert_eq!(card_of(&plain, "Open").snippet, None, "omitted by default");
    assert_eq!(card_of(&plain, "Close").snippet, None);

    // positive: the flag returns the stored text; a symbol without one stays `None`
    let with = api
        .code_members(CodeMembersRequest {
            include_snippets: true,
            ..members(by_label_name("Struct", "Pg"), &[])
        })
        .unwrap();
    assert_eq!(
        card_of(&with, "Open").snippet.as_deref(),
        Some("func (p *Pg) Open(dsn string) (Conn, error) {\n\treturn nil, nil\n}")
    );
    assert_eq!(card_of(&with, "Close").snippet, None, "no stored snippet");

    // `code_definition` returns it without a flag
    let def = api
        .code_definition(definition(by_label_name("Method", "Open")))
        .unwrap();
    assert_eq!(
        def.definition.snippet.as_deref(),
        Some("func (p *Pg) Open(dsn string) (Conn, error) {\n\treturn nil, nil\n}")
    );
    let none = api
        .code_definition(definition(by_label_name("Method", "Close")))
        .unwrap();
    assert_eq!(none.definition.snippet, None);
}

#[test]
fn selector_positional_returns_all_name_refuses_several() {
    // two `Open().` methods in two types, each with one parameter: `{label: Method, name:
    // Open}` matches both, which every tool refuses as `ambiguous_symbol` (Q-2, backlog 16,
    // 27; before, a list tool merged their results); `{path, line}` keeps the G1-F12 union:
    // the nodes defined on one line are listed together
    let mut synth = Synth::default();
    synth.file("mem.go").file("pg.go");
    let mem = synth.symbol("Mem#", "Struct", "Mem", "mem.go");
    let mem_open = synth.symbol("Mem#Open().", "Method", "Open", "mem.go");
    let mem_dsn = synth.symbol("Mem#Open().(dsn)", "Parameter", "dsn", "mem.go");
    let pg = synth.symbol("Pg#", "Struct", "Pg", "pg.go");
    let pg_open = synth.symbol("Pg#Open().", "Method", "Open", "pg.go");
    let pg_dsn = synth.symbol("Pg#Open().(dsn)", "Parameter", "dsn", "pg.go");
    synth
        .edge(&mem, EdgeType::Contains, &mem_open)
        .edge(&mem_open, EdgeType::Contains, &mem_dsn)
        .edge(&pg, EdgeType::Contains, &pg_open)
        .edge(&pg_open, EdgeType::Contains, &pg_dsn);
    let s = Scratch::new("returns-all");
    let api = synth_api(&s, &synth);

    let e = api
        .code_members(members(by_label_name("Method", "Open"), &[]))
        .unwrap_err();
    let text = class_text(e, "ambiguous_symbol");
    assert!(
        text.contains(&mem_open) && text.contains(&pg_open),
        "{text}"
    );
    let e = api
        .code_definition(definition(by_label_name("Method", "Open")))
        .unwrap_err();
    class_text(e, "ambiguous_symbol");
    // one match through the same form is one definition
    let one = api
        .code_definition(definition(by_label_name("Struct", "Pg")))
        .unwrap();
    assert_eq!(one.definition.r#ref, pg);
    // the positional form is untouched: the node defined on line 4 of pg.go is `Pg#Open().`
    // (the synthetic rows sit one per line in insertion order)
    let line4 = SymbolSelector {
        path: Some("pg.go".into()),
        line: Some(4),
        ..Default::default()
    };
    assert_eq!(
        refs(&api.code_members(members(line4.clone(), &[])).unwrap()),
        vec![pg_dsn]
    );
    assert_eq!(
        api.code_definition(definition(line4))
            .unwrap()
            .definition
            .r#ref,
        pg_open
    );
}

/// Q-2 finding 1 (backlog 16, 27): a `{label, name}` selector matching several symbols is
/// `ambiguous_symbol` with the candidates (label, name, path, first line, ref) on every tool,
/// never a merged list; `path` beside `{label, name}` picks one of them.
#[test]
fn name_selector_ambiguity_lists_candidates_never_merges() {
    let mut synth = Synth::default();
    synth.file("writer.go").file("idem.go");
    let rows_w = synth.symbol("writer/Rows#", "Struct", "Rows", "writer.go");
    let edges_w = synth.symbol("writer/Rows#edges.", "Field", "edges", "writer.go");
    let rows_i = synth.symbol("idem/Rows#", "Struct", "Rows", "idem.go");
    let audit_i = synth.symbol("idem/Rows#audit.", "Field", "audit", "idem.go");
    synth
        .edge(&rows_w, EdgeType::Contains, &edges_w)
        .edge(&rows_i, EdgeType::Contains, &audit_i);
    let s = Scratch::new("ambiguity-candidates");
    let api = synth_api(&s, &synth);

    // a list tool refuses instead of merging the two structs' members
    let e = api
        .code_members(members(by_label_name("Struct", "Rows"), &[]))
        .unwrap_err();
    let text = class_text(e, "ambiguous_symbol");
    let line = text
        .lines()
        .find_map(|l| l.strip_prefix("candidates: "))
        .unwrap_or_else(|| panic!("no candidates line: {text}"));
    let candidates: Vec<Value> = serde_json::from_str(line).unwrap();
    let got: Vec<(String, String, String, u64)> = candidates
        .iter()
        .map(|c| {
            (
                c["label"].as_str().unwrap().to_owned(),
                c["path"].as_str().unwrap().to_owned(),
                c["ref"].as_str().unwrap().to_owned(),
                c["line"].as_u64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        got,
        vec![
            ("Struct".to_owned(), "idem.go".to_owned(), rows_i.clone(), 2),
            (
                "Struct".to_owned(),
                "writer.go".to_owned(),
                rows_w.clone(),
                0
            ),
        ],
        "{text}"
    );
    // the same refusal from the one-node tools and the other list tools
    let e = api
        .code_definition(definition(by_label_name("Struct", "Rows")))
        .unwrap_err();
    assert!(class_text(e, "ambiguous_symbol").contains(&rows_w));
    let e = api
        .code_references(references(by_label_name("Struct", "Rows")))
        .unwrap_err();
    assert!(class_text(e, "ambiguous_symbol").contains(&rows_i));

    // `path` beside `{label, name}` disambiguates
    let in_idem = SymbolSelector {
        path: Some("idem.go".into()),
        ..by_label_name("Struct", "Rows")
    };
    let one = api.code_members(members(in_idem.clone(), &[])).unwrap();
    assert_eq!(refs(&one), vec![audit_i]);
    assert_eq!(
        api.code_definition(definition(in_idem))
            .unwrap()
            .definition
            .r#ref,
        rows_i
    );
    // a path that defines no symbol of that name is `unknown_symbol`
    let elsewhere = SymbolSelector {
        path: Some("other.go".into()),
        ..by_label_name("Struct", "Rows")
    };
    class_text(
        api.code_definition(definition(elsewhere)).unwrap_err(),
        "unknown_symbol",
    );
}

/// Q-2 finding 2 (backlog 21; HD-1, R-3): a wrong label is not a dead end. A name that exists
/// under exactly one other label answers that node with `target_note.label_corrected`; a name
/// under several other labels is `unknown_symbol` with `did_you_mean` (up to 5: label, ref,
/// path); an exact `(label, name)` match answers as before, without a note.
#[test]
fn wrong_label_corrects_or_suggests() {
    let mut synth = Synth::default();
    synth
        .file("rust.rs")
        .file("runner.rs")
        .file("union.rs")
        .file("many.rs");
    let admit = synth.symbol(
        "admit_toolchain().",
        "Function",
        "admit_toolchain",
        "rust.rs",
    );
    let runner = synth.symbol("Runner#", "Struct", "Runner", "runner.rs");
    let run = synth.symbol("Runner#run().", "Method", "run", "runner.rs");
    let alias = synth.symbol("ImplementsSet#", "TypeAlias", "ImplementsSet", "union.rs");
    // `card` under two other labels; `seven` under seven
    synth.symbol("card().", "Function", "card", "many.rs");
    synth.symbol("Deck#card().", "Method", "card", "many.rs");
    for (i, label) in [
        "Function", "Method", "Field", "Constant", "Variable", "Module", "Enum",
    ]
    .iter()
    .enumerate()
    {
        synth.symbol(&format!("S{i}#seven."), label, "seven", "many.rs");
    }
    synth.edge(&runner, EdgeType::Contains, &run);
    let s = Scratch::new("wrong-label");
    let api = synth_api(&s, &synth);

    // an exact match: unchanged, no note
    let r = api
        .code_definition(definition(by_label_name("Function", "admit_toolchain")))
        .unwrap();
    assert_eq!(
        (r.definition.r#ref.as_str(), r.target_note),
        (admit.as_str(), None)
    );

    // `Function` for a method: the method answers, with the correction, on every tool shape
    let r = api
        .code_definition(definition(by_label_name("Function", "run")))
        .unwrap();
    assert_eq!(r.definition.r#ref, run);
    assert_eq!(
        r.target_note,
        Some(TargetNote {
            r#ref: run.clone(),
            label_corrected: Some("Method".into()),
            external: false,
        })
    );
    let r = api
        .code_references(references(by_label_name("Function", "run")))
        .unwrap();
    assert_eq!(
        r.target_note.and_then(|n| n.label_corrected).as_deref(),
        Some("Method")
    );
    let r = api
        .code_members(members(by_label_name("Trait", "Runner"), &[]))
        .unwrap();
    assert_eq!(refs(&r), vec![run.clone()]);
    assert_eq!(
        r.target_note.and_then(|n| n.label_corrected).as_deref(),
        Some("Struct")
    );
    // `Struct` for a type alias
    let r = api
        .code_definition(definition(by_label_name("Struct", "ImplementsSet")))
        .unwrap();
    assert_eq!(r.definition.r#ref, alias);
    assert_eq!(
        r.target_note.and_then(|n| n.label_corrected).as_deref(),
        Some("TypeAlias")
    );

    // several other labels: `unknown_symbol` with the alternatives
    let text = class_text(
        api.code_definition(definition(by_label_name("Struct", "card")))
            .unwrap_err(),
        "unknown_symbol",
    );
    let line = text
        .lines()
        .find_map(|l| l.strip_prefix("did_you_mean: "))
        .unwrap_or_else(|| panic!("no did_you_mean line: {text}"));
    let hints: Vec<Value> = serde_json::from_str(line).unwrap();
    let labels: Vec<&str> = hints.iter().map(|h| h["label"].as_str().unwrap()).collect();
    // the reader's `(path, ref)` order: `Deck#card().` sorts before `card().`
    assert_eq!(labels, vec!["Method", "Function"], "{text}");
    assert!(
        hints
            .iter()
            .all(|h| h["path"] == "many.rs" && h["ref"].is_string())
    );
    // bounded to 5
    let text = class_text(
        api.code_members(members(by_label_name("Struct", "seven"), &[]))
            .unwrap_err(),
        "unknown_symbol",
    );
    let line = text
        .lines()
        .find_map(|l| l.strip_prefix("did_you_mean: "))
        .unwrap_or_else(|| panic!("no did_you_mean line: {text}"));
    let hints: Vec<Value> = serde_json::from_str(line).unwrap();
    assert_eq!(hints.len(), 5, "{text}");
    // a name nobody has: the bare sentence
    let text = class_text(
        api.code_definition(definition(by_label_name("Struct", "nobody")))
            .unwrap_err(),
        "unknown_symbol",
    );
    assert_eq!(text.lines().count(), 1, "{text}");
}

/// Q-2 finding 3 (R-3 consumer request 2): an external trait is reachable by name. The
/// placeholder row of a dependency carries `UnspecifiedKind`, so `{Trait, Serialize}` used to
/// be `unknown_symbol` although the `IMPLEMENTS` edges exist; now the placeholder whose
/// descriptor fits the asked label answers, marked `external` in `target_note`, and the
/// derive macro `Serialize:` or the `[Serialize]` type parameter of an impl never stand in.
#[test]
fn external_trait_answers_by_name() {
    let mut synth = Synth::default();
    synth.file("a.rs").file("b.rs");
    let a = synth.symbol("a/A#", "Struct", "A", "a.rs");
    let b = synth.symbol("b/B#", "Struct", "B", "b.rs");
    let local_enum = synth.symbol("b/Serialize#", "Enum", "Serialize", "b.rs");
    let serde = synth.placeholder(
        "rust-analyzer cargo serde_core 1.0.229 ser/Serialize#",
        "Serialize",
    );
    let derive = synth.placeholder(
        "rust-analyzer cargo serde_derive 1.0.229 Serialize:",
        "Serialize",
    );
    synth.placeholder(
        "scip-go gomod example.com/synth v1 `example.com/synth/pkg`/a/impl#[A][Serialize]",
        "Serialize",
    );
    let clap = synth.placeholder(
        "rust-analyzer cargo clap_builder 4.6.7 derive/Parser#",
        "Parser",
    );
    let pulldown = synth.placeholder(
        "rust-analyzer cargo pulldown-cmark 0.13.4 parse/Parser#",
        "Parser",
    );
    synth
        .edge(&a, EdgeType::Implements, &serde)
        .edge(&b, EdgeType::Implements, &serde);
    let s = Scratch::new("external-by-name");
    let api = synth_api(&s, &synth);

    // by name as by ref, and the answer says the target is external
    let by_ref = api.code_implementers(implementers(by_ref(&serde))).unwrap();
    assert_eq!(refs(&by_ref), vec![a.clone(), b.clone()]);
    let by_name = api
        .code_implementers(implementers(by_label_name("Trait", "Serialize")))
        .unwrap();
    assert_eq!(refs(&by_name), refs(&by_ref));
    assert_eq!(
        by_name.target_note,
        Some(TargetNote {
            r#ref: serde.clone(),
            label_corrected: None,
            external: true,
        })
    );
    // the exact local kind still wins its own label
    let r = api
        .code_definition(definition(by_label_name("Enum", "Serialize")))
        .unwrap();
    assert_eq!((r.definition.r#ref, r.target_note), (local_enum, None));
    // the derive macro is the `Macro` reading of the name
    let r = api
        .code_definition(definition(by_label_name("Macro", "Serialize")))
        .unwrap();
    assert_eq!(r.definition.r#ref, derive);
    assert!(r.target_note.is_some_and(|n| n.external));
    // two placeholders fitting the label: ambiguous, both offered
    let text = class_text(
        api.code_implementers(implementers(by_label_name("Trait", "Parser")))
            .unwrap_err(),
        "ambiguous_symbol",
    );
    assert!(text.contains(&clap) && text.contains(&pulldown), "{text}");
}

#[test]
fn limits_defaults_and_maxima() {
    let synth = implementers_snapshot(250);
    let s = Scratch::new("limits");
    let api = synth_api(&s, &synth);
    let iface = by_label_name("Interface", "Iface");
    let call = |limit: Option<usize>| {
        api.code_implementers(CodeImplementersRequest {
            target: iface.clone(),
            limit,
            ..Default::default()
        })
        .unwrap()
    };
    // no limit and limit 0 are the default 50
    let r = call(None);
    assert_eq!((r.count, r.results.len(), r.truncated), (50, 50, true));
    let r = call(Some(0));
    assert_eq!((r.count, r.results.len(), r.truncated), (50, 50, true));
    // above the maximum is the maximum
    let r = call(Some(201));
    assert_eq!((r.count, r.results.len(), r.truncated), (200, 200, true));
    let r = call(Some(10_000));
    assert_eq!(r.count, 200);
    // the maximum itself
    let r = call(Some(200));
    assert_eq!((r.count, r.truncated), (200, true));
    // a limit equal to the set size is still above the maximum: 200 rows, truncated
    let r = call(Some(250));
    assert_eq!((r.count, r.truncated), (200, true));
    assert_eq!(r.ranking, "full");
    assert_eq!(r.follow_ups, Vec::<FollowUp>::new());
}

#[test]
fn sorted_before_truncation() {
    let synth = implementers_snapshot(60);
    let s = Scratch::new("sorted");
    let api = synth_api(&s, &synth);
    let r = api
        .code_implementers(CodeImplementersRequest {
            target: by_label_name("Interface", "Iface"),
            limit: Some(5),
            ..Default::default()
        })
        .unwrap();
    assert!(r.truncated);
    assert_eq!(r.count, 5);
    // the complete set is ordered by (path, ref) first: p000.go holds Impl059, p001.go
    // Impl058, ... so the five lexically first paths come back, not the five first refs
    let paths: Vec<&str> = r
        .results
        .iter()
        .map(|c| c.path.as_deref().unwrap())
        .collect();
    assert_eq!(
        paths,
        ["p000.go", "p001.go", "p002.go", "p003.go", "p004.go"]
    );
    let names: Vec<&str> = r.results.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(
        names,
        ["Impl059", "Impl058", "Impl057", "Impl056", "Impl055"]
    );
    // the full set, the same leading order
    let all = api
        .code_implementers(implementers(by_label_name("Interface", "Iface")))
        .unwrap();
    assert_eq!(all.count, 50);
    assert_eq!(&refs(&all)[..5], &refs(&r)[..]);
    // a limit at or above the set (and within the maximum) is not truncated
    let fits = api
        .code_implementers(CodeImplementersRequest {
            target: by_label_name("Interface", "Iface"),
            limit: Some(60),
            ..Default::default()
        })
        .unwrap();
    assert_eq!((fits.count, fits.truncated), (60, false));
    // CG8.1, ranking active: every file is a root file, so the caller's own file (p059.go,
    // Impl000) is at distance 0 and every other at 1; the complete set is ranked by
    // (distance, path, ref) before the cut, so Impl000 leads the first page and the (path, ref)
    // order follows
    let near = api
        .code_implementers(CodeImplementersRequest {
            target: by_label_name("Interface", "Iface"),
            limit: Some(5),
            current_path: Some("p059.go".to_owned()),
            ..Default::default()
        })
        .unwrap();
    assert!(near.truncated);
    assert_eq!(near.ranking, "full");
    let ranked: Vec<(&str, u32)> = near
        .results
        .iter()
        .map(|c| (c.name.as_str(), c.distance))
        .collect();
    assert_eq!(
        ranked,
        [
            ("Impl000", 0),
            ("Impl059", 1),
            ("Impl058", 1),
            ("Impl057", 1),
            ("Impl056", 1)
        ]
    );
}

#[test]
fn resolve_current_caches_ancestors_by_head_sha() {
    let s = Scratch::new("head-cache");
    let root = s.repo();
    // a git wrapper that counts invocations (the runner clears the environment, so the count
    // file is written into the script) and runs the real git
    let count = s.0.join("git-calls");
    let wrapper = s.0.join("git-counting.sh");
    std::fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\necho x >> '{}'\nexec git \"$@\"\n",
            count.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
    let calls = || std::fs::read_to_string(&count).map_or(0, |t| t.lines().count());
    let api = api_without_provider(&s, &root).with_git(GitRunner {
        program: wrapper,
        timeout: Duration::from_secs(10),
    });
    let dir = s.codegraph_dir();
    let path = publish_go(&dir, "pv");
    let api = api.with_snapshot_provider(dir, Arc::new(FixedSnapshot::new(path)));
    let target = definition(by_label_name("Interface", "DatabaseDriver"));

    api.code_definition(target.clone()).unwrap();
    assert_eq!(calls(), 2, "first call: rev-parse HEAD and rev-list");
    api.code_definition(target.clone()).unwrap();
    assert_eq!(
        calls(),
        3,
        "the same sha: rev-parse HEAD only, the ancestors are cached"
    );

    // a second commit on the same branch rewrites the branch ref, not `.git/HEAD`: the sha
    // changes, so the next call lists the ancestors again, once (G1-F8 keys on the sha)
    std::fs::write(root.join("second.txt"), "two\n").unwrap();
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-q", "-m", "second"]);
    api.code_definition(target.clone()).unwrap();
    assert_eq!(calls(), 5, "a new sha: rev-parse HEAD and one rev-list");
    api.code_definition(target.clone()).unwrap();
    assert_eq!(calls(), 6, "the new sha is cached");

    // rewriting `.git/HEAD` to another branch at the same commit changes no sha: no rev-list
    git(&root, &["checkout", "-q", "-b", "other"]);
    api.code_definition(target).unwrap();
    assert_eq!(
        calls(),
        7,
        "the same sha under another ref: rev-parse HEAD only"
    );
}

/// A provider that records every head it was asked to resolve, then answers as the fixed
/// snapshot would.
struct Recording {
    inner: FixedSnapshot,
    seen: Mutex<Vec<HeadInput>>,
}

impl SnapshotProvider for Recording {
    fn resolve(&self, repo: &str, head: &HeadInput) -> Result<Resolved, NoSnapshot> {
        self.seen.lock().unwrap().push(head.clone());
        self.inner.resolve(repo, head)
    }
}

/// `git rev-parse HEAD` of a scratch repository, as the test's own witness.
fn head_sha(root: &Path) -> String {
    let out = Command::new("git")
        .args(["-C"])
        .arg(root)
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    assert!(out.status.success());
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

/// Regression (G1-F8, CG7.1 residual 1): a same-branch commit does not touch `.git/HEAD`, and
/// the mtime-keyed cache served the old head until a checkout. The head is now resolved on
/// every call, so the commit after the first call is seen by the next one, with the previous
/// head as its first ancestor.
#[test]
fn resolve_current_sees_same_branch_commit() {
    let s = Scratch::new("head-same-branch");
    let root = s.repo();
    let dir = s.codegraph_dir();
    let path = publish_go(&dir, "pv");
    let provider = Arc::new(Recording {
        inner: FixedSnapshot::new(path),
        seen: Mutex::new(Vec::new()),
    });
    let api = api_without_provider(&s, &root)
        .with_snapshot_provider(dir, provider.clone() as Arc<dyn SnapshotProvider>);
    let target = definition(by_label_name("Interface", "DatabaseDriver"));

    let first = head_sha(&root);
    api.code_definition(target.clone()).unwrap();
    assert_eq!(
        provider.seen.lock().unwrap().as_slice(),
        [HeadInput {
            commit: first.clone(),
            ancestors: vec![],
        }]
    );

    std::fs::write(root.join("second.txt"), "two\n").unwrap();
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-q", "-m", "second"]);
    let second = head_sha(&root);
    assert_ne!(second, first);

    api.code_definition(target).unwrap();
    let seen = provider.seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    assert_eq!(
        seen[1],
        HeadInput {
            commit: second,
            ancestors: vec![first],
        },
        "the same-branch commit is the head of the very next call"
    );
}

/// A provider that records every `resolve` and finds nothing: proves the gate ran first.
#[derive(Debug, Default)]
struct Tripwire(AtomicUsize);

impl SnapshotProvider for Tripwire {
    fn resolve(&self, _repo: &str, _head: &HeadInput) -> Result<Resolved, NoSnapshot> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(NoSnapshot::NoFile)
    }
}

/// [red] G1-S1 / G1-S4: a credential canary in a content selector field is rejected by the
/// gate with the rule id before any grammar check and before the reader is consulted.
#[test]
fn selector_canary_rejected_before_parse() {
    let s = Scratch::new("canary");
    let root = s.repo();
    let trip = Arc::new(Tripwire::default());
    let provider: Arc<dyn SnapshotProvider> = trip.clone();
    let api = api_without_provider(&s, &root).with_snapshot_provider(s.codegraph_dir(), provider);
    let canary = canary();
    let cases: Vec<(&str, SymbolSelector)> = vec![
        ("target.name", by_label_name("Method", &canary)),
        ("target.symbol", by_symbol(&canary)),
        (
            "target.symbol",
            // grammar-valid, with the canary inside a backtick-escaped descriptor
            by_symbol(&format!("scip-go gomod example.com/x . `{canary}`/A#")),
        ),
        ("target.ref", by_ref(&format!("file:{canary}"))),
        ("target.path", by_path(&canary)),
    ];
    for (field, target) in cases {
        for err in [
            api.code_members(members(target.clone(), &[])).unwrap_err(),
            api.code_implementers(implementers(target.clone()))
                .unwrap_err(),
            api.code_definition(definition(target.clone())).unwrap_err(),
        ] {
            match &err {
                ApiError::Rejected {
                    field: f, rules, ..
                } => {
                    assert_eq!(f, field);
                    assert!(
                        rules.iter().any(|r| r == "aws-access-key-id"),
                        "{field}: {rules:?}"
                    );
                }
                other => panic!("{field}: expected Rejected, got {other:?}"),
            }
            assert!(!err.to_string().contains(&canary), "{err}");
        }
    }
    // `current_path` and `labels[]` are gated too
    let e = api
        .code_members(CodeMembersRequest {
            target: by_path("drivers/postgres.go"),
            labels: vec![canary.clone()],
            ..Default::default()
        })
        .unwrap_err();
    assert!(matches!(e, ApiError::Rejected { .. }), "{e}");
    let e = api
        .code_definition(CodeDefinitionRequest {
            target: by_path("drivers/postgres.go"),
            current_path: Some(canary.clone()),
        })
        .unwrap_err();
    assert!(matches!(e, ApiError::Rejected { .. }), "{e}");
    assert_eq!(
        trip.0.load(Ordering::SeqCst),
        0,
        "the provider (and so the reader) was never consulted"
    );
}

/// [red] G1-S8: every error class is a fixed text; the offending input never appears in it.
#[test]
fn ref_invalid_and_errors_never_echo() {
    let s = Scratch::new("no-echo");
    let api = go_api(&s);
    // a `ref` with a parent component
    let bad_ref = "file:../../etc/passwd";
    let text = class_text(
        api.code_definition(definition(by_ref(bad_ref)))
            .unwrap_err(),
        "ref_invalid",
    );
    assert!(!text.contains("passwd") && !text.contains(".."), "{text}");
    // an unknown prefix
    let text = class_text(
        api.code_members(members(by_ref("overlay:drivers/x.go"), &[]))
            .unwrap_err(),
        "ref_invalid",
    );
    assert!(!text.contains("overlay"), "{text}");
    // an over-long symbol (the grammar's 1,024-byte bound)
    let long = format!(
        "scip-go gomod example.com/drivers . `p`/{}#",
        "Z".repeat(1100)
    );
    let text = class_text(
        api.code_implementers(implementers(by_symbol(&long)))
            .unwrap_err(),
        "symbol_invalid",
    );
    assert!(!text.contains("ZZZZ"), "{text}");
    // a malformed symbol
    let text = class_text(
        api.code_definition(definition(by_symbol("not a symbol at all Qx9")))
            .unwrap_err(),
        "symbol_invalid",
    );
    assert!(!text.contains("Qx9"), "{text}");
    // a grammar-valid key absent from the snapshot
    let text = class_text(
        api.code_definition(definition(by_symbol(&format!("{GO_KEY}NopeQ7#"))))
            .unwrap_err(),
        "unknown_symbol",
    );
    assert!(!text.contains("NopeQ7"), "{text}");
    // a versioned raw symbol is not a key: it matches nothing (and is not echoed)
    let raw = &format!("{GO}PostgresDriver#")["scip:".len()..];
    let text = class_text(
        api.code_definition(definition(by_symbol(raw))).unwrap_err(),
        "unknown_symbol",
    );
    assert!(!text.contains("PostgresDriver"), "{text}");
    // an unknown ref
    let text = class_text(
        api.code_members(members(by_ref("file:drivers/nothing-here-Q7.go"), &[]))
            .unwrap_err(),
        "unknown_symbol",
    );
    assert!(!text.contains("Q7"), "{text}");
    // a `labels[]` value outside the closed set
    let e = api
        .code_members(members(by_path("drivers/postgres.go"), &["MethodQ7"]))
        .unwrap_err();
    assert_eq!(e.to_string(), "labels[] must name SCIP kinds");
}

// ---------------------------------------------------------------------------------------------
// task 2: the goldens (stage1-spec §8.2), the fixed texts, the schema walk

#[test]
fn tool_code_implementers_golden_go_drivers() {
    let s = Scratch::new("go-impl");
    let api = go_api(&s);
    let r = api
        .code_implementers(implementers(by_label_name("Interface", "DatabaseDriver")))
        .unwrap();
    assert_eq!(
        refs(&r),
        vec![
            format!("{GO}MemDriver#"),
            format!("{GO}PostgresDriver#"),
            format!("{GO}Wrapped#"),
            format!("{GO_SQLITE}Driver#"),
        ],
        "4 rows by (path, ref); HttpDriver absent"
    );
    assert!(!refs(&r).iter().any(|r| r.contains("HttpDriver")));
    assert_eq!((r.count, r.truncated), (4, false));
    assert_eq!(r.ranking, "full");
    assert_eq!(r.follow_ups, Vec::<FollowUp>::new());
    // the snapshot ref
    assert_eq!(r.snapshot.commit, GO_COMMIT);
    assert!(r.snapshot.exact);
    assert_eq!(r.snapshot.schema_version, SCHEMA_VERSION);
    assert_eq!(r.snapshot.policy, "current");
    assert_eq!(r.snapshot.isolation, "none");
    // the cards
    let mem = &r.results[0];
    assert_eq!(mem.kind, "symbol");
    assert_eq!(mem.label, "Struct");
    assert_eq!(mem.name, "MemDriver");
    assert_eq!(mem.path.as_deref(), Some("drivers/memory.go"));
    assert_eq!(mem.language.as_deref(), Some("go"));
    assert_eq!(
        mem.symbol.as_deref(),
        Some(format!("{GO_KEY}MemDriver#").as_str())
    );
    assert_eq!(
        mem.raw.as_deref(),
        Some(&format!("{GO}MemDriver#")["scip:".len()..])
    );
    assert_eq!(range4(mem.range), Some((3, 5, 3, 14)));
    assert!(!mem.external && !mem.is_test);
    assert_eq!((mem.distance, mem.snippet.as_deref()), (0, None));
    // `include_external` adds nothing here (no external implementer in the fixture)
    let ext = api
        .code_implementers(CodeImplementersRequest {
            target: by_label_name("Interface", "DatabaseDriver"),
            include_external: true,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(refs(&ext), refs(&r));
}

#[test]
fn tool_code_members_golden_go_drivers() {
    let s = Scratch::new("go-members");
    let api = go_api(&s);
    let pg = api
        .code_members(members(
            by_ref(&format!("{GO}PostgresDriver#")),
            &["Method"],
        ))
        .unwrap();
    assert_eq!(
        refs(&pg),
        vec![
            format!("{GO}PostgresDriver#Close()."),
            format!("{GO}PostgresDriver#Open()."),
        ]
    );
    assert!(pg.results.iter().all(|c| c.label == "Method"));
    assert_eq!(pg.snapshot.policy, "current");
    // Wrapped embeds the interface: one Field, no declared methods (the promoted method set is
    // not materialised, §8.2)
    let wrapped = api
        .code_members(members(by_symbol(&format!("{GO_KEY}Wrapped#")), &[]))
        .unwrap();
    assert_eq!(wrapped.count, 1);
    let field = &wrapped.results[0];
    assert_eq!(
        (field.label.as_str(), field.name.as_str()),
        ("Field", "DatabaseDriver")
    );
    assert_eq!(field.r#ref, format!("{GO}Wrapped#DatabaseDriver."));
}

#[test]
fn tool_code_definition_golden_go_drivers() {
    let s = Scratch::new("go-def");
    let api = go_api(&s);
    let r = api
        .code_definition(definition(by_symbol(&format!(
            "{GO_KEY}PostgresDriver#Open()."
        ))))
        .unwrap();
    let card = &r.definition;
    assert_eq!(card.r#ref, format!("{GO}PostgresDriver#Open()."));
    assert_eq!(card.kind, "symbol");
    assert_eq!(card.label, "Method");
    assert_eq!(card.name, "Open");
    assert_eq!(card.path.as_deref(), Some("drivers/postgres.go"));
    assert_eq!(card.language.as_deref(), Some("go"));
    assert_eq!(range4(card.range), Some((10, 25, 10, 29)));
    assert_eq!(range4(card.enclosing), Some((10, 0, 15, 1)));
    // the golden dump records the lengths (53, 24), not the text
    assert_eq!(card.signature.as_deref().map(str::len), Some(53));
    assert_eq!(card.doc.as_deref().map(str::len), Some(24));
    assert_eq!(card.snippet, None, "snippets arrive with CG8.2");
    assert_eq!(card.distance, 0);
    assert_eq!(r.follow_ups, Vec::<FollowUp>::new());
    assert_eq!(r.snapshot.commit, GO_COMMIT);
    // the interface's doc carried CANARY_DOC and was dropped at index time
    let iface = api
        .code_definition(definition(by_label_name("Interface", "DatabaseDriver")))
        .unwrap();
    assert_eq!(iface.definition.doc, None);
    assert_eq!(
        iface.definition.signature.as_deref().map(str::len),
        Some(86)
    );
}

/// SCR-2: scip-typescript 0.4.0 labels every symbol `UnspecifiedKind`, so the rows select by
/// key and ask for members without a label filter.
#[test]
fn tool_code_implementers_golden_ts_drivers() {
    let s = Scratch::new("ts-impl");
    let api = ts_api(&s);
    let r = api
        .code_implementers(implementers(by_symbol(&format!(
            "{TS_KEY}`driver.ts`/DatabaseDriver#"
        ))))
        .unwrap();
    assert_eq!(
        refs(&r),
        vec![
            format!("{TS}`mem.ts`/MemDriver#"),
            format!("{TS}`postgres.ts`/PostgresDriver#"),
        ],
        "2 rows; MockDriver (structural only) absent"
    );
    assert!(
        r.results
            .iter()
            .all(|c| c.language.as_deref() == Some("typescript"))
    );
    assert!(r.results.iter().all(|c| c.label == "UnspecifiedKind"));
    assert_eq!(r.snapshot.commit, TS_COMMIT);
    assert_eq!(
        r.snapshot.policy, "stale",
        "published under another policy version"
    );
    assert!(r.snapshot.exact);
    // the by-label form: no row carries `Interface` in this snapshot (SCR-2), so the one
    // `UnspecifiedKind` row of that name answers with the correction (Q-2) instead of the
    // `unknown_symbol` it used to be
    let by_label = api
        .code_implementers(implementers(by_label_name("Interface", "DatabaseDriver")))
        .unwrap();
    assert_eq!(refs(&by_label), refs(&r));
    assert_eq!(
        by_label.target_note,
        Some(TargetNote {
            r#ref: format!("{TS}`driver.ts`/DatabaseDriver#"),
            label_corrected: Some("UnspecifiedKind".into()),
            external: false,
        })
    );
}

#[test]
fn tool_code_members_golden_ts_drivers() {
    let s = Scratch::new("ts-members");
    let api = ts_api(&s);
    let r = api
        .code_members(members(
            by_symbol(&format!("{TS_KEY}`postgres.ts`/PostgresDriver#")),
            &[],
        ))
        .unwrap();
    assert_eq!(
        refs(&r),
        vec![
            format!("{TS}`postgres.ts`/PostgresDriver#close()."),
            format!("{TS}`postgres.ts`/PostgresDriver#open()."),
        ]
    );
    // a label filter the producer never emits finds nothing
    let none = api
        .code_members(members(
            by_symbol(&format!("{TS_KEY}`postgres.ts`/PostgresDriver#")),
            &["Method"],
        ))
        .unwrap();
    assert_eq!(none.count, 0);
}

#[test]
fn tool_code_definition_golden_ts_drivers() {
    let s = Scratch::new("ts-def");
    let api = ts_api(&s);
    let r = api
        .code_definition(definition(by_symbol(&format!(
            "{TS_KEY}`postgres.ts`/PostgresDriver#open()."
        ))))
        .unwrap();
    let card = &r.definition;
    assert_eq!(card.name, "open");
    assert_eq!(card.path.as_deref(), Some("src/postgres.ts"));
    assert_eq!(card.language.as_deref(), Some("typescript"));
    assert_eq!(range4(card.range), Some((7, 8, 7, 12)));
    assert_eq!(range4(card.enclosing), Some((7, 2, 11, 3)));
    assert_eq!(card.snippet, None);
    assert_eq!(r.snapshot.isolation, "none");
}

#[test]
fn no_snapshot_hint() {
    let s = Scratch::new("no-snapshot");
    let root = s.repo();
    // no provider at all (a process without the codegraph key)
    let api = api_without_provider(&s, &root);
    let text = class_text(
        api.code_definition(definition(by_path("drivers/postgres.go")))
            .unwrap_err(),
        "no_snapshot",
    );
    assert!(text.contains("kaban-index index"), "{text}");
    assert!(!text.contains(&*s.0.to_string_lossy()), "no path: {text}");
    // a provider over an empty directory: the same class, the same text
    let dir = s.codegraph_dir();
    let api = api.with_snapshot_provider(
        dir.clone(),
        Arc::new(kaban_codegraph::DiscoverProvider::new(dir)),
    );
    let again = class_text(
        api.code_members(members(by_path("drivers/postgres.go"), &[]))
            .unwrap_err(),
        "no_snapshot",
    );
    assert_eq!(again, text);
    // without a repository root the class is the same
    let s2 = Scratch::new("no-repo");
    let store = Arc::new(SqliteStore::open(&s2.0.join("k.db"), &s2.0.join("keys")).unwrap());
    let api = KabanApi::new(store.clone(), Policy::empty(), store.signer(), None)
        .with_snapshot_provider(s2.codegraph_dir(), Arc::new(Tripwire::default()));
    let no_repo = class_text(
        api.code_definition(definition(by_path("x.go")))
            .unwrap_err(),
        "no_snapshot",
    );
    assert_eq!(no_repo, text);
}

#[test]
fn snapshot_too_new_fixed_text() {
    let s = Scratch::new("too-new");
    let root = s.repo();
    let api = api_without_provider(&s, &root);
    let dir = s.codegraph_dir();
    let path = Synth::default().publish(&dir, &"7".repeat(40), SCHEMA_VERSION + 1, "pv");
    let api = api.with_snapshot_provider(dir.clone(), Arc::new(FixedSnapshot::new(path)));
    let text = class_text(
        api.code_definition(definition(by_path("x.go")))
            .unwrap_err(),
        "snapshot_too_new",
    );
    assert!(!text.contains(&*dir.to_string_lossy()), "{text}");
    assert!(!text.contains(&"7".repeat(40)), "{text}");
    assert!(
        !text.contains(&(SCHEMA_VERSION + 1).to_string()),
        "no version number: {text}"
    );
}

/// Every path at which a string may sit in a schemars 1 schema (the walk of
/// `tests/control.rs`, reduced to what the three code tools need).
fn string_paths(schema: &Value) -> BTreeSet<String> {
    fn walk(root: &Value, node: &Value, path: &str, out: &mut BTreeSet<String>) {
        let obj = match node {
            Value::Bool(true) => {
                out.insert(path.to_owned());
                return;
            }
            Value::Bool(false) => return,
            Value::Object(o) => o,
            other => panic!("unexpected schema node at {path}: {other}"),
        };
        if let Some(Value::String(r)) = obj.get("$ref") {
            let name = r.strip_prefix("#/$defs/").unwrap();
            walk(root, &root["$defs"][name], path, out);
            return;
        }
        for combinator in ["anyOf", "oneOf", "allOf"] {
            if let Some(Value::Array(branches)) = obj.get(combinator) {
                for b in branches {
                    walk(root, b, path, out);
                }
            }
        }
        let constraining = [
            "type",
            "anyOf",
            "oneOf",
            "allOf",
            "properties",
            "items",
            "enum",
            "const",
            "additionalProperties",
        ];
        if !constraining.iter().any(|k| obj.contains_key(*k)) {
            out.insert(path.to_owned());
            return;
        }
        let types: Vec<&str> = match obj.get("type") {
            Some(Value::String(t)) => vec![t.as_str()],
            Some(Value::Array(ts)) => ts.iter().filter_map(Value::as_str).collect(),
            _ => Vec::new(),
        };
        if types.contains(&"string") || obj.contains_key("enum") || obj.contains_key("const") {
            out.insert(path.to_owned());
        }
        if let Some(Value::Object(props)) = obj.get("properties") {
            for (k, v) in props {
                let mut p = path.to_owned();
                append_key(&mut p, k);
                walk(root, v, &p, out);
            }
        }
        if let Some(items) = obj.get("items") {
            let mut p = path.to_owned();
            append_index(&mut p);
            match items {
                Value::Array(items) => {
                    for v in items {
                        walk(root, v, &p, out);
                    }
                }
                v => walk(root, v, &p, out),
            }
        }
        match obj.get("additionalProperties") {
            Some(Value::Bool(false)) | None => {}
            Some(v) => {
                let mut p = path.to_owned();
                append_key(&mut p, "*");
                walk(root, v, &p, out);
            }
        }
        if types.contains(&"object")
            && !obj.contains_key("properties")
            && !obj.contains_key("additionalProperties")
        {
            let mut p = path.to_owned();
            append_key(&mut p, "*");
            out.insert(p);
        }
    }
    let mut out = BTreeSet::new();
    walk(schema, schema, "", &mut out);
    out
}

/// [red] G1-S2: every string leaf of the three tools' schemas is content unless stage1-spec
/// §6.1 lists it as control; the registry covers exactly those paths, on both sides.
#[test]
fn control_schema_walk_code_tools() {
    let snapshot = ["snapshot.commit", "snapshot.policy", "snapshot.isolation"];
    let list_rows = [
        "results[].kind",
        "results[].label",
        "results[].language",
        "ranking",
    ];
    let expected: Vec<(&str, Vec<&str>, Vec<&str>)> = vec![
        (
            "code_implementers",
            vec!["target.label"],
            snapshot.iter().chain(list_rows.iter()).copied().collect(),
        ),
        (
            "code_members",
            vec!["target.label", "labels[]"],
            snapshot.iter().chain(list_rows.iter()).copied().collect(),
        ),
        (
            "code_definition",
            vec!["target.label"],
            snapshot
                .iter()
                .copied()
                .chain(["definition.kind", "definition.label", "definition.language"])
                .collect(),
        ),
        (
            "code_references",
            vec!["target.label"],
            snapshot
                .iter()
                .copied()
                .chain([
                    "by_enclosing[].enclosing.kind",
                    "by_enclosing[].enclosing.label",
                    "by_enclosing[].enclosing.language",
                    "by_enclosing[].sites[].roles[]",
                ])
                .collect(),
        ),
        (
            "code_tests_for",
            vec!["target.label"],
            snapshot
                .iter()
                .copied()
                .chain([
                    "tests[].test.kind",
                    "tests[].test.label",
                    "tests[].test.language",
                    "tests[].matched_by",
                    "tests[].how",
                ])
                .collect(),
        ),
        (
            "code_neighbours",
            vec!["target.label", "edge_types[]"],
            snapshot
                .iter()
                .copied()
                .chain([
                    "nodes[].kind",
                    "nodes[].label",
                    "nodes[].language",
                    "edges[].type",
                ])
                .collect(),
        ),
    ];
    // last segments that are control on the code tools (stage1-spec §6.1): a content path
    // ending in one of them is an unregistered control field, a declaration error
    let control_names = [
        "kind",
        "label",
        "language",
        "policy",
        "isolation",
        "commit",
        "ranking",
        "roles",
        "matched_by",
        "how",
        "type",
        "edge_types",
    ];
    let tools = tool_schemas();
    for (tool, request, response) in expected {
        let t = tools
            .iter()
            .find(|t| t.tool == tool)
            .unwrap_or_else(|| panic!("{tool} missing from tool_schemas()"));
        for (direction, schema, want) in [
            (Direction::Request, &t.input, request),
            (Direction::Response, &t.output, response),
        ] {
            let paths = string_paths(&serde_json::to_value(schema).unwrap());
            let want: BTreeSet<&str> = want.into_iter().collect();
            let registered: BTreeSet<&str> = REGISTRY
                .iter()
                .filter(|e| e.tool == tool && e.direction == direction)
                .map(|e| e.path)
                .collect();
            assert_eq!(
                registered, want,
                "{tool} {direction:?}: the registry must cover exactly the §6.1 control paths"
            );
            for p in &want {
                assert!(
                    paths.contains(*p),
                    "{tool} {direction:?}: {p} is not a string path"
                );
            }
            let content: BTreeSet<&str> = paths
                .iter()
                .map(String::as_str)
                .filter(|p| !want.contains(p))
                .collect();
            for p in &content {
                let last = p.rsplit('.').next().unwrap().trim_end_matches("[]");
                assert!(
                    !control_names.contains(&last),
                    "{tool} {direction:?}: {p} looks like a control field but is unregistered"
                );
            }
            // the content side of each tool: the selector strings and `current_path` on the
            // way in, every card string but the three enums on the way out
            if direction == Direction::Request {
                for p in [
                    "target.symbol",
                    "target.ref",
                    "target.name",
                    "target.path",
                    "current_path",
                ] {
                    assert!(content.contains(p), "{tool}: {p} must be content");
                }
            } else {
                for p in [
                    "symbol",
                    "ref",
                    "raw",
                    "name",
                    "path",
                    "signature",
                    "doc",
                    "snippet",
                ] {
                    let under = match tool {
                        "code_definition" => format!("definition.{p}"),
                        "code_references" => format!("by_enclosing[].enclosing.{p}"),
                        "code_tests_for" => format!("tests[].test.{p}"),
                        "code_neighbours" => format!("nodes[].{p}"),
                        _ => format!("results[].{p}"),
                    };
                    assert!(
                        content.contains(under.as_str()),
                        "{tool}: {under} must be content"
                    );
                }
                // the site path and the edge refs are content too (CG7.2 default 1)
                let extra: &[&str] = match tool {
                    "code_references" => &["by_enclosing[].sites[].path"],
                    "code_neighbours" => &["edges[].from", "edges[].to"],
                    _ => &[],
                };
                for p in extra {
                    assert!(content.contains(p), "{tool}: {p} must be content");
                }
            }
        }
    }
    // `symbol` and `ref` are content everywhere: no shape validates a SCIP symbol (G1-S2)
    assert!(
        !REGISTRY.iter().any(|e| e.tool.starts_with("code_")
            && (e.path.ends_with("symbol") || e.path.ends_with("ref"))),
        "symbol and ref are content"
    );
}

// ---------------------------------------------------------------------------------------------
// task 6 (CG7.2, [red]): the error classes of every `code_*` handler are fixed texts (G1-S8)

/// One `code_*` handler as the error table sees it: the target and `current_path`, every other
/// field at its default; the result is dropped.
struct Handler {
    name: &'static str,
    call: fn(&KabanApi, SymbolSelector, Option<String>) -> Result<(), ApiError>,
}

/// The six handlers of G1-F12 (CG7.1 and CG7.2).
fn handlers() -> Vec<Handler> {
    vec![
        Handler {
            name: "code_implementers",
            call: |api, target, current_path| {
                api.code_implementers(CodeImplementersRequest {
                    target,
                    current_path,
                    ..Default::default()
                })
                .map(drop)
            },
        },
        Handler {
            name: "code_members",
            call: |api, target, current_path| {
                api.code_members(CodeMembersRequest {
                    target,
                    current_path,
                    ..Default::default()
                })
                .map(drop)
            },
        },
        Handler {
            name: "code_definition",
            call: |api, target, current_path| {
                api.code_definition(CodeDefinitionRequest {
                    target,
                    current_path,
                })
                .map(drop)
            },
        },
        Handler {
            name: "code_references",
            call: |api, target, current_path| {
                api.code_references(CodeReferencesRequest {
                    target,
                    current_path,
                    ..Default::default()
                })
                .map(drop)
            },
        },
        Handler {
            name: "code_tests_for",
            call: |api, target, current_path| {
                api.code_tests_for(CodeTestsForRequest {
                    target,
                    current_path,
                    ..Default::default()
                })
                .map(drop)
            },
        },
        Handler {
            name: "code_neighbours",
            call: |api, target, current_path| {
                api.code_neighbours(CodeNeighboursRequest {
                    target,
                    current_path,
                    ..Default::default()
                })
                .map(drop)
            },
        },
    ]
}

/// A selector with every field spelled out (the one-of table reads better in one line).
fn sel(
    symbol: Option<&str>,
    r#ref: Option<&str>,
    label: Option<&str>,
    name: Option<&str>,
    path: Option<&str>,
    line: Option<u32>,
) -> SymbolSelector {
    SymbolSelector {
        symbol: symbol.map(str::to_owned),
        r#ref: r#ref.map(str::to_owned),
        label: label.map(str::to_owned),
        name: name.map(str::to_owned),
        path: path.map(str::to_owned),
        line,
    }
}

/// The bare `path` form, which `code_implementers` refuses before anything else.
fn is_bare_path(s: &SymbolSelector) -> bool {
    s.path.is_some()
        && s.line.is_none()
        && s.symbol.is_none()
        && s.r#ref.is_none()
        && s.label.is_none()
        && s.name.is_none()
}

/// `Rejected` at `field` by the credential rule, and the canary is not in the text.
fn rejected(err: &ApiError, field: &str, canary: &str, tool: &str) {
    match err {
        ApiError::Rejected {
            field: f, rules, ..
        } => {
            assert_eq!(f, field, "{tool}");
            assert!(
                rules.iter().any(|r| r == "aws-access-key-id"),
                "{tool} {field}: {rules:?}"
            );
        }
        other => panic!("{tool} {field}: expected Rejected, got {other:?}"),
    }
    assert!(!err.to_string().contains(canary), "{tool} {field}: {err}");
}

/// [red] G1-S8: a table of (handler x error path) over all six `code_*` handlers. Every error is
/// a class prefix with one fixed sentence per class, the same sentence from every handler, and
/// no byte of the offending input in it; a canary in any content position is rejected before
/// the reader is consulted.
#[test]
fn error_classes_fixed() {
    let s = Scratch::new("classes-go");
    let api = go_api(&s);
    let s_none = Scratch::new("classes-none");
    let root_none = s_none.repo();
    let api_none = api_without_provider(&s_none, &root_none);
    let s_new = Scratch::new("classes-new");
    let root_new = s_new.repo();
    let dir_new = s_new.codegraph_dir();
    let too_new = Synth::default().publish(&dir_new, &"7".repeat(40), SCHEMA_VERSION + 1, "pv");
    let api_new = api_without_provider(&s_new, &root_new)
        .with_snapshot_provider(dir_new, Arc::new(FixedSnapshot::new(too_new)));
    let s_trip = Scratch::new("classes-trip");
    let root_trip = s_trip.repo();
    let trip = Arc::new(Tripwire::default());
    let provider: Arc<dyn SnapshotProvider> = trip.clone();
    let api_trip = api_without_provider(&s_trip, &root_trip)
        .with_snapshot_provider(s_trip.codegraph_dir(), provider);
    let handlers = handlers();

    // every offending input carries this marker; no error text may
    let m = "Zq8v";
    let name_m = format!("Nope{m}");
    let ref_m = format!("file:drivers/nope-{m}.go");
    let path_m = format!("drivers/nope-{m}.go");
    let up_m = format!("../{m}.go");
    let abs_m = format!("/abs/{m}.go");
    let kind_m = format!("Kind{m}");
    let well_formed = by_label_name("Struct", &name_m);
    let valid_symbol = format!("{GO_KEY}PostgresDriver#Open().");
    let bad_symbol = format!("not a symbol at all {m}");
    let long_symbol = format!("{GO_KEY}{}#", m.repeat(300));
    let overlay_ref = format!("overlay:drivers/{m}.go");
    let parent_ref = format!("file:../{m}");
    let unknown_key = format!("{GO_KEY}Nope{m}#");
    // (class, service, target): every handler takes every row
    let table: Vec<(&str, &KabanApi, SymbolSelector)> = vec![
        ("no_snapshot", &api_none, well_formed.clone()),
        ("snapshot_too_new", &api_new, well_formed.clone()),
        // the one-of rule: nothing, half a form, a line alone, two forms, three, a form and a half
        ("selector_invalid", &api, SymbolSelector::default()),
        (
            "selector_invalid",
            &api,
            sel(None, None, Some("Struct"), None, None, None),
        ),
        (
            "selector_invalid",
            &api,
            sel(None, None, None, Some(&name_m), None, None),
        ),
        (
            "selector_invalid",
            &api,
            sel(None, None, None, None, None, Some(3)),
        ),
        (
            "selector_invalid",
            &api,
            sel(Some(&valid_symbol), Some(&ref_m), None, None, None, None),
        ),
        (
            "selector_invalid",
            &api,
            sel(
                Some(&valid_symbol),
                None,
                Some("Struct"),
                Some(&name_m),
                None,
                None,
            ),
        ),
        (
            "selector_invalid",
            &api,
            sel(None, Some(&ref_m), None, None, Some(&path_m), None),
        ),
        (
            "selector_invalid",
            &api,
            sel(
                Some(&valid_symbol),
                None,
                None,
                None,
                Some(&path_m),
                Some(1),
            ),
        ),
        (
            "selector_invalid",
            &api,
            sel(
                None,
                None,
                Some("Struct"),
                Some(&name_m),
                Some(&path_m),
                Some(1),
            ),
        ),
        (
            "selector_invalid",
            &api,
            sel(None, None, Some("Struct"), None, Some(&path_m), None),
        ),
        // a label outside the closed set; a path failing the repository-relative rule
        ("selector_invalid", &api, by_label_name(&kind_m, "X")),
        (
            "selector_invalid",
            &api,
            sel(None, None, None, None, Some(&up_m), Some(1)),
        ),
        (
            "selector_invalid",
            &api,
            sel(None, None, None, None, Some(&abs_m), Some(1)),
        ),
        ("selector_invalid", &api, by_path(&up_m)),
        ("symbol_invalid", &api, by_symbol(&bad_symbol)),
        ("symbol_invalid", &api, by_symbol(&long_symbol)),
        ("ref_invalid", &api, by_ref(&overlay_ref)),
        ("ref_invalid", &api, by_ref(&parent_ref)),
        ("unknown_symbol", &api, by_symbol(&unknown_key)),
        ("unknown_symbol", &api, by_ref(&ref_m)),
        ("unknown_symbol", &api, well_formed.clone()),
        ("unknown_symbol", &api, by_path(&path_m)),
        (
            "unknown_symbol",
            &api,
            sel(None, None, None, None, Some(&path_m), Some(0)),
        ),
    ];
    let mut texts: BTreeMap<&str, BTreeSet<String>> = BTreeMap::new();
    for h in &handlers {
        for (class, service, target) in &table {
            let want = if h.name == "code_implementers" && is_bare_path(target) {
                "selector_invalid"
            } else {
                *class
            };
            let err = (h.call)(service, target.clone(), None)
                .expect_err("every row of the table is an error");
            let text = class_text(err, want);
            assert!(!text.contains(m), "{} {want}: {text}", h.name);
            texts.entry(want).or_default().insert(text);
        }
    }
    // several nodes of one name: every handler refuses (Q-2; the list handlers used to
    // union), and the text names the candidates (snapshot rows, so "Close" appears: it is
    // their name, not an echo of an unmatched request)
    let several = by_label_name("Method", "Close");
    for h in &handlers {
        let r = (h.call)(&api, several.clone(), None);
        let text = class_text(r.expect_err("several nodes"), "ambiguous_symbol");
        assert!(
            text.contains("PostgresDriver#Close().") && text.contains("MemDriver#Close()."),
            "{}: {text}",
            h.name
        );
        texts.entry("ambiguous_symbol").or_default().insert(text);
    }
    // depth outside 1..=3 is the selector class (G1-F12)
    for depth in [0, 4] {
        let text = class_text(
            api.code_neighbours(neighbours(by_symbol(&valid_symbol), depth, &[]))
                .unwrap_err(),
            "selector_invalid",
        );
        texts.entry("selector_invalid").or_default().insert(text);
    }
    // one sentence per class, from every handler
    for (class, set) in &texts {
        assert_eq!(set.len(), 1, "{class} has several texts: {set:?}");
    }
    assert_eq!(
        texts.keys().copied().collect::<Vec<_>>(),
        [
            "ambiguous_symbol",
            "no_snapshot",
            "ref_invalid",
            "selector_invalid",
            "snapshot_too_new",
            "symbol_invalid",
            "unknown_symbol",
        ]
    );
    // the two array filters: fixed texts, the value never echoed
    let label_m = format!("Method{m}");
    let e = api
        .code_members(members(by_path("drivers/postgres.go"), &[label_m.as_str()]))
        .unwrap_err();
    assert_eq!(e.to_string(), "labels[] must name SCIP kinds");
    let type_m = format!("Type{m}");
    for bad in ["CALLS", type_m.as_str()] {
        let e = api
            .code_neighbours(neighbours(by_symbol(&valid_symbol), 1, &[bad]))
            .unwrap_err();
        assert_eq!(e.to_string(), "edge_types[] must name edge types", "{bad}");
    }
    // the canary in every content position every handler carries, before the reader
    let canary = canary();
    let positions: Vec<(&str, SymbolSelector)> = vec![
        ("target.symbol", by_symbol(&canary)),
        (
            "target.symbol",
            by_symbol(&format!("scip-go gomod example.com/x . `{canary}`/A#")),
        ),
        ("target.ref", by_ref(&format!("file:{canary}"))),
        ("target.name", by_label_name("Method", &canary)),
        ("target.path", by_path(&canary)),
        (
            "target.path",
            sel(None, None, None, None, Some(&canary), Some(1)),
        ),
    ];
    for h in &handlers {
        for (field, target) in &positions {
            let err = (h.call)(&api_trip, target.clone(), None).expect_err("rejected");
            rejected(&err, field, &canary, h.name);
        }
        let err = (h.call)(
            &api_trip,
            by_path("drivers/postgres.go"),
            Some(canary.clone()),
        )
        .expect_err("rejected");
        rejected(&err, "current_path", &canary, h.name);
    }
    let e = api_trip
        .code_members(members(by_path("drivers/postgres.go"), &[canary.as_str()]))
        .unwrap_err();
    rejected(&e, "labels", &canary, "code_members");
    let e = api_trip
        .code_neighbours(neighbours(by_symbol(&valid_symbol), 1, &[canary.as_str()]))
        .unwrap_err();
    rejected(&e, "edge_types", &canary, "code_neighbours");
    assert_eq!(
        trip.0.load(Ordering::SeqCst),
        0,
        "the provider (and so the reader) was never consulted"
    );
}

// ---------------------------------------------------------------------------------------------
// tasks 2 to 5 and 7 (CG7.2): references, tests_for, neighbours; the enum pins

#[test]
fn inline_test_references_are_optional_and_test_associations_remain_available() {
    let scratch = Scratch::new("inline-test-references");
    let mut graph = Synth::default();
    graph.file("src/lib.rs");
    let target = graph.symbol("target().", "Function", "target", "src/lib.rs");
    let production = graph.symbol("production().", "Function", "production", "src/lib.rs");
    let test = graph.symbol(
        "tests/test_target().",
        "Function",
        "test_target",
        "src/lib.rs",
    );
    graph
        .symbols
        .iter_mut()
        .find(|symbol| symbol.r#ref == test)
        .unwrap()
        .is_test = true;
    graph.occurrence(&target, "src/lib.rs", 10, 0, 8, &production);
    graph.occurrence(&target, "src/lib.rs", 20, 0, 8, &test);
    graph.edge(&target, EdgeType::TestedBy, &test);
    let api = synth_api(&scratch, &graph);

    let production_only = api.code_references(references(by_ref(&target))).unwrap();
    assert_eq!(production_only.count, 1);
    assert_eq!(production_only.by_enclosing[0].enclosing.r#ref, production);
    let all = api
        .code_references(CodeReferencesRequest {
            include_tests: true,
            ..references(by_ref(&target))
        })
        .unwrap();
    assert_eq!(all.count, 2);
    assert!(
        all.by_enclosing
            .iter()
            .any(|group| group.enclosing.r#ref == test)
    );
    let associated = api.code_tests_for(tests_for(by_ref(&target))).unwrap();
    assert_eq!(associated.tests.len(), 1);
    assert_eq!(associated.tests[0].test.r#ref, test);
    assert!(associated.tests[0].test.is_test);
    assert_eq!(
        api.code_tests_for(tests_for(by_ref(&production)))
            .unwrap()
            .tests,
        []
    );
}

/// Invented SCIP + committed Rust source -> real writer publication -> the public API.
/// No flags/edges are manually set, and this is not qualification of an actual producer.
#[test]
fn committed_rust_tests_publish_associations_and_preserve_mixed_file_production() {
    use kaban_core::process::InheritedFds;
    use protobuf::Message;
    use scip::types::{Document, Index, Occurrence, SymbolInformation, symbol_information::Kind};

    const SOURCE: &str = "pub fn target() {}\n\
pub fn production() { target(); }\n\
// Unicode 🦀 and an ordinary production item before an inline test\n\
#[cfg(test)] mod checks {\n\
    pub fn helper() { super::target(); }\n\
    #[test] fn inline() { super::target(); }\n\
}\n\
const É: &str = \"🦀\"; #[test] fn café() { target(); }\n\
pub fn untouched() {}\n\
#[cfg(any(test, feature = \"feature\"))]\n\
pub fn conditional() { target(); }\n\
// #[test] fn negative() { target(); } -- not an attribute\n\
pub fn negative() { target(); }\n\
use crate::target as production_target;\n\
#[cfg(test)] use crate::target as test_target;\n\
#[cfg(test)] const _: fn() = target;\n\
pub mod production_module { #[cfg(test)] use crate::target as nested; }\n";
    const INTEGRATION: &str = "#[test] fn integration() { target(); }\n";
    let symbol = |name: &str| {
        let descriptor = if name.is_ascii() {
            name.to_owned()
        } else {
            format!("`{name}`")
        };
        format!("kaban-rust-scip cargo invented 0.1.0 lib/{descriptor}().")
    };
    let target = symbol("target");
    let document = |path: &str, source: &str, definitions: &[(&str, usize)]| {
        let mut doc = Document {
            relative_path: path.into(),
            language: "rust".into(),
            ..Default::default()
        };
        let lines: Vec<_> = source.lines().collect();
        for &(name, line) in definitions {
            let text = lines[line];
            let col = text.find(&format!("fn {name}(")).unwrap() + 3;
            let coordinate = |value| i32::try_from(value).unwrap();
            doc.symbols.push(SymbolInformation {
                symbol: symbol(name),
                kind: Kind::Function.into(),
                ..Default::default()
            });
            doc.occurrences.push(Occurrence {
                symbol: symbol(name),
                symbol_roles: 1,
                range: vec![
                    coordinate(line),
                    coordinate(col),
                    coordinate(col + name.len()),
                ],
                enclosing_range: vec![coordinate(line), 0, coordinate(text.len())],
                ..Default::default()
            });
            if let Some(col) = text.rfind("target();") {
                doc.occurrences.push(Occurrence {
                    symbol: target.clone(),
                    range: vec![coordinate(line), coordinate(col), coordinate(col + 6)],
                    ..Default::default()
                });
            }
        }
        if path == "src/lib.rs" {
            // Reference-only items deliberately have no definition/encloser of their own.
            // Production and test-only imports share both the target and file attribution.
            for (line, text) in lines.iter().enumerate().skip(13) {
                let col = text
                    .find("::target")
                    .map_or_else(|| text.rfind("target").unwrap(), |col| col + 2);
                doc.occurrences.push(Occurrence {
                    symbol: target.clone(),
                    symbol_roles: 2 | 8, // Producer Import + ReadAccess must survive.
                    range: vec![
                        i32::try_from(line).unwrap(),
                        i32::try_from(col).unwrap(),
                        i32::try_from(col + 6).unwrap(),
                    ],
                    ..Default::default()
                });
            }
        }
        // A hostile/mismatched producer's document text must not control classification.
        doc.text = "#[test] fn production() {}".into();
        doc
    };
    let scratch = Scratch::new("committed-rust-classification");
    let repo = scratch.repo();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::create_dir_all(repo.join("packages/example/tests")).unwrap();
    std::fs::write(repo.join("src/lib.rs"), SOURCE).unwrap();
    std::fs::write(
        repo.join("packages/example/tests/integration.rs"),
        INTEGRATION,
    )
    .unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "invented Rust source"]);
    let head = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(&repo)
        .output()
        .unwrap();
    assert!(head.status.success());
    let commit = Commit::parse(String::from_utf8(head.stdout).unwrap().trim()).unwrap();
    // Neither the dirty checkout nor the producer's text is the committed source.
    std::fs::write(repo.join("src/lib.rs"), "#[test] fn production() {}\n").unwrap();
    let index = Index {
        documents: vec![
            document(
                "src/lib.rs",
                SOURCE,
                &[
                    ("target", 0),
                    ("production", 1),
                    ("helper", 4),
                    ("inline", 5),
                    ("café", 7),
                    ("untouched", 8),
                    ("conditional", 10),
                    ("negative", 12),
                ],
            ),
            document(
                "packages/example/tests/integration.rs",
                INTEGRATION,
                &[("integration", 0)],
            ),
        ],
        ..Default::default()
    };
    let policy = canary_policy();
    let gated = decode_and_gate(&index.write_to_bytes().unwrap(), &policy).unwrap();
    let admitted = admit_index(&gated);
    let rows = Rows::derive_committed_with(
        &admitted,
        None,
        &policy,
        &repo,
        &commit,
        InheritedFds::Tolerate,
    )
    .unwrap();
    let marked: BTreeSet<_> = rows
        .symbols()
        .iter()
        .filter(|s| s.is_test && s.kind != NodeKind::File)
        .map(|s| s.name.as_str())
        .collect();
    assert_eq!(
        marked,
        BTreeSet::from(["helper", "inline", "café", "integration"])
    );
    assert!(
        !rows
            .files()
            .iter()
            .find(|f| f.path == "src/lib.rs")
            .unwrap()
            .is_test
    );
    assert!(
        rows.files()
            .iter()
            .find(|f| f.path.ends_with("integration.rs"))
            .unwrap()
            .is_test
    );
    let api = api_without_provider(&scratch, &repo);
    let dir = scratch.codegraph_dir();
    let meta = Meta {
        commit,
        repo_id: REPO_ID.into(),
        policy_version: api.policy_version(),
        provenance: Provenance {
            isolation: IsolationTier::None,
            isolation_detail: "invented consumer fixture".into(),
            resources: Vec::new(),
            container_image_digest: None,
        },
    };
    let path = writer::publish(&dir, &rows, &meta, &policy).unwrap();
    let api = api.with_snapshot_provider(dir, Arc::new(FixedSnapshot::new(path)));
    let target_ref = format!("scip:{target}");
    let refs = api
        .code_references(references(by_ref(&target_ref)))
        .unwrap();
    assert_eq!(
        refs.count, 4,
        "test-only reference items must not escape filtering"
    );
    let names: BTreeSet<_> = refs
        .by_enclosing
        .iter()
        .map(|g| g.enclosing.name.as_str())
        .collect();
    assert_eq!(
        names,
        BTreeSet::from(["production", "conditional", "negative", "lib.rs"])
    );
    assert!(refs.by_enclosing.iter().all(|g| !g.enclosing.is_test));
    let all = api
        .code_references(CodeReferencesRequest {
            include_tests: true,
            ..references(by_ref(&target_ref))
        })
        .unwrap();
    assert_eq!(
        all.count, 11,
        "classification must not drop reference sites"
    );
    let file_sites = all
        .by_enclosing
        .iter()
        .find(|g| g.enclosing.kind == "file")
        .unwrap();
    assert!(
        !file_sites.enclosing.is_test,
        "mixed file must stay production"
    );
    assert_eq!(file_sites.sites.len(), 4);
    assert!(
        file_sites
            .sites
            .iter()
            .all(|s| s.roles.contains(&"Import".into()) && s.roles.contains(&"ReadAccess".into()))
    );
    let associated = api.code_tests_for(tests_for(by_ref(&target_ref))).unwrap();
    let tests: BTreeSet<_> = associated
        .tests
        .iter()
        .map(|t| t.test.name.as_str())
        .collect();
    assert_eq!(
        tests,
        BTreeSet::from(["helper", "inline", "café", "integration"])
    );
    assert!(associated.tests.iter().all(|t| t.test.is_test));
    assert_eq!(
        api.code_tests_for(tests_for(by_ref(&format!("scip:{}", symbol("untouched")))))
            .unwrap()
            .tests,
        []
    );
}

/// Copy trusted fixture data only; never execute any copied repository program.
fn copy_rust_sources(source: &Path, destination: &Path) {
    std::fs::create_dir_all(destination).unwrap();
    for entry in std::fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let target = destination.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_rust_sources(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

/// Accepted A8 bytes -> content gate -> committed-source derivation -> immutable
/// publication -> typed tools. This does not execute or qualify the Rust runner.
#[test]
fn actual_rust_scip_publishes_original_oracle_through_public_tools() {
    use kaban_core::process::InheritedFds;

    let mut prior_keys = None;
    let mut prior_commit = None;
    for variant in ["minimal", "workspace"] {
        for checkout in [0, 1] {
            let scratch = Scratch::new(&format!("actual-rust-{variant}-{checkout}"));
            let repo = scratch.repo();
            let fixture = fixtures_dir().join("rust-drivers").join(variant);
            copy_rust_sources(&fixture.join("source"), &repo);
            std::fs::write(repo.join("checkout.txt"), checkout.to_string()).unwrap();
            git(&repo, &["add", "-A"]);
            git(&repo, &["commit", "-q", "-m", "trusted actual Rust inputs"]);
            let head = Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(&repo)
                .output()
                .unwrap();
            assert!(head.status.success());
            let commit = Commit::parse(String::from_utf8(head.stdout).unwrap().trim()).unwrap();
            let policy = canary_policy();
            let bytes = std::fs::read(fixture.join("index.scip")).unwrap();
            let gated = decode_and_gate(&bytes, &policy).unwrap();
            let admitted = admit_index(&gated);
            let rows = Rows::derive_committed_with(
                &admitted,
                None,
                &policy,
                &repo,
                &commit,
                InheritedFds::Tolerate,
            )
            .unwrap();
            assert!(rows.edges().iter().all(|e| e.r#type.as_str() != "CALLS"));
            let local = rows
                .symbols()
                .iter()
                .find(|s| {
                    s.kind == NodeKind::Local && s.def.is_some_and(|r| r.sl == 46 && r.sc == 8)
                })
                .map(|s| s.r#ref.clone());
            let keys: BTreeSet<_> = rows
                .symbols()
                .iter()
                .filter_map(|s| s.key.clone())
                .collect();
            if checkout == 0 {
                prior_keys = Some(keys);
                prior_commit = Some(commit.clone());
            } else {
                assert_ne!(prior_commit.as_ref().unwrap(), &commit);
                assert_eq!(prior_keys.as_ref().unwrap(), &keys);
            }
            let api = api_without_provider(&scratch, &repo);
            let dir = scratch.codegraph_dir();
            let meta = Meta {
                commit,
                repo_id: REPO_ID.into(),
                policy_version: api.policy_version(),
                provenance: Provenance {
                    isolation: IsolationTier::None,
                    isolation_detail: "A8 data consumer only".into(),
                    resources: Vec::new(),
                    container_image_digest: None,
                },
            };
            let path = writer::publish(&dir, &rows, &meta, &policy).unwrap();
            let api = api.with_snapshot_provider(dir, Arc::new(FixedSnapshot::new(path)));
            if variant == "minimal" {
                assert_actual_rust_minimal(&api);
                let local = local.unwrap();
                let refs = api.code_references(references(by_ref(&local))).unwrap();
                assert_eq!(refs.count, 1);
                assert_eq!(
                    range4(Some(refs.by_enclosing[0].sites[0].site)),
                    Some((47, 4, 47, 9))
                );
            } else {
                let generated = "scip:kaban-rust-scip cargo rust-qualification-app 0.1.0 `lib:rust_qualification_app:app/src/lib.rs`/generated_value().";
                let def = api
                    .code_definition(definition(by_ref(generated)))
                    .unwrap()
                    .definition;
                assert_eq!(def.path.as_deref(), Some("app/src/lib.rs"));
                assert_eq!(range4(def.range), Some((2, 0, 2, 33)));
                let refs = api.code_references(references(by_ref(generated))).unwrap();
                assert_eq!(refs.count, 1);
                assert_eq!(refs.by_enclosing[0].enclosing.name, "answer");
                let support = "scip:kaban-rust-scip cargo qualification-support 0.1.0 `lib:qualification_support:support/src/lib.rs`/base_value().";
                let refs = api.code_references(references(by_ref(support))).unwrap();
                assert_eq!(refs.count, 1);
                assert_eq!(refs.by_enclosing[0].enclosing.name, "answer");
            }
        }
    }
}

fn assert_actual_rust_minimal(api: &KabanApi) {
    let symbol = |descriptor: &str| {
        format!(
            "scip:kaban-rust-scip cargo rust-qualification-minimal 0.1.0 `lib:rust_qualification_minimal:src/lib.rs`/{descriptor}"
        )
    };
    let driver = symbol("DatabaseDriver#");
    let implementations = api
        .code_implementers(implementers(by_ref(&driver)))
        .unwrap();
    let names: BTreeSet<_> = implementations
        .results
        .iter()
        .map(|n| n.name.as_str())
        .collect();
    assert_eq!(names, BTreeSet::from(["Postgres", "Sqlite"]));
    assert_eq!(implementations.count, 2);
    let method = symbol("DatabaseDriver#execute().");
    assert_eq!(
        api.code_implementers(implementers(by_ref(&method)))
            .unwrap()
            .count,
        0
    );
    // q1-1 pin (this fixture has no nested module around an impl, so it never showed the
    // defect): the file's members are what no same-file symbol encloses: the crate module
    // (its ranges are the file), the items, the methods of the top-level `impl` blocks
    // (nothing encloses an impl); not the trait's own `execute` (inside the trait body), not
    // `production_works` (inside `mod tests`), not the locals
    let lib = api
        .code_members(members(by_path("src/lib.rs"), &[]))
        .unwrap();
    assert_eq!(
        lib.results
            .iter()
            .map(|n| n.name.as_str())
            .collect::<Vec<_>>(),
        [
            "crate",
            "DatabaseDriver",
            "Postgres",
            "Sqlite",
            "NotADriver",
            "execute",
            "execute",
            "new",
            "production",
            "middle",
            "leaf",
            "function_value",
            "café",
            "tests",
            "fresh",
        ],
        "src/lib.rs members in range order"
    );
    let members = api
        .code_neighbours(neighbours(by_ref(&method), 1, &["IMPLEMENTS_MEMBER"]))
        .unwrap();
    assert_eq!(
        members
            .edges
            .iter()
            .map(|edge| (edge.from.clone(), edge.r#type.as_str(), edge.to.clone()))
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([
            (
                symbol("impl#[Postgres][DatabaseDriver]execute()."),
                "IMPLEMENTS_MEMBER",
                method.clone()
            ),
            (
                symbol("impl#[Sqlite][DatabaseDriver]execute()."),
                "IMPLEMENTS_MEMBER",
                method.clone()
            ),
        ])
    );
    for name in ["production", "middle", "leaf", "function_value", "`café`"] {
        let def = api
            .code_definition(definition(by_ref(&symbol(&format!("{name}().")))))
            .unwrap()
            .definition;
        assert!(!def.is_test, "production definition: {name}");
    }
    let constructor = api
        .code_definition(definition(by_ref(&symbol("Postgres#new()."))))
        .unwrap()
        .definition;
    assert!(!constructor.is_test);
    let cafe = api
        .code_definition(definition(by_ref(&symbol("`café`()."))))
        .unwrap()
        .definition;
    assert_eq!(range4(cafe.range), Some((45, 7, 45, 12)));
    let production = symbol("production().");
    let refs = api
        .code_references(references(by_ref(&production)))
        .unwrap();
    assert_eq!(refs.count, 1);
    assert!(!refs.by_enclosing[0].enclosing.is_test);
    let all = api
        .code_references(CodeReferencesRequest {
            include_tests: true,
            ..references(by_ref(&production))
        })
        .unwrap();
    assert_eq!(all.count, 3);
    let tests = api.code_tests_for(tests_for(by_ref(&production))).unwrap();
    assert_eq!(
        tests
            .tests
            .iter()
            .map(|t| t.test.name.as_str())
            .collect::<BTreeSet<_>>(),
        BTreeSet::from(["production_works", "integration_works"])
    );
    assert!(tests.tests.iter().all(|t| t.test.is_test));
    assert_eq!(
        api.code_tests_for(tests_for(by_ref(&symbol("NotADriver#"))))
            .unwrap()
            .tests,
        []
    );
    for (target, expected) in [
        ("middle().", BTreeSet::from(["production"])),
        ("leaf().", BTreeSet::from(["middle", "function_value"])),
    ] {
        let refs = api
            .code_references(references(by_ref(&symbol(target))))
            .unwrap();
        assert_eq!(
            refs.by_enclosing
                .iter()
                .map(|g| g.enclosing.name.as_str())
                .collect::<BTreeSet<_>>(),
            expected
        );
    }
    let external = "scip:rust-analyzer cargo core https://github.com/rust-lang/rust/library/core mem/size_of().";
    let refs = api.code_references(references(by_ref(external))).unwrap();
    assert_eq!(refs.count, 1);
    assert_eq!(refs.by_enclosing[0].enclosing.name, "café");
}

/// edges.txt:5: `DatabaseDriver#` is referenced once, from `drivers/wrapped.go`, where the
/// embedded-field occurrence has no enclosing symbol, so the group's enclosing is the file node
/// (G1-F4; the plan's `Wrapped#` is deviation (a) of the brief); `HttpDriver` never appears and
/// `include_tests` adds nothing. edges.txt:90,96,100: `PostgresDriver#` is referenced from
/// `Close()`, `Open()` and, in the test file only when asked, `TestPostgresOpen()`.
#[test]
fn tool_code_references_golden_go_drivers() {
    let s = Scratch::new("go-refs");
    let api = go_api(&s);
    let r = api
        .code_references(references(by_label_name("Interface", "DatabaseDriver")))
        .unwrap();
    assert_eq!(group_refs(&r), vec!["file:drivers/wrapped.go".to_owned()]);
    let g = &r.by_enclosing[0];
    assert_eq!(
        (g.enclosing.kind.as_str(), g.enclosing.label.as_str()),
        ("file", "File")
    );
    assert_eq!(g.enclosing.path.as_deref(), Some("drivers/wrapped.go"));
    assert_eq!(g.sites.len(), 1);
    let site = &g.sites[0];
    assert_eq!(site.path, "drivers/wrapped.go");
    assert!(
        site.roles.iter().all(|r| ROLE_VALUES.contains(&r.as_str())),
        "{:?}",
        site.roles
    );
    assert!(
        !site.roles.iter().any(|r| r == "Definition"),
        "a reference site is never the definition: {:?}",
        site.roles
    );
    assert!(site.site.end_col > site.site.start_col || site.site.end_line > site.site.start_line);
    assert_eq!((r.count, r.truncated), (1, false));
    assert_eq!(r.follow_ups, Vec::<FollowUp>::new());
    assert_eq!(r.snapshot.commit, GO_COMMIT);
    assert_eq!(r.snapshot.policy, "current");
    let text = serde_json::to_string(&r).unwrap();
    assert!(!text.contains("HttpDriver"), "{text}");
    // the same set with test files included: no test file references the interface
    let with_tests = api
        .code_references(CodeReferencesRequest {
            target: by_symbol(&format!("{GO_KEY}DatabaseDriver#")),
            include_tests: true,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(group_refs(&with_tests), group_refs(&r));
    assert_eq!(with_tests.count, 1);
    // PostgresDriver#: its two methods by (path, ref), the test function only when asked
    let pg_ref = format!("{GO}PostgresDriver#");
    let pg = api.code_references(references(by_ref(&pg_ref))).unwrap();
    assert_eq!(
        group_refs(&pg),
        vec![
            format!("{GO}PostgresDriver#Close()."),
            format!("{GO}PostgresDriver#Open()."),
        ]
    );
    assert_eq!((pg.count, pg.truncated), (2, false));
    assert!(
        pg.by_enclosing
            .iter()
            .all(|g| g.sites.len() == 1 && g.sites[0].path == "drivers/postgres.go")
    );
    let pg_tests = api
        .code_references(CodeReferencesRequest {
            target: by_ref(&pg_ref),
            include_tests: true,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(
        group_refs(&pg_tests),
        vec![
            format!("{GO}PostgresDriver#Close()."),
            format!("{GO}PostgresDriver#Open()."),
            format!("{GO}TestPostgresOpen()."),
        ]
    );
    assert_eq!(pg_tests.count, 3);
    let t = &pg_tests.by_enclosing[2];
    assert!(t.enclosing.is_test);
    assert_eq!(t.sites[0].path, "drivers/postgres_test.go");
    // the limit counts sites
    let one = api
        .code_references(CodeReferencesRequest {
            target: by_ref(&pg_ref),
            limit: Some(1),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(
        (one.count, one.truncated, group_refs(&one)),
        (1, true, vec![format!("{GO}PostgresDriver#Close().")])
    );
}

/// edges.txt:98, :3, :85 (G1-F6): the reference form from a method and from its type, the
/// naming form from the file (by `path` and by `file:` ref), an empty list for an untested type.
#[test]
fn tool_code_tests_for_golden_go_drivers() {
    let s = Scratch::new("go-tests-for");
    let api = go_api(&s);
    let test_fn = format!("{GO}TestPostgresOpen().");
    let open = api
        .code_tests_for(tests_for(by_symbol(&format!(
            "{GO_KEY}PostgresDriver#Open()."
        ))))
        .unwrap();
    assert_eq!(
        link_rows(&open),
        vec![(test_fn.clone(), "reference".to_owned(), false)]
    );
    assert!(open.tests[0].test.is_test);
    assert_eq!(
        open.tests[0].test.path.as_deref(),
        Some("drivers/postgres_test.go")
    );
    assert_eq!(open.tests[0].test.label, "Function");
    assert!(!open.truncated);
    assert_eq!(open.follow_ups, Vec::<FollowUp>::new());
    assert_eq!(open.snapshot.commit, GO_COMMIT);
    assert_eq!(open.snapshot.policy, "current");
    // the naming form: the file's test file, flagged heuristic
    let file = api
        .code_tests_for(tests_for(by_path("drivers/postgres.go")))
        .unwrap();
    assert_eq!(
        link_rows(&file),
        vec![(
            "file:drivers/postgres_test.go".to_owned(),
            "naming".to_owned(),
            true
        )]
    );
    assert_eq!(file.tests[0].test.kind, "file");
    assert!(file.tests[0].test.is_test);
    // the type: the reference form (edges.txt:85)
    let pg = api
        .code_tests_for(tests_for(by_ref(&format!("{GO}PostgresDriver#"))))
        .unwrap();
    assert_eq!(
        link_rows(&pg),
        vec![(test_fn, "reference".to_owned(), false)]
    );
    // a `file:` ref is the naming form too; an untested type is an empty list, not an error
    let by_file_ref = api
        .code_tests_for(tests_for(by_ref("file:drivers/postgres.go")))
        .unwrap();
    assert_eq!(link_rows(&by_file_ref), link_rows(&file));
    let none = api
        .code_tests_for(tests_for(by_symbol(&format!("{GO_KEY}MemDriver#"))))
        .unwrap();
    assert!(none.tests.is_empty() && !none.truncated);
}

/// edges.txt:92,98 (hop 1) and :75,141,85,161,162 (hop 2): depth 2 from `PostgresDriver#Open()`
/// over `IMPLEMENTS_MEMBER` and `TESTED_BY`, the start first, then `(depth, ref)`, each edge
/// once; the list limit cuts the nodes after the start and keeps the edges between survivors.
#[test]
fn tool_code_neighbours_golden_go_drivers() {
    let s = Scratch::new("go-neighbours");
    let api = go_api(&s);
    let open = format!("{GO}PostgresDriver#Open().");
    let go_std = "scip:scip-go gomod github.com/golang/go/src go1.22 ";
    let types = ["IMPLEMENTS_MEMBER", "TESTED_BY"];
    let r = api
        .code_neighbours(neighbours(by_ref(&open), 2, &types))
        .unwrap();
    let node_refs: Vec<String> = r.nodes.iter().map(|n| n.r#ref.clone()).collect();
    let want_nodes = vec![
        open.clone(),
        format!("{GO}DatabaseDriver#Open."),
        format!("{GO}TestPostgresOpen()."),
        format!("{GO}MemDriver#Open()."),
        format!("{GO}PostgresDriver#"),
        format!("{GO_SQLITE}Driver#Open()."),
        format!("{go_std}testing/"),
        format!("{go_std}testing/T#"),
    ];
    assert_eq!(node_refs, want_nodes);
    assert_eq!(r.nodes[0].name, "Open");
    assert!(r.nodes[6].external, "testing/ is a placeholder");
    let got: Vec<(String, String, String)> = r
        .edges
        .iter()
        .map(|e| (e.from.clone(), e.r#type.clone(), e.to.clone()))
        .collect();
    assert_eq!(got.len(), 7, "each edge once: {got:?}");
    let want_edges: BTreeSet<(String, String, String)> = [
        (
            open.clone(),
            "IMPLEMENTS_MEMBER",
            format!("{GO}DatabaseDriver#Open."),
        ),
        (
            open.clone(),
            "TESTED_BY",
            format!("{GO}TestPostgresOpen()."),
        ),
        (
            format!("{GO}MemDriver#Open()."),
            "IMPLEMENTS_MEMBER",
            format!("{GO}DatabaseDriver#Open."),
        ),
        (
            format!("{GO_SQLITE}Driver#Open()."),
            "IMPLEMENTS_MEMBER",
            format!("{GO}DatabaseDriver#Open."),
        ),
        (
            format!("{GO}PostgresDriver#"),
            "TESTED_BY",
            format!("{GO}TestPostgresOpen()."),
        ),
        (
            format!("{go_std}testing/"),
            "TESTED_BY",
            format!("{GO}TestPostgresOpen()."),
        ),
        (
            format!("{go_std}testing/T#"),
            "TESTED_BY",
            format!("{GO}TestPostgresOpen()."),
        ),
    ]
    .into_iter()
    .map(|(from, t, to)| (from, t.to_owned(), to))
    .collect();
    assert_eq!(got.into_iter().collect::<BTreeSet<_>>(), want_edges);
    assert!(!r.truncated);
    assert_eq!(r.snapshot.commit, GO_COMMIT);
    assert_eq!(r.snapshot.policy, "current");
    // deterministic: the same call gives the same bytes
    let again = api
        .code_neighbours(neighbours(by_ref(&open), 2, &types))
        .unwrap();
    assert_eq!(
        serde_json::to_string(&again).unwrap(),
        serde_json::to_string(&r).unwrap()
    );
    // a limit of 2 keeps the start and the two hop-1 nodes, and only the edges between them
    let two = api
        .code_neighbours(CodeNeighboursRequest {
            target: by_ref(&open),
            depth: 2,
            edge_types: types.iter().map(|t| (*t).to_owned()).collect(),
            include_locals: false,
            limit: Some(2),
            current_path: None,
            include_snippets: false,
        })
        .unwrap();
    assert_eq!(
        two.nodes
            .iter()
            .map(|n| n.r#ref.clone())
            .collect::<Vec<_>>(),
        want_nodes[..3]
    );
    assert_eq!(two.edges.len(), 2);
    assert!(two.truncated);
    // depth 1 over every type: Open()'s direct edges only (edges.txt:80,91-98,101), each once;
    // the two REFERENCES to its own locals are hidden and counted (G4) unless asked for
    let one = api
        .code_neighbours(neighbours(by_ref(&open), 1, &[]))
        .unwrap();
    assert_eq!((one.nodes.len(), one.edges.len()), (7, 8));
    assert_eq!(one.locals_hidden, 2);
    assert!(one.edges.iter().all(|e| e.from == open || e.to == open));
    assert!(!one.truncated);
    let with_locals = api
        .code_neighbours(CodeNeighboursRequest {
            include_locals: true,
            ..neighbours(by_ref(&open), 1, &[])
        })
        .unwrap();
    assert_eq!((with_locals.nodes.len(), with_locals.edges.len()), (9, 10));
    assert_eq!(with_locals.locals_hidden, 0);
    assert_eq!(
        with_locals
            .nodes
            .iter()
            .filter(|n| n.kind == "local")
            .count(),
        2
    );
}

/// G1-F12 traversal bounds: a hub with 201 implementers is cut at 200 edges per hub and flagged;
/// the list limit then cuts the nodes after the start (default 50, max 200).
#[test]
fn neighbours_bounded_per_hub_and_by_limit() {
    let s = Scratch::new("neighbours-bounded");
    let synth = implementers_snapshot(201);
    let api = synth_api(&s, &synth);
    let iface = "scip-go gomod example.com/synth . `example.com/synth/pkg`/Iface#";
    let r = api
        .code_neighbours(CodeNeighboursRequest {
            target: by_symbol(iface),
            depth: 1,
            edge_types: vec!["IMPLEMENTS".to_owned()],
            include_locals: false,
            limit: Some(200),
            current_path: None,
            include_snippets: false,
        })
        .unwrap();
    assert_eq!(
        (r.nodes.len(), r.edges.len(), r.truncated),
        (201, 200, true)
    );
    assert_eq!(r.nodes[0].name, "Iface");
    assert!(
        r.edges
            .iter()
            .all(|e| e.r#type == "IMPLEMENTS" && e.to == r.nodes[0].r#ref)
    );
    let d = api
        .code_neighbours(neighbours(by_symbol(iface), 1, &["IMPLEMENTS"]))
        .unwrap();
    assert_eq!((d.nodes.len(), d.edges.len(), d.truncated), (51, 50, true));
    // a request above the maximum is the maximum
    let big = api
        .code_neighbours(CodeNeighboursRequest {
            target: by_symbol(iface),
            depth: 1,
            edge_types: vec!["IMPLEMENTS".to_owned()],
            include_locals: false,
            limit: Some(1_000),
            current_path: None,
            include_snippets: false,
        })
        .unwrap();
    assert_eq!(big.nodes.len(), 201);
}

/// G1-F12: a depth outside `1..=3` is the selector class with no number in the text; an unknown
/// edge type is the fixed filter text; the bounds themselves are accepted.
#[test]
fn neighbours_depth_out_of_range() {
    let s = Scratch::new("neighbours-depth");
    let api = go_api(&s);
    let open = format!("{GO_KEY}PostgresDriver#Open().");
    for depth in [0, 4, u8::MAX] {
        let text = class_text(
            api.code_neighbours(neighbours(by_symbol(&open), depth, &[]))
                .unwrap_err(),
            "selector_invalid",
        );
        assert!(
            !text.chars().any(|c| c.is_ascii_digit()),
            "no number: {text}"
        );
    }
    let e = api
        .code_neighbours(neighbours(by_symbol(&open), 1, &["CALLS"]))
        .unwrap_err();
    assert_eq!(e.to_string(), "edge_types[] must name edge types");
    for depth in [1, 3] {
        api.code_neighbours(neighbours(by_symbol(&open), depth, &["TESTED_BY"]))
            .unwrap();
    }
}

/// The registry's closed sets are the crate's: `EDGE_TYPE_VALUES` is `EdgeType::ALL` by name and
/// order (each name parses back), `ROLE_VALUES` the seven SCIP roles in bit order,
/// `MATCHED_BY_VALUES` the two derivation forms.
#[test]
fn edge_type_values_match_schema() {
    let all: Vec<&str> = EdgeType::ALL.iter().map(|t| t.as_str()).collect();
    assert_eq!(EDGE_TYPE_VALUES, all.as_slice());
    for (name, t) in EDGE_TYPE_VALUES.iter().zip(EdgeType::ALL) {
        assert_eq!(name.parse::<EdgeType>().ok(), Some(t));
    }
    assert_eq!(ROLE_VALUES.len(), 7);
    assert_eq!(ROLE_VALUES[0], "Definition");
    assert_eq!(ROLE_VALUES[6], "ForwardDefinition");
    assert_eq!(
        MATCHED_BY_VALUES,
        &[
            kaban_codegraph::reader::MatchedBy::Reference.as_str(),
            kaban_codegraph::reader::MatchedBy::Naming.as_str(),
        ]
    );
}

/// SCR-2 (select by key, `policy: stale`); ts edges.txt:37,53,125,141: four enclosing nodes
/// reference the interface, one site each, by `(path, ref)`.
#[test]
fn tool_code_references_golden_ts_drivers() {
    let s = Scratch::new("ts-refs");
    let api = ts_api(&s);
    let r = api
        .code_references(references(by_symbol(&format!(
            "{TS_KEY}`driver.ts`/DatabaseDriver#"
        ))))
        .unwrap();
    assert_eq!(
        group_refs(&r),
        vec![
            format!("{TS}`mem.ts`/"),
            format!("{TS}`mem.ts`/MemDriver#"),
            format!("{TS}`postgres.ts`/"),
            format!("{TS}`postgres.ts`/PostgresDriver#"),
        ]
    );
    assert_eq!((r.count, r.truncated), (4, false));
    assert!(r.by_enclosing.iter().all(|g| g.sites.len() == 1));
    assert_eq!(r.by_enclosing[0].sites[0].path, "src/mem.ts");
    assert_eq!(r.by_enclosing[2].sites[0].path, "src/postgres.ts");
    assert!(
        r.by_enclosing
            .iter()
            .all(|g| g.enclosing.language.as_deref() == Some("typescript"))
    );
    assert_eq!(r.snapshot.commit, TS_COMMIT);
    assert_eq!(r.snapshot.policy, "stale");
}

/// ts edges.txt:1: the naming form links the file to its `.test.ts` neighbour.
#[test]
fn tool_code_tests_for_golden_ts_drivers() {
    let s = Scratch::new("ts-tests-for");
    let api = ts_api(&s);
    let r = api
        .code_tests_for(tests_for(by_path("src/postgres.ts")))
        .unwrap();
    assert_eq!(
        link_rows(&r),
        vec![(
            "file:src/postgres.test.ts".to_owned(),
            "naming".to_owned(),
            true
        )]
    );
    assert_eq!(r.tests[0].test.kind, "file");
    assert_eq!(r.snapshot.policy, "stale");
}

/// OD-G1-3: the reference form has no `TESTED_BY` edge in the TypeScript fixture (its one test is
/// an anonymous `test()` callback, ts edges.txt:109-114), so the expected row is pinned here and
/// the test is skipped until the fixture or the derivation carries it (RV-10: visible, not
/// silent).
#[test]
#[ignore = "OD-G1-3: ts fixture has no reference-form TESTED_BY"]
fn tool_code_tests_for_reference_form_ts_drivers() {
    let s = Scratch::new("ts-tests-ref");
    let api = ts_api(&s);
    let r = api
        .code_tests_for(tests_for(by_symbol(&format!(
            "{TS_KEY}`postgres.ts`/PostgresDriver#open()."
        ))))
        .unwrap();
    assert_eq!(
        link_rows(&r),
        vec![(
            format!("{TS}`postgres.test.ts`/test()."),
            "reference".to_owned(),
            false
        )]
    );
}

/// ts edges.txt:63,151: depth 2 from the interface method over `IMPLEMENTS_MEMBER` reaches its
/// two implementations and nothing beyond them.
#[test]
fn tool_code_neighbours_golden_ts_drivers() {
    let s = Scratch::new("ts-neighbours");
    let api = ts_api(&s);
    let open = format!("{TS}`driver.ts`/DatabaseDriver#open().");
    let r = api
        .code_neighbours(neighbours(
            by_symbol(&format!("{TS_KEY}`driver.ts`/DatabaseDriver#open().")),
            2,
            &["IMPLEMENTS_MEMBER"],
        ))
        .unwrap();
    assert_eq!(
        r.nodes.iter().map(|n| n.r#ref.clone()).collect::<Vec<_>>(),
        vec![
            open.clone(),
            format!("{TS}`mem.ts`/MemDriver#open()."),
            format!("{TS}`postgres.ts`/PostgresDriver#open()."),
        ]
    );
    assert_eq!(r.edges.len(), 2);
    assert!(
        r.edges
            .iter()
            .all(|e| e.r#type == "IMPLEMENTS_MEMBER" && e.to == open)
    );
    assert!(!r.truncated);
    assert_eq!(r.snapshot.policy, "stale");
}

// ---------------------------------------------------------------------------------------------
// the tail (CG7.2 task 9): the references and tests_for limits on synthetic snapshots

/// `Sym#` referenced from `F<i>()` defined in `p<n-1-i>.go`, so the `(path, ref)` group order is
/// the reverse of insertion; `F<n-1>()` (in `p000.go`) holds two sites, inserted with the later
/// range first and with several role bits each.
fn references_snapshot(n: usize) -> Synth {
    let mut s = Synth::default();
    s.file("sym.go");
    let sym = s.symbol("Sym#", "Struct", "Sym", "sym.go");
    for i in 0..n {
        let path = format!("p{:03}.go", n - 1 - i);
        s.file(&path);
        let f = s.symbol(
            &format!("F{i:03}()."),
            "Function",
            &format!("F{i:03}"),
            &path,
        );
        if i == n - 1 {
            // ReadAccess | Test, then Import | WriteAccess | ForwardDefinition at an earlier line
            s.occurrence(&sym, &path, 20, 5, 8 | 32, &f);
            s.occurrence(&sym, &path, 12, 2, 2 | 4 | 64, &f);
        } else {
            s.occurrence(&sym, &path, 10, 4, 8, &f);
        }
    }
    s
}

/// RV-7: the complete site set is grouped and ordered before the limit; the limit counts sites,
/// cutting inside the last group kept; roles decode every bit set, in bit order.
#[test]
fn references_sorted_before_truncation() {
    let s = Scratch::new("refs-sorted");
    let n = 60;
    let api = synth_api(&s, &references_snapshot(n));
    let sym = "scip-go gomod example.com/synth . `example.com/synth/pkg`/Sym#";
    let names = |r: &CodeReferencesResponse| -> Vec<String> {
        r.by_enclosing
            .iter()
            .map(|g| g.enclosing.name.clone())
            .collect()
    };
    // Standard Test-role sites are excluded even when their file/encloser is production.
    let production = api
        .code_references(CodeReferencesRequest {
            target: by_symbol(sym),
            limit: Some(200),
            ..Default::default()
        })
        .unwrap();
    assert_eq!((production.count, production.truncated), (60, false));
    assert!(
        production
            .by_enclosing
            .iter()
            .flat_map(|g| &g.sites)
            .all(|s| !s.roles.iter().any(|role| role == "Test"))
    );
    // The original complete-set ordering/role-decoding contract explicitly includes tests.
    // The default limit keeps 50 of 61 sites, over the first 49 groups by path.
    let r = api
        .code_references(CodeReferencesRequest {
            include_tests: true,
            ..references(by_symbol(sym))
        })
        .unwrap();
    assert_eq!((r.count, r.truncated, r.by_enclosing.len()), (50, true, 49));
    assert_eq!(r.by_enclosing[0].enclosing.name, "F059");
    assert_eq!(r.by_enclosing[0].enclosing.path.as_deref(), Some("p000.go"));
    assert_eq!(r.by_enclosing[1].enclosing.name, "F058");
    assert_eq!(r.by_enclosing[48].enclosing.name, "F011");
    // everything: 60 groups in (path, ref) order, 61 sites, range order and roles inside a group
    let all = api
        .code_references(CodeReferencesRequest {
            target: by_symbol(sym),
            limit: Some(200),
            include_tests: true,
            ..Default::default()
        })
        .unwrap();
    assert_eq!((all.count, all.truncated), (61, false));
    let want: Vec<String> = (0..n).rev().map(|i| format!("F{i:03}")).collect();
    assert_eq!(names(&all), want);
    let first = &all.by_enclosing[0].sites;
    assert_eq!(
        first
            .iter()
            .map(|s| (s.site.start_line, s.roles.clone()))
            .collect::<Vec<_>>(),
        vec![
            (
                12,
                vec![
                    "Import".to_owned(),
                    "WriteAccess".to_owned(),
                    "ForwardDefinition".to_owned()
                ]
            ),
            (20, vec!["ReadAccess".to_owned(), "Test".to_owned()]),
        ]
    );
    assert!(all.by_enclosing[1..].iter().all(|g| g.sites.len() == 1
        && g.sites[0].roles == ["ReadAccess"]
        && g.sites[0].path == g.enclosing.path.clone().unwrap()));
    // a limit of 1 keeps the first site of the first group
    let one = api
        .code_references(CodeReferencesRequest {
            target: by_symbol(sym),
            limit: Some(1),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(
        (one.count, one.truncated, names(&one)),
        (1, true, vec!["F059".to_owned()])
    );
    assert_eq!(one.by_enclosing[0].sites[0].site.start_line, 12);
    // a limit above the maximum is the maximum, `0` is the default
    let zero = api
        .code_references(CodeReferencesRequest {
            target: by_symbol(sym),
            limit: Some(0),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(zero.count, 50);
}

/// `Sym#` tested by `Test<i>()` in `t<n-1-i>.go` (the reference form, 60 links).
fn tested_snapshot(n: usize) -> Synth {
    let mut s = Synth::default();
    s.file("sym.go");
    let sym = s.symbol("Sym#", "Struct", "Sym", "sym.go");
    for i in 0..n {
        let path = format!("t{:03}.go", n - 1 - i);
        s.file(&path);
        let t = s.symbol(
            &format!("Test{i:03}()."),
            "Function",
            &format!("Test{i:03}"),
            &path,
        );
        s.edge(&sym, EdgeType::TestedBy, &t);
    }
    s
}

/// RV-7 and G1-F12 on `code_tests_for`: links are ordered by `(path, ref)` before the cut; the
/// default limit is 50, `0` is the default, a request above 200 is 200.
#[test]
fn tests_for_limits() {
    let s = Scratch::new("tests-for-limits");
    let api = synth_api(&s, &tested_snapshot(60));
    let sym = "scip-go gomod example.com/synth . `example.com/synth/pkg`/Sym#";
    let names = |r: &CodeTestsForResponse| -> Vec<String> {
        r.tests.iter().map(|l| l.test.name.clone()).collect()
    };
    let r = api.code_tests_for(tests_for(by_symbol(sym))).unwrap();
    assert_eq!((r.tests.len(), r.truncated), (50, true));
    assert_eq!(names(&r)[0], "Test059");
    assert_eq!(names(&r)[49], "Test010");
    assert!(
        r.tests
            .iter()
            .all(|l| l.matched_by == "reference" && !l.heuristic)
    );
    for (limit, want_len, want_truncated) in [(Some(1_000), 60, false), (Some(0), 50, true)] {
        let r = api
            .code_tests_for(CodeTestsForRequest {
                target: by_symbol(sym),
                limit,
                current_path: None,
                include_snippets: false,
            })
            .unwrap();
        assert_eq!(
            (r.tests.len(), r.truncated),
            (want_len, want_truncated),
            "{limit:?}"
        );
    }
    let two = api
        .code_tests_for(CodeTestsForRequest {
            target: by_symbol(sym),
            limit: Some(2),
            current_path: None,
            include_snippets: false,
        })
        .unwrap();
    assert_eq!(
        names(&two),
        vec!["Test059".to_owned(), "Test058".to_owned()]
    );
    assert!(two.truncated);
}

/// `(test ref, how, via)` of every link, in order (TESTS-1).
fn tier_rows(r: &CodeTestsForResponse) -> Vec<(String, TestTier, Option<String>)> {
    r.tests
        .iter()
        .map(|l| (l.test.r#ref.clone(), l.how, l.via.clone()))
        .collect()
}

/// TESTS-1 (backlog 20, 24): a test one hop away through a caller is a `via` row naming the
/// caller, after the `direct` rows; a test that is itself a referrer is never an intermediate;
/// a test already linked directly is not repeated as `via`.
#[test]
fn tests_for_reaches_tests_one_hop_away_through_a_caller() {
    let scratch = Scratch::new("tests-for-via");
    let mut graph = Synth::default();
    graph.file("src/lib.rs").file("tests/process.rs");
    let target = graph.symbol("admit().", "Function", "admit", "src/lib.rs");
    let caller = graph.symbol("run().", "Function", "run", "src/lib.rs");
    let direct = graph.symbol(
        "tests/a_direct().",
        "Function",
        "a_direct",
        "tests/process.rs",
    );
    let via = graph.symbol("tests/b_via().", "Function", "b_via", "tests/process.rs");
    let both = graph.symbol("tests/c_both().", "Function", "c_both", "tests/process.rs");
    let module = graph.symbol("facade/", "Module", "facade", "src/lib.rs");
    let via_module = graph.symbol(
        "tests/d_via_module().",
        "Function",
        "d_via_module",
        "tests/process.rs",
    );
    for test in [&direct, &via, &both, &via_module] {
        graph
            .symbols
            .iter_mut()
            .find(|s| &s.r#ref == test)
            .unwrap()
            .is_test = true;
    }
    // the caller references the target; the tests reference the caller (and `direct`, `both`
    // the target itself); `direct` is also a referrer of the target, so it must not act as a
    // caller that links its own tests
    graph.edge(&caller, EdgeType::References, &target);
    graph.edge(&direct, EdgeType::References, &target);
    graph.edge(&both, EdgeType::References, &target);
    graph.edge(&target, EdgeType::TestedBy, &direct);
    graph.edge(&target, EdgeType::TestedBy, &both);
    graph.edge(&caller, EdgeType::TestedBy, &via);
    graph.edge(&caller, EdgeType::TestedBy, &both);
    // a module re-exporting the target references it but is not a caller
    graph.edge(&module, EdgeType::References, &target);
    graph.edge(&module, EdgeType::TestedBy, &via_module);
    let api = synth_api(&scratch, &graph);

    let r = api.code_tests_for(tests_for(by_ref(&target))).unwrap();
    assert_eq!(
        tier_rows(&r),
        vec![
            (direct.clone(), TestTier::Direct, None),
            (both.clone(), TestTier::Direct, None),
            (via.clone(), TestTier::Via, Some(caller.clone())),
        ]
    );
    assert_eq!(
        link_rows(&r),
        vec![
            (direct, "reference".to_owned(), false),
            (both, "reference".to_owned(), false),
            (via, "reference".to_owned(), false),
        ]
    );
    assert!(!r.truncated);
    // the caller's own answer is unchanged: its direct tests, nothing via
    let c = api.code_tests_for(tests_for(by_ref(&caller))).unwrap();
    assert!(
        c.tests
            .iter()
            .all(|l| l.how == TestTier::Direct && l.via.is_none())
    );
    assert_eq!(c.tests.len(), 2);
}

/// The line of `src/lib.rs` both targets of [`two_targets_on_one_line`] are defined at.
const TWO_TARGETS_LINE: u32 = 7;

/// Two functions on one line of `src/lib.rs`, the wrapper referencing the callee; `t` the direct
/// test of the wrapper (one hop from the callee), `z` the direct test of the callee; `t` sorts
/// before `z`. `callee_first` decides the insertion and ref order of the two targets, so the
/// reader yields them one way or the other (winA-1).
fn two_targets_on_one_line(scratch: &Scratch, callee_first: bool) -> (KabanApi, String, String) {
    let mut graph = Synth::default();
    graph.file("src/lib.rs").file("tests/t.rs");
    let (callee, wrapper) = if callee_first {
        let callee = graph.symbol("a_callee().", "Function", "a_callee", "src/lib.rs");
        let wrapper = graph.symbol("b_wrapper().", "Function", "b_wrapper", "src/lib.rs");
        (callee, wrapper)
    } else {
        let wrapper = graph.symbol("a_wrapper().", "Function", "a_wrapper", "src/lib.rs");
        let callee = graph.symbol("b_callee().", "Function", "b_callee", "src/lib.rs");
        (callee, wrapper)
    };
    let t = graph.symbol(
        "tests/t_direct_of_wrapper().",
        "Function",
        "t_direct_of_wrapper",
        "tests/t.rs",
    );
    let z = graph.symbol(
        "tests/z_direct_of_callee().",
        "Function",
        "z_direct_of_callee",
        "tests/t.rs",
    );
    for s in &mut graph.symbols {
        if s.r#ref == callee || s.r#ref == wrapper {
            s.def.sl = TWO_TARGETS_LINE;
            s.def.el = TWO_TARGETS_LINE;
        }
        if s.r#ref == t || s.r#ref == z {
            s.is_test = true;
        }
    }
    graph.edge(&wrapper, EdgeType::References, &callee);
    graph.edge(&t, EdgeType::References, &wrapper);
    graph.edge(&z, EdgeType::References, &callee);
    graph.edge(&wrapper, EdgeType::TestedBy, &t);
    graph.edge(&callee, EdgeType::TestedBy, &z);
    (synth_api(scratch, &graph), t, z)
}

/// winA-1 (s05 review of TESTS-1): a `{path, line}` selector matching two targets, one
/// referencing the other: the wrapper's direct test is `direct`, not the `via` row the callee
/// reaches it as, whichever target the reader yields first; under `limit: 1` it is the row
/// kept, since it sorts before the callee's direct test.
#[test]
fn tests_for_keeps_a_direct_test_direct_across_a_multi_target_selector() {
    for (order, callee_first) in [("callee-first", true), ("wrapper-first", false)] {
        let scratch = Scratch::new(&format!("tests-for-multi-{order}"));
        let (api, t, z) = two_targets_on_one_line(&scratch, callee_first);
        let target = SymbolSelector {
            path: Some("src/lib.rs".to_owned()),
            line: Some(TWO_TARGETS_LINE),
            ..Default::default()
        };

        let all = api.code_tests_for(tests_for(target.clone())).unwrap();
        let one = api
            .code_tests_for(CodeTestsForRequest {
                target,
                limit: Some(1),
                current_path: None,
                include_snippets: false,
            })
            .unwrap();

        assert_eq!(
            tier_rows(&all),
            vec![
                (t.clone(), TestTier::Direct, None),
                (z, TestTier::Direct, None),
            ],
            "{order}: every direct test is direct"
        );
        assert_eq!(
            tier_rows(&one),
            vec![(t, TestTier::Direct, None)],
            "{order}: the first direct test survives the limit"
        );
        assert!(one.truncated, "{order}");
    }
}

/// TESTS-1 (backlog 20): a target with no `direct` and no `via` test answers the `textual`
/// tier, a heuristic: test functions whose path or snippet spell every word of the target's
/// name (a CLI handler `ingest_finish` is reached by the black-box tests of
/// `tests/ingest_blackbox_cli.rs` that spawn the binary); a test spelling one word only, or a
/// target with tests in a stronger tier, gets no textual row.
#[test]
fn tests_for_falls_back_to_a_textual_tier_only_when_the_graph_has_nothing() {
    let scratch = Scratch::new("tests-for-textual");
    let mut graph = Synth::default();
    graph
        .file("src/cli/ingest.rs")
        .file("tests/ingest_blackbox_cli.rs")
        .file("tests/pack.rs")
        .file("other/tests/ingest_finish.rs");
    let handler = graph.symbol(
        "ingest_finish().",
        "Function",
        "ingest_finish",
        "src/cli/ingest.rs",
    );
    let tested = graph.symbol(
        "ingest_brief().",
        "Function",
        "ingest_brief",
        "src/cli/ingest.rs",
    );
    let blackbox = graph.symbol(
        "tests/cli_finish_retains_output().",
        "Function",
        "cli_finish_retains_output",
        "tests/ingest_blackbox_cli.rs",
    );
    let one_word = graph.symbol(
        "tests/a_finish_of_the_pack().",
        "Function",
        "a_finish_of_the_pack",
        "tests/pack.rs",
    );
    let brief_test = graph.symbol(
        "tests/cli_brief_runs().",
        "Function",
        "cli_brief_runs",
        "tests/ingest_blackbox_cli.rs",
    );
    let rig = graph.symbol(
        "tests/FinishRig#",
        "Struct",
        "FinishRig",
        "tests/ingest_blackbox_cli.rs",
    );
    let far = graph.symbol(
        "far/finish_rolls_back().",
        "Function",
        "finish_rolls_back",
        "other/tests/ingest_finish.rs",
    );
    for test in [&blackbox, &one_word, &brief_test, &rig, &far] {
        graph
            .symbols
            .iter_mut()
            .find(|s| &s.r#ref == test)
            .unwrap()
            .is_test = true;
    }
    graph.snippet(&blackbox, "fn cli_finish_retains_output() {");
    graph.snippet(&one_word, "fn a_finish_of_the_pack() {");
    graph.snippet(&brief_test, "fn cli_brief_runs() {");
    graph.snippet(&rig, "struct FinishRig {");
    graph.snippet(&far, "fn finish_rolls_back() {");
    graph.edge(&tested, EdgeType::TestedBy, &brief_test);
    let api = synth_api(&scratch, &graph);

    // the near crate's black-box test first (ranked from the handler's file), the far crate's
    // after it; the test-file struct is not a test
    let r = api.code_tests_for(tests_for(by_ref(&handler))).unwrap();
    assert_eq!(
        tier_rows(&r),
        vec![
            (blackbox.clone(), TestTier::Textual, None),
            (far.clone(), TestTier::Textual, None),
        ]
    );
    assert_eq!(
        link_rows(&r),
        vec![
            (blackbox, "naming".to_owned(), true),
            (far, "naming".to_owned(), true),
        ]
    );
    // a target with a direct test never gets the textual tier, even when names would match
    let b = api.code_tests_for(tests_for(by_ref(&tested))).unwrap();
    assert_eq!(tier_rows(&b), vec![(brief_test, TestTier::Direct, None)]);
}

// ---------------------------------------------------------------------------------------------
// task 7 (CG7.3): the `kaban://code-index` projection and the pinned-head seam

/// A redacting policy whose one rule masks the AWS-shaped canary (the builtin rule rejects
/// it, which would withhold the whole body; the resource test wants the leaf masked). The
/// term is assembled at run time, like [`canary`].
fn redact_canary_policy() -> Policy {
    let yaml = format!(
        "words:\n  - {{ term: {}, action: redact, id: argv-canary }}\nextend:\n  use_builtin: false\n",
        canary()
    );
    Policy::from_file(Policy::parse(yaml.as_bytes()).unwrap(), "test".into()).unwrap()
}

/// A service over a snapshot whose `indexer:go` meta row carries the canary in an argv
/// string, under the redacting policy.
fn code_index_api(s: &Scratch) -> (KabanApi, String) {
    let root = s.repo();
    let store = Arc::new(SqliteStore::open(&s.0.join("kaban.db"), &s.0.join("keys")).unwrap());
    let api = KabanApi::with_config(
        store.clone(),
        redact_canary_policy(),
        store.signer(),
        None,
        ApiConfig {
            repo_root: Some(root.clone()),
            repo: Some("fixture".to_owned()),
            ..Default::default()
        },
    )
    .with_git(patient_git());
    let dir = s.codegraph_dir();
    let commit = "c0deface".repeat(5);
    let argv = format!("scip-go --aws-key {} -o index.scip", canary());
    let mut synth = implementers_snapshot(2);
    synth
        .meta("languages", "go,typescript")
        .meta("indexer:go", &argv)
        .meta("indexer:typescript", "scip-typescript 0.3.14")
        .meta("kaban_index_version", "3.0.0-alpha.1")
        .meta("scip_version", "0.4.0")
        .meta("isolation_detail", "bwrap 0.11")
        .meta("resources", "modcache,toolchain")
        .meta("counts:files", "3")
        .meta("counts:symbols", "3")
        .meta("counts:edges", "2")
        .meta("counts:not_a_number", "many");
    let path = synth.publish(&dir, &commit, SCHEMA_VERSION, &api.policy_version());
    (
        api.with_snapshot_provider(dir, Arc::new(FixedSnapshot::new(path))),
        commit,
    )
}

/// [red] G1-F18, G1-S1: the resource body is the resolved `meta` projected over the known keys
/// and gated as `code_index`: the canary inside the indexer argv string is masked, the
/// control leaves (`snapshot.commit`, `languages[]`) stay, a missing key is absent and the
/// gated value still deserialises as the declared type (R3-9).
#[test]
fn resource_code_index_gated() {
    let s = Scratch::new("code-index-gated");
    let (api, commit) = code_index_api(&s);

    let r = api.code_index().unwrap();
    assert_eq!(r.snapshot.commit, commit);
    assert!(r.snapshot.exact);
    assert_eq!(r.snapshot.policy, "current");
    assert_eq!(r.snapshot.isolation, "none");
    assert_eq!(r.languages, vec!["go".to_owned(), "typescript".to_owned()]);
    assert!(r.indexers["go"].contains(&canary()), "ungated: {r:?}");
    assert_eq!(r.indexers["typescript"], "scip-typescript 0.3.14");
    assert_eq!(r.kaban_index_version.as_deref(), Some("3.0.0-alpha.1"));
    assert_eq!(r.scip_version.as_deref(), Some("0.4.0"));
    assert_eq!(r.schema_version, Some(SCHEMA_VERSION));
    assert_eq!(r.policy_version, Some(api.policy_version()));
    assert_eq!(r.isolation_detail.as_deref(), Some("bwrap 0.11"));
    assert_eq!(
        r.resources,
        vec!["modcache".to_owned(), "toolchain".to_owned()]
    );
    assert_eq!(
        r.counts,
        BTreeMap::from([
            ("files".to_owned(), 3),
            ("symbols".to_owned(), 3),
            ("edges".to_owned(), 2),
        ]),
        "counts:* numeric only; a non-number is absent, never an error"
    );

    // the adapter's one read gate over the serialized value (`mcp/mod.rs gate_value`)
    let value = serde_json::to_value(&r).unwrap();
    let clean = api.gate_response("code_index", &value).unwrap();
    let text = clean.to_string();
    assert!(
        !text.contains(&canary()),
        "the canary left the gate: {text}"
    );
    assert_eq!(
        clean["indexers"]["go"],
        json_str(&format!(
            "scip-go --aws-key {} -o index.scip",
            "[REDACTED:argv-canary]"
        )),
        "the argv leaf is masked in place"
    );
    assert_eq!(clean["snapshot"]["commit"], json_str(&commit));
    assert_eq!(clean["languages"], serde_json::json!(["go", "typescript"]));
    let again: kaban_api::CodeIndexResponse = serde_json::from_value(clean).unwrap();
    assert_eq!(again.snapshot, r.snapshot);
}

fn json_str(s: &str) -> Value {
    Value::String(s.to_owned())
}

/// The pinned-head seam (CG7.3 item 6): a service built with `with_pinned_head` resolves the
/// provider's snapshot without spawning git (the runner's program does not exist).
#[test]
fn pinned_head_resolves_without_git() {
    let s = Scratch::new("pinned-head");
    let root = s.0.join("not-a-repo");
    std::fs::create_dir_all(&root).unwrap();
    let store = Arc::new(SqliteStore::open(&s.0.join("kaban.db"), &s.0.join("keys")).unwrap());
    let policy = Policy::from_file(Policy::parse(b"").unwrap(), "test".into()).unwrap();
    let dir = s.codegraph_dir();
    let commit = "ab12cd34".repeat(5);
    let path = implementers_snapshot(3).publish(&dir, &commit, SCHEMA_VERSION, "pv-bench");
    let api = KabanApi::with_config(
        store.clone(),
        policy,
        store.signer(),
        None,
        ApiConfig {
            repo_root: Some(root),
            repo: Some("bench".to_owned()),
            ..Default::default()
        },
    )
    .with_git(GitRunner {
        program: PathBuf::from("/nonexistent/kaban-no-such-git"),
        timeout: Duration::from_secs(1),
    })
    .with_snapshot_provider(dir, Arc::new(FixedSnapshot::new(path)))
    .with_pinned_head(&commit);

    let resolved = api.resolve_current().unwrap();
    assert_eq!(resolved.r#ref.commit, commit);
    let r = api
        .code_implementers(CodeImplementersRequest {
            target: SymbolSelector {
                label: Some("Interface".to_owned()),
                name: Some("Iface".to_owned()),
                ..Default::default()
            },
            ..Default::default()
        })
        .unwrap();
    assert_eq!(r.count, 3);
    assert_eq!(r.snapshot.policy, "stale");
    // the index resource reads the same pinned head
    assert_eq!(api.code_index().unwrap().snapshot.commit, commit);
}

// ---------------------------------------------------------------------------------------------
// CG8.1: deterministic ranking with `current_path`, computed before any limit (G1-F16, RV-7)

/// `Iface#` in `iface.go` with one implementer and one member in each of `pkg/a/x.go`,
/// `pkg/a/y.go` and `pkg/b/z.go`: three files in two directories.
fn two_directories_snapshot() -> Synth {
    let mut s = Synth::default();
    s.file("iface.go");
    let iface = s.symbol("Iface#", "Interface", "Iface", "iface.go");
    for (suffix, path) in [
        ("X", "pkg/a/x.go"),
        ("Y", "pkg/a/y.go"),
        ("Z", "pkg/b/z.go"),
    ] {
        s.file(path);
        let imp = s.symbol(
            &format!("Impl{suffix}#"),
            "Struct",
            &format!("Impl{suffix}"),
            path,
        );
        s.edge(&imp, EdgeType::Implements, &iface);
        let member = s.symbol(
            &format!("Iface#M{suffix}()."),
            "Method",
            &format!("M{suffix}"),
            path,
        );
        s.edge(&iface, EdgeType::Contains, &member);
    }
    s
}

/// `(name, distance)` of every card, in order.
fn ranked(r: &CodeListResponse) -> Vec<(&str, u32)> {
    r.results
        .iter()
        .map(|c| (c.name.as_str(), c.distance))
        .collect()
}

/// G1-F16: the complete candidate set is ordered by `(path_distance(current_path, path), path,
/// ref)`; without `current_path` every distance is 0 and the order is CG7.1's `(path, ref)`.
#[test]
fn ranking_with_and_without_current_path() {
    let synth = two_directories_snapshot();
    let s = Scratch::new("ranking");
    let api = synth_api(&s, &synth);
    let iface = by_label_name("Interface", "Iface");
    let impls = |current_path: Option<&str>| {
        api.code_implementers(CodeImplementersRequest {
            target: iface.clone(),
            current_path: current_path.map(str::to_owned),
            ..Default::default()
        })
        .unwrap()
    };
    // without `current_path`: (path, ref), every distance 0
    let r = impls(None);
    assert_eq!(ranked(&r), [("ImplX", 0), ("ImplY", 0), ("ImplZ", 0)]);
    assert_eq!(
        (r.count, r.truncated, r.ranking.as_str()),
        (3, false, "full")
    );
    // the caller's own file first (0), then the two files in the sibling directory (2 + 1 + 1)
    let r = impls(Some("pkg/b/z.go"));
    assert_eq!(ranked(&r), [("ImplZ", 0), ("ImplX", 4), ("ImplY", 4)]);
    assert_eq!(r.ranking, "full");
    // the same file, the same directory, the other directory
    assert_eq!(
        ranked(&impls(Some("pkg/a/y.go"))),
        [("ImplY", 0), ("ImplX", 1), ("ImplZ", 4)]
    );
    // a file the snapshot does not hold ranks by its directory alone
    assert_eq!(
        ranked(&impls(Some("pkg/a/w.go"))),
        [("ImplX", 1), ("ImplY", 1), ("ImplZ", 4)]
    );
    // a root file: every candidate is two directory components away (2 + 0 + 2) and the tie
    // falls to (path, ref)
    assert_eq!(
        ranked(&impls(Some("main.go"))),
        [("ImplX", 4), ("ImplY", 4), ("ImplZ", 4)]
    );
    // `code_members` ranks the same way
    let members_of = |current_path: Option<&str>| {
        api.code_members(CodeMembersRequest {
            target: iface.clone(),
            current_path: current_path.map(str::to_owned),
            ..Default::default()
        })
        .unwrap()
    };
    assert_eq!(ranked(&members_of(None)), [("MX", 0), ("MY", 0), ("MZ", 0)]);
    assert_eq!(
        ranked(&members_of(Some("pkg/b/z.go"))),
        [("MZ", 0), ("MX", 4), ("MY", 4)]
    );
    // `code_definition` fills the one card's distance
    let def = |current_path: Option<&str>| {
        api.code_definition(CodeDefinitionRequest {
            target: by_label_name("Struct", "ImplX"),
            current_path: current_path.map(str::to_owned),
        })
        .unwrap()
        .definition
        .distance
    };
    assert_eq!(
        (def(None), def(Some("pkg/a/x.go")), def(Some("pkg/b/z.go"))),
        (0, 0, 4)
    );
}

/// `Iface#` with 299 implementers under `a/` and one more, the lexically last row by path and
/// by ref, alone under `z/`.
fn far_corner_snapshot() -> Synth {
    let mut s = Synth::default();
    s.file("iface.go");
    let iface = s.symbol("Iface#", "Interface", "Iface", "iface.go");
    for i in 0..299 {
        let path = format!("a/p{i:03}.go");
        s.file(&path);
        let imp = s.symbol(
            &format!("Impl{i:03}#"),
            "Struct",
            &format!("Impl{i:03}"),
            &path,
        );
        s.edge(&imp, EdgeType::Implements, &iface);
    }
    s.file("z/last.go");
    let last = s.symbol("Last#", "Struct", "Last", "z/last.go");
    s.edge(&last, EdgeType::Implements, &iface);
    s
}

/// RV-7: 300 candidates, the lexically last row the only one in the caller's directory; it is
/// the first result at `limit: 5` because the complete set is ranked before the cut (a reader
/// that limited first could never return it: it is beyond the maximum page).
#[test]
fn ranking_top_result_beyond_first_page() {
    let synth = far_corner_snapshot();
    let s = Scratch::new("far-corner");
    let api = synth_api(&s, &synth);
    let call = |limit: usize, current_path: Option<&str>| {
        api.code_implementers(CodeImplementersRequest {
            target: by_label_name("Interface", "Iface"),
            limit: Some(limit),
            current_path: current_path.map(str::to_owned),
            ..Default::default()
        })
        .unwrap()
    };
    // without `current_path` the row is beyond even the maximum page
    let r = call(200, None);
    assert_eq!((r.count, r.truncated), (200, true));
    assert!(!r.results.iter().any(|c| c.name == "Last"));
    let r = call(5, None);
    assert_eq!(
        ranked(&r),
        [
            ("Impl000", 0),
            ("Impl001", 0),
            ("Impl002", 0),
            ("Impl003", 0),
            ("Impl004", 0)
        ]
    );
    // from the caller's directory it leads the first page
    let r = call(5, Some("z/caller.go"));
    assert_eq!(
        (r.count, r.truncated, r.ranking.as_str()),
        (5, true, "full")
    );
    assert_eq!(
        ranked(&r),
        [
            ("Last", 1),
            ("Impl000", 4),
            ("Impl001", 4),
            ("Impl002", 4),
            ("Impl003", 4)
        ]
    );
    assert_eq!(r.results[0].path.as_deref(), Some("z/last.go"));
}

/// [red] G1-F16 / RS-12: `current_path` is checked by the repository-relative path rule before
/// any lookup: a parent component, an absolute path or an empty path is `selector_invalid` (the
/// fixed sentence, no echo) from every handler and the provider is never consulted; a canary is
/// `Rejected` by the gate, as before; a valid path proceeds to the provider.
#[test]
fn current_path_validated_before_lookup() {
    let s = Scratch::new("current-path");
    let root = s.repo();
    let trip = Arc::new(Tripwire::default());
    let provider: Arc<dyn SnapshotProvider> = trip.clone();
    let api = api_without_provider(&s, &root).with_snapshot_provider(s.codegraph_dir(), provider);
    let m = "Zq8v";
    let bad = [
        format!("../{m}.go"),
        format!("a/../{m}.go"),
        format!("/abs/{m}.go"),
        String::new(),
    ];
    let valid_symbol = format!("{GO_KEY}PostgresDriver#Open().");
    let canary = canary();
    let mut texts = BTreeSet::new();
    let handlers = handlers();
    for h in &handlers {
        for cp in &bad {
            let err = (h.call)(&api, by_symbol(&valid_symbol), Some(cp.clone()))
                .expect_err("an invalid current_path is refused");
            let text = class_text(err, "selector_invalid");
            assert!(
                !text.contains(m) && !text.contains(".."),
                "{}: {text}",
                h.name
            );
            texts.insert(text);
        }
        let err =
            (h.call)(&api, by_symbol(&valid_symbol), Some(canary.clone())).expect_err("rejected");
        rejected(&err, "current_path", &canary, h.name);
    }
    assert_eq!(texts.len(), 1, "one fixed sentence: {texts:?}");
    assert_eq!(
        trip.0.load(Ordering::SeqCst),
        0,
        "the provider (and so the reader) was never consulted"
    );
    // a valid path passes the rule: the lookup proceeds to the provider (which has nothing)
    for (i, h) in handlers.iter().enumerate() {
        let err = (h.call)(
            &api,
            by_symbol(&valid_symbol),
            Some("drivers/postgres.go".to_owned()),
        )
        .expect_err("no snapshot behind the tripwire");
        class_text(err, "no_snapshot");
        assert_eq!(trip.0.load(Ordering::SeqCst), i + 1, "{}", h.name);
    }
}

// ---------------------------------------------------------------------------------------------
// Q-1 (G3): the members of a file are its top-level symbols, whatever the `CONTAINS` parents are

/// G3: a Go file node has no `CONTAINS` children (the containment parent is the package), yet
/// `code_members(path)` answers: the symbols whose definition lies in the file and whose
/// `CONTAINS` parent is not another symbol of the same file, in range order (not ref order),
/// `labels` honoured; a struct target keeps its `CONTAINS` children.
#[test]
fn members_of_a_file_are_its_top_level_symbols_in_range_order() {
    let s = Scratch::new("members-file");
    let mut synth = Synth::default();
    synth.file("pkg/a.go").file("pkg/b.go");
    synth.file_node("pkg/a.go");
    // insertion order is line order: `Zed` is defined before `Alpha` (ref order says otherwise)
    let zed = synth.symbol("Zed().", "Function", "Zed", "pkg/a.go");
    let alpha = synth.symbol("Alpha#", "Struct", "Alpha", "pkg/a.go");
    let field = synth.symbol("Alpha#f.", "Field", "f", "pkg/a.go");
    let pkg = synth.symbol("", "Package", "example.com/synth/pkg", "pkg/b.go");
    synth
        .edge(&pkg, EdgeType::Contains, &zed)
        .edge(&pkg, EdgeType::Contains, &alpha)
        .edge(&alpha, EdgeType::Contains, &field);
    let api = synth_api(&s, &synth);

    let all = api.code_members(members(by_path("pkg/a.go"), &[])).unwrap();
    assert_eq!(
        refs(&all),
        vec![zed.clone(), alpha.clone()],
        "top level, by range"
    );
    assert_eq!(all.count, 2);
    assert!(!all.truncated);

    let structs = api
        .code_members(members(by_path("pkg/a.go"), &["Struct"]))
        .unwrap();
    assert_eq!(refs(&structs), vec![alpha.clone()], "labels honoured");

    let limited = api
        .code_members(CodeMembersRequest {
            limit: Some(1),
            ..members(by_path("pkg/a.go"), &[])
        })
        .unwrap();
    assert_eq!(refs(&limited), vec![zed], "the cut keeps range order");
    assert!(limited.truncated);

    // a struct target is unchanged: its direct `CONTAINS` children
    let of_struct = api.code_members(members(by_ref(&alpha), &[])).unwrap();
    assert_eq!(refs(&of_struct), vec![field]);
}

// ---------------------------------------------------------------------------------------------
// Q-1 (G4): locals are not neighbours unless the caller asks for them

/// G4: a function's `REFERENCES` edges to its own locals (scip-go publishes every parameter
/// and variable as `local N`, named by the id) are hidden from `code_neighbours` by default and
/// counted in `locals_hidden`; `include_locals: true` returns them; `code_context` callees
/// never carry them.
#[test]
fn neighbours_hide_locals_unless_asked_and_count_them() {
    let s = Scratch::new("neighbours-locals");
    let mut synth = Synth::default();
    synth.file("pkg/a.go");
    let f = synth.symbol("F().", "Function", "F", "pkg/a.go");
    let g = synth.symbol("G().", "Function", "G", "pkg/a.go");
    let l1 = synth.local("pkg/a.go", 1);
    let l2 = synth.local("pkg/a.go", 2);
    synth
        .edge(&f, EdgeType::References, &g)
        .edge(&f, EdgeType::References, &l1)
        .edge(&f, EdgeType::References, &l2);
    let api = synth_api(&s, &synth);

    let hidden = api.code_neighbours(neighbours(by_ref(&f), 1, &[])).unwrap();
    assert_eq!(
        hidden
            .nodes
            .iter()
            .map(|n| n.r#ref.clone())
            .collect::<Vec<_>>(),
        vec![f.clone(), g.clone()],
        "the start and its one non-local neighbour"
    );
    assert_eq!(hidden.edges.len(), 1);
    assert_eq!(hidden.locals_hidden, 2);
    assert!(!hidden.truncated, "hiding is not truncation");

    let shown = api
        .code_neighbours(CodeNeighboursRequest {
            include_locals: true,
            ..neighbours(by_ref(&f), 1, &[])
        })
        .unwrap();
    assert_eq!(
        shown
            .nodes
            .iter()
            .map(|n| n.r#ref.clone())
            .collect::<Vec<_>>(),
        vec![f.clone(), l1, l2, g],
        "(depth, ref) order with the locals (`local:` refs sort before `scip:`)"
    );
    assert_eq!(shown.edges.len(), 3);
    assert_eq!(shown.locals_hidden, 0);

    let ctx = api
        .code_context(kaban_api::CodeContextRequest {
            target: by_ref(&f),
            current_path: None,
        })
        .unwrap();
    assert_eq!(
        ctx.callees
            .iter()
            .map(|c| c.name.clone())
            .collect::<Vec<_>>(),
        vec!["G".to_owned()],
        "callees are never locals"
    );
}

// ---------------------------------------------------------------------------------------------
// Q-1: a snapshot indexed with the content scan off is labelled `off`, not `stale`

/// DF-3 made the content-policy scan off by default, so such a snapshot carries
/// `policy_version: "off"`; the wire label says `off` (nothing was gated), `stale` stays for a
/// real version mismatch.
#[test]
fn snapshot_policy_label_is_off_when_the_scan_was_disabled() {
    let mut synth = Synth::default();
    synth.file("pkg/a.go");
    let f = synth.symbol("F().", "Function", "F", "pkg/a.go");
    for (version, want) in [("off", "off"), ("pv-synth", "stale")] {
        let s = Scratch::new(&format!("policy-{want}"));
        let root = s.repo();
        let api = api_without_provider(&s, &root);
        let dir = s.codegraph_dir();
        let path = synth.publish(&dir, &"6".repeat(40), SCHEMA_VERSION, version);
        let api = api.with_snapshot_provider(dir, Arc::new(FixedSnapshot::new(path)));
        let r = api.code_definition(definition(by_ref(&f))).unwrap();
        assert_eq!(r.snapshot.policy, want, "policy_version {version}");
    }
}

/// q1-3: `off` is a value of the `snapshot.policy` control path, validated and never scanned
/// as content: through the adapter's read gate (`mcp/mod.rs gate_value`) a redact rule on the
/// word leaves the label intact and a reject rule still delivers the response.
#[test]
fn policy_label_off_passes_the_response_gate() {
    let mut synth = Synth::default();
    synth.file("pkg/a.go");
    let f = synth.symbol("F().", "Function", "F", "pkg/a.go");
    for action in ["redact", "reject"] {
        let s = Scratch::new(&format!("policy-off-{action}"));
        let root = s.repo();
        let yaml = format!(
            "words:\n  - {{ term: off, action: {action}, id: off-{action} }}\nextend:\n  use_builtin: false\n"
        );
        let policy =
            Policy::from_file(Policy::parse(yaml.as_bytes()).unwrap(), "test".into()).unwrap();
        let api = api_under_policy(&s, &root, policy);
        let dir = s.codegraph_dir();
        let path = synth.publish(&dir, &"7".repeat(40), SCHEMA_VERSION, "off");
        let api = api.with_snapshot_provider(dir, Arc::new(FixedSnapshot::new(path)));
        let r = api.code_definition(definition(by_ref(&f))).unwrap();
        assert_eq!(r.snapshot.policy, "off");
        let value = serde_json::to_value(&r).unwrap();
        let clean = api
            .gate_response("code_definition", &value)
            .unwrap_or_else(|e| panic!("{action}: a valid response was withheld: {e}"));
        assert_eq!(clean["snapshot"]["policy"], json_str("off"), "{action}");
        assert_eq!(clean, value, "{action}: nothing else to mask");
    }
}

// ---------------------------------------------------------------------------------------------
// FIX-H (q2-1): the hint rows of `ambiguous_symbol` and `unknown_symbol` pass the live response
// gate field by field before they are serialized

/// The rows of the `<key>: [...]` hint line of an error text.
fn hint_rows(text: &str, key: &str) -> Vec<Value> {
    let line = text
        .lines()
        .find_map(|l| l.strip_prefix(&format!("{key}: ")))
        .unwrap_or_else(|| panic!("no {key} line: {text}"));
    serde_json::from_str(line).unwrap_or_else(|e| panic!("{key} is not a JSON list ({e}): {text}"))
}

/// q2-1: the candidate rows of a name selector's refusal are snapshot values like any card, so
/// the live policy has to see them as the adapter's response gate sees a card: one field at a
/// time. An anchored path rule (`^crates/kaban-index/`) matches the `path` leaf of an ordinary
/// answer but can never match inside one combined error string, so on a stale or `off`
/// snapshot (nothing was gated at index time) a redact rule has to mask a candidate's path
/// exactly as it masks the card's, and a reject rule has to withhold the list exactly as it
/// withholds the card, for both hint shapes, through the production handlers.
#[test]
fn candidate_hints_pass_the_live_response_gate_field_by_field() {
    const INDEX_PATH: &str = "crates/kaban-index/src/writer.rs";
    const INGEST_PATH: &str = "crates/kaban-ingest/tests/idempotency.rs";
    let mut synth = Synth::default();
    synth.file(INDEX_PATH).file(INGEST_PATH);
    let rows_w = synth.symbol("writer/Rows#", "Struct", "Rows", INDEX_PATH);
    synth.symbol("idem/Rows#", "Struct", "Rows", INGEST_PATH);
    // `run` under two other labels: `{Struct, run}` is `unknown_symbol` with `did_you_mean`
    synth.symbol("Writer#run().", "Method", "run", INDEX_PATH);
    synth.symbol("run().", "Function", "run", INGEST_PATH);
    for (action, policy_version) in [
        ("reject", "off"),
        ("reject", "pv-other"),
        ("redact", "off"),
        ("redact", "pv-other"),
    ] {
        let case = format!("{action} on a {policy_version} snapshot");
        let s = Scratch::new(&format!("hint-gate-{action}-{policy_version}"));
        let root = s.repo();
        let yaml = format!(
            "rules:\n  - {{ id: index-path, regex: '^crates/kaban-index/', action: {action} }}\nextend:\n  use_builtin: false\n"
        );
        let policy =
            Policy::from_file(Policy::parse(yaml.as_bytes()).unwrap(), "test".into()).unwrap();
        let api = api_under_policy(&s, &root, policy);
        let dir = s.codegraph_dir();
        let path = synth.publish(&dir, &"8".repeat(40), SCHEMA_VERSION, policy_version);
        let api = api.with_snapshot_provider(dir, Arc::new(FixedSnapshot::new(path)));

        // what the live gate does with the same path on an ordinary answer
        let card = api.code_definition(definition(by_ref(&rows_w))).unwrap();
        assert_ne!(card.snapshot.policy, "current", "{case}");
        let gated_card =
            api.gate_response("code_definition", &serde_json::to_value(&card).unwrap());

        // both hint shapes, through the production handlers
        let ambiguous = class_text(
            api.code_members(members(by_label_name("Struct", "Rows"), &[]))
                .unwrap_err(),
            "ambiguous_symbol",
        );
        let unknown = class_text(
            api.code_definition(definition(by_label_name("Struct", "run")))
                .unwrap_err(),
            "unknown_symbol",
        );
        if action == "reject" {
            assert!(gated_card.is_err(), "{case}: the card is withheld");
            for (text, key) in [(&ambiguous, "candidates"), (&unknown, "did_you_mean")] {
                assert!(
                    !text.contains("crates/kaban-index/"),
                    "{case}: the rejected path leaks through {key}: {text}"
                );
                assert!(
                    !text.contains(INGEST_PATH),
                    "{case}: the whole list is withheld, as the whole card is: {text}"
                );
                assert!(
                    text.contains(&format!("\n{key}: withheld")),
                    "{case}: the text says the list was withheld: {text}"
                );
            }
        } else {
            let gated_card = gated_card.unwrap_or_else(|e| panic!("{case}: {e}"));
            let masked = gated_card["definition"]["path"]
                .as_str()
                .unwrap()
                .to_owned();
            assert!(
                !masked.starts_with("crates/kaban-index/") && masked.ends_with("writer.rs"),
                "{case}: the card's path is masked: {masked}"
            );
            // the reader's `(path, ref)` order: the kaban-index row first
            for (text, key) in [(&ambiguous, "candidates"), (&unknown, "did_you_mean")] {
                let paths: Vec<String> = hint_rows(text, key)
                    .iter()
                    .map(|c| c["path"].as_str().unwrap().to_owned())
                    .collect();
                assert_eq!(
                    paths,
                    vec![masked.clone(), INGEST_PATH.to_owned()],
                    "{case}: {key} masked exactly like the card: {text}"
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// FIX-H (q2-2): label correction never manufactures uniqueness

/// q2-2: the descriptor check that keeps a derive macro or a type parameter from standing in
/// for an external trait applies to external placeholders only. A supported producer that
/// labels nothing (scip-typescript) writes local rows as `UnspecifiedKind`, so with a
/// production `run().` of that label and a test-only `Harness#run().` labelled `Method`,
/// `{Struct, run}` has two same-named local symbols: the answer is `unknown_symbol` with both
/// in `did_you_mean`, never the test method with `label_corrected`. An explicit `path` still
/// picks one, corrected to its own label.
#[test]
fn label_correction_counts_every_local_row() {
    let mut synth = Synth::default();
    synth.file("src/runner.ts").file("tests/runner.test.ts");
    let production = synth.symbol("run().", "UnspecifiedKind", "run", "src/runner.ts");
    let test_only = synth.symbol("Harness#run().", "Method", "run", "tests/runner.test.ts");
    let s = Scratch::new("mixed-label-collision");
    let api = synth_api(&s, &synth);

    for (name, err) in [
        (
            "code_definition",
            api.code_definition(definition(by_label_name("Struct", "run")))
                .unwrap_err(),
        ),
        (
            "code_members",
            api.code_members(members(by_label_name("Struct", "run"), &[]))
                .unwrap_err(),
        ),
    ] {
        let text = class_text(err, "unknown_symbol");
        let hints = hint_rows(&text, "did_you_mean");
        let got: Vec<(&str, &str)> = hints
            .iter()
            .map(|h| (h["label"].as_str().unwrap(), h["ref"].as_str().unwrap()))
            .collect();
        assert_eq!(
            got,
            vec![
                ("UnspecifiedKind", production.as_str()),
                ("Method", test_only.as_str()),
            ],
            "{name}: {text}"
        );
    }

    // `path` disambiguates, and the correction names the row's own label
    for (path, want_ref, want_label) in [
        ("src/runner.ts", &production, "UnspecifiedKind"),
        ("tests/runner.test.ts", &test_only, "Method"),
    ] {
        let in_path = SymbolSelector {
            path: Some(path.into()),
            ..by_label_name("Struct", "run")
        };
        let r = api.code_definition(definition(in_path)).unwrap();
        assert_eq!(r.definition.r#ref, *want_ref, "{path}");
        assert_eq!(
            r.target_note,
            Some(TargetNote {
                r#ref: want_ref.clone(),
                label_corrected: Some(want_label.into()),
                external: false,
            }),
            "{path}"
        );
    }
}
