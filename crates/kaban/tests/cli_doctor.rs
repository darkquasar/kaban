//! `kaban doctor` (core-spec §7; core-plan C4.2 as amended by M4-19, M4-20): the report over
//! a healthy database, a corrupted FTS shadow table (`ok: false`, exit 5), the plain FTS
//! `integrity-check` form with archived and tombstoned rows in the index's content table, the
//! file modes as `stat`ed before the open (never tightened), a database whose parent
//! directory cannot take the WAL sidecars (exit 5), and a `CodeRef` whose symbol the current
//! snapshot no longer holds (`stale_anchors`, E3.11 Step 6). Doctor never creates the database,
//! the audit key or the codegraph key.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

#[path = "../../kaban-api/tests/common/cg_fixture.rs"]
mod cg_fixture;
mod common;
#[path = "common/mcp_client.rs"]
mod mcp_client;

use common::Scratch;
use kaban_core::{
    Actor, ChangeLogStore, ForgetMode, IngestStore, LeaseStore, MemoryStore, NewNode, NodeId,
    RememberOutcome, WorkspaceId, WriteCtx,
};
use kaban_store_sqlite::SqliteStore;
use serde_json::Value;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn ctx() -> WriteCtx {
    WriteCtx::new("v1", WorkspaceId::default(), Actor::Agent)
}

fn note(text: &str) -> NewNode {
    NewNode {
        kind: "Fact".into(),
        text: text.into(),
        ..Default::default()
    }
}

fn created(outcome: RememberOutcome) -> NodeId {
    match outcome {
        RememberOutcome::Created(node) => NodeId::parse(&node.id).unwrap(),
        other @ RememberOutcome::Existing { .. } => {
            panic!("expected a created node, got {other:?}")
        }
    }
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn sidecar(db: &Path, suffix: &str) -> PathBuf {
    let mut s = db.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

/// `kaban --json --db <db> doctor [args..]` with the scratch home.
fn doctor(s: &Scratch, db: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_kaban"))
        .env("KABAN_HOME", s.home())
        .args(["--json", "--db", db.to_str().unwrap(), "doctor"])
        .args(args)
        .output()
        .unwrap()
}

/// The one JSON document on stdout, and the check named `name`.
fn report(out: &Output) -> Value {
    let text = std::str::from_utf8(&out.stdout).unwrap();
    assert_eq!(text.lines().count(), 1, "one compact document: {text:?}");
    serde_json::from_str(text).unwrap()
}

fn check<'a>(report: &'a Value, name: &str) -> &'a Value {
    report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == name)
        .unwrap_or_else(|| panic!("no check {name} in {report}"))
}

/// A directory whose mode is restored on drop, so the scratch can be removed after a panic.
struct ModeGuard(PathBuf, u32);
impl Drop for ModeGuard {
    fn drop(&mut self) {
        let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(self.1));
    }
}

fn seed(s: &Scratch, texts: &[&str]) -> Vec<NodeId> {
    let store = SqliteStore::open(&s.db(), &s.home().join("keys")).unwrap();
    texts
        .iter()
        .map(|t| created(store.remember(note(t), &[], &ctx()).unwrap()))
        .collect()
}

