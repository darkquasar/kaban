//! `CodeRef.symbol`, `snapshot_commit` and `observed_at` at `remember` (stage1-spec G1-F9, G1-F10
//! part, G1-S4 handler step, §4.4; SO-2; stage1-plan CG5.1): the content scan before the grammar
//! check, the 40-hex canonicalisation of a short commit, the derivation of `symbol` from
//! `relative_path` and `name_path` through the resolved snapshot, and the `observed_at` default.
//! The snapshot is a `FixedSnapshot` over a file built with the public write helpers; the head
//! comes from a scripted two-commit repository.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use std::fs::OpenOptions;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::Scratch;
use kaban_api::{ApiConfig, ApiError, CodeRefWire, GitRunner, KabanApi, RememberRequest};
use kaban_codegraph::testing::FixedSnapshot;
use kaban_codegraph::write::{FileRow, SymbolRow};
use kaban_codegraph::{
    FileId, HeadInput, NoSnapshot, NodeId, NodeKind, Range, Resolved, SCHEMA_VERSION,
    SnapshotProvider, symbol, write,
};
use kaban_core::{MemoryStore, TraverseQuery};
use kaban_policy::Policy;
use kaban_store_sqlite::SqliteStore;
use serde_json::json;

/// The fixture snapshot's commit (40 hex).
const SNAPSHOT: &str = "abababababababababababababababababababab";
/// A normalised key the fixture holds once (the `Open` row of `src/store.go`), and the
/// versioned symbol of that row (admitted by the code tools' `ref` selector, refused as a
/// `CodeRef.symbol` since RU3-6).
const OPEN_KEY: &str = "scip-go gomod example.com/m . store/Store#Open().";
const OPEN_SYMBOL: &str = "scip-go gomod example.com/m v1.0.0 store/Store#Open().";

/// An AWS access key id shaped canary (the builtin `aws-access-key-id` rule), assembled at run
/// time so that no credential-shaped literal sits in the source.
fn canary() -> String {
    format!("{}{}", "AKIA", "QWERTYUIOPASDFGH")
}

fn symbol_row(scip: &str, key: Option<&str>, name: &str, external: bool) -> SymbolRow {
    SymbolRow {
        id: NodeId(0),
        kind: NodeKind::Symbol,
        r#ref: format!("scip:{scip}"),
        scip: Some(scip.to_owned()),
        key: key.map(str::to_owned),
        label: "Method".into(),
        name: name.into(),
        file_id: Some(FileId(1)),
        def: Some(Range {
            sl: 1,
            sc: 0,
            el: 1,
            ec: 4,
        }),
        enc: None,
        signature: None,
        doc: None,
        external,
        is_test: false,
        ident_terms: name.to_lowercase(),
    }
}

/// The key of the fixture's `Leak` row: a normalised key whose descriptor carries the canary
/// (a snapshot admitted under a policy without the AWS rule; RU3-1).
fn leak_key() -> String {
    format!("scip-go gomod example.com/m . store/`{} key`.", canary())
}

