//! The north-star acceptance tests over the public tools (stage1-spec §8.3 steps 2 to 7;
//! stage1-plan CG10.2; walkthrough-example Step 6): black box over `kaban serve` started in
//! a scratch copy of the fixture repository, the snapshot published into the data home the
//! server reads (`<KABAN_HOME>/codegraph/<repo-id>`, the id derived with the server's own
//! rule), and every step a `tools/call`.
//!
//! No indexer runs on this box (CG10.2 default 1): both halves are **fixture-derived**, the
//! snapshot is published from the committed `index.scip` bytes through the `kaban-index`
//! writer, exactly as `kaban-api/tests/code_tools.rs` publishes it; the "real `kaban-index`"
//! path of the plan is the visible `NOT RUN` marker each test prints.
//!
//! ```text
//!   fixtures/codegraph/go-drivers/repo  --copy-->  <scratch>/repo   (git replay: C1, C2 [, C3])
//!   index.scip + index-notest.scip  ==> decode_and_gate / admit_index / Rows::derive
//!                                   ==> snippets::collect (git show at C2)  ==> writer::publish
//!                                   ==> <home>/codegraph/<repo_id(key, common dir)>/<C2>.cg.db
//!   kaban serve (cwd = <scratch>/repo, --policy <redact variant>)  <== stdio (mcp_client.rs)
//!     remember D1, accept, remember D2, accept, link D2 SUPERSEDES D1,
//!     why {symbol}, why {symbol, include_superseded}, governance_history {id: D1}, code_context
//!   canary sweep: .cg.db, every response, kaban.db, -wal, -shm, audit export, stdout, stderr
//! ```

#![cfg(feature = "code-graph")] // the code tools and the fixture snapshots exist only with the feature
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion
#![allow(clippy::print_stderr)] // the visible `fixture-derived` / `NOT RUN` marker (OD-G1-8)

mod common;
#[path = "common/mcp_client.rs"]
mod mcp_client;

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use common::Scratch;
use kaban_codegraph::{IsolationTier, NodeId, write};
use kaban_core::process::InheritedFds;
use kaban_index::writer::{self, Commit, Meta, Provenance, Rows};
use kaban_index::{admit_index, decode_and_gate, snippets};
use kaban_policy::Policy;
use mcp_client::{Finished, McpClient, Spawn, snapshot_dir, write_codegraph_key};
use serde_json::{Value, json};

// ---------------------------------------------------------------------------------------------
// fixture facts

/// The go-drivers fixture's commits (`fixtures/codegraph/go-drivers/COMMITS`) and dates.
const GO_C2: &str = "730da842221a844e8c2c82891dda9094f6428858";
const C1_DATE: &str = "2026-10-26T08:16:02Z";
const C2_DATE: &str = "2026-10-26T09:04:36Z";
const C3_DATE: &str = "2026-10-26T09:53:10Z";
const C2_SUMMARY: &str = "C2: rename mem.go, extend PostgresDriver.Open";
const C3_SUMMARY: &str = "C3: rename MemDriver to MemoryDriver";

/// Normalised key prefixes (version `.`) of the Go `drivers` package, the Go `sqlite`
/// package and the TypeScript package (`code_tools.rs` `GO_KEY`, `TS_KEY`).
const GO_KEY: &str = "scip-go gomod example.com/drivers . `example.com/drivers/drivers`/";
const GO_SQLITE_KEY: &str = "scip-go gomod example.com/drivers . `example.com/drivers/sqlite`/";
const TS_KEY: &str = "scip-typescript npm @example/drivers . src/";
/// An external TypeScript key (the standard library's `Promise`): no anchor, no defining file
/// under `src/`, so nothing governs it (the negative of step 4 for the TS half; every `src/`
/// row is covered by the Decisions' `path_scope`).
const TS_ELSEWHERE: &str =
    "scip-typescript npm typescript . lib/`lib.es2015.iterable.d.ts`/Promise#";

/// The commit the canary-sym variant is published under (not in any history).
const CANARY_SYM_COMMIT: &str = "5151515151515151515151515151515151515151";

const WORKSPACE: &str = "north-star";

/// The three canaries of `examples/policy-test-canaries.yaml`, assembled at run time so that
/// the source carries no literal the rules match (`obs-no-sensitive-data`).
fn needles() -> [(String, &'static str); 3] {
    [
        (format!("{}_{}", "CANARY", "DOC"), "doc"),
        (format!("{}_{}", "CANARY", "SNIPPET"), "snippet"),
        (format!("{}_{}", "CANARY", "SYM"), "sym"),
    ]
}

fn contains(hay: &[u8], needle: &str) -> bool {
    hay.windows(needle.len()).any(|w| w == needle.as_bytes())
}

// ---------------------------------------------------------------------------------------------
// the scripted repository (mkfixture.sh replayed)

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/codegraph")
}

fn fixture_bytes(rel: &str) -> Vec<u8> {
    let path = fixtures_dir().join(rel);
    std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// Copy a tree (a fixture `repo/`, no `.git` inside) into `to`.
fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).unwrap();
        }
    }
}