#[test]
fn stale_identifier_marker_requires_explicit_reindex_and_doctor_never_repairs_it() {
    let s = Scratch::new("kaban-doctor", "ident-reindex");
    let ids = seed(&s, &["CanonicalReader stays unchanged"]);
    let store = SqliteStore::open(&s.db(), &s.home().join("keys")).unwrap();
    let canonical = serde_json::to_value(store.get(&ids[0], false).unwrap()).unwrap();
    store.set_index_meta("ident_terms_version", "0").unwrap();
    let metadata = store.index_meta().unwrap().entries;
    for _ in 0..2 {
        let out = doctor(&s, &s.db(), &[]);
        assert_eq!(out.status.code(), Some(5), "{out:?}");
        let value = report(&out);
        assert_eq!(check(&value, "ident_terms_version")["ok"], false);
        assert!(
            check(&value, "ident_terms_version")["detail"]
                .as_str()
                .unwrap()
                .contains("kaban reindex text")
        );
        assert_eq!(check(&value, "audit")["ok"], true);
        assert_eq!(store.index_meta().unwrap().entries, metadata);
        assert_eq!(
            serde_json::to_value(store.get(&ids[0], false).unwrap()).unwrap(),
            canonical
        );
    }
    let out = Command::new(env!("CARGO_BIN_EXE_kaban"))
        .env("KABAN_HOME", s.home())
        .args([
            "--json",
            "--db",
            s.db().to_str().unwrap(),
            "reindex",
            "text",
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let out = doctor(&s, &s.db(), &[]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(check(&report(&out), "ident_terms_version")["ok"], true);
    assert_eq!(
        store.index_meta().unwrap().get("ident_terms_version"),
        Some(kaban_core::ident::IDENT_TERMS_VERSION)
    );
    assert_eq!(
        serde_json::to_value(store.get(&ids[0], false).unwrap()).unwrap(),
        canonical
    );
}

#[test]
fn engine_overrides_are_reported_and_invalid_config_never_creates_a_database() {
    let s = Scratch::new("kaban-doctor", "engine-config");
    std::fs::create_dir_all(s.home()).unwrap();
    let path = s.home().join("config.yaml");
    // ... the retired trigram key among them (spec 05 OWN-M): an unknown key like any other
    for yaml in [
        "budget:\n  l0_rss_mb: 0\n",
        "budget:\n  unknown: 1\n",
        "index:\n  trigram: true\n",
    ] {
        std::fs::write(&path, yaml).unwrap();
        let out = doctor(&s, &s.db(), &[]);
        assert_eq!(out.status.code(), Some(4), "{out:?}");
        assert!(!s.db().exists());
        assert!(!s.home().join("keys").exists());
    }
    std::fs::remove_file(&path).unwrap();
    seed(&s, &["diagnostic config test"]);
    std::fs::write(&path, "budget:\n  l0_rss_mb: 321\n").unwrap();
    let out = doctor(&s, &s.db(), &[]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let value = report(&out);
    let rows = value["budgets"].as_array().unwrap();
    assert_eq!(rows.len(), 8);
    let row = rows
        .iter()
        .find(|row| row["key"] == "budget.l0_rss_mb")
        .unwrap();
    assert_eq!(row["effective"], 321);
    assert_eq!(row["source"], "config");
    assert_eq!(row["cap_lifted"], false);
    assert_eq!(
        check(&value, "layer_embed")["detail"],
        "not_available_in_this_release"
    );
    assert_eq!(
        check(&value, "layer_extractors")["detail"],
        "explicit_selection_only"
    );
}

#[test]
fn a_healthy_database_passes_and_a_corrupted_fts_shadow_table_fails_deterministically() {
    let s = Scratch::new("kaban-doctor", "corrupt");
    // never creates: a missing database is a store error and nothing appears on disk
    let out = doctor(&s, &s.db(), &[]);
    assert_eq!(out.status.code(), Some(5), "{out:?}");
    assert!(!s.db().exists(), "doctor must not create the database");
    assert!(
        out.stdout.is_empty(),
        "no document on a failed open: {out:?}"
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("does not exist"),
        "{out:?}"
    );

    seed(
        &s,
        &[
            "the parser keeps a token lookahead",
            "sqlite wal mode needs the shm sidecar",
            "doctor checks the index against itself",
        ],
    );
    // spec 05 SS-21: a consumer registered at the head (SS-06) that one more write leaves
    // behind; doctor reports its retention lag by name (the rows of that write)
    let lag = {
        let store = SqliteStore::open(&s.db(), &s.home().join("keys")).unwrap();
        let lease = store
            .lease_acquire("export", "doctor-test", 60)
            .unwrap()
            .unwrap();
        let cursor = store.changes_register("export", lease.fence).unwrap();
        store.lease_release(&lease).unwrap();
        created(
            store
                .remember(note("one write after the consumer registered"), &[], &ctx())
                .unwrap(),
        );
        let head = store
            .changes_read(cursor, 10_000)
            .unwrap()
            .last()
            .map_or(cursor, |c| c.seq);
        head - cursor
    };
    assert!(lag > 0, "the write left the consumer behind");
    // the key must already exist: a fresh home gets no key created for this database
    let other = s.0.join("other-home");
    let out = Command::new(env!("CARGO_BIN_EXE_kaban"))
        .env("KABAN_HOME", &other)
        .args(["--db", s.db().to_str().unwrap(), "doctor"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(5), "{out:?}");
    assert!(!other.join("keys").exists(), "doctor must not create a key");

    let out = doctor(&s, &s.db(), &[]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let healthy = report(&out);
    assert_eq!(healthy["ok"], Value::Bool(true), "{healthy}");
    let names: Vec<&str> = healthy["checks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "db_modes",
            "integrity",
            "fts_integrity",
            "claim_exhausted",
            "stale_artifacts",
            "retired_producers",
            "reconcile_cursor_lag",
            "schema_version",
            // engine-plan E2.0 (EN-E10b): `changes_lag` with the deprecated alias row
            "changes_lag",
            "outbox_lag",
            "conflicts",
            // spec 05 SS-21: the retention lag of every registered consumer, by name
            "consumer_lag",
            // spec 05 SS-21: the text index, its payload watermark and its lag behind the
            // canonical head (OWN-M: every database serves its own Tantivy index)
            "text_index",
            "text_index_watermark",
            "text_index_lag",
            "ident_terms_version",
            "layer_fts",
            "layer_gazetteer",
            "layer_embed",
            "layer_extractors",
            "key_file",
            "audit",
            // engine-plan E1.8 (EN-F5): the discovered-kinds facts of the bound workspace
            "discovered",
            // engine-plan E2.0 (OWN-65): `unstamped` events per reason
            "unstamped",
            // engine-plan E2.4 (EN-F12b): the unresolved conflict groups of the workspace
            "unresolved",
            // engine-plan E3.11 Step 6: CodeRef anchors the current snapshot no longer holds
            "stale_anchors"
        ]
    );
    // no codegraph key in this home: nothing to inspect, nothing called renamed or created
    assert_eq!(
        check(&healthy, "stale_anchors")["detail"],
        "cannot inspect: no snapshot"
    );
    assert!(!s.home().join("keys").join("codegraph.key").exists());
    assert!(!s.home().join("codegraph").exists());
    assert!(
        healthy["checks"]
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c["ok"] == true),
        "{healthy}"
    );
    // a fresh database stands at the version every open reaches: nothing is pending
    assert_eq!(
        check(&healthy, "schema_version")["detail"],
        kaban_store_sqlite::LATEST_SCHEMA_VERSION.to_string()
    );
    assert_eq!(
        check(&healthy, "consumer_lag")["detail"],
        format!("export: {lag}; text-index: 0"),
        "{healthy}"
    );
    assert_eq!(check(&healthy, "audit")["detail"], "0 entries");
    assert_eq!(check(&healthy, "text_index")["detail"], "tantivy");
}

/// OWN-P: doctor on a database at another schema version prints the refusal with its
/// guidance and exits 4 (request-shaped: the file is the wrong input for this kaban); no
/// report document, nothing modified.
#[test]
fn doctor_on_a_database_at_another_schema_version_prints_the_refusal_and_exits_4() {
    let s = Scratch::new("kaban-doctor", "other-schema");
    seed(&s, &["a note of the last ladder release"]);
    let older = kaban_store_sqlite::LATEST_SCHEMA_VERSION - 1;
    rusqlite::Connection::open(s.db())
        .unwrap()
        .execute(
            "UPDATE index_meta SET value = ?1 WHERE key = 'schema_version'",
            [older.to_string()],
        )
        .unwrap();
    let out = doctor(&s, &s.db(), &[]);
    assert_eq!(out.status.code(), Some(4), "{out:?}");
    assert!(
        out.stdout.is_empty(),
        "no report on a refused open: {out:?}"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(&format!("schema version {older}"))
            && stderr.contains("re-ingest")
            && !stderr.contains("retired"),
        "the refusal names the version and the way out: {stderr}"
    );
    let version: String = rusqlite::Connection::open(s.db())
        .unwrap()
        .query_row(
            "SELECT value FROM index_meta WHERE key = 'schema_version'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(version, older.to_string(), "doctor modified nothing");
}

#[test]
fn fts_integrity_passes_with_archived_and_tombstoned_rows_and_vacuum_is_its_own_check() {
    let s = Scratch::new("kaban-doctor", "lifecycle");
    let ids = seed(
        &s,
        &[
            "an active note stays in the index",
            "an archived note leaves the index",
            "a tombstoned note leaves the index too",
            "a purged note leaves nothing behind",
        ],
    );
    {
        let store = SqliteStore::open(&s.db(), &s.home().join("keys")).unwrap();
        store.forget(&ids[1], ForgetMode::Archive, &ctx()).unwrap();
        store
            .forget(&ids[2], ForgetMode::Tombstone, &ctx())
            .unwrap();
        store.forget(&ids[3], ForgetMode::Purge, &ctx()).unwrap();
    }
    // the fixture really holds rows the index does not: the rank form of the check would
    // report every one of them as corruption (M4-19), the plain form must not
    let raw = rusqlite::Connection::open(s.db()).unwrap();
    let outside: i64 = raw
        .query_row(
            "SELECT count(*) FROM node WHERE lifecycle IN ('archived', 'tombstoned', 'purged')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(outside, 3);
    drop(raw);

    let out = doctor(&s, &s.db(), &["--vacuum"]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let v = report(&out);
    assert_eq!(v["ok"], Value::Bool(true), "{v}");
    assert_eq!(check(&v, "fts_integrity")["ok"], true, "{v}");
    assert_eq!(check(&v, "integrity")["ok"], true, "{v}");
    assert_eq!(check(&v, "vacuum")["ok"], true, "{v}");
    // the seven checks, the E2.0 `outbox_lag` alias row of `changes_lag` (EN-E10b, until 3.2),
    // the E1.8 `discovered` line, the E2.0 `unstamped` line (OWN-65), the E2.4 `unresolved`
    // line, the three artifact/cursor checks, the E3.11 `stale_anchors` line, the spec 05
    // SS-21 `consumer_lag`, `text_index`, `text_index_watermark` and `text_index_lag` lines
    // and the vacuum line
    assert_eq!(v["checks"].as_array().unwrap().len(), 27);
    // the store's own projector is a registered consumer at the head (spec 05 OWN-M)
    assert_eq!(check(&v, "consumer_lag")["detail"], "text-index: 0");
    // without the flag there is no vacuum line
    let v = report(&doctor(&s, &s.db(), &[]));
    assert!(
        v["checks"]
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c["name"] != "vacuum"),
        "{v}"
    );
    // the database still serves after the vacuum
    let store = SqliteStore::open(&s.db(), &s.home().join("keys")).unwrap();
    assert!(store.get(&ids[0], false).unwrap().is_some());
}

#[test]
fn file_modes_are_reported_as_found_before_the_open_and_never_tightened() {
    let s = Scratch::new("kaban-doctor", "modes");
    seed(&s, &["a note so the index has content"]);
    // another holder keeps the connection, so the sidecars exist while doctor runs
    let holder = SqliteStore::open(&s.db(), &s.home().join("keys")).unwrap();
    holder
        .remember(note("a second note keeps the wal around"), &[], &ctx())
        .unwrap();
    let (wal, shm) = (sidecar(&s.db(), "-wal"), sidecar(&s.db(), "-shm"));
    assert!(wal.exists() && shm.exists(), "WAL sidecars expected");
    for p in [&s.db(), &wal, &shm] {
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o644)).unwrap();
    }

    let out = doctor(&s, &s.db(), &[]);
    assert_eq!(out.status.code(), Some(5), "{out:?}");
    let v = report(&out);
    assert_eq!(v["ok"], Value::Bool(false), "{v}");
    assert_eq!(check(&v, "db_modes")["ok"], false, "{v}");
    assert_eq!(
        check(&v, "db_modes")["detail"],
        "db 0644, wal 0644, shm 0644"
    );
    // every other check passed: the loose modes are the only finding
    let failed: Vec<&str> = v["checks"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["ok"] == false)
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert_eq!(failed, ["db_modes"], "{v}");
    // doctor reports and does not repair: a server open would tighten these (C1.6)
    assert_eq!(
        (mode(&s.db()), mode(&wal), mode(&shm)),
        (0o644, 0o644, 0o644)
    );
    drop(holder);
}

#[test]
fn a_parent_directory_that_cannot_take_the_wal_sidecars_is_a_store_error() {
    let s = Scratch::new("kaban-doctor", "readonly-dir");
    seed(&s, &["a note before the directory is locked"]);
    assert!(
        !sidecar(&s.db(), "-wal").exists(),
        "a clean close leaves no wal"
    );
    let dir = s.0.clone();
    let root = std::os::unix::fs::MetadataExt::uid(&std::fs::metadata(&dir).unwrap()) == 0;
    let (db, _guard) = if root {
        // root ignores directory modes (M4-20): a symlink is refused before any open instead
        let link = dir.join("link.db");
        std::os::unix::fs::symlink(s.db(), &link).unwrap();
        (link, None)
    } else {
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).unwrap();
        (s.db(), Some(ModeGuard(dir.clone(), 0o700)))
    };
    let out = doctor(&s, &db, &[]);
    assert_eq!(out.status.code(), Some(5), "{out:?}");
    assert!(
        out.stdout.is_empty(),
        "no document when the open fails: {out:?}"
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.starts_with("error: opening database"), "{err}");
    assert!(!err.contains("SQLITE"), "no driver text: {err}");
}

/// `git` in `root` with a fixed identity and no user or system configuration.
fn git(root: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_AUTHOR_NAME", "kaban fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@kaban.invalid")
        .env("GIT_COMMITTER_NAME", "kaban fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@kaban.invalid")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
}

/// `kaban --json --db <db> doctor` run inside `cwd`.
fn doctor_in(s: &Scratch, cwd: &Path) -> Value {
    let out = Command::new(env!("CARGO_BIN_EXE_kaban"))
        .current_dir(cwd)
        .env("KABAN_HOME", s.home())
        .args(["--json", "--db", s.db().to_str().unwrap(), "doctor"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    report(&out)
}

/// Rows of the tables a write would touch.
fn write_counts(db: &Path) -> [i64; 3] {
    let conn = rusqlite::Connection::open(db).unwrap();
    ["changes", "events", "audit"].map(|t| {
        conn.query_row(&format!("SELECT count(*) FROM {t}"), [], |r| r.get(0))
            .unwrap()
    })
}

/// E3.11 Step 6 through the binary: the provider `kaban doctor` wires from the codegraph key
/// and the enclosing repository resolves the snapshot at `HEAD`. Without the file the check
/// cannot inspect; with the `afero` snapshot nothing is stale; after a republication without
/// the symbol (`afero-renamed`) the renamed `CodeRef` is named with `code: snapshot_mismatch`
/// and the one on the surviving key is not. Nothing is written to the database.
#[test]
fn a_symbol_renamed_away_in_the_current_snapshot_is_listed_stale() {
    use cg_fixture::{AFERO_KEY, Variant};
    // the struct row both variants hold
    const INTACT: &str = "scip-go gomod github.com/spf13/afero . MemMapFs#";
    let s = Scratch::new("kaban-doctor", "stale-anchor");
    let repo = s.0.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(repo.join("memmap.go"), "package afero\n").unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["add", "memmap.go"]);
    git(&repo, &["commit", "-q", "-m", "c1"]);
    let head = String::from_utf8(
        Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_owned();
    let key = mcp_client::write_codegraph_key(&s.home());
    let dir = mcp_client::snapshot_dir(&s.home(), &key, &repo);

    // the bound workspace and repository identity are the root's name (M3-23)
    let store = SqliteStore::open(&s.db(), &s.home().join("keys")).unwrap();
    let ctx =
        WriteCtx::new("v1", WorkspaceId::new("repo"), Actor::Agent).with_repo(Some("repo".into()));
    let coderef = |symbol: &str| -> String {
        let node = NewNode {
            kind: "CodeRef".into(),
            text: format!("repo memmap.go {symbol}"),
            workspace_id: Some("repo".into()),
            repo: Some("repo".into()),
            props: Some(serde_json::json!({
                "repo": "repo", "commit": head, "tool": "fixture",
                "relative_path": "memmap.go", "symbol": symbol,
            })),
            ..Default::default()
        };
        created(store.remember(node, &[], &ctx).unwrap()).to_string()
    };
    let renamed = coderef(AFERO_KEY);
    let intact = coderef(INTACT);
    drop(store);
    let before = write_counts(&s.db());
    let stale = |v: &Value| check(v, "stale_anchors").clone();

    let none = stale(&doctor_in(&s, &repo));
    assert_eq!(none["detail"], "cannot inspect: no snapshot", "{none}");
    assert_eq!(none["ok"], true, "{none}");

    // publish a variant as `<dir>/<HEAD>.cg.db` (built elsewhere, renamed into place)
    let publish = |variant: Variant| {
        let built = cg_fixture::build(&s.0.join("build"), variant, &head);
        std::fs::rename(&built.path, dir.join(format!("{head}.cg.db"))).unwrap();
    };
    publish(Variant::Afero);
    let clean = stale(&doctor_in(&s, &repo));
    assert_eq!(
        clean["detail"],
        format!("n: 0; snapshot: {head}"),
        "{clean}"
    );

    publish(Variant::AferoRenamed);
    let listed = stale(&doctor_in(&s, &repo));
    assert_eq!(listed["ok"], true, "a stale anchor is a fact: {listed}");
    assert_eq!(
        listed["detail"],
        format!(
            "n: 1; snapshot: {head}; anchors: {renamed} code: snapshot_mismatch symbol: {AFERO_KEY}"
        ),
        "{listed}"
    );
    assert!(!listed["detail"].as_str().unwrap().contains(&intact));
    assert_eq!(write_counts(&s.db()), before, "doctor writes nothing");

    // a loose codegraph key is admitted by nothing: the check fails naming the class, the
    // snapshot is not read and the key is reported, never tightened
    let key_path = s.home().join("keys").join("codegraph.key");
    std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o644)).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_kaban"))
        .current_dir(&repo)
        .env("KABAN_HOME", s.home())
        .args(["--json", "--db", s.db().to_str().unwrap(), "doctor"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(5), "{out:?}");
    let refused = stale(&report(&out));
    assert_eq!(refused["ok"], false, "{refused}");
    assert_eq!(
        refused["detail"], "not checked: codegraph key refused (mode)",
        "{refused}"
    );
    assert_eq!(mode(&key_path), 0o644);
}
