//! The one-sitting UX acceptance of EN-F14a (engine-plan E2.5 acceptance (b)): `kaban review`
//! lists a pending item with its sentence and a first action whose command parses with clap;
//! running that command against the same store removes the item from `review`. Negative:
//! `review --status accepted` is a clap error (not a variant), an unknown `explain` address
//! exits 3, a malformed one exits 4.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::Scratch;
use kaban_core::{
    Actor, BatchAssertion, BatchSubject, ClaimClass, Fixed, IngestStore, Layer, MemoryStore,
    Method, NamespaceSet, NewNode, NodeId, OntologyFile, Op, Rank, RememberOutcome, WorkspaceId,
    WriteBatch, WriteCtx, resolve,
};
use kaban_store_sqlite::SqliteStore;
use serde_json::Value;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::sync::Arc;

const WORKSPACE: &str = "default";

/// `fixtures/reconcile/orange-apple.json`, parsed (the EN-F13 fixture; `explain_golden.rs`
/// is the model of these helpers).
fn fixture() -> Value {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../kaban-store-sqlite/fixtures/reconcile/orange-apple.json"
    );
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// A store reconciling against the shipped files plus the fixture's `infra` ontology; the
/// same ontology is written under `repo/.kaban/ontology/` so the binary loads it too.
fn store_with_infra(s: &Scratch, fx: &Value, repo: &Path) -> SqliteStore {
    let yaml = fx["ontology"].as_str().unwrap();
    let dir = repo.join(".kaban").join("ontology");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("infra.yaml"), yaml).unwrap();
    let mut files = kaban_core::shipped::files().unwrap();
    files.push(OntologyFile::from_yaml(yaml).unwrap());
    let ontology = resolve(&files, &NamespaceSet::new()).unwrap();
    SqliteStore::open(&s.db(), &s.home().join("keys"))
        .unwrap()
        .with_ontology(Arc::new(ontology))
        .unwrap()
}

fn u32_of(v: &Value) -> u32 {
    u32::try_from(v.as_u64().unwrap()).unwrap()
}

/// One fixture row as the claim a producer batch would carry about chunk `c17`.
fn claim(row: &Value, chunk: &NodeId) -> BatchAssertion {
    let text = |k: &str| row[k].as_str().unwrap();
    BatchAssertion {
        subject: BatchSubject::Span {
            chunk: chunk.clone(),
            offset: u32_of(&row["offset"]),
            len: u32_of(&row["len"]),
        },
        claim_class: ClaimClass::Type,
        predicate: None,
        object: None,
        value: row["value"].clone(),
        layer: Layer::parse(text("layer")).unwrap(),
        method: Method::parse(text("method")).unwrap(),
        extractor_id: Some(text("id").to_owned()),
        extractor_version: Some("1".into()),
        source_id: Some("c17".into()),
        lineage_id: text("lineage_id").to_owned(),
        raw_score: Fixed::new(u32_of(&row["raw_score"])).unwrap(),
        rank: row
            .get("rank")
            .and_then(Value::as_str)
            .and_then(Rank::parse)
            .unwrap_or(Rank::Normal),
        rank_reason: None,
        calibration_version: 1,
        supersedes: None,
    }
}

/// Seed chunk `c17` with the fixture rows `ids` as one agent batch: `a5`, `a6`, `a7` make
/// span 56..62 a three-way `candidate: below_margin` group (EN-F13 forward).
fn seed_span_candidate(store: &SqliteStore, fx: &Value, ids: &[&str]) -> NodeId {
    let chunk = NodeId::new();
    let mut b = WriteBatch::new(
        WorkspaceId::default(),
        vec![Op::UpsertNode {
            id: chunk.clone(),
            node: NewNode {
                kind: "Chunk".into(),
                text: "c17".into(),
                ..Default::default()
            },
        }],
    );
    b.assertions = ids
        .iter()
        .map(|id| {
            let row = fx["assertions"]
                .as_array()
                .unwrap()
                .iter()
                .find(|r| r["id"] == *id)
                .unwrap();
            claim(row, &chunk)
        })
        .collect();
    store
        .apply_batch(
            b,
            &WriteCtx::new("v1", WorkspaceId::default(), Actor::Agent),
        )
        .unwrap();
    chunk
}