/// `git <args>` in `root` under `mkfixture.sh:52-54`'s fixed identity, dates and config.
fn git(root: &Path, date: &str, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_AUTHOR_NAME", "kaban fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@kaban.invalid")
        .env("GIT_COMMITTER_NAME", "kaban fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@kaban.invalid")
        .env("GIT_AUTHOR_DATE", date)
        .env("GIT_COMMITTER_DATE", date)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn rev_parse(root: &Path, what: &str) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", what])
        .output()
        .unwrap();
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

/// The Go history: `(root, c1, c2, c3)`. `mkfixture.sh:53-68` replayed (C1 the committed
/// tree, C2 the `mem.go` rename plus the `_ = dsn` line), then CG10.2's C3 when asked:
/// `MemDriver` renamed to `MemoryDriver` in `drivers/memory.go` at `C3_DATE`. HEAD is the
/// last commit made, on `main`.
fn go_repo(dir: &Path, with_c3: bool) -> (PathBuf, String, String, Option<String>) {
    let root = dir.join("repo");
    copy_tree(&fixtures_dir().join("go-drivers/repo"), &root);
    git(&root, C1_DATE, &["init", "-q", "-b", "main"]);
    git(&root, C1_DATE, &["add", "-A"]);
    git(
        &root,
        C1_DATE,
        &["commit", "-q", "-m", "C1: drivers fixture"],
    );
    let c1 = rev_parse(&root, "HEAD");
    git(
        &root,
        C2_DATE,
        &["mv", "drivers/mem.go", "drivers/memory.go"],
    );
    let postgres = root.join("drivers/postgres.go");
    let src = std::fs::read_to_string(&postgres).unwrap();
    let patched = src.replace("\n\t_ = marker\n", "\n\t_ = marker\n\t_ = dsn\n");
    assert_ne!(src, patched, "the marker line is in the fixture");
    std::fs::write(&postgres, patched).unwrap();
    git(&root, C2_DATE, &["add", "-A"]);
    git(&root, C2_DATE, &["commit", "-q", "-m", C2_SUMMARY]);
    let c2 = rev_parse(&root, "HEAD");
    assert_eq!(c2, GO_C2, "the scripted history reproduces C2 (COMMITS)");
    let c3 = with_c3.then(|| {
        let memory = root.join("drivers/memory.go");
        let src = std::fs::read_to_string(&memory).unwrap();
        let renamed = src.replace("MemDriver", "MemoryDriver");
        assert_ne!(src, renamed, "memory.go names MemDriver");
        std::fs::write(&memory, renamed).unwrap();
        git(&root, C3_DATE, &["add", "-A"]);
        git(&root, C3_DATE, &["commit", "-q", "-m", C3_SUMMARY]);
        rev_parse(&root, "HEAD")
    });
    (root, c1, c2, c3)
}

/// The TypeScript fixture tree (`ts-drivers/repo`, committed with no git history) as a
/// repository with one commit under the fixed identity: `(root, head)`.
fn ts_repo(dir: &Path) -> (PathBuf, String) {
    let root = dir.join("repo");
    copy_tree(&fixtures_dir().join("ts-drivers/repo"), &root);
    git(&root, C1_DATE, &["init", "-q", "-b", "main"]);
    git(&root, C1_DATE, &["add", "-A"]);
    git(
        &root,
        C1_DATE,
        &["commit", "-q", "-m", "TS: drivers fixture"],
    );
    let head = rev_parse(&root, "HEAD");
    (root, head)
}

// ---------------------------------------------------------------------------------------------
// policies and snapshots

/// `examples/policy-test-canaries.yaml` as committed (`defaults.action: reject`).
fn reject_policy() -> Policy {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/policy-test-canaries.yaml");
    Policy::load(&path).expect("the canary policy loads")
}

/// The canary policy as a redacting one (`defaults.action: redact`, the rules unchanged),
/// written under `dir`: the server loads it by path and the snapshot is published under the
/// same effective rule set, so `snapshot.policy` is `current`. Returns `(path, policy)`.
fn redact_policy(dir: &Path) -> (PathBuf, Policy) {
    let src =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/policy-test-canaries.yaml");
    let text = std::fs::read_to_string(&src).unwrap();
    let redact = text.replacen("action: reject", "action: redact", 1);
    assert_ne!(text, redact, "the canary policy's default action is reject");
    let path = dir.join("policy-redact.yaml");
    std::fs::write(&path, redact).unwrap();
    let policy = Policy::load(&path).expect("the redact variant loads");
    (path, policy)
}

fn meta(commit: &str, repo_id: &str, policy: &Policy) -> Meta {
    Meta {
        commit: Commit::parse(commit).expect("40-hex commit"),
        repo_id: repo_id.to_owned(),
        policy_version: policy.effective_hash().to_owned(),
        provenance: Provenance {
            isolation: IsolationTier::None,
            isolation_detail: "test".to_owned(),
            resources: Vec::new(),
            container_image_digest: None,
        },
    }
}

/// Decode, gate, admit and derive the rows of index A (and Go run B) under `policy`.
fn derive_rows(a_bytes: &[u8], b_bytes: Option<&[u8]>, policy: &Policy) -> Rows {
    let a = decode_and_gate(a_bytes, policy).expect("decode and gate A");
    let b = b_bytes.map(|b| decode_and_gate(b, policy).expect("decode and gate B"));
    let admitted_a = admit_index(&a);
    let admitted_b = b.as_ref().map(|b| admit_index(b));
    Rows::derive(&admitted_a, admitted_b.as_ref(), policy)
}

/// The go-drivers rows (run A + run B) under `policy`.
fn go_rows(policy: &Policy) -> Rows {
    derive_rows(
        &fixture_bytes("go-drivers/index.scip"),
        Some(&fixture_bytes("go-drivers/index-notest.scip")),
        policy,
    )
}

/// Publish `<dir>/<commit>.cg.db` from `rows` with the snippets collected from `repo` at
/// `commit` (`kaban-index`'s own order: derive, collect, publish). Returns the file.
fn publish_with_snippets(
    dir: &Path,
    repo: &Path,
    commit: &str,
    repo_id: &str,
    policy: &Policy,
    mut rows: Rows,
) -> PathBuf {
    let collected = snippets::collect_with(repo, commit, &rows, policy, InheritedFds::Tolerate);
    assert!(
        !collected.rows.is_empty(),
        "snippets were collected from the scratch repository at {commit}"
    );
    rows.set_snippets(collected);
    writer::publish(dir, &rows, &meta(commit, repo_id, policy), policy).expect("publish")
}

/// Publish `<dir>/<commit>.cg.db` from `rows` with every symbol whose `ref` contains
/// `marker` removed (its edges and occurrences with it), through the public write helpers
/// (`code_tools.rs` `Synth::publish` pattern): CG10.2 stop 2's default for the C3 snapshot,
/// which no indexer on this box can produce.
fn publish_without(
    dir: &Path,
    commit: &str,
    repo_id: &str,
    policy: &Policy,
    rows: &Rows,
    marker: &str,
) -> PathBuf {
    let mut map: BTreeMap<i64, i64> = BTreeMap::new();
    let mut symbol_rows: Vec<write::SymbolRow> = Vec::new();
    for s in rows.symbols() {
        if s.r#ref.contains(marker) {
            continue;
        }
        let new = NodeId(i64::try_from(symbol_rows.len() + 1).unwrap());
        map.insert(s.id.0, new.0);
        let mut row = s.clone();
        row.id = new;
        symbol_rows.push(row);
    }
    assert!(
        symbol_rows.len() < rows.symbols().len(),
        "the marker names rows to remove"
    );
    let remap = |id: NodeId| map.get(&id.0).map(|n| NodeId(*n));
    let edge_rows: Vec<write::EdgeRow> = rows
        .edges()
        .iter()
        .filter_map(|e| {
            Some(write::EdgeRow {
                src: remap(e.src)?,
                dst: remap(e.dst)?,
                r#type: e.r#type,
                weight: e.weight,
            })
        })
        .collect();
    let occ_rows: Vec<write::OccurrenceRow> = rows
        .occurrences()
        .iter()
        .filter_map(|o| {
            Some(write::OccurrenceRow {
                symbol_id: remap(o.symbol_id)?,
                file_id: o.file_id,
                range: o.range,
                roles: o.roles,
                enclosing_id: o.enclosing_id.and_then(remap),
            })
        })
        .collect();
    let mut meta_rows: BTreeMap<String, String> = rows
        .meta_rows(&meta(commit, repo_id, policy), policy)
        .rows
        .into_iter()
        .collect();
    meta_rows.insert("counts:symbols".into(), symbol_rows.len().to_string());
    meta_rows.insert("counts:edges".into(), edge_rows.len().to_string());
    meta_rows.insert("counts:occurrences".into(), occ_rows.len().to_string());
    let meta_refs: Vec<(&str, &str)> = meta_rows
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();

    let path = dir.join(format!("{commit}.cg.db"));
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
    write::insert_files(&conn, rows.files()).unwrap();
    write::insert_symbols(&conn, &symbol_rows).unwrap();
    write::insert_edges(&conn, &edge_rows).unwrap();
    write::insert_occurrences(&conn, &occ_rows).unwrap();
    write::insert_snippets(&conn, &[]).unwrap();
    write::insert_edge_types(&conn, &Rows::edge_types()).unwrap();
    write::insert_meta(&conn, &meta_refs).unwrap();
    write::rebuild_fts(&conn).unwrap();
    write::finish(&conn).unwrap();
    path
}