/// `src/store.go` with: one `Open` row (RU3-7: the two-rows-one-key case lives in
/// `tests/anchors_v4.rs`), `Close` in two types (two keys), an external `Open` placeholder
/// (ignored), a local `Open` (no key) and `Leak`, whose key carries the credential canary
/// (RU3-1).
fn write_snapshot(dir: &Path) -> PathBuf {
    let path = dir.join(format!("{SNAPSHOT}.cg.db"));
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
    write::insert_files(
        &conn,
        &[FileRow {
            id: FileId(1),
            path: "src/store.go".into(),
            language: "go".into(),
            is_test: false,
            lines: 40,
        }],
    )
    .unwrap();
    let mut rows = vec![
        symbol_row(OPEN_SYMBOL, Some(OPEN_KEY), "Open", false),
        symbol_row(
            "scip-go gomod example.com/m v1.0.0 store/Store#Close().",
            Some("scip-go gomod example.com/m . store/Store#Close()."),
            "Close",
            false,
        ),
        symbol_row(
            "scip-go gomod example.com/m v1.0.0 store/Pool#Close().",
            Some("scip-go gomod example.com/m . store/Pool#Close()."),
            "Close",
            false,
        ),
        symbol_row(
            "scip-go gomod example.com/other v2.0.0 x/Open().",
            Some("scip-go gomod example.com/other . x/Open()."),
            "Open",
            true,
        ),
        symbol_row(
            "scip-go gomod example.com/m v1.0.0 store/Store#Leak().",
            Some(&leak_key()),
            "Leak",
            false,
        ),
    ];
    let mut local = symbol_row("", None, "Open", false);
    local.kind = NodeKind::Local;
    local.r#ref = "local:1".into();
    local.scip = None;
    rows.push(local);
    rows.sort_by(|a, b| a.r#ref.as_bytes().cmp(b.r#ref.as_bytes()));
    for (i, r) in rows.iter_mut().enumerate() {
        r.id = NodeId(i64::try_from(i + 1).unwrap());
    }
    write::insert_symbols(&conn, &rows).unwrap();
    write::insert_edges(&conn, &[]).unwrap();
    write::insert_occurrences(&conn, &[]).unwrap();
    write::insert_snippets(&conn, &[]).unwrap();
    write::insert_edge_types(&conn, &[]).unwrap();
    let schema = SCHEMA_VERSION.to_string();
    write::insert_meta(
        &conn,
        &[
            ("commit", SNAPSHOT),
            ("isolation", "none"),
            ("policy_version", "pv-cg51"),
            ("repo_id", "0123456789abcdef0123456789abcdef"),
            ("schema_version", schema.as_str()),
            ("symbols_fts_version", "1"),
        ],
    )
    .unwrap();
    write::rebuild_fts(&conn).unwrap();
    write::finish(&conn).unwrap();
    path
}

fn git(cwd: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

/// A repository with two commits; returns `(root, head, ancestor)`, 40 hex each.
fn repo_with_two_commits(dir: &Path) -> (PathBuf, String, String) {
    let root = dir.join("repo");
    std::fs::create_dir_all(&root).unwrap();
    git(&root, &["init", "-q"]);
    std::fs::write(root.join("a.txt"), "one").unwrap();
    git(&root, &["add", "a.txt"]);
    git(&root, &["commit", "-q", "-m", "one"]);
    let ancestor = git(&root, &["rev-parse", "HEAD"]);
    std::fs::write(root.join("a.txt"), "two").unwrap();
    git(&root, &["commit", "-q", "-am", "two"]);
    let head = git(&root, &["rev-parse", "HEAD"]);
    assert_eq!(head.len(), 40);
    assert_ne!(head, ancestor);
    (root, head, ancestor)
}

/// The service bound to repository `kaban`, with the fixture provider and the head repository
/// when asked; the shipped builtin policy (the AWS key rule is live).
fn service(
    s: &Scratch,
    provider: bool,
    repo_root: Option<PathBuf>,
) -> (Arc<SqliteStore>, KabanApi) {
    let provider = provider.then(|| {
        let path = write_snapshot(&s.0);
        Arc::new(FixedSnapshot::new(&path)) as Arc<dyn SnapshotProvider>
    });
    service_with(s, provider, repo_root, None)
}

/// [`service`] over an explicit provider and, when given, a git runner (a wrapper script).
fn service_with(
    s: &Scratch,
    provider: Option<Arc<dyn SnapshotProvider>>,
    repo_root: Option<PathBuf>,
    git: Option<GitRunner>,
) -> (Arc<SqliteStore>, KabanApi) {
    let store = Arc::new(SqliteStore::open(&s.db(), &s.keys()).unwrap());
    let policy = Policy::from_file(Policy::parse(b"").unwrap(), "test".into()).unwrap();
    let mut api = KabanApi::with_config(
        store.clone(),
        policy,
        store.signer(),
        None,
        ApiConfig {
            repo: Some("kaban".into()),
            repo_root,
            ..Default::default()
        },
    );
    if let Some(git) = git {
        api = api.with_git(git);
    }
    if let Some(provider) = provider {
        api = api.with_snapshot_provider(s.0.join("codegraph"), provider);
    }
    (store, api)
}

/// A provider that records every head it was asked to resolve, then answers as the fixed
/// snapshot would (the `code_tools.rs` pattern).
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

fn wire() -> CodeRefWire {
    CodeRefWire {
        repo: "kaban".into(),
        commit: "8d10f88".into(),
        tool: "manual".into(),
        relative_path: "src/store.go".into(),
        name_path: Some("Store/Open".into()),
        kind: Some("Method".into()),
        ..Default::default()
    }
}

/// A `Fact` with a text of its own (the note must not deduplicate: each call links its refs
/// from a new node).
fn note(refs: Vec<CodeRefWire>) -> RememberRequest {
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    RememberRequest {
        kind: "Fact".into(),
        text: format!("note {n} about the store"),
        refs,
        ..Default::default()
    }
}

/// The `CodeRef` nodes linked ABOUT from `id`.
fn coderefs_about(store: &SqliteStore, id: &str) -> Vec<kaban_core::Node> {
    let r = store
        .traverse(&TraverseQuery {
            start_ids: vec![kaban_core::NodeId::parse(id).unwrap()],
            rels: vec!["ABOUT".into()],
            direction: kaban_core::TraverseDirection::Out,
            max_depth: 1,
            node_kinds: vec!["CodeRef".into()],
            governance: vec![],
            limit: 100,
            workspace_id: None,
            ..Default::default()
        })
        .unwrap();
    r.nodes
        .into_iter()
        .filter(|n| n.kind == "CodeRef")
        .collect()
}

fn one_ref(store: &SqliteStore, api: &KabanApi, w: CodeRefWire) -> kaban_core::Node {
    let r = api.remember(note(vec![w])).unwrap();
    let refs = coderefs_about(store, &r.id);
    assert_eq!(refs.len(), 1);
    refs.into_iter().next().unwrap()
}

fn invalid_text(r: Result<kaban_api::RememberResponse, ApiError>) -> String {
    match r {
        Err(ApiError::Invalid(msg)) => msg,
        other => panic!("expected Invalid, got {other:?}"),
    }
}

/// Every column of every audit row, rendered as text (blobs as hex), for a leak scan.
fn audit_rows(s: &Scratch) -> Vec<String> {
    use std::fmt::Write as _;
    let raw = rusqlite::Connection::open(s.db()).unwrap();
    let mut st = raw.prepare("SELECT * FROM audit ORDER BY seq").unwrap();
    let n = st.column_count();
    st.query_map([], |r| {
        let mut row = String::new();
        for i in 0..n {
            match r.get::<_, rusqlite::types::Value>(i)? {
                rusqlite::types::Value::Blob(b) => {
                    for x in b {
                        let _ = write!(row, "{x:02x}");
                    }
                }
                other => {
                    let _ = write!(row, "{other:?}");
                }
            }
            row.push(' ');
        }
        Ok(row)
    })
    .unwrap()
    .collect::<Result<Vec<_>, _>>()
    .unwrap()
}

/// Whether any byte sequence of the database (main file and WAL) is `needle`.
fn db_holds(s: &Scratch, needle: &str) -> bool {
    let mut bytes = std::fs::read(s.db()).unwrap();
    for suffix in ["-wal", "-journal"] {
        let mut side = s.db().into_os_string();
        side.push(suffix);
        if let Ok(more) = std::fs::read(side) {
            bytes.extend(more);
        }
    }
    bytes.windows(needle.len()).any(|w| w == needle.as_bytes())
}

// --- symbol: scan before grammar, grammar, roundtrip ------------------------------------------

/// G1-S4 `symbol_canary_scanned_before_grammar`: a grammar-valid symbol carrying a credential
/// canary in a backtick descriptor is rejected by the scanner with the rule id; the parser never
/// saw the string; nothing is stored.
#[test]
fn remember_symbol_canary_scanned() {
    let s = Scratch::new("kaban-cg51", "canary");
    let (store, api) = service(&s, false, None);
    let recorder = symbol::testing::record_inputs();
    let mut w = wire();
    w.symbol = Some(format!(
        "scip-go gomod example.com/m v1.0.0 `{} key`.",
        canary()
    ));
    let r = api.remember(note(vec![w]));
    match r {
        Err(ApiError::Rejected { field, rules }) => {
            assert_eq!(field, "refs.symbol");
            assert!(rules.iter().any(|r| r == "aws-access-key-id"), "{rules:?}");
        }
        other => panic!("expected a denylist rejection, got {other:?}"),
    }
    assert!(
        recorder.inputs().is_empty(),
        "the grammar never saw the canary: {:?}",
        recorder.inputs()
    );
    assert_eq!(store.stats(None).unwrap()["nodes"], json!(0));
}

#[test]
fn remember_symbol_invalid_grammar_rejected() {
    let s = Scratch::new("kaban-cg51", "grammar");
    let (store, api) = service(&s, false, None);
    let mut w = wire();
    w.symbol = Some("not a symbol".into());
    assert_eq!(
        invalid_text(api.remember(note(vec![w]))),
        "refs[].symbol: symbol_invalid"
    );
    // 1,025 bytes: refused by the size bound before the parser runs, no echo
    let recorder = symbol::testing::record_inputs();
    let mut w = wire();
    w.symbol = Some("a".repeat(1025));
    let msg = invalid_text(api.remember(note(vec![w])));
    assert_eq!(msg, "refs[].symbol: symbol_invalid");
    assert!(!msg.contains("aaaa"));
    assert!(recorder.inputs().is_empty(), "size bound before the parser");
    assert_eq!(store.stats(None).unwrap()["nodes"], json!(0));
}

/// A valid explicit symbol (the key form, RU3-6) is stored as given and is part of the dedup
/// text: the same ref twice is one node, a ref with and without `symbol` are two.
#[test]
fn remember_symbol_roundtrip() {
    let s = Scratch::new("kaban-cg51", "roundtrip");
    let (store, api) = service(&s, false, None);
    let mut w = wire();
    w.symbol = Some(OPEN_KEY.into());
    let first = one_ref(&store, &api, w.clone());
    assert_eq!(first.props["symbol"], json!(OPEN_KEY));
    assert!(first.text.contains("\"symbol\":"), "{}", first.text);
    let again = one_ref(&store, &api, w);
    assert_eq!(again.id, first.id, "same stored fields, one node");
    let without = one_ref(&store, &api, wire());
    assert_ne!(
        without.id, first.id,
        "with and without symbol are two nodes"
    );
    assert!(without.props.get("symbol").is_none());
}

/// RU3-6 (OWN-81 (3); stage1-spec §4.2, G1-S4): `CodeRef.symbol` admits normalised global keys
/// only. A versioned form and a document-local form are `symbol_invalid` (the fixed text, no
/// echo) and nothing is stored; the code tools' `ref` selector keeps accepting them.
#[test]
fn remember_symbol_versioned_and_local_invalid() {
    let s = Scratch::new("kaban-cg51", "versioned-local");
    let (store, api) = service(&s, false, None);
    for value in [OPEN_SYMBOL, "local 1"] {
        let mut w = wire();
        w.symbol = Some(value.into());
        let msg = invalid_text(api.remember(note(vec![w])));
        assert_eq!(msg, "refs[].symbol: symbol_invalid", "{value}");
        assert!(!msg.contains("example.com"), "no echo");
    }
    assert_eq!(store.stats(None).unwrap()["nodes"], json!(0));
}

// --- snapshot_commit: SO-2 canonicalisation ---------------------------------------------------

#[test]
fn remember_snapshot_commit_short_nonmatching_refused() {
    let s = Scratch::new("kaban-cg51", "short-refused");
    let (root, _head, _ancestor) = repo_with_two_commits(&s.0);
    let (store, api) = service(&s, true, Some(root));
    for value in ["0000000", &"f".repeat(64), "ABABABA"] {
        let mut w = wire();
        w.name_path = None;
        w.snapshot_commit = Some(value.to_owned());
        let msg = invalid_text(api.remember(note(vec![w])));
        assert_eq!(msg, "refs[].snapshot_commit", "{value}");
    }
    // a short value with no resolved snapshot
    let s2 = Scratch::new("kaban-cg51", "short-no-snapshot");
    let (store2, api2) = service(&s2, false, None);
    let mut w = wire();
    w.name_path = None;
    w.snapshot_commit = Some("abababa".into());
    assert_eq!(
        invalid_text(api2.remember(note(vec![w]))),
        "refs[].snapshot_commit"
    );
    assert_eq!(store.stats(None).unwrap()["nodes"], json!(0));
    assert_eq!(store2.stats(None).unwrap()["nodes"], json!(0));
}

#[test]
fn remember_snapshot_commit_short_prefix_canonicalised() {
    let s = Scratch::new("kaban-cg51", "short-ok");
    let (root, _head, ancestor) = repo_with_two_commits(&s.0);
    let (store, api) = service(&s, true, Some(root));
    // 7 hex of the resolved snapshot's commit
    let mut w = wire();
    w.name_path = None;
    w.snapshot_commit = Some(SNAPSHOT[..7].into());
    let n = one_ref(&store, &api, w);
    assert_eq!(n.props["snapshot_commit"], json!(SNAPSHOT));
    // 12 hex of an ancestor already fetched for the head
    let mut w = wire();
    w.name_path = None;
    w.snapshot_commit = Some(ancestor[..12].into());
    let n = one_ref(&store, &api, w);
    assert_eq!(n.props["snapshot_commit"], json!(ancestor));
    // 40 hex is stored as given, resolved or not
    let full = "cd".repeat(20);
    let mut w = wire();
    w.name_path = None;
    w.snapshot_commit = Some(full.clone());
    let n = one_ref(&store, &api, w);
    assert_eq!(n.props["snapshot_commit"], json!(full));
}

// --- derivation through the provider -----------------------------------------------------------

#[test]
fn remember_symbol_derived_unique() {
    let s = Scratch::new("kaban-cg51", "derived");
    let (root, _head, _ancestor) = repo_with_two_commits(&s.0);
    let (store, api) = service(&s, true, Some(root));
    let n = one_ref(&store, &api, wire());
    assert_eq!(n.props["symbol"], json!(OPEN_KEY), "{}", n.props);
    assert_eq!(n.props["name_path"], json!("Store/Open"));
    // the overload suffix is not part of the name
    let mut w = wire();
    w.name_path = Some("Store/Open[1]".into());
    let n = one_ref(&store, &api, w);
    assert_eq!(n.props["symbol"], json!(OPEN_KEY));
    // another repository's ref is never looked up (G1-S7): stored without a symbol, no error
    let mut w = wire();
    w.repo = "other".into();
    let n = one_ref(&store, &api, w);
    assert!(n.props.get("symbol").is_none(), "{}", n.props);
}

/// RU3-1 (G1-S1, G1-S4): the key `derive_symbol` returns is gated under the live policy as an
/// explicit `refs.symbol` is. The fixture's `Leak` row was admitted at index time with a key
/// carrying a credential-shaped term; the `remember` that derives it is refused by the gate's
/// own error, and no node, `CodeRef`, audit row or database byte holds the term.
#[test]
fn remember_symbol_derived_key_gated() {
    let s = Scratch::new("kaban-cg51", "derived-gated");
    let (root, _head, _ancestor) = repo_with_two_commits(&s.0);
    let (store, api) = service(&s, true, Some(root));
    let canary = canary();
    let mut w = wire();
    w.name_path = Some("Store/Leak".into());
    match api.remember(note(vec![w])) {
        Err(ApiError::Rejected { field, rules }) => {
            assert_eq!(field, "refs.symbol");
            assert!(rules.iter().any(|r| r == "aws-access-key-id"), "{rules:?}");
        }
        Ok(r) => {
            let refs = coderefs_about(&store, &r.id);
            panic!(
                "the derived key reached the store ungated: {} CodeRef(s), term in a CodeRef: {}",
                refs.len(),
                refs.iter()
                    .any(|n| n.text.contains(&canary) || n.props.to_string().contains(&canary))
            );
        }
        other => panic!("expected the gate's rejection of the derived key, got {other:?}"),
    }
    assert_eq!(store.stats(None).unwrap()["nodes"], json!(0));
    for row in audit_rows(&s) {
        assert!(!row.contains(&canary), "an audit row holds the term");
    }
    drop(api);
    drop(store);
    assert!(!db_holds(&s, &canary), "a database byte holds the term");
}

#[test]
fn remember_symbol_derived_ambiguous() {
    let s = Scratch::new("kaban-cg51", "ambiguous");
    let (root, _head, _ancestor) = repo_with_two_commits(&s.0);
    let (store, api) = service(&s, true, Some(root));
    // two distinct keys named `Close` in the file
    let mut w = wire();
    w.name_path = Some("Store/Close".into());
    assert_eq!(
        invalid_text(api.remember(note(vec![w]))),
        "refs[].name_path: ambiguous_symbol"
    );
    // no row of that name
    let mut w = wire();
    w.name_path = Some("Store/Missing".into());
    assert_eq!(
        invalid_text(api.remember(note(vec![w]))),
        "refs[].name_path: ambiguous_symbol"
    );
    assert_eq!(store.stats(None).unwrap()["nodes"], json!(0));
}

#[test]
fn remember_symbol_derived_sets_snapshot_commit() {
    let s = Scratch::new("kaban-cg51", "derived-commit");
    let (root, _head, _ancestor) = repo_with_two_commits(&s.0);
    let (store, api) = service(&s, true, Some(root));
    let n = one_ref(&store, &api, wire());
    assert_eq!(n.props["snapshot_commit"], json!(SNAPSHOT));
    // an explicit 40-hex value is kept beside the derived symbol
    let full = "cd".repeat(20);
    let mut w = wire();
    w.snapshot_commit = Some(full.clone());
    let n = one_ref(&store, &api, w);
    assert_eq!(n.props["symbol"], json!(OPEN_KEY));
    assert_eq!(n.props["snapshot_commit"], json!(full));
    // an explicit symbol is never looked up and sets no commit
    let mut w = wire();
    w.symbol = Some(OPEN_KEY.into());
    let n = one_ref(&store, &api, w);
    assert_eq!(n.props["symbol"], json!(OPEN_KEY));
    assert!(n.props.get("snapshot_commit").is_none(), "{}", n.props);
}

/// RU3-5 (G1-F8, cg-join §4): the ancestry handed to the provider is that of the sha
/// `rev-parse` returned, not of whatever `HEAD` names when `rev-list` runs. The repository
/// stands at its first commit A; a wrapper git checks out the second commit B right after
/// answering `rev-parse HEAD`: the provider receives A with A's ancestry (none), never B's.
#[test]
fn remember_head_ancestry_is_the_resolved_sha() {
    let s = Scratch::new("kaban-cg51", "head-ancestry");
    let (root, second, first) = repo_with_two_commits(&s.0);
    git(&root, &["checkout", "-q", &first]);
    // the runner clears the environment, so the repository and the target are baked in
    let wrapper = s.0.join("git-switching.sh");
    std::fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\ncase \" $* \" in\n  *\" rev-parse HEAD \"*)\n    git \"$@\" || exit $?\n    \
             git -C '{}' checkout -q {second} >/dev/null 2>&1\n    exit 0 ;;\nesac\nexec git \"$@\"\n",
            root.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
    let recording = Arc::new(Recording {
        inner: FixedSnapshot::new(write_snapshot(&s.0)),
        seen: Mutex::new(Vec::new()),
    });
    let (store, api) = service_with(
        &s,
        Some(recording.clone() as Arc<dyn SnapshotProvider>),
        Some(root.clone()),
        Some(GitRunner {
            program: wrapper,
            timeout: Duration::from_secs(10),
        }),
    );

    // a derivation needs the head: one fetch, then the ref resolves as usual
    let n = one_ref(&store, &api, wire());
    assert_eq!(n.props["symbol"], json!(OPEN_KEY));
    assert_eq!(
        git(&root, &["rev-parse", "HEAD"]),
        second,
        "the wrapper did switch"
    );
    let seen = recording.seen.lock().unwrap();
    assert_eq!(
        seen.as_slice(),
        [HeadInput {
            commit: first.clone(),
            ancestors: vec![],
        }],
        "sha A arrives with A's ancestry, not with the ancestry of the HEAD rev-list saw"
    );
}

#[test]
fn remember_without_snapshot_stores_symbol_unverified() {
    let s = Scratch::new("kaban-cg51", "no-provider");
    let (store, api) = service(&s, false, None);
    let full = "cd".repeat(20);
    let mut w = wire();
    w.symbol = Some(OPEN_KEY.into());
    w.snapshot_commit = Some(full.clone());
    let n = one_ref(&store, &api, w);
    assert_eq!(n.props["symbol"], json!(OPEN_KEY));
    assert_eq!(n.props["snapshot_commit"], json!(full));
    // `name_path` without a symbol: derivation is skipped, nothing fails
    let n = one_ref(&store, &api, wire());
    assert!(n.props.get("symbol").is_none());
    assert!(n.props.get("snapshot_commit").is_none());
}

// --- observed_at -------------------------------------------------------------------------------

#[test]
fn observed_at_set_on_remember() {
    let s = Scratch::new("kaban-cg51", "observed");
    let (store, api) = service(&s, false, None);
    let n = one_ref(&store, &api, wire());
    let at = n.props["observed_at"].as_str().expect("set by remember");
    assert!(
        at.len() >= 20 && &at[4..5] == "-" && &at[10..11] == "T" && at.ends_with('Z'),
        "RFC 3339 UTC: {at}"
    );
    assert!(
        !n.text.contains("observed_at"),
        "not part of the dedup text"
    );
    // an explicit value is shape-checked and kept (a fresh ref: `observed_at` is not part of
    // the dedup text, so the ref above would stand with its own)
    let mut w = wire();
    w.relative_path = "src/other.go".into();
    w.observed_at = Some("2026-10-26T11:02:39+02:00".into());
    let first = one_ref(&store, &api, w);
    assert_eq!(
        first.props["observed_at"],
        json!("2026-10-26T11:02:39+02:00")
    );
    // the same ref observed again is the same node; the first `observed_at` stands
    let mut w = wire();
    w.relative_path = "src/other.go".into();
    w.observed_at = Some("2026-10-26T09:53:10Z".into());
    let again = one_ref(&store, &api, w);
    assert_eq!(again.id, first.id);
    assert_eq!(
        again.props["observed_at"],
        json!("2026-10-26T11:02:39+02:00")
    );
    for bad in [
        "yesterday",
        "2026-13-01T00:00:00Z",
        "2026-10-26 09:04:36Z",
        "2026-10-26T18:44:22",
    ] {
        let mut w = wire();
        w.observed_at = Some(bad.into());
        assert_eq!(
            invalid_text(api.remember(note(vec![w]))),
            "refs[].observed_at",
            "{bad}"
        );
    }
}

/// RU3-9 (stage1-spec §4.4, CG5.1 D6): an explicit `observed_at` is a calendar date, not a
/// pair of independent ranges: the month's length and the leap day (four-yearly, not on a
/// century unless four-centennial) are checked; the fixed error text stands.
#[test]
fn observed_at_calendar_validity() {
    let s = Scratch::new("kaban-cg51", "observed-calendar");
    let (store, api) = service(&s, false, None);
    for bad in [
        "2027-02-31T00:00:00Z",
        "2027-02-29T00:00:00Z",
        "2027-04-31T00:00:00Z",
        "2100-02-29T00:00:00Z",
        "2027-00-10T00:00:00Z",
        "2027-10-00T00:00:00Z",
    ] {
        let mut w = wire();
        w.observed_at = Some(bad.into());
        assert_eq!(
            invalid_text(api.remember(note(vec![w]))),
            "refs[].observed_at",
            "{bad}"
        );
    }
    assert_eq!(store.stats(None).unwrap()["nodes"], json!(0));
    for (i, good) in [
        "2028-02-29T00:00:00Z",
        "2400-02-29T12:00:00+01:00",
        "2027-01-31T23:59:60Z",
        "2026-12-31T00:00:00.5-11:30",
    ]
    .into_iter()
    .enumerate()
    {
        let mut w = wire();
        w.relative_path = format!("src/day{i}.go");
        w.observed_at = Some(good.into());
        let n = one_ref(&store, &api, w);
        assert_eq!(n.props["observed_at"], json!(good));
    }
}