/// Seed one record of an undeclared kind through the store (no policy gate): under the
/// shipped `review` policy it is a `discovered_kind` candidate (`x:Widget`).
fn seed_discovered(s: &Scratch) -> String {
    let store = SqliteStore::open(&s.db(), &s.home().join("keys")).unwrap();
    let ctx = WriteCtx::new("seed", WorkspaceId::default(), Actor::Agent);
    match store
        .remember(
            NewNode {
                kind: "Widget".into(),
                text: "a widget nobody declared".into(),
                workspace_id: Some(WORKSPACE.into()),
                ..Default::default()
            },
            &[],
            &ctx,
        )
        .unwrap()
    {
        RememberOutcome::Created(n) | RememberOutcome::Existing { node: n, .. } => n.id,
    }
}

fn kaban(s: &Scratch, args: &[&str]) -> Output {
    kaban_in(s, &s.0, args)
}

/// [`kaban`] run with `cwd` as the working directory (`.kaban/ontology/*.yaml` is read there).
fn kaban_in(s: &Scratch, cwd: &Path, args: &[&str]) -> Output {
    kaban_ws(s, cwd, WORKSPACE, args)
}

/// [`kaban_in`] bound to the workspace `ws` instead of [`WORKSPACE`].
fn kaban_ws(s: &Scratch, cwd: &Path, ws: &str, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_kaban"))
        .current_dir(cwd)
        .env("KABAN_HOME", s.home())
        .args(["--db", s.db().to_str().unwrap(), "--workspace", ws])
        .args(args)
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn json_of(out: &Output) -> Value {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "{e}: stdout {:?} stderr {:?}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    })
}

#[test]
fn the_first_action_of_a_review_item_parses_and_removes_the_item_from_review() {
    let s = Scratch::new("kaban-review", "one-sitting");
    let id = seed_discovered(&s);

    let out = kaban(&s, &["review"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let doc = json_of(&out);
    let items = doc["items"].as_array().unwrap();
    // the candidate node and its discovered kind
    assert_eq!(items.len(), 2, "{doc}");
    assert_eq!(
        items[1]["subject"],
        serde_json::json!({"discovered_kind": "x:Widget"})
    );
    assert_eq!(items[0]["subject"], serde_json::json!({"node": id}));
    assert_eq!(
        items[0]["status"],
        serde_json::json!({"candidate": "discovered_kind"})
    );
    assert!(
        items[0]["sentence"]
            .as_str()
            .unwrap()
            .starts_with("candidate: discovered_kind"),
        "{doc}"
    );
    let command = items[0]["first_action"]["command"].as_str().unwrap();
    let owned = shell_words(command);
    let words: Vec<&str> = owned.iter().map(String::as_str).collect();
    assert_eq!(words[0], "kaban", "{command}");

    // the command parses with clap: `--help` on the same argv prints the subcommand's help
    let mut help: Vec<&str> = words[1..].to_vec();
    help.push("--help");
    let parsed = kaban(&s, &help);
    assert_eq!(
        parsed.status.code(),
        Some(0),
        "{command} --help: {}",
        String::from_utf8_lossy(&parsed.stderr)
    );

    // and run against the store it removes the item
    let run = kaban(&s, &words[1..]);
    assert_eq!(
        run.status.code(),
        Some(0),
        "{command}: {}",
        String::from_utf8_lossy(&run.stderr)
    );
    let after = json_of(&kaban(&s, &["review"]));
    let left: Vec<&Value> = after["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["subject"]["node"] == serde_json::json!(id))
        .collect();
    assert!(
        left.is_empty(),
        "the accepted record is still listed: {after}"
    );
    // the kind itself stays pending (one released record of an undeclared kind)
    assert_eq!(after["items"].as_array().unwrap().len(), 1, "{after}");

    // `explain` of the released record: settled, nothing pending, no action
    let x = json_of(&kaban(&s, &["explain", &id]));
    assert_eq!(x["status"], serde_json::json!({"settled": "active"}), "{x}");
    assert_eq!(x["next_actions"], serde_json::json!([]));
}

/// Seed one chunk with two `candidate: below_margin` span groups: the fixture rows `a5`, `a6`,
/// `a7` at their own span (56..62) and the same three at 10..16 (RW3-3's two groups on one
/// node; `review` lists them in group-key order, 10..16 first).
fn seed_two_span_candidates(store: &SqliteStore, fx: &Value) -> NodeId {
    seed_two_span_candidates_in(store, fx, WorkspaceId::DEFAULT)
}

/// [`seed_two_span_candidates`] in the workspace `ws`.
fn seed_two_span_candidates_in(store: &SqliteStore, fx: &Value, ws: &str) -> NodeId {
    let chunk = NodeId::new();
    let rows: Vec<BatchAssertion> = ["a5", "a6", "a7"]
        .iter()
        .map(|id| {
            let row = fx["assertions"]
                .as_array()
                .unwrap()
                .iter()
                .find(|r| r["id"] == *id)
                .unwrap();
            claim(row, &chunk)
        })
        .collect();
    let at_10 = rows.iter().cloned().map(|mut c| {
        c.subject = BatchSubject::Span {
            chunk: chunk.clone(),
            offset: 10,
            len: 6,
        };
        c
    });
    let mut b = WriteBatch::new(
        WorkspaceId::new(ws),
        vec![Op::UpsertNode {
            id: chunk.clone(),
            node: NewNode {
                kind: "Chunk".into(),
                text: "c17".into(),
                ..Default::default()
            },
        }],
    );
    b.assertions = rows.iter().cloned().chain(at_10).collect();
    store
        .apply_batch(b, &WriteCtx::new("v1", WorkspaceId::new(ws), Actor::Agent))
        .unwrap();
    chunk
}

/// The words of a rendered action as a POSIX shell would hand them to the binary: split on
/// spaces, a single-quoted word kept whole without its quotes (a group key is canonical JSON),
/// an unquoted backslash taking the next character literally (the `'\''` apostrophe idiom).
fn shell_words(command: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut word = String::new();
    let mut quoted = false;
    let mut open = false;
    let mut chars = command.chars();
    while let Some(ch) = chars.next() {
        match ch {
            '\'' => {
                quoted = !quoted;
                open = true;
            }
            '\\' if !quoted => {
                if let Some(next) = chars.next() {
                    word.push(next);
                    open = true;
                }
            }
            ' ' if !quoted => {
                if open || !word.is_empty() {
                    out.push(std::mem::take(&mut word));
                    open = false;
                }
            }
            _ => word.push(ch),
        }
    }
    if open || !word.is_empty() {
        out.push(word);
    }
    out
}

/// RW3-3 (EN-F14a, EN-F5a): with two candidate groups on one chunk, `kaban review` lists two
/// items and the second item's rendered first action, run as rendered, decides the second
/// group and leaves the first untouched: one item remains and it is the first group's. Red at
/// the base: the action names no group, the store takes the first group, and the item left is
/// the second's. Negative: the same command again is exit 3 (that group is decided) and the
/// first group's item is still listed.
#[test]
fn the_second_candidate_groups_action_decides_that_group_and_leaves_the_first() {
    let s = Scratch::new("kaban-review", "second-group");
    let repo = s.0.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let fx = fixture();
    let store = store_with_infra(&s, &fx, &repo);
    let chunk = seed_two_span_candidates(&store, &fx);
    drop(store);

    let doc = json_of(&kaban_in(&s, &repo, &["review"]));
    let items = doc["items"].as_array().unwrap();
    assert_eq!(items.len(), 2, "{doc}");
    assert!(
        items
            .iter()
            .all(|i| i["subject"] == serde_json::json!({"node": chunk.as_str()})),
        "{doc}"
    );
    let first_group = items[0]["group_key"].as_str().unwrap().to_owned();
    let second_group = items[1]["group_key"].as_str().unwrap().to_owned();
    assert_ne!(first_group, second_group, "{doc}");
    let command = items[1]["first_action"]["command"].as_str().unwrap();
    assert!(command.starts_with("kaban resolve "), "{command}");
    let words = shell_words(command);
    let args: Vec<&str> = words[1..].iter().map(String::as_str).collect();

    let run = kaban_in(&s, &repo, &args);
    assert_eq!(
        run.status.code(),
        Some(0),
        "{command}: {}",
        String::from_utf8_lossy(&run.stderr)
    );
    let report = json_of(&run);
    assert_eq!(report["id"], serde_json::json!(chunk.as_str()), "{report}");
    assert!(
        report.get("group_key").is_none(),
        "the group's canonical key names row ids and stays inside the database (SS-27): {report}"
    );
    let after = json_of(&kaban_in(&s, &repo, &["review"]));
    let left = after["items"].as_array().unwrap();
    assert_eq!(left.len(), 1, "{after}");
    assert_eq!(
        left[0]["group_key"],
        serde_json::json!(first_group),
        "the first group is untouched and the second decided: {after}"
    );

    // negative: the decided group is gone, so the same command is exit 3 and nothing moves
    let again = kaban_in(&s, &repo, &args);
    assert_eq!(
        again.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&again.stderr)
    );
    let still = json_of(&kaban_in(&s, &repo, &["review"]));
    assert_eq!(still["items"], after["items"], "{still}");
}

/// Acceptance (b) over `resolve --winner` (engine-plan E2.5; EN-F5a, EN-F12c): the first
/// action of a `candidate: below_margin` span item is `kaban resolve <chunk> --winner
/// <value>`; it parses, and run it removes the chunk from `review` and settles `explain`.
/// Negative: the same command run again is exit 3 (the chunk has no candidate group left),
/// and nothing else changes.
#[test]
fn the_winner_first_action_of_a_span_candidate_parses_and_removes_the_item_from_review() {
    let s = Scratch::new("kaban-review", "winner");
    let repo = s.0.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let fx = fixture();
    let store = store_with_infra(&s, &fx, &repo);
    let chunk = seed_span_candidate(&store, &fx, &["a5", "a6", "a7"]);
    drop(store);

    let out = kaban_in(&s, &repo, &["review"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let doc = json_of(&out);
    let items = doc["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "{doc}");
    let item = &items[0];
    assert_eq!(item["subject"], serde_json::json!({"node": chunk.as_str()}));
    assert_eq!(
        item["status"],
        serde_json::json!({"candidate": "below_margin"}),
        "{doc}"
    );
    let command = item["first_action"]["command"].as_str().unwrap();
    let group = item["group_key"].as_str().unwrap();
    assert_eq!(
        command,
        format!("kaban resolve {chunk} --winner infra:Team --group '{group}'"),
        "{doc}"
    );
    let owned = shell_words(command);
    let words: Vec<&str> = owned.iter().map(String::as_str).collect();

    // the command parses with clap
    let mut help: Vec<&str> = words[1..].to_vec();
    help.push("--help");
    let parsed = kaban_in(&s, &repo, &help);
    assert_eq!(
        parsed.status.code(),
        Some(0),
        "{command} --help: {}",
        String::from_utf8_lossy(&parsed.stderr)
    );

    // and run against the store it removes the item: the typed response names the value
    let run = kaban_in(&s, &repo, &words[1..]);
    assert_eq!(
        run.status.code(),
        Some(0),
        "{command}: {}",
        String::from_utf8_lossy(&run.stderr)
    );
    let report = json_of(&run);
    assert_eq!(report["id"], serde_json::json!(chunk.as_str()), "{report}");
    assert_eq!(report["value"], serde_json::json!("infra:Team"), "{report}");
    let after = json_of(&kaban_in(&s, &repo, &["review"]));
    assert_eq!(
        after["items"],
        serde_json::json!([]),
        "the decided chunk is still listed: {after}"
    );
    let x = json_of(&kaban_in(&s, &repo, &["explain", chunk.as_str()]));
    assert_eq!(x["status"], serde_json::json!({"settled": "active"}), "{x}");
    assert_eq!(x["next_actions"], serde_json::json!([]));

    // negative: no candidate group is left, so the same command is exit 3 and writes nothing
    let again = kaban_in(&s, &repo, &words[1..]);
    assert_eq!(
        again.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&again.stderr)
    );
    let x = json_of(&kaban_in(&s, &repo, &["explain", chunk.as_str()]));
    assert_eq!(x["status"], serde_json::json!({"settled": "active"}), "{x}");
}

#[test]
fn a_settled_word_is_a_clap_error_and_an_unknown_address_exits_3() {
    let s = Scratch::new("kaban-review", "negative");
    let _ = seed_discovered(&s);
    let out = kaban(&s, &["review", "--status", "accepted"]);
    assert_eq!(
        out.status.code(),
        Some(4),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let out = kaban(&s, &["review", "--status", "discovered"]);
    assert_eq!(out.status.code(), Some(0));
    let doc = json_of(&out);
    assert_eq!(doc["items"].as_array().unwrap().len(), 1, "{doc}");
    assert_eq!(
        doc["items"][0]["subject"],
        serde_json::json!({"discovered_kind": "x:Widget"})
    );
    let out = kaban(&s, &["review", "--status", "candidate"]);
    assert_eq!(json_of(&out)["items"].as_array().unwrap().len(), 1);
    let unknown = kaban_core::NodeId::new().to_string();
    let out = kaban(&s, &["explain", &unknown]);
    assert_eq!(
        out.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let out = kaban(&s, &["explain", "x:Nope"]);
    assert_eq!(
        out.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let out = kaban(&s, &["explain", "not an address"]);
    assert_eq!(
        out.status.code(),
        Some(4),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // the discovered kind itself is an address
    let out = kaban(&s, &["explain", "x:Widget"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        json_of(&out)["status"],
        serde_json::json!({"pending": {"discovered": {"count": 1}}})
    );
}

/// The G1-F9 key of the `afero` fixture (`MemMapFs.Create`, versions normalised to `.`).
const AFERO_KEY: &str = "scip-go gomod github.com/spf13/afero . MemMapFs#Create().";
/// The key the symbol has after a rename: the one a person re-targets the anchor to.
const RENAMED_KEY: &str = "scip-go gomod github.com/spf13/afero . MemMapFs#CreateFile().";
/// The same symbol in its versioned form (the `scip` column as emitted): refused as a
/// `CodeRef.symbol` (stage1-spec §4.2, OWN-81 (3)).
const VERSIONED_KEY: &str = "scip-go gomod github.com/spf13/afero v1.11.0 MemMapFs#CreateFile().";

/// Seed one `CodeRef` anchored on `key` through the store (no snapshot: the item is not
/// stale, the key form is the only thing under test).
fn seed_coderef(s: &Scratch, key: &str) -> String {
    let store = SqliteStore::open(&s.db(), &s.home().join("keys")).unwrap();
    let ctx = WriteCtx::new("seed", WorkspaceId::default(), Actor::Agent);
    match store
        .remember(
            NewNode {
                kind: "CodeRef".into(),
                text: format!("kaban memmap.go {key}"),
                props: Some(serde_json::json!({
                    "repo": "kaban", "tool": "fixture", "relative_path": "memmap.go",
                    "name_path": "MemMapFs/Create", "kind": "Method", "line": 7, "symbol": key,
                })),
                repo: Some("kaban".into()),
                workspace_id: Some(WORKSPACE.into()),
                ..Default::default()
            },
            &[],
            &ctx,
        )
        .unwrap()
    {
        RememberOutcome::Created(n) | RememberOutcome::Existing { node: n, .. } => n.id,
    }
}

/// The stored `props.symbol` of `id` and the number of human claims about it, read at rest
/// (the property under test is what is on disk after a refusal: nothing).
fn symbol_and_human_claims(s: &Scratch, id: &str) -> (String, i64) {
    let conn = rusqlite::Connection::open(s.db()).unwrap();
    let props: String = conn
        .query_row("SELECT props FROM node WHERE id = ?1", [id], |r| r.get(0))
        .unwrap();
    let props: Value = serde_json::from_str(&props).unwrap();
    let claims: i64 = conn
        .query_row(
            "SELECT count(*) FROM assertion WHERE layer = 'human'
               AND subject_rid = (SELECT rid FROM node WHERE id = ?1)",
            [id],
            |r| r.get(0),
        )
        .unwrap();
    (props["symbol"].as_str().unwrap().to_owned(), claims)
}

/// `kaban resolve <coderef> --symbol <key>` (engine-plan E2.5, SO-6; stage1-spec §4.2):
/// a versioned key and a `local` key are refused `symbol_invalid` (exit 4) and nothing is
/// written: the stored `symbol` and the human claim count are what they were. Positive: the
/// normalised key is accepted, the typed report names it, the stored `symbol` follows and one
/// human claim records the re-target. Red kept (`e25c4-red-symbol-invalid.log`).
#[test]
fn a_versioned_or_local_symbol_is_refused_by_resolve_symbol_and_nothing_is_written() {
    let s = Scratch::new("kaban-review", "symbol-invalid");
    let id = seed_coderef(&s, AFERO_KEY);
    assert_eq!(symbol_and_human_claims(&s, &id), (AFERO_KEY.to_owned(), 0));

    for key in [VERSIONED_KEY, "local x"] {
        let run = kaban(&s, &["resolve", &id, "--symbol", key]);
        assert_eq!(
            run.status.code(),
            Some(4),
            "{key}: {}",
            String::from_utf8_lossy(&run.stderr)
        );
        let stderr = String::from_utf8_lossy(&run.stderr);
        assert!(
            stderr.contains("symbol_invalid") && !stderr.contains("afero"),
            "the refusal names the class, never the key: {stderr}"
        );
        assert_eq!(
            symbol_and_human_claims(&s, &id),
            (AFERO_KEY.to_owned(), 0),
            "{key}: nothing is written on a refusal"
        );
    }

    let run = kaban(&s, &["resolve", &id, "--symbol", RENAMED_KEY]);
    assert_eq!(
        run.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    let report = json_of(&run);
    assert_eq!(report["id"], serde_json::json!(id), "{report}");
    assert_eq!(report["symbol"], serde_json::json!(RENAMED_KEY), "{report}");
    assert_eq!(
        symbol_and_human_claims(&s, &id),
        (RENAMED_KEY.to_owned(), 1)
    );
}

/// RW4-1 (EN-F14a, RW3-3; wave-4 review): a rendered action is safe to paste into a POSIX
/// shell whatever the workspace is called. With a workspace named `team's; touch
/// $W4C_SENTINEL; : '` (an apostrophe, a `;` and a `$`), the second candidate group's
/// rendered `resolve --winner` action run through `sh -c` runs only itself: the sentinel the
/// name plants is never created, the intended group is decided and the first group is left;
/// split by a shell's rules the action is the intended argv, its group the key itself.
/// Red at the base: the key's apostrophe closes the quote, the shell runs `touch` and the
/// action breaks. Negative: the same argv again is exit 3 and nothing moves.
#[cfg(unix)]
#[test]
fn a_rendered_action_is_shell_safe_whatever_the_workspace_is_called() {
    const WS: &str = "team's; touch $W4C_SENTINEL; : '";
    let s = Scratch::new("kaban-review", "shell-safe");
    let repo = s.0.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let fx = fixture();
    let store = store_with_infra(&s, &fx, &repo);
    let chunk = seed_two_span_candidates_in(&store, &fx, WS);
    drop(store);

    let doc = json_of(&kaban_ws(&s, &repo, WS, &["review"]));
    let items = doc["items"].as_array().unwrap();
    assert_eq!(items.len(), 2, "{doc}");
    assert!(
        items
            .iter()
            .all(|i| i["subject"] == serde_json::json!({"node": chunk.as_str()})),
        "{doc}"
    );
    let first_group = items[0]["group_key"].as_str().unwrap().to_owned();
    let second_group = items[1]["group_key"].as_str().unwrap().to_owned();
    assert_ne!(first_group, second_group, "{doc}");
    let command = items[1]["first_action"]["command"].as_str().unwrap();
    assert!(command.starts_with("kaban resolve "), "{command}");

    // the action pasted into a shell: `kaban` on PATH is the binary under test, the store and
    // the workspace come from the environment (`KABAN_DB`, `KABAN_WORKSPACE`), the sentinel
    // is the file the workspace name creates if the shell sees past the action
    let bin = s.0.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_kaban"), bin.join("kaban")).unwrap();
    let sentinel = s.0.join("sentinel");
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let run = Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(&repo)
        .env("PATH", path)
        .env("KABAN_HOME", s.home())
        .env("KABAN_DB", s.db())
        .env("KABAN_WORKSPACE", WS)
        .env("W4C_SENTINEL", &sentinel)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        !sentinel.exists(),
        "the shell ran a command beyond the action (the sentinel exists): {command}"
    );
    assert_eq!(
        run.status.code(),
        Some(0),
        "{command}: {}",
        String::from_utf8_lossy(&run.stderr)
    );
    let report = json_of(&run);
    assert!(
        report.get("group_key").is_none(),
        "the group's canonical key stays inside the database (SS-27): {report}"
    );
    let after = json_of(&kaban_ws(&s, &repo, WS, &["review"]));
    let left = after["items"].as_array().unwrap();
    assert_eq!(left.len(), 1, "{after}");
    assert_eq!(
        left[0]["group_key"],
        serde_json::json!(first_group),
        "the first group is untouched: {after}"
    );

    // split by a shell's rules the action is the intended argv: the `--group` word is the key
    let words = shell_words(command);
    assert_eq!(
        words.last().map(String::as_str),
        Some(second_group.as_str()),
        "{command}"
    );
    assert_eq!(words[words.len() - 2], "--group", "{command}");

    // negative: the group is decided, so the same argv is exit 3 and nothing moves
    let args: Vec<&str> = words[1..].iter().map(String::as_str).collect();
    let again = kaban_ws(&s, &repo, WS, &args);
    assert_eq!(
        again.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&again.stderr)
    );
    let still = json_of(&kaban_ws(&s, &repo, WS, &["review"]));
    assert_eq!(still["items"], after["items"], "{still}");
}