/// The `canary-sym` variant (`fixtures/codegraph/synthetic/canary-sym.scip`, README) published
/// under the committed reject policy into `dir`, as `goldens.rs` publishes it.
fn publish_canary_sym(dir: &Path, repo_id: &str) -> PathBuf {
    let policy = reject_policy();
    let rows = derive_rows(&fixture_bytes("synthetic/canary-sym.scip"), None, &policy);
    writer::publish(
        dir,
        &rows,
        &meta(CANARY_SYM_COMMIT, repo_id, &policy),
        &policy,
    )
    .expect("publish canary-sym")
}

/// One `meta` value of a published snapshot.
fn meta_value(snapshot: &Path, key: &str) -> String {
    let conn =
        rusqlite::Connection::open_with_flags(snapshot, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    conn.query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
        .unwrap()
}

// ---------------------------------------------------------------------------------------------
// the scenario shared by the north-star tests and the sweep

/// One fixture-derived black-box run: the scratch (kept alive for the sweep), the files and
/// every answer.
struct Run {
    s: Scratch,
    repo: PathBuf,
    snapshots: Vec<PathBuf>,
    export: PathBuf,
    d1: String,
    d2: String,
    why_current: Value,
    why_all: Value,
    why_elsewhere: Value,
    history: Value,
    context: Value,
    tests_for: Value,
    finished: Finished,
}

/// Which fixture a run drives.
struct Half {
    /// Scratch name.
    name: &'static str,
    /// The anchor key (`PostgresDriver#Open` / `PostgresDriver#open`).
    key: String,
    /// A key nothing governs (the negative of step 4).
    elsewhere: String,
    /// `path_scope` of both Decisions.
    scope: &'static str,
    /// The anchor's `relative_path`.
    path: &'static str,
    /// The expected `last_change.commit` (C2 for Go, the one commit for TS).
    commit: String,
    /// The `TESTED_BY` form the fixture yields for the anchor (stage1-spec §4.3 table):
    /// `reference` for Go (`TestPostgresOpen` is a Function in a test file), `naming` for TS
    /// (`test('open', () => ..)` defines no Function or Method, so the only edge is the
    /// file-level naming form, as the committed `ts-drivers/goldens/edges.txt` holds).
    test_form: &'static str,
}

/// Steps 2 to 6 of §8.3 over MCP against a server started in `repo`, with the snapshot(s)
/// already published under `home`.
fn drive(
    s: Scratch,
    repo: PathBuf,
    policy_path: &Path,
    snapshots: Vec<PathBuf>,
    half: &Half,
) -> Run {
    let export = s.0.join("audit.jsonl");
    let repo_name = repo.file_name().unwrap().to_str().unwrap().to_owned();
    let mut c = McpClient::start(&Spawn {
        home: &s.home(),
        db: &s.db(),
        cwd: &repo,
        policy: Some(policy_path),
        workspace: WORKSPACE,
        audit_export: Some(&export),
    });
    let anchor = json!({
        "repo": repo_name, "commit": half.commit, "tool": "manual",
        "relative_path": half.path, "name_path": "PostgresDriver/Open", "kind": "Method",
        "symbol": half.key, "snapshot_commit": half.commit,
    });
    // step 2: D1 with the scope and the symbol anchor, accepted
    let d1 = c.call(
        "remember",
        &json!({
            "kind": "Decision", "text": "drivers implement `DatabaseDriver` directly",
            "props": {"rationale": "one interface, no base type"},
            "path_scope": [half.scope], "refs": [anchor],
        }),
    )["id"]
        .as_str()
        .unwrap()
        .to_owned();
    c.accept_confirmed(&d1, "reviewed", &[half.scope]);
    // step 3: D2 same scope and anchor, accepted explicitly, then the SUPERSEDES link
    let d2 = c.call(
        "remember",
        &json!({
            "kind": "Decision", "text": "drivers wrap a shared base",
            "props": {"rationale": "shared retry and logging"},
            "path_scope": [half.scope], "refs": [anchor],
        }),
    )["id"]
        .as_str()
        .unwrap()
        .to_owned();
    c.accept_confirmed(&d2, "reviewed", &[half.scope]);
    let link = c.call("link", &json!({"from": d2, "rel": "SUPERSEDES", "to": d1}));
    assert!(link["id"].is_string(), "{link}");
    // step 4
    let why_current = c.call("why", &json!({"targets": [{"symbol": half.key}]}));
    let why_all = c.call(
        "why",
        &json!({"targets": [{"symbol": half.key}], "include_superseded": true}),
    );
    let why_elsewhere = c.call("why", &json!({"targets": [{"symbol": half.elsewhere}]}));
    // step 5
    let history = c.call("governance_history", &json!({"id": d1}));
    // step 6
    let context = c.call("code_context", &json!({"target": {"symbol": half.key}}));
    // the test link of the defining file (both forms; the TS half's only form is naming)
    let tests_for = c.call("code_tests_for", &json!({"target": {"path": half.path}}));
    let finished = c.finish();
    assert!(finished.status.success(), "the server exited cleanly");
    Run {
        s,
        repo,
        snapshots,
        export,
        d1,
        d2,
        why_current,
        why_all,
        why_elsewhere,
        history,
        context,
        tests_for,
        finished,
    }
}

/// The Go half: replay C1..C2, publish `<C2>.cg.db` (snippets from the scratch repository),
/// drive the steps.
fn run_go() -> Run {
    eprintln!(
        "acceptance_north_star_go: fixture-derived (committed go-drivers/index.scip, VERSIONS row 1); scip-go NOT RUN"
    );
    let s = Scratch::new("kaban-north-star", "go");
    let (repo, _c1, c2, _) = go_repo(&s.0, false);
    let (policy_path, policy) = redact_policy(&s.0);
    let key = write_codegraph_key(&s.home());
    let dir = snapshot_dir(&s.home(), &key, &repo);
    let repo_id = dir.file_name().unwrap().to_str().unwrap().to_owned();
    let snapshot = publish_with_snippets(&dir, &repo, &c2, &repo_id, &policy, go_rows(&policy));
    let half = Half {
        name: "go",
        key: format!("{GO_KEY}PostgresDriver#Open()."),
        elsewhere: format!("{GO_SQLITE_KEY}Driver#Open()."),
        scope: "drivers/",
        path: "drivers/postgres.go",
        commit: c2,
        test_form: "reference",
    };
    drive(s, repo, &policy_path, vec![snapshot], &half)
}

/// The TypeScript half: one commit of the committed tree, the committed `index.scip`
/// published under it (the index is commit-agnostic), the same steps with `src/`.
fn run_ts() -> Run {
    eprintln!(
        "acceptance_north_star_ts: fixture-derived (committed ts-drivers/index.scip, VERSIONS row 3, no scripted history: one commit of repo/); scip-typescript NOT RUN"
    );
    let s = Scratch::new("kaban-north-star", "ts");
    let (repo, head) = ts_repo(&s.0);
    let (policy_path, policy) = redact_policy(&s.0);
    let key = write_codegraph_key(&s.home());
    let dir = snapshot_dir(&s.home(), &key, &repo);
    let repo_id = dir.file_name().unwrap().to_str().unwrap().to_owned();
    let rows = derive_rows(&fixture_bytes("ts-drivers/index.scip"), None, &policy);
    let snapshot = publish_with_snippets(&dir, &repo, &head, &repo_id, &policy, rows);
    let half = Half {
        name: "ts",
        key: format!("{TS_KEY}`postgres.ts`/PostgresDriver#open()."),
        elsewhere: TS_ELSEWHERE.to_owned(),
        scope: "src/",
        path: "src/postgres.ts",
        commit: head,
        test_form: "naming",
    };
    drive(s, repo, &policy_path, vec![snapshot], &half)
}

fn ids(cards: &Value) -> Vec<&str> {
    cards
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["node"]["id"].as_str().unwrap())
        .collect()
}

/// The §8.3 step 4 to 6 assertions over a run's answers (both halves).
fn assert_north_star(r: &Run, half: &Half) {
    let (d1, d2) = (r.d1.as_str(), r.d2.as_str());
    // step 4, positive: D2 and not D1, matched by the symbol, accepted, the snapshot current
    assert_eq!(
        ids(&r.why_current["results"]),
        vec![d2],
        "{}",
        r.why_current
    );
    let card = &r.why_current["results"][0];
    assert_eq!(card["matched_by"], json!("symbol"), "{card}");
    assert_eq!(card["governance"], json!("accepted"), "{card}");
    assert_eq!(card["superseded_by"], json!([]), "{card}");
    assert_eq!(
        card["code"],
        json!({"snapshot": {"commit": half.commit, "exact": true, "policy": "current"}}),
        "{card}"
    );
    assert!(
        card["currency"]["state"].is_string(),
        "currency computed: {card}"
    );
    // step 4, negative: with the superseded records, D1 behind D2 with `superseded_by: [D2]`
    let all = ids(&r.why_all["results"]);
    assert_eq!(all, vec![d2, d1], "{}", r.why_all);
    let old = &r.why_all["results"][1];
    assert_eq!(old["governance"], json!("superseded"), "{old}");
    assert_eq!(old["superseded_by"], json!([d2]), "{old}");
    // a key nothing governs answers nothing
    assert_eq!(r.why_elsewhere["results"], json!([]), "{}", r.why_elsewhere);
    // step 5: both records with their chains, D1's events newest first
    let records = r.history["records"].as_array().unwrap();
    let find = |id: &str| {
        records
            .iter()
            .find(|h| h["card"]["id"] == json!(id))
            .unwrap_or_else(|| panic!("{id} in governance_history: {}", r.history))
    };
    let h1 = find(d1);
    assert_eq!(h1["governance"], json!("superseded"), "{h1}");
    assert_eq!(h1["superseded_by"], json!([d2]), "{h1}");
    let kinds: Vec<&str> = h1["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, vec!["superseded", "accepted"], "{h1}");
    let h2 = find(d2);
    assert_eq!(h2["governance"], json!("accepted"), "{h2}");
    assert_eq!(h2["supersedes"], json!([d1]), "{h2}");
    assert_eq!(h2["superseded_by"], json!([]), "{h2}");
    // step 6
    let ctx = &r.context;
    assert_eq!(ctx["snapshot"]["commit"], json!(half.commit), "{ctx}");
    assert_eq!(ids(&ctx["governing"]), vec![d2], "{ctx}");
    assert_eq!(ctx["governing"][0]["superseded_by"], json!([]), "{ctx}");
    assert_eq!(ctx["approximate"], json!(true), "{ctx}");
    // G1-F17 slices `def.sl ..= min(def.el, def.sl + 7)`: a Go or TypeScript method's `def`
    // is its identifier range (one line), so the fixture snippet is the signature line and
    // the body line carrying the canary is never collected (the spec's "snippet with the
    // canary redacted" has no byte to redact here; reported). What holds: a snippet is stored
    // and carries no canary byte (the sweep scans the whole response as well).
    let snippet = ctx["definition"]["snippet"]
        .as_str()
        .unwrap_or_else(|| panic!("the definition carries a snippet: {ctx}"));
    assert!(
        snippet.to_ascii_lowercase().contains("open(dsn"),
        "the definition line: {snippet}"
    );
    for (needle, class) in needles() {
        assert!(!snippet.contains(&needle), "canary {class} in the snippet");
    }
    let tests = ctx["tests"].as_array().unwrap();
    if half.test_form == "reference" {
        assert_eq!(tests.len(), 1, "one test link: {ctx}");
        assert_eq!(tests[0]["matched_by"], json!("reference"), "{ctx}");
        assert_eq!(tests[0]["heuristic"], json!(false), "{ctx}");
    } else {
        // no Function or Method in the test file: the symbol has no reference-form link
        // and `code_context` lists none; the test module reaches `callers` (`is_test`)
        assert_eq!(tests.len(), 0, "no symbol-level test link: {ctx}");
        assert!(
            ctx["callers"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c["is_test"] == json!(true)),
            "the test module references the symbol: {ctx}"
        );
    }
    // the file's test link through `code_tests_for(path)`: the naming form, heuristic
    let by_file = r.tests_for["tests"].as_array().unwrap();
    assert!(
        by_file
            .iter()
            .any(|t| t["matched_by"] == json!("naming") && t["heuristic"] == json!(true)),
        "the defining file's naming-form test link: {}",
        r.tests_for
    );
    assert_eq!(
        ctx["last_change"]["commit"],
        json!(half.commit),
        "last_change ({}): {ctx}",
        half.name
    );
}

// ---------------------------------------------------------------------------------------------
// the tests

/// §8.3 over the go-drivers fixture through the public tools (fixture-derived, default 1).
#[test]
fn acceptance_north_star_go() {
    let r = run_go();
    let half = Half {
        name: "go",
        key: format!("{GO_KEY}PostgresDriver#Open()."),
        elsewhere: format!("{GO_SQLITE_KEY}Driver#Open()."),
        scope: "drivers/",
        path: "drivers/postgres.go",
        commit: GO_C2.to_owned(),
        test_form: "reference",
    };
    assert_north_star(&r, &half);
    assert_eq!(r.context["last_change"]["summary"], json!(C2_SUMMARY));
    let test = &r.context["tests"][0]["test"];
    assert_eq!(
        test["symbol"],
        json!(format!("{GO_KEY}TestPostgresOpen().")),
        "{test}"
    );
    assert_eq!(test["path"], json!("drivers/postgres_test.go"));
}

/// §8.3 over the ts-drivers fixture through the public tools (fixture-derived: the committed
/// index under one scratch commit, item 5).
#[test]
fn acceptance_north_star_ts() {
    let r = run_ts();
    let head = r.context["snapshot"]["commit"].as_str().unwrap().to_owned();
    let half = Half {
        name: "ts",
        key: format!("{TS_KEY}`postgres.ts`/PostgresDriver#open()."),
        elsewhere: TS_ELSEWHERE.to_owned(),
        scope: "src/",
        path: "src/postgres.ts",
        commit: head,
        test_form: "naming",
    };
    assert_north_star(&r, &half);
    let link = &r.tests_for["tests"][0];
    assert_eq!(
        link["test"]["path"],
        json!("src/postgres.test.ts"),
        "{link}"
    );
    assert_eq!(
        r.context["last_change"]["summary"],
        json!("TS: drivers fixture")
    );
}

/// Every byte channel of a run, named: the snapshot files, each `tools/call` response, the
/// memory database with its `-wal` and `-shm`, the audit export, the server's stdout and
/// stderr. The fixture repository copy is not a channel (its sources carry the canaries by
/// design) and neither is the key.
fn channels(r: &Run) -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    for p in &r.snapshots {
        out.push((
            format!("snapshot {}", p.file_name().unwrap().to_string_lossy()),
            std::fs::read(p).unwrap(),
        ));
    }
    for (i, resp) in r.finished.responses.iter().enumerate() {
        out.push((format!("mcp response #{i}"), resp.to_string().into_bytes()));
    }
    for (name, suffix) in [
        ("memory db", ""),
        ("memory db -wal", "-wal"),
        ("memory db -shm", "-shm"),
    ] {
        let p = PathBuf::from(format!("{}{suffix}", r.s.db().display()));
        if p.exists() {
            out.push((name.to_owned(), std::fs::read(&p).unwrap()));
        }
    }
    out.push(("audit export".to_owned(), std::fs::read(&r.export).unwrap()));
    out.push((
        "server stdout".to_owned(),
        r.finished.stdout.clone().into_bytes(),
    ));
    out.push((
        "server stderr".to_owned(),
        r.finished.stderr.clone().into_bytes(),
    ));
    out
}

/// §8.3 step 7 (G1-S1; security tier): the three canaries are on no byte of any channel of
/// the Go run, the rename run and the `canary-sym` snapshot; the failing assertion names the
/// channel and the canary class, never the bytes.
#[test]
fn acceptance_canary_absent_every_channel() {
    let needles = needles();
    // the scanner sees a planted needle (the negative of a byte scan)
    for (n, _) in &needles {
        assert!(contains(format!("x{n}y").as_bytes(), n));
    }
    let go = run_go();
    // the inputs carried them: absence downstream is removal, not a fixture without canaries
    let driver = std::fs::read(go.repo.join("drivers/driver.go")).unwrap();
    let postgres = std::fs::read(go.repo.join("drivers/postgres.go")).unwrap();
    assert!(
        contains(&driver, &needles[0].0),
        "the fixture doc comment carries the doc canary"
    );
    assert!(
        contains(&postgres, &needles[1].0),
        "the fixture body carries the snippet canary"
    );
    let canary_dir = go.s.0.join("canary");
    std::fs::create_dir_all(&canary_dir).unwrap();
    std::fs::set_permissions(&canary_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let canary_sym = publish_canary_sym(&canary_dir, "0123456789abcdef0123456789abcdef");
    assert_eq!(
        meta_value(&canary_sym, "counts:policy_skipped_row"),
        "4",
        "the owning rows were skipped (policy_skipped_row)"
    );
    let rename = run_rename();

    let mut failures: Vec<String> = Vec::new();
    let mut scan = |run: &str, name: &str, bytes: &[u8]| {
        for (needle, class) in &needles {
            if contains(bytes, needle) {
                failures.push(format!("{run}: {name}: canary {class} present"));
            }
        }
    };
    for (name, bytes) in channels(&go) {
        scan("go", &name, &bytes);
    }
    scan(
        "canary-sym",
        "snapshot",
        &std::fs::read(&canary_sym).unwrap(),
    );
    for (name, bytes) in rename.channels() {
        scan("rename", &name, &bytes);
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

// ---------------------------------------------------------------------------------------------
// rename over time (walkthrough Step 6; EN-F18a)

/// The rename run's channels for the sweep.
struct RenameRun {
    s: Scratch,
    snapshots: Vec<PathBuf>,
    at_c2: Finished,
    at_c3: Finished,
}

impl RenameRun {
    fn channels(&self) -> Vec<(String, Vec<u8>)> {
        let mut out = Vec::new();
        for p in &self.snapshots {
            out.push((
                format!("snapshot {}", p.file_name().unwrap().to_string_lossy()),
                std::fs::read(p).unwrap(),
            ));
        }
        for (label, f) in [("C2", &self.at_c2), ("C3", &self.at_c3)] {
            for (i, resp) in f.responses.iter().enumerate() {
                out.push((
                    format!("{label} mcp response #{i}"),
                    resp.to_string().into_bytes(),
                ));
            }
            out.push((
                format!("{label} server stdout"),
                f.stdout.clone().into_bytes(),
            ));
            out.push((
                format!("{label} server stderr"),
                f.stderr.clone().into_bytes(),
            ));
        }
        for (name, suffix) in [
            ("memory db", ""),
            ("memory db -wal", "-wal"),
            ("memory db -shm", "-shm"),
        ] {
            let p = PathBuf::from(format!("{}{suffix}", self.s.db().display()));
            if p.exists() {
                out.push((name.to_owned(), std::fs::read(&p).unwrap()));
            }
        }
        out
    }
}

/// C1..C3 replayed; `<C2>.cg.db` from the committed index and `<C3>.cg.db` from the same
/// index with the `MemDriver#` rows removed (stop 2 default). At C2: D anchored on
/// `MemDriver#Open()` with no snapshot scope and D' with `snapshot_commit: C2`, both
/// accepted, both resolved. At C3 (a second server after `git checkout`): `why` returns D
/// with `code: snapshot_mismatch`, `exact: true`; `code_definition` is `unknown_symbol`; D
/// and D' stay `accepted`; D' is not reached (the CG-JOIN v1 §3 scope predicate).
fn run_rename() -> RenameRun {
    eprintln!(
        "anchor_stale_after_rename: fixture-derived (C3 snapshot = committed index minus MemDriver# rows); scip-go NOT RUN"
    );
    let scratch = Scratch::new("kaban-north-star", "rename");
    let (repo, _c1, c2, c3) = go_repo(&scratch.0, true);
    let c3 = c3.unwrap();
    let (policy_path, policy) = redact_policy(&scratch.0);
    let key = write_codegraph_key(&scratch.home());
    let dir = snapshot_dir(&scratch.home(), &key, &repo);
    let repo_id = dir.file_name().unwrap().to_str().unwrap().to_owned();
    let rows = go_rows(&policy);
    let snap_c2 = writer::publish(&dir, &rows, &meta(&c2, &repo_id, &policy), &policy).unwrap();
    let snap_c3 = publish_without(&dir, &c3, &repo_id, &policy, &rows, "MemDriver#");
    let mem_key = format!("{GO_KEY}MemDriver#Open().");
    let repo_name = repo.file_name().unwrap().to_str().unwrap().to_owned();
    let anchor = |scoped: bool| {
        let mut a = json!({
            "repo": repo_name, "commit": c2, "tool": "manual",
            "relative_path": "drivers/memory.go", "name_path": "MemDriver/Open", "kind": "Method",
            "symbol": mem_key,
        });
        if scoped {
            a["snapshot_commit"] = json!(c2);
        }
        a
    };
    let spawn = |cwd: &Path| {
        McpClient::start(&Spawn {
            home: &scratch.home(),
            db: &scratch.db(),
            cwd,
            policy: Some(&policy_path),
            workspace: WORKSPACE,
            audit_export: None,
        })
    };

    // at C2
    git(&repo, C2_DATE, &["checkout", "-q", &c2]);
    let mut at_c2_srv = spawn(&repo);
    let remember = |c: &mut McpClient, text: &str, scoped: bool| -> String {
        c.call(
            "remember",
            &json!({
                "kind": "Decision", "text": text, "props": {"rationale": "memory is enough in tests"},
                "path_scope": ["drivers/"], "refs": [anchor(scoped)],
            }),
        )["id"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let decision = remember(
        &mut at_c2_srv,
        "MemDriver keeps connections in memory",
        false,
    );
    at_c2_srv.accept_confirmed(&decision, "reviewed", &["drivers/"]);
    let d_scoped = remember(&mut at_c2_srv, "MemDriver is the test double", true);
    at_c2_srv.accept_confirmed(&d_scoped, "reviewed", &["drivers/"]);
    let before = at_c2_srv.call("why", &json!({"targets": [{"symbol": mem_key}]}));
    let mut seen = ids(&before["results"]);
    seen.sort_unstable();
    let mut want = vec![decision.as_str(), d_scoped.as_str()];
    want.sort_unstable();
    assert_eq!(seen, want, "both anchored records resolve at C2: {before}");
    for card in before["results"].as_array().unwrap() {
        assert_eq!(
            card["code"],
            json!({"snapshot": {"commit": c2, "exact": true, "policy": "current"}}),
            "{card}"
        );
    }
    let def = at_c2_srv.call("code_definition", &json!({"target": {"symbol": mem_key}}));
    assert_eq!(def["definition"]["symbol"], json!(mem_key), "{def}");
    assert_eq!(
        def["definition"]["path"],
        json!("drivers/memory.go"),
        "{def}"
    );
    let at_c2 = at_c2_srv.finish();
    assert!(at_c2.status.success());

    // at C3: the rename landed, the new snapshot has no MemDriver# row
    git(&repo, C3_DATE, &["checkout", "-q", "main"]);
    assert_eq!(rev_parse(&repo, "HEAD"), c3);
    let mut at_c3_srv = spawn(&repo);
    let after = at_c3_srv.call("why", &json!({"targets": [{"symbol": mem_key}]}));
    assert_eq!(
        ids(&after["results"]),
        vec![decision.as_str()],
        "the unscoped anchor reaches D at C3, the C2-scoped one is not joined: {after}"
    );
    let card = &after["results"][0];
    assert_eq!(
        card["code"],
        json!({"snapshot_mismatch": {"snapshot": {"commit": c3, "exact": true, "policy": "current"}}}),
        "stale, not guessed: {card}"
    );
    assert_eq!(card["governance"], json!("accepted"), "D untouched: {card}");
    assert_eq!(card["matched_by"], json!("symbol"), "{card}");
    let err = at_c3_srv.call_err("code_definition", &json!({"target": {"symbol": mem_key}}));
    assert!(
        err.starts_with("unknown_symbol:"),
        "the old key is unknown in the C3 snapshot: {err}"
    );
    assert!(
        !err.contains("MemDriver"),
        "the fixed text echoes no input: {err}"
    );
    // nothing is guessed onto the new name: no record is reached through the old anchor
    let renamed = format!("{GO_KEY}MemoryDriver#Open().");
    let renamed_why = at_c3_srv.call("why", &json!({"targets": [{"symbol": renamed}]}));
    assert_eq!(renamed_why["results"], json!([]), "{renamed_why}");
    // both Decisions still accepted: D' is unreached by why at C3, not changed
    for id in [&decision, &d_scoped] {
        let h = at_c3_srv.call("governance_history", &json!({"id": id}));
        let rec = &h["records"][0];
        assert_eq!(rec["card"]["id"], json!(id), "{h}");
        assert_eq!(rec["governance"], json!("accepted"), "{h}");
    }
    let at_c3 = at_c3_srv.finish();
    assert!(at_c3.status.success());
    RenameRun {
        s: scratch,
        snapshots: vec![snap_c2, snap_c3],
        at_c2,
        at_c3,
    }
}

/// Walkthrough Step 6 / EN-F18a through the public tools.
#[test]
fn anchor_stale_after_rename() {
    let r = run_rename();
    assert!(
        r.at_c3
            .responses
            .iter()
            .any(|resp| resp.to_string().contains("snapshot_mismatch")),
        "the mismatch reached the wire"
    );
}
