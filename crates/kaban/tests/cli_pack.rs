//! `kaban pack build | verify | fork` (spec 05 SS-18, SS-19, SS-20) through the binary. The
//! rejection paths first: a corrupted file, an unsupported version, a base that disagrees with
//! the included base snapshot, a declared-but-missing component, a competing writer (busy) and
//! an interrupted build (no valid pack); then a build that verifies, is read through `--pack`
//! without a file changing, and resolves its base from the enclosing repository.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

#[path = "../../kaban-api/tests/common/cg_fixture.rs"]
mod cg_fixture;
mod common;
#[path = "common/mcp_client.rs"]
mod mcp_client;

use common::Scratch;
use kaban_core::{
    Actor, AuditEntry, AuditOp, AuditStore, MemoryStore, NewNode, NodeId, RememberOutcome,
    WorkspaceId, WriteCtx,
};
use kaban_store_sqlite::SqliteStore;
use serde_json::Value;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// An explicit full base commit (no repository needed).
const BASE: &str = "c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3";
const WORKSPACE: &str = "default";

fn kaban(s: &Scratch) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_kaban"));
    cmd.env("KABAN_HOME", s.home())
        .env("RUST_LOG", "warn")
        .args(["--workspace", WORKSPACE])
        .stdin(Stdio::null());
    cmd
}

fn document(out: &Output) -> Value {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| panic!("{e}: {out:?}"))
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// Seed the scratch database with one note and return its id.
fn seed(s: &Scratch, text: &str) -> NodeId {
    let store = SqliteStore::open(&s.db(), &s.home().join("keys")).unwrap();
    let ctx = WriteCtx::new("v1", WorkspaceId::default(), Actor::Agent);
    let node = NewNode {
        kind: "Fact".into(),
        text: text.into(),
        ..Default::default()
    };
    match store.remember(node, &[], &ctx).unwrap() {
        RememberOutcome::Created(node) => NodeId::parse(&node.id).unwrap(),
        other @ RememberOutcome::Existing { .. } => panic!("expected a created node: {other:?}"),
    }
}

/// `kaban --db <scratch db> pack build <dest> --base BASE`.
fn build(s: &Scratch, dest: &Path) -> Output {
    kaban(s)
        .args(["--json", "--db", s.db().to_str().unwrap(), "pack", "build"])
        .arg(dest)
        .args(["--base", BASE])
        .output()
        .unwrap()
}

fn build_ok(s: &Scratch, dest: &Path) -> Value {
    let out = build(s, dest);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    document(&out)
}

/// `kaban pack verify <dir>` run inside `cwd`.
fn verify_in(s: &Scratch, dir: &Path, cwd: &Path) -> Output {
    kaban(s)
        .current_dir(cwd)
        .args(["--json", "pack", "verify"])
        .arg(dir)
        .output()
        .unwrap()
}

fn verify(s: &Scratch, dir: &Path) -> Output {
    verify_in(s, dir, &s.0)
}

fn manifest_path(dir: &Path) -> PathBuf {
    dir.join("manifest.json")
}

fn read_manifest(dir: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(manifest_path(dir)).unwrap()).unwrap()
}

/// Rewrite the manifest (the pack's files are read-only: the mode is restored afterwards).
fn write_manifest(dir: &Path, manifest: &Value) {
    let path = manifest_path(dir);
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::write(&path, serde_json::to_vec(manifest).unwrap()).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o400)).unwrap();
}

/// Every regular file under `dir` with its bytes, sorted by path: the fingerprint "nothing in
/// the pack changed" compares.
fn fingerprint(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    fn walk(dir: &Path, out: &mut Vec<(PathBuf, Vec<u8>)>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(&path, out);
            } else {
                out.push((path.clone(), std::fs::read(&path).unwrap()));
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, &mut out);
    out.sort();
    out
}

/// `git <args>` in `repo`.
fn git(repo: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
}

/// A repository with one commit; returns its root and `HEAD`.
fn repository(s: &Scratch) -> (PathBuf, String) {
    let repo = s.0.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(repo.join("memmap.go"), "package afero\n").unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["add", "memmap.go"]);
    git(&repo, &["commit", "-q", "-m", "c1"]);
    let out = Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    (
        repo,
        String::from_utf8(out.stdout).unwrap().trim().to_owned(),
    )
}

/// Publish the `afero` fixture built for `inner_commit` as `<snapshot dir>/<name_commit>.cg.db`
/// of the repository at `repo` (the key and directory `kaban` derives them from).
fn publish_snapshot(s: &Scratch, repo: &Path, name_commit: &str, inner_commit: &str) -> PathBuf {
    let key = mcp_client::write_codegraph_key(&s.home());
    let dir = mcp_client::snapshot_dir(&s.home(), &key, repo);
    let built = cg_fixture::build(&s.0.join("build"), cg_fixture::Variant::Afero, inner_commit);
    let target = dir.join(format!("{name_commit}.cg.db"));
    std::fs::rename(&built.path, &target).unwrap();
    target
}

/// `kaban --db <scratch db> pack build <dest>` inside `repo` (the base resolved locally).
fn build_in_repo(s: &Scratch, repo: &Path, dest: &Path, extra: &[&str]) -> Output {
    kaban(s)
        .current_dir(repo)
        .args(["--json", "--db", s.db().to_str().unwrap(), "pack", "build"])
        .arg(dest)
        .args(extra)
        .output()
        .unwrap()
}

#[test]
fn a_corrupted_file_is_rejected_and_verify_writes_nothing() {
    let s = Scratch::new("kaban-pack", "corrupt");
    seed(&s, "a note to pack");
    let dir = s.0.join("pack");
    build_ok(&s, &dir);
    let ok = verify(&s, &dir);
    assert_eq!(ok.status.code(), Some(0), "{ok:?}");

    // flip one byte deep inside the snapshot (past the header, so SQLite still opens it)
    let db = dir.join("kaban.db");
    std::fs::set_permissions(&db, std::fs::Permissions::from_mode(0o600)).unwrap();
    let mut bytes = std::fs::read(&db).unwrap();
    let at = bytes.len() / 2;
    bytes[at] ^= 0xff;
    std::fs::write(&db, &bytes).unwrap();
    std::fs::set_permissions(&db, std::fs::Permissions::from_mode(0o400)).unwrap();
    let before = fingerprint(&dir);

    let out = verify(&s, &dir);
    assert_eq!(out.status.code(), Some(5), "{out:?}");
    assert!(stderr(&out).contains("hash"), "{out:?}");
    assert_eq!(fingerprint(&dir), before, "verify never writes to the pack");
}

#[test]
fn an_unsupported_version_or_format_is_rejected() {
    let s = Scratch::new("kaban-pack", "version");
    seed(&s, "a note to pack");
    let dir = s.0.join("pack");
    build_ok(&s, &dir);
    let original = read_manifest(&dir);

    let mut newer = original.clone();
    newer["versions"]["schema"] = Value::from(999);
    write_manifest(&dir, &newer);
    let out = verify(&s, &dir);
    assert_eq!(out.status.code(), Some(5), "{out:?}");
    assert!(stderr(&out).contains("unsupported"), "{out:?}");

    let mut other_format = original.clone();
    other_format["format"] = Value::from("kaban-pack/99");
    write_manifest(&dir, &other_format);
    let out = verify(&s, &dir);
    assert_eq!(out.status.code(), Some(5), "{out:?}");
    assert!(stderr(&out).contains("unsupported"), "{out:?}");

    // review e-7 (SS-18, SS-19): the ontology and projection versions are the database's
    // own (`index_meta`), and an edited declaration disagrees with the snapshot
    assert_eq!(
        original["versions"]["ontology"],
        index_meta(&s.db(), "ident_terms_version").unwrap(),
        "{original}"
    );
    assert_eq!(
        original["versions"]["projection"],
        index_meta(&s.db(), "fts_projection_signature").unwrap(),
        "{original}"
    );
    for field in ["ontology", "projection"] {
        let mut edited = original.clone();
        edited["versions"][field] = Value::from("tampered");
        write_manifest(&dir, &edited);
        let out = verify(&s, &dir);
        assert_eq!(out.status.code(), Some(5), "{field}: {out:?}");
        assert!(stderr(&out).contains("inconsistent"), "{field}: {out:?}");
    }

    write_manifest(&dir, &original);
    let out = verify(&s, &dir);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
}

#[test]
fn a_base_that_disagrees_with_the_included_base_snapshot_is_rejected() {
    let s = Scratch::new("kaban-pack", "base-mismatch");
    seed(&s, "a note to pack");
    let (repo, head) = repository(&s);
    // the file is named after HEAD but was built for another commit
    publish_snapshot(&s, &repo, &head, cg_fixture::OTHER_COMMIT);
    let dir = s.0.join("pack");
    let out = build_in_repo(&s, &repo, &dir, &[]);
    assert_eq!(out.status.code(), Some(5), "{out:?}");
    assert!(stderr(&out).contains("base"), "{out:?}");
    assert!(!dir.exists(), "no pack is left behind: {out:?}");

    // a coherent snapshot builds; a manifest whose declared base no longer matches it fails
    publish_snapshot(&s, &repo, &head, &head);
    let out = build_in_repo(&s, &repo, &dir, &[]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let built = document(&out);
    assert_eq!(built["base_commit"], head, "{built}");
    assert_eq!(built["snapshots"][0]["commit"], head, "{built}");
    let mut altered = read_manifest(&dir);
    altered["base_commit"] = Value::from(cg_fixture::OTHER_COMMIT);
    write_manifest(&dir, &altered);
    let out = verify_in(&s, &dir, &repo);
    assert_eq!(out.status.code(), Some(5), "{out:?}");
    assert!(stderr(&out).contains("base"), "{out:?}");

    // a short or non-hex explicit base is refused before anything is staged
    let short = s.0.join("short");
    let out = build_in_repo(&s, &repo, &short, &["--base", "abc1234"]);
    assert_eq!(out.status.code(), Some(4), "{out:?}");
    assert!(!short.exists());
}

#[test]
fn a_declared_but_missing_component_is_rejected() {
    let s = Scratch::new("kaban-pack", "missing");
    seed(&s, "a note to pack");
    let (repo, head) = repository(&s);
    publish_snapshot(&s, &repo, &head, &head);
    let dir = s.0.join("pack");
    let out = build_in_repo(&s, &repo, &dir, &[]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let snapshot = dir.join("snapshots").join(format!("{head}.cg.db"));
    assert!(
        snapshot.is_file(),
        "the admitted snapshot is part of the pack"
    );

    std::fs::remove_file(&snapshot).unwrap();
    let out = verify_in(&s, &dir, &repo);
    assert_eq!(out.status.code(), Some(5), "{out:?}");
    assert!(stderr(&out).contains("missing"), "{out:?}");

    // the database itself declared and gone
    let db = dir.join("kaban.db");
    std::fs::remove_file(&db).unwrap();
    let out = verify_in(&s, &dir, &repo);
    assert_eq!(out.status.code(), Some(5), "{out:?}");
    assert!(stderr(&out).contains("missing"), "{out:?}");
}

/// Rename the pack's snapshot of `from` to `to` and declare it so (names, base; the hash and
/// size kept): an internally inconsistent base that only opening the snapshot can tell.
fn substitute_snapshot(dir: &Path, from: &str, to: &str) {
    let snapshots = dir.join("snapshots");
    std::fs::set_permissions(&snapshots, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::rename(
        snapshots.join(format!("{from}.cg.db")),
        snapshots.join(format!("{to}.cg.db")),
    )
    .unwrap();
    let mut manifest = read_manifest(dir);
    let digest = manifest["files"][format!("snapshots/{from}.cg.db")].clone();
    manifest["files"]
        .as_object_mut()
        .unwrap()
        .remove(&format!("snapshots/{from}.cg.db"));
    manifest["files"][format!("snapshots/{to}.cg.db")] = digest;
    manifest["snapshots"] =
        serde_json::json!([{ "commit": to, "file": format!("snapshots/{to}.cg.db") }]);
    manifest["base_commit"] = Value::from(to);
    write_manifest(dir, &manifest);
}

/// Review e-5 (SS-19): the store-level admission (behind `--pack` reads and `pack fork`)
/// opens every declared snapshot and rejects a substituted one exactly as `pack verify` does.
#[test]
fn a_substituted_snapshot_is_rejected_by_every_entry_point() {
    let s = Scratch::new("kaban-pack", "substituted");
    seed(&s, "a note to pack");
    let (repo, head) = repository(&s);
    publish_snapshot(&s, &repo, &head, &head);
    let dir = s.0.join("pack");
    let out = build_in_repo(&s, &repo, &dir, &[]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    substitute_snapshot(&dir, &head, cg_fixture::OTHER_COMMIT);
    let before = fingerprint(&dir);

    let out = verify_in(&s, &dir, &repo);
    assert_eq!(out.status.code(), Some(5), "pack verify: {out:?}");
    assert!(stderr(&out).contains("base"), "{out:?}");
    let recall = kaban(&s)
        .current_dir(&repo)
        .args(["--json", "--pack", dir.to_str().unwrap(), "recall", "note"])
        .output()
        .unwrap();
    assert_eq!(recall.status.code(), Some(5), "--pack read: {recall:?}");
    assert!(stderr(&recall).contains("base"), "{recall:?}");
    let child = s.0.join("child.db");
    let forked = fork(&s, &dir, &child);
    assert_eq!(forked.status.code(), Some(5), "pack fork: {forked:?}");
    assert!(stderr(&forked).contains("base"), "{forked:?}");
    assert!(!child.exists(), "no fork of an inconsistent base");
    assert_eq!(fingerprint(&dir), before, "nothing in the pack changed");
}

/// Switch the SQLite file at `path` to WAL mode and close it: the header then says WAL and no
/// sidecar remains (the only connection checkpoints and removes them on close).
fn make_wal(path: &Path) {
    let conn = rusqlite::Connection::open(path).unwrap();
    let mode: String = conn
        .query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))
        .unwrap();
    assert_eq!(mode, "wal");
    drop(conn);
    let header = std::fs::read(path).unwrap();
    assert_eq!(&header[18..20], &[2, 2], "the header declares WAL");
    assert!(!path.with_extension("db-wal").exists() && !path.with_extension("db-shm").exists());
}

/// Review e-6 (SS-19): a WAL-mode component is never opened through a non-immutable
/// admission inside a pack (that open creates `-wal`/`-shm` beside it): the build refuses the
/// snapshot, and a pack holding one is refused by verification before anything opens it,
/// with the pack's inventory byte-identical afterwards.
#[test]
fn a_wal_mode_snapshot_is_refused_without_a_sidecar_being_created() {
    let s = Scratch::new("kaban-pack", "wal-snapshot");
    seed(&s, "a note to pack");
    let (repo, head) = repository(&s);
    let live = publish_snapshot(&s, &repo, &head, &head);
    // a sound pack first, then the live snapshot turns WAL and a rebuild is refused
    let dir = s.0.join("pack");
    let out = build_in_repo(&s, &repo, &dir, &[]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    make_wal(&live);
    let refused = s.0.join("refused");
    let out = build_in_repo(&s, &repo, &refused, &[]);
    assert_eq!(out.status.code(), Some(5), "{out:?}");
    assert!(stderr(&out).contains("WAL"), "{out:?}");
    assert!(!refused.exists(), "no pack with a WAL component: {out:?}");

    // the sound pack with its snapshot swapped for the WAL-mode file, declared with the right
    // hash: refused before the snapshot is opened, nothing created or changed
    let bundled = dir.join("snapshots").join(format!("{head}.cg.db"));
    std::fs::set_permissions(
        dir.join("snapshots"),
        std::fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    std::fs::remove_file(&bundled).unwrap();
    std::fs::copy(&live, &bundled).unwrap();
    std::fs::set_permissions(&bundled, std::fs::Permissions::from_mode(0o400)).unwrap();
    let bytes = std::fs::read(&bundled).unwrap();
    let mut manifest = read_manifest(&dir);
    manifest["files"][format!("snapshots/{head}.cg.db")] = serde_json::json!({
        "sha256": kaban_policy::sha256_hex(&bytes),
        "bytes": bytes.len(),
    });
    write_manifest(&dir, &manifest);
    let before = fingerprint(&dir);
    let out = verify_in(&s, &dir, &repo);
    assert_eq!(out.status.code(), Some(5), "{out:?}");
    assert!(stderr(&out).contains("WAL"), "{out:?}");
    assert_eq!(
        fingerprint(&dir),
        before,
        "verify created or changed a file"
    );
    let recall = kaban(&s)
        .current_dir(&repo)
        .args(["--json", "--pack", dir.to_str().unwrap(), "recall", "note"])
        .output()
        .unwrap();
    assert_eq!(recall.status.code(), Some(5), "{recall:?}");
    assert_eq!(
        fingerprint(&dir),
        before,
        "a refused open created or changed a file"
    );
}

#[test]
fn a_competing_writer_yields_busy_and_no_pack() {
    let s = Scratch::new("kaban-pack", "busy");
    seed(&s, "a note to pack");
    // another canonical writer holds the database for the whole attempt
    let holder = rusqlite::Connection::open(s.db()).unwrap();
    holder.execute_batch("BEGIN IMMEDIATE;").unwrap();
    let dir = s.0.join("pack");
    let out = build(&s, &dir);
    assert_eq!(out.status.code(), Some(5), "{out:?}");
    assert!(stderr(&out).contains("busy"), "{out:?}");
    assert!(!dir.exists(), "busy leaves no pack: {out:?}");
    holder.execute_batch("ROLLBACK;").unwrap();
    drop(holder);
    // once the writer is gone the same build is coherent
    build_ok(&s, &dir);
    assert_eq!(verify(&s, &dir).status.code(), Some(0));
}

#[test]
fn an_interrupted_build_leaves_no_valid_pack() {
    let s = Scratch::new("kaban-pack", "interrupted");
    let id = seed(&s, "a note that survives the interruption");
    let dir = s.0.join("pack");
    // the snapshot write runs into a 4 KiB file-size limit: the process is killed (SIGXFSZ)
    // or fails mid-write, after staging began and before anything was finalized
    let out = Command::new("sh")
        .arg("-c")
        .arg("ulimit -f 8 && exec \"$0\" \"$@\"")
        .arg(env!("CARGO_BIN_EXE_kaban"))
        .env("KABAN_HOME", s.home())
        .args(["--json", "--db", s.db().to_str().unwrap(), "pack", "build"])
        .arg(&dir)
        .args(["--base", BASE])
        .output()
        .unwrap();
    assert_ne!(out.status.code(), Some(0), "{out:?}");
    assert!(!manifest_path(&dir).exists(), "no manifest: {out:?}");
    let out = verify(&s, &dir);
    assert_eq!(out.status.code(), Some(5), "{out:?}");
    // the source is untouched
    let recall = kaban(&s)
        .args([
            "--json",
            "--db",
            s.db().to_str().unwrap(),
            "recall",
            "survives",
        ])
        .output()
        .unwrap();
    assert_eq!(recall.status.code(), Some(0), "{recall:?}");
    let hits = document(&recall)["results"].as_array().unwrap().clone();
    assert!(hits.iter().any(|hit| hit["id"] == id.as_str()), "{hits:?}");
}

#[test]
fn a_pack_builds_verifies_and_is_read_without_a_file_changing() {
    let s = Scratch::new("kaban-pack", "build-read");
    let id = seed(&s, "WAL mode is the default journal of a kaban database");
    let dir = s.0.join("pack");
    let manifest = build_ok(&s, &dir);
    assert_eq!(
        manifest,
        read_manifest(&dir),
        "the document is the manifest"
    );
    assert_eq!(manifest["format"], "kaban-pack/1", "{manifest}");
    assert_eq!(manifest["base_commit"], BASE, "{manifest}");
    // the store's own Tantivy index is captured whatever the build (spec 05 OWN-M: one
    // storage model, no FTS5 pack)
    assert_eq!(manifest["text_index"], "tantivy", "{manifest}");
    assert!(
        manifest["pack_id"].as_str().unwrap().len() == 36,
        "{manifest}"
    );
    assert_eq!(manifest["parent_pack_id"], Value::Null, "{manifest}");
    assert_eq!(
        manifest["versions"]["schema"],
        kaban_store_sqlite::LATEST_SCHEMA_VERSION,
        "{manifest}"
    );
    assert!(
        manifest["canonical_seq"].as_u64().unwrap() >= 1,
        "{manifest}"
    );
    let db_id: String = rusqlite::Connection::open(s.db())
        .unwrap()
        .query_row(
            "SELECT value FROM index_meta WHERE key = 'db_id'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(manifest["instance_id"], db_id, "{manifest}");
    let digest = &manifest["files"]["kaban.db"];
    let bytes = std::fs::read(dir.join("kaban.db")).unwrap();
    assert_eq!(digest["bytes"], bytes.len(), "{manifest}");
    assert_eq!(
        digest["sha256"],
        kaban_policy::sha256_hex(&bytes),
        "{manifest}"
    );
    // the files are sealed read-only and there is no sidecar
    for name in ["kaban.db", "manifest.json"] {
        let mode = std::fs::metadata(dir.join(name))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o400, "{name}");
    }
    assert!(!dir.join("kaban.db-wal").exists() && !dir.join("kaban.db-journal").exists());
    let out = verify(&s, &dir);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(document(&out), manifest);

    // read through `--pack`: the snapshot answers, nothing in the pack changes
    let before = fingerprint(&dir);
    let recall = kaban(&s)
        .args([
            "--json",
            "--pack",
            dir.to_str().unwrap(),
            "recall",
            "WAL mode",
        ])
        .output()
        .unwrap();
    assert_eq!(recall.status.code(), Some(0), "{recall:?}");
    let hits = document(&recall)["results"].as_array().unwrap().clone();
    assert!(hits.iter().any(|hit| hit["id"] == id.as_str()), "{hits:?}");
    // a write through the pack is refused and changes nothing either
    let write = kaban(&s)
        .args([
            "--json",
            "--pack",
            dir.to_str().unwrap(),
            "remember",
            "Fact",
            "a new note",
        ])
        .output()
        .unwrap();
    assert_eq!(
        write.status.code(),
        Some(4),
        "refused before any open: {write:?}"
    );
    assert_eq!(
        fingerprint(&dir),
        before,
        "reads and refused writes leave the pack as it is"
    );
    assert!(!dir.join("kaban.db-wal").exists() && !dir.join("kaban.db-shm").exists());
    // the destination is never overwritten
    let again = build(&s, &dir);
    assert_eq!(again.status.code(), Some(4), "{again:?}");
    assert_eq!(fingerprint(&dir), before);
}

/// The bytes of the live database and its WAL (when present): what a command that honours
/// `--pack` must leave untouched.
fn live_bytes(s: &Scratch) -> Vec<(String, Vec<u8>)> {
    ["kaban.db", "kaban.db-wal"]
        .into_iter()
        .filter_map(|name| {
            std::fs::read(s.0.join(name))
                .ok()
                .map(|bytes| (name.to_owned(), bytes))
        })
        .collect()
}

/// Review e-3 (SS-19): `--pack` is honoured by every command that runs with it, never
/// silently replaced by `--db`: `verify-audit` verifies the pack's chain, and the commands
/// that would write or that work on `--db` alone (`doctor --optimize`, `remember`, `pack
/// build`) are refused before any database is opened.
#[test]
fn pack_mode_is_honoured_or_refused_by_every_command() {
    let s = Scratch::new("kaban-pack", "pack-commands");
    seed(&s, "a note to pack");
    audit_rows(&s, 2);
    let dir = s.0.join("pack");
    let manifest = build_ok(&s, &dir);
    assert_eq!(manifest["audit"]["entries"], 2, "{manifest}");
    // the live chain moves on; the pack's stays at the snapshot
    audit_rows(&s, 3);
    let before_pack = fingerprint(&dir);
    let before_live = live_bytes(&s);

    let out = kaban(&s)
        .args(["--json", "--pack", dir.to_str().unwrap(), "verify-audit"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let v = document(&out);
    assert_eq!(v["chain"], "origin", "{v}");
    assert_eq!(v["ok"], true, "{v}");
    assert_eq!(v["entries"], 2, "the pack's chain, not the live one: {v}");
    assert_eq!(v["pack"]["head"], manifest["audit"]["head"], "{v}");
    assert_eq!(v["pack"]["pack_id"], manifest["pack_id"], "{v}");

    for refused in [
        vec!["doctor", "--optimize"],
        vec!["remember", "Fact", "a new note"],
        vec!["pack", "build", "elsewhere"],
    ] {
        let out = kaban(&s)
            .args(["--json", "--pack", dir.to_str().unwrap()])
            .args(&refused)
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(4), "{refused:?}: {out:?}");
        assert!(stderr(&out).contains("--pack"), "{refused:?}: {out:?}");
        assert!(stderr(&out).contains(refused[0]), "{refused:?}: {out:?}");
    }
    assert!(!s.0.join("elsewhere").exists());
    assert_eq!(fingerprint(&dir), before_pack, "the pack is untouched");
    assert_eq!(
        live_bytes(&s),
        before_live,
        "the live database is untouched"
    );
}

/// Review e-4 (SS-18): pack reads resolve the code side against the pack's bundled snapshot,
/// pinned to the manifest's base, while the live checkout has moved on and its snapshot
/// cache is gone; a pack without a bundled snapshot reports absence. The `code_*` tools exist
/// only with the `code-graph` feature; the twin below is the no-default read.
#[cfg(feature = "code-graph")]
#[test]
fn pack_reads_answer_code_tools_from_the_bundled_snapshot() {
    let s = Scratch::new("kaban-pack", "bundled-snapshot");
    seed(&s, "a note to pack");
    let (repo, head) = repository(&s);
    let live = publish_snapshot(&s, &repo, &head, &head);
    let dir = s.0.join("pack");
    let out = build_in_repo(&s, &repo, &dir, &[]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(document(&out)["snapshots"][0]["commit"], head);
    // the checkout moves on and the live cache is gone: only the pack knows the base
    std::fs::write(repo.join("memmap.go"), "package afero\n\n// moved on\n").unwrap();
    git(&repo, &["commit", "-q", "-am", "c2"]);
    std::fs::remove_file(&live).unwrap();
    let before = fingerprint(&dir);

    let definition = |pack: &Path| -> Value {
        let (mut srv, _) = common::Server::start_with(
            &s,
            &["--pack", pack.to_str().unwrap(), "--workspace", WORKSPACE],
            std::time::Duration::from_secs(10),
        );
        let id = srv.request(
            "tools/call",
            &serde_json::json!({
                "name": "code_definition",
                "arguments": { "target": { "symbol": cg_fixture::AFERO_KEY } },
            }),
        );
        let resp = srv
            .response(id, std::time::Duration::from_secs(10))
            .unwrap();
        srv.finish();
        resp["result"].clone()
    };
    let result = definition(&dir);
    assert_ne!(result["isError"], true, "{result}");
    let def: Value = serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(def["snapshot"]["commit"], head, "the bundled base: {def}");
    assert_eq!(def["definition"]["path"], cg_fixture::AFERO_PATH, "{def}");
    assert_eq!(fingerprint(&dir), before, "the pack is untouched");

    // no bundled snapshot: absence, not the live repository's provider
    let bare = s.0.join("bare");
    build_ok(&s, &bare);
    let result = definition(&bare);
    assert_eq!(result["isError"], true, "{result}");
    assert!(
        result["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("no_snapshot"),
        "{result}"
    );
}

/// The same pack in a build without the code tools (`--no-default-features`): the bundled
/// snapshot is still carried and admitted, the pack still reads (recall answers from it), and
/// `code_definition` is not a tool of this build, so the call is an error naming an unknown
/// tool, never an answer from the live repository; the pack is untouched.
#[cfg(not(feature = "code-graph"))]
#[test]
fn pack_reads_answer_without_code_tools_when_the_build_has_none() {
    let s = Scratch::new("kaban-pack", "bundled-snapshot-no-code");
    seed(&s, "a note to pack");
    let (repo, head) = repository(&s);
    publish_snapshot(&s, &repo, &head, &head);
    let dir = s.0.join("pack");
    let out = build_in_repo(&s, &repo, &dir, &[]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(document(&out)["snapshots"][0]["commit"], head);
    let before = fingerprint(&dir);

    let recall = kaban(&s)
        .args(["--json", "--pack", dir.to_str().unwrap(), "recall", "note"])
        .output()
        .unwrap();
    assert_eq!(recall.status.code(), Some(0), "--pack read: {recall:?}");
    assert_eq!(
        document(&recall)["results"].as_array().unwrap().len(),
        1,
        "{recall:?}"
    );

    let (mut srv, _) = common::Server::start_with(
        &s,
        &["--pack", dir.to_str().unwrap(), "--workspace", WORKSPACE],
        std::time::Duration::from_secs(10),
    );
    let id = srv.request(
        "tools/call",
        &serde_json::json!({
            "name": "code_definition",
            "arguments": { "target": { "symbol": cg_fixture::AFERO_KEY } },
        }),
    );
    let resp = srv
        .response(id, std::time::Duration::from_secs(10))
        .unwrap();
    srv.finish();
    assert!(
        resp["result"]["isError"] == true || resp.get("error").is_some(),
        "no code tool in this build: {resp}"
    );
    assert!(
        !resp.to_string().contains(cg_fixture::AFERO_PATH),
        "nothing answered from the snapshot: {resp}"
    );
    assert_eq!(fingerprint(&dir), before, "the pack is untouched");
}

#[test]
fn the_base_is_resolved_locally_from_the_repository() {
    let s = Scratch::new("kaban-pack", "base-local");
    seed(&s, "a note to pack");
    let (repo, head) = repository(&s);
    // no snapshot published: the pack has none and still names the local head
    let dir = s.0.join("pack");
    let out = build_in_repo(&s, &repo, &dir, &[]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let manifest = document(&out);
    assert_eq!(manifest["base_commit"], head, "{manifest}");
    assert_eq!(manifest["source_commit"], head, "{manifest}");
    assert_eq!(
        manifest["snapshots"],
        Value::Array(Vec::new()),
        "{manifest}"
    );
    assert!(!dir.join("snapshots").exists());
    // an explicit full base wins over the head and is recorded as declared
    let explicit = s.0.join("explicit");
    let out = build_in_repo(&s, &repo, &explicit, &["--base", BASE]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let manifest = document(&out);
    assert_eq!(manifest["base_commit"], BASE, "{manifest}");
    assert_eq!(manifest["source_commit"], head, "{manifest}");
    // outside a repository and without --base there is nothing to resolve from
    let nowhere = s.0.join("nowhere");
    let out = kaban(&s)
        .current_dir(&s.0)
        .args(["--json", "--db", s.db().to_str().unwrap(), "pack", "build"])
        .arg(&nowhere)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(4), "{out:?}");
    assert!(!nowhere.exists());
}

// ---- E2: mode-A forks (SS-20, SS-26, SS-27) --------------------------------------------------

/// `kaban pack fork <pack> <dest>`.
fn fork(s: &Scratch, pack: &Path, dest: &Path) -> Output {
    kaban(s)
        .args(["--json", "pack", "fork"])
        .arg(pack)
        .arg(dest)
        .output()
        .unwrap()
}

fn fork_ok(s: &Scratch, pack: &Path, dest: &Path) -> Value {
    let out = fork(s, pack, dest);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    document(&out)
}

/// `kaban --db <db> verify-audit`.
fn verify_audit(s: &Scratch, db: &Path) -> Output {
    kaban(s)
        .args(["--json", "--db", db.to_str().unwrap(), "verify-audit"])
        .output()
        .unwrap()
}

fn index_meta(db: &Path, key: &str) -> Option<String> {
    let conn = rusqlite::Connection::open(db).unwrap();
    conn.query_row("SELECT value FROM index_meta WHERE key = ?1", [key], |r| {
        r.get(0)
    })
    .ok()
}

fn fork_rows(db: &Path) -> i64 {
    let conn = rusqlite::Connection::open(db).unwrap();
    conn.query_row("SELECT count(*) FROM audit WHERE op = 'fork'", [], |r| {
        r.get(0)
    })
    .unwrap()
}

/// Append `n` rejection rows to the scratch database's chain (parent history for a fork).
fn audit_rows(s: &Scratch, n: usize) {
    let store = SqliteStore::open(&s.db(), &s.home().join("keys")).unwrap();
    for i in 0..n {
        store
            .audit_append(&[AuditEntry {
                op: AuditOp::Reject,
                tool: "remember".into(),
                field: "text".into(),
                workspace_id: WorkspaceId::default(),
                policy_version: "pv".into(),
                input_hmac: format!("{i:064x}"),
                rules: vec!["jwt".into()],
                node_id: None,
            }])
            .unwrap();
    }
}

/// Rewrite one column of one audit row behind the append-only triggers.
fn tamper_audit(db: &Path, sql: &str) {
    let conn = rusqlite::Connection::open(db).unwrap();
    conn.execute_batch(&format!(
        "DROP TRIGGER audit_no_update; {sql}; CREATE TRIGGER audit_no_update BEFORE UPDATE ON audit BEGIN SELECT RAISE(ABORT, 'audit is append-only'); END;"
    ))
    .unwrap();
}

#[test]
fn an_altered_parent_binding_fails_child_verification() {
    let s = Scratch::new("kaban-pack", "fork-binding");
    seed(&s, "a note to fork");
    audit_rows(&s, 3);
    let pack = s.0.join("pack");
    build_ok(&s, &pack);
    let child = s.0.join("child.db");
    let report = fork_ok(&s, &pack, &child);
    let genesis_seq = report["genesis_seq"].as_u64().unwrap();
    assert_eq!(genesis_seq, 4, "{report}");

    // the legitimate fork: the child chain from its genesis, the parent history under the
    // parent's key (present in this home), two results and never one global chain
    let out = verify_audit(&s, &child);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let v = document(&out);
    assert_eq!(v["chain"], "child", "{v}");
    assert_eq!(v["ok"], true, "{v}");
    assert_eq!(
        v["entries"], 1,
        "the child chain is the genesis so far: {v}"
    );
    assert_eq!(v["fork"]["genesis_seq"], 4, "{v}");
    assert_eq!(v["parent_history"]["key"], "present", "{v}");
    assert_eq!(v["parent_history"]["ok"], true, "{v}");
    assert_eq!(v["parent_history"]["entries"], 3, "{v}");

    // the binding altered: the child chain fails at its genesis
    tamper_audit(
        &child,
        "UPDATE audit SET rules = replace(rules, 'parent_audit_head', 'parent_audit_hea_') WHERE op = 'fork'",
    );
    let out = verify_audit(&s, &child);
    assert_eq!(out.status.code(), Some(5), "{out:?}");
    let v = document(&out);
    assert_eq!(v["ok"], false, "{v}");
    assert_eq!(v["first_bad_seq"], 4, "{v}");

    // a fresh fork whose parent history is altered: the child chain holds, the parent
    // history fails, reported apart
    let second = s.0.join("second.db");
    fork_ok(&s, &pack, &second);
    tamper_audit(&second, "UPDATE audit SET tool = 'x' WHERE seq = 2");
    let out = verify_audit(&s, &second);
    assert_eq!(out.status.code(), Some(5), "{out:?}");
    let v = document(&out);
    assert_eq!(v["ok"], true, "{v}");
    assert_eq!(v["parent_history"]["ok"], false, "{v}");
    assert_eq!(v["parent_history"]["first_bad_seq"], 2, "{v}");

    // review e-8: a parent key that is there but truncated or unreadable is a verification
    // failure (exit 5, the key error's own message), never reported as ordinary absence
    let parent_instance = read_manifest(&pack)["instance_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let parent_key = s.home().join("keys").join(format!("{parent_instance}.key"));
    let fourth = s.0.join("fourth.db");
    fork_ok(&s, &pack, &fourth);
    let good_key = std::fs::read(&parent_key).unwrap();
    std::fs::write(&parent_key, &good_key[..16]).unwrap();
    let out = verify_audit(&s, &fourth);
    assert_eq!(out.status.code(), Some(5), "truncated key: {out:?}");
    assert!(stderr(&out).contains("16 bytes"), "{out:?}");
    assert!(
        out.stdout.is_empty(),
        "no document claims a verdict: {out:?}"
    );
    // unreadable: a directory under the key's name
    std::fs::remove_file(&parent_key).unwrap();
    std::fs::create_dir(&parent_key).unwrap();
    let out = verify_audit(&s, &fourth);
    assert_eq!(out.status.code(), Some(5), "unreadable key: {out:?}");
    assert!(stderr(&out).contains("audit i/o failed"), "{out:?}");
    std::fs::remove_dir(&parent_key).unwrap();

    // without the parent's key the parent history is reported unverified, not claimed
    let third = s.0.join("third.db");
    fork_ok(&s, &pack, &third);
    let out = verify_audit(&s, &third);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let v = document(&out);
    assert_eq!(v["ok"], true, "{v}");
    assert_eq!(v["parent_history"]["key"], "absent", "{v}");
    assert_eq!(v["parent_history"]["ok"], Value::Null, "{v}");
}

/// Seed `n` notes of about 2 KiB each through one store handle (a pack whose copy takes a
/// few milliseconds, so a test can catch an attempt between its copy and its commit).
fn seed_many(s: &Scratch, n: usize) {
    let store = SqliteStore::open(&s.db(), &s.home().join("keys")).unwrap();
    let ctx = WriteCtx::new("v1", WorkspaceId::default(), Actor::Agent);
    for i in 0..n {
        let node = NewNode {
            kind: "Fact".into(),
            text: format!("note {i} {}", "lorem ".repeat(340)),
            ..Default::default()
        };
        store.remember(node, &[], &ctx).unwrap();
    }
}

/// The staging copies of forks into `dest` present in its directory right now.
fn staging_copies(dest: &Path) -> Vec<PathBuf> {
    let prefix = format!(".{}.forking", dest.file_name().unwrap().to_str().unwrap());
    let mut found: Vec<PathBuf> = std::fs::read_dir(dest.parent().unwrap())
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| {
            let name = p.file_name().unwrap().to_string_lossy();
            name.starts_with(&prefix) && !name.ends_with("-journal")
        })
        .collect();
    found.sort();
    found
}

/// Spawn `pack fork` of `pack` into `child` and park it before its identity commit: a write
/// lock taken on its staging copy as soon as the copy appears, which its `BEGIN IMMEDIATE`
/// then waits on. The window is the copy plus the key creation, about 10 ms on this box
/// (TESTS-1, backlog 28), and a test thread descheduled under load can miss it, in which case
/// the fork simply completes: that run exercised nothing, so the completed fork is removed and
/// the attempt is repeated, a bounded number of times. Returns the parked attempt and the
/// connection holding its staging copy's lock.
fn fork_parked_before_identity_commit(
    s: &Scratch,
    pack: &Path,
    child: &Path,
) -> (std::process::Child, rusqlite::Connection) {
    const ATTEMPTS: usize = 5;
    for _ in 0..ATTEMPTS {
        let attempt = kaban(s)
            .args(["--json", "pack", "fork"])
            .arg(pack)
            .arg(child)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        // `None` when the window was missed (the fork published), else the lock
        let parked = loop {
            if child.exists() {
                break None;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "no staging copy appeared"
            );
            if let Some(staging) = staging_copies(child).into_iter().next()
                && let Ok(conn) = rusqlite::Connection::open(&staging)
                && conn.execute_batch("BEGIN IMMEDIATE").is_ok()
            {
                break Some(conn);
            }
            std::thread::sleep(std::time::Duration::from_micros(50));
        };
        if let Some(conn) = parked {
            return (attempt, conn);
        }
        // undo the completed fork (database, index copy and key) and try again
        let out = attempt.wait_with_output().unwrap();
        assert_eq!(out.status.code(), Some(0), "{out:?}");
        let instance = index_meta(child, "db_id").unwrap();
        std::fs::remove_file(child).unwrap();
        let _ = std::fs::remove_dir_all(child.with_extension("index"));
        std::fs::remove_file(s.home().join("keys").join(format!("{instance}.key"))).unwrap();
    }
    panic!("the fork published before the test could park it, {ATTEMPTS} times; enlarge the pack");
}

/// SS-20 (review e-2, e-9): an attempt interrupted between its copy and its identity commit
/// publishes nothing. The attempt is parked there by a write lock on its staging copy (its
/// `BEGIN IMMEDIATE` waits on it); the destination name appears meanwhile; the attempt then
/// commits its identity and is refused at publication: the destination is left intact, the
/// staging copy and the never-published identity's key are removed, the pack is untouched.
#[test]
fn an_interrupted_fork_initialization_leaves_exactly_one_identity() {
    let s = Scratch::new("kaban-pack", "fork-interrupted");
    seed_many(&s, 400);
    let pack = s.0.join("pack");
    build_ok(&s, &pack);
    let keys_before = std::fs::read_dir(s.home().join("keys")).unwrap().count();
    let before = fingerprint(&pack);
    let child = s.0.join("child.db");

    let (attempt, parked) = fork_parked_before_identity_commit(&s, &pack, &child);
    // the destination appears while the attempt is parked before its identity commit
    std::fs::write(&child, b"not a fork").unwrap();
    parked.execute_batch("ROLLBACK").unwrap();
    drop(parked);
    let out = attempt.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(4), "{out:?}");
    assert_eq!(
        std::fs::read(&child).unwrap(),
        b"not a fork",
        "the destination that appeared is left intact"
    );
    assert_eq!(
        staging_copies(&child),
        Vec::<PathBuf>::new(),
        "the staging copy is removed"
    );
    assert_eq!(
        std::fs::read_dir(s.home().join("keys")).unwrap().count(),
        keys_before,
        "no key for an identity that was never published"
    );
    assert_eq!(fingerprint(&pack), before, "the pack is untouched");
    std::fs::remove_file(&child).unwrap();

    // a completed fork has exactly one identity and one genesis; a repeat is refused and
    // changes neither
    let report = fork_ok(&s, &pack, &child);
    let instance = index_meta(&child, "db_id").unwrap();
    assert_eq!(report["instance_id"], instance, "{report}");
    assert_eq!(fork_rows(&child), 1);
    let again = fork(&s, &pack, &child);
    assert_eq!(again.status.code(), Some(4), "{again:?}");
    assert_eq!(index_meta(&child, "db_id").unwrap(), instance);
    assert_eq!(fork_rows(&child), 1);
}

/// Seed a note carrying provenance (agent and session), linked `RELATES_TO` `to`; returns
/// its id and the edge's UUID.
fn seed_linked(s: &Scratch, to: &NodeId) -> (NodeId, String) {
    let store = SqliteStore::open(&s.db(), &s.home().join("keys")).unwrap();
    let ctx = WriteCtx::new("v1", WorkspaceId::default(), Actor::Agent);
    let node = NewNode {
        kind: "Fact".into(),
        text: "the cited note before the fork".into(),
        agent_id: Some("scribe".into()),
        session_id: Some("session-7".into()),
        ..Default::default()
    };
    let id = match store.remember(node, &[], &ctx).unwrap() {
        RememberOutcome::Created(node) => NodeId::parse(&node.id).unwrap(),
        other @ RememberOutcome::Existing { .. } => panic!("expected a created node: {other:?}"),
    };
    let edge = store.link(&id, to, "RELATES_TO", None, None, &ctx).unwrap();
    (id, edge.id)
}

/// A node's provenance and stamp as stored: `(agent_id, session_id, commit_sha, instance_id)`.
type Provenance = (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
);

fn provenance(db: &Path, id: &NodeId) -> Provenance {
    let conn = rusqlite::Connection::open(db).unwrap();
    conn.query_row(
        "SELECT agent_id, session_id, commit_sha, instance_id FROM node WHERE id = ?1",
        [id.as_str()],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )
    .unwrap()
}

/// The node UUIDs an edge joins, by the edge's UUID.
fn edge_ends(db: &Path, edge: &str) -> (String, String) {
    let conn = rusqlite::Connection::open(db).unwrap();
    conn.query_row(
        "SELECT src.id, dst.id FROM edge e
         JOIN node src ON src.rid = e.src JOIN node dst ON dst.rid = e.dst
         WHERE e.id = ?1",
        [edge],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .unwrap()
}

/// `(actor_id, instance_id, pack_lineage_id)` of the nodes whose text matches `like`.
fn stamps(db: &Path, like: &str) -> Vec<(Option<String>, Option<String>, Option<String>)> {
    let conn = rusqlite::Connection::open(db).unwrap();
    kaban_store_sqlite::register_body_reader(&conn).unwrap();
    conn.prepare(
        "SELECT actor_id, instance_id, pack_lineage_id FROM node WHERE kaban_body(text) LIKE ?1",
    )
    .unwrap()
    .query_map([like], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
    .unwrap()
    .map(Result::unwrap)
    .collect()
}

#[test]
fn two_forks_search_immediately_and_edit_independently() {
    let s = Scratch::new("kaban-pack", "two-forks");
    let shared = seed(&s, "the shared note before the fork");
    let (cited, edge) = seed_linked(&s, &shared);
    let pack = s.0.join("pack");
    let manifest = build_ok(&s, &pack);
    let parent_instance = index_meta(&s.db(), "db_id").unwrap();
    let lineage = index_meta(&s.db(), "pack_lineage_id").unwrap();

    let a = s.0.join("a.db");
    let b = s.0.join("b.db");
    let report_a = fork_ok(&s, &pack, &a);
    let report_b = fork_ok(&s, &pack, &b);
    // identities (SS-20, SS-26): new instances, one lineage, the exact parent pack and base
    let instance_a = index_meta(&a, "db_id").unwrap();
    let instance_b = index_meta(&b, "db_id").unwrap();
    assert_ne!(instance_a, parent_instance);
    assert_ne!(instance_b, parent_instance);
    assert_ne!(instance_a, instance_b);
    assert_eq!(report_a["instance_id"], instance_a, "{report_a}");
    assert_eq!(report_b["pack_lineage_id"], lineage, "{report_b}");
    assert_eq!(index_meta(&a, "pack_lineage_id").unwrap(), lineage);
    assert_eq!(index_meta(&b, "pack_lineage_id").unwrap(), lineage);
    assert_eq!(
        index_meta(&a, "parent_pack_id").unwrap(),
        manifest["pack_id"]
    );
    assert_eq!(index_meta(&b, "base_commit").unwrap(), BASE);
    // a hard link would share the inode with the pack's snapshot
    let pack_ino = std::fs::metadata(pack.join("kaban.db")).unwrap().ino();
    assert_ne!(std::fs::metadata(&a).unwrap().ino(), pack_ino);
    assert_ne!(std::fs::metadata(&b).unwrap().ino(), pack_ino);

    // both search immediately and the inherited record keeps its UUID (SS-27)
    let recall = |db: &Path, query: &str| -> Value {
        let out = kaban(&s)
            .args(["--json", "--db", db.to_str().unwrap(), "recall", query])
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(0), "{out:?}");
        document(&out)
    };
    let hits = |v: &Value| -> Vec<String> {
        v["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|hit| hit["id"].as_str().unwrap().to_owned())
            .collect()
    };
    assert!(hits(&recall(&a, "shared note")).contains(&shared.to_string()));
    assert!(hits(&recall(&b, "shared note")).contains(&shared.to_string()));
    // the inherited edge keeps its UUID and joins the inherited node UUIDs (SS-27), and the
    // inherited provenance (agent, session, commit fields) and the parent's own stamp are
    // preserved, not re-stamped by the fork (SS-26)
    let source = provenance(&s.db(), &cited);
    assert_eq!(source.0.as_deref(), Some("scribe"), "{source:?}");
    assert_eq!(source.1.as_deref(), Some("session-7"), "{source:?}");
    assert_eq!(
        source.3.as_deref(),
        Some(parent_instance.as_str()),
        "{source:?}"
    );
    for db in [&a, &b] {
        assert_eq!(provenance(db, &cited), source, "{}", db.display());
        assert_eq!(
            edge_ends(db, &edge),
            (cited.to_string(), shared.to_string()),
            "{}",
            db.display()
        );
    }

    // an edit in each fork is that fork's alone, stamped with its instance and the actor
    // that wrote it (SS-26)
    let remember = |db: &Path, actor: &str, text: &str| {
        let out = kaban(&s)
            .args(["--json", "--actor-id", actor, "--db", db.to_str().unwrap()])
            .args(["remember", "Fact", text])
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(0), "{out:?}");
        let text = serde_json::to_string(&document(&out)).unwrap();
        assert!(
            !text.contains("\"rid\""),
            "no local row id in a receipt: {text}"
        );
    };
    remember(&a, "alpha", "a note written only in zephyr");
    remember(&b, "beta", "a note written only in quasar");
    assert_eq!(hits(&recall(&a, "zephyr")).len(), 1);
    assert_eq!(hits(&recall(&b, "quasar")).len(), 1);
    assert_eq!(hits(&recall(&b, "zephyr")), Vec::<String>::new());
    assert_eq!(hits(&recall(&a, "quasar")), Vec::<String>::new());
    for query in ["zephyr", "quasar"] {
        assert_eq!(
            hits(&recall(&s.db(), query)),
            Vec::<String>::new(),
            "the source is untouched"
        );
    }
    assert_eq!(
        stamps(&a, "%zephyr%"),
        vec![(
            Some("alpha".to_owned()),
            Some(instance_a.clone()),
            Some(lineage.clone())
        )]
    );
    assert_eq!(
        stamps(&b, "%quasar%"),
        vec![(
            Some("beta".to_owned()),
            Some(instance_b.clone()),
            Some(lineage.clone())
        )]
    );

    // the manifest carries UUIDs and commits as references, no local row ids (SS-27)
    let manifest_text = std::fs::read_to_string(manifest_path(&pack)).unwrap();
    assert!(!manifest_text.contains("\"rid\"") && !manifest_text.contains("rowid"));
    // the pack itself is unchanged by both forks
    assert_eq!(verify(&s, &pack).status.code(), Some(0));
}

// ---- spec 05 SS-18, SS-19, SS-20: the Tantivy pack component (lane T4) ----

/// On a database marked for the Tantivy index (feature `text-index-tantivy`): a build while
/// another process holds the projection (lease and writer lock) is busy and leaves no pack;
/// otherwise the pack captures the index directory at the snapshot's sequence with its payload
/// in the manifest; a substituted index directory is rejected by verify, re-declared files or
/// not, with the pack unchanged; a fork searches at once through the copied index, which is
/// not rebuilt (it keeps the parent's payload), and publishes its own writes on it.
#[cfg(feature = "text-index-tantivy")]
mod tantivy {
    use super::*;
    use kaban_core::config::Recall;
    use kaban_core::text_index::TextIndex as _;
    use kaban_index_tantivy::TantivyIndex;
    use kaban_index_tantivy::projector::{Halt, Projector};
    use kaban_store_sqlite::index_rows::IndexRows;
    use std::process::Child;
    use std::sync::atomic::AtomicBool;
    use std::time::{Duration, Instant};

    const HOLDER_TTL_S: u32 = 2;

    /// Mark the database for the Tantivy index with its projection built at the head (what
    /// the activation sets the marker on, SS-13b): a lone writer's barrier publishes on it
    /// and never rebuilds (fix11: a one-shot command runs no mode-B loop).
    fn mark_tantivy(db: &Path) {
        rusqlite::Connection::open(db)
            .unwrap()
            .execute(
                "INSERT OR REPLACE INTO index_meta(key, value) VALUES ('text_index', 'tantivy')",
                [],
            )
            .unwrap();
        adapter(db).rebuild().unwrap();
    }

    /// `kaban --json --db <db> remember Fact <text>`: a lone writer whose barrier publishes
    /// its commit through the adapter.
    fn remember(s: &Scratch, db: &Path, text: &str) -> Value {
        let out = kaban(s)
            .args([
                "--json",
                "--db",
                db.to_str().unwrap(),
                "remember",
                "Fact",
                text,
            ])
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(0), "{out:?}");
        let doc = document(&out);
        assert_eq!(doc["receipt"]["outcome"], "committed", "{doc}");
        doc
    }

    /// The ids `kaban --json --db <db> recall <query>` returns.
    fn recall(s: &Scratch, db: &Path, query: &str) -> Vec<String> {
        let out = kaban(s)
            .args(["--json", "--db", db.to_str().unwrap(), "recall", query])
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(0), "{out:?}");
        document(&out)["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|card| card["id"].as_str().unwrap().to_owned())
            .collect()
    }

    fn adapter(db: &Path) -> TantivyIndex {
        TantivyIndex::open(
            &TantivyIndex::index_dir(db),
            &kaban_core::shipped::resolved().unwrap(),
            &Recall::default(),
            Box::new(IndexRows::open(db).unwrap()),
        )
        .unwrap()
    }

    /// The payload of the index directory's last commit (`meta.json`'s `payload`).
    fn index_payload(dir: &Path) -> Value {
        let meta: Value =
            serde_json::from_slice(&std::fs::read(dir.join("meta.json")).unwrap()).unwrap();
        serde_json::from_str(meta["payload"].as_str().unwrap()).unwrap()
    }

    /// The holder role of the busy case: in a child process (`KABAN_PACK_HOLDER_DB` set) hold
    /// lease and lock over that database until killed. A no-op elsewhere.
    #[test]
    #[ignore = "a role of the busy case"]
    fn pack_holder_role() {
        let Ok(db) = std::env::var("KABAN_PACK_HOLDER_DB") else {
            return;
        };
        let db = PathBuf::from(db);
        let store = SqliteStore::open(&db, &db.parent().unwrap().join("home/keys")).unwrap();
        let index = adapter(&db);
        // a lone writer's drain may hold the lease for a moment: try again until held
        loop {
            let progress = Projector::new(&index, &store, &store, "holder", HOLDER_TTL_S)
                .hold(&AtomicBool::new(false))
                .unwrap();
            if progress.halt != Some(Halt::LeaseHeld) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// The holder, in a child process, known to hold the lease (and so, a moment later, the
    /// lock) once the lease row names it: a probe that read nothing else could take it.
    struct Holder(Child);

    impl Holder {
        fn start(s: &Scratch) -> Self {
            let child = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "tantivy::pack_holder_role", "--ignored"])
                .env("KABAN_PACK_HOLDER_DB", s.db())
                .stdout(Stdio::null())
                .spawn()
                .unwrap();
            let conn = rusqlite::Connection::open(s.db()).unwrap();
            let deadline = Instant::now() + Duration::from_secs(30);
            loop {
                let owner: Option<String> = conn
                    .query_row(
                        "SELECT owner FROM leases WHERE name = 'text-index' AND expires_at > unixepoch()",
                        [],
                        |r| r.get(0),
                    )
                    .ok();
                if owner.as_deref() == Some("holder") {
                    break;
                }
                assert!(Instant::now() < deadline, "the holder never held");
                std::thread::sleep(Duration::from_millis(50));
            }
            // the lock follows the lease at once; give it that moment
            std::thread::sleep(Duration::from_millis(200));
            Self(child)
        }
    }

    impl Drop for Holder {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[test]
    fn a_held_projection_is_busy_and_a_substituted_index_directory_is_rejected() {
        let s = Scratch::new("kaban-pack", "tantivy-build");
        remember(&s, &s.db(), "the seeded note");
        mark_tantivy(&s.db());
        let note = remember(&s, &s.db(), "quokka captured in the pack");
        let dir = s.0.join("pack");

        // 1. another process holds the projection: busy, no pack
        let holder = Holder::start(&s);
        let out = build(&s, &dir);
        assert_eq!(out.status.code(), Some(5), "{out:?}");
        assert!(stderr(&out).contains("busy"), "{out:?}");
        assert!(!dir.exists(), "busy leaves no pack");
        drop(holder);

        // 2. coherent: once the killed holder's lease expired (busy until then: a build never
        // waits for ownership), the index directory at the snapshot's sequence, its payload
        // declared
        let deadline = Instant::now() + Duration::from_secs(15);
        let manifest = loop {
            let out = build(&s, &dir);
            if out.status.code() == Some(0) {
                break document(&out);
            }
            assert!(stderr(&out).contains("busy"), "{out:?}");
            assert!(
                Instant::now() < deadline,
                "the holder's lease never expired"
            );
            std::thread::sleep(Duration::from_millis(250));
        };
        assert_eq!(manifest["text_index"], "tantivy", "{manifest}");
        let payload = &manifest["index"]["payload"];
        assert_eq!(payload["seq"], manifest["canonical_seq"], "{manifest}");
        assert_eq!(
            payload["pack_lineage_id"], manifest["pack_lineage_id"],
            "{manifest}"
        );
        assert!(
            manifest["files"]
                .as_object()
                .unwrap()
                .contains_key("index/tantivy/meta.json"),
            "{manifest}"
        );
        assert_eq!(
            index_payload(&dir.join("index/tantivy")),
            *payload,
            "the captured directory carries the declared payload"
        );
        assert_eq!(verify(&s, &dir).status.code(), Some(0));
        assert!(
            recall(&s, &s.db(), "quokka captured")
                .contains(&note["id"].as_str().unwrap().to_owned())
        );

        // 3. a substituted index directory (another lineage's, at its own sequence): rejected
        // as it stands and once its files are re-declared with their true hashes, since the
        // payload no longer matches the manifest's lineage and sequence; nothing is written
        let other = Scratch::new("kaban-pack", "tantivy-other");
        remember(&other, &other.db(), "the other seeded note");
        mark_tantivy(&other.db());
        remember(&other, &other.db(), "quokka of another lineage");
        let other_pack = other.0.join("pack");
        build_ok(&other, &other_pack);
        let index_dir = dir.join("index/tantivy");
        std::fs::set_permissions(&index_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        for entry in std::fs::read_dir(&index_dir).unwrap() {
            let path = entry.unwrap().path();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
            std::fs::remove_file(&path).unwrap();
        }
        for entry in std::fs::read_dir(other_pack.join("index/tantivy")).unwrap() {
            let path = entry.unwrap().path();
            std::fs::copy(&path, index_dir.join(path.file_name().unwrap())).unwrap();
        }
        let before = fingerprint(&dir);
        let out = verify(&s, &dir);
        assert_eq!(out.status.code(), Some(5), "{out:?}");
        assert_eq!(fingerprint(&dir), before, "verify wrote nothing");
        let mut redeclared = read_manifest(&dir);
        let other_manifest = read_manifest(&other_pack);
        let files = redeclared["files"].as_object_mut().unwrap();
        files.retain(|name, _| !name.starts_with("index/"));
        for (name, digest) in other_manifest["files"].as_object().unwrap() {
            if name.starts_with("index/") {
                files.insert(name.clone(), digest.clone());
            }
        }
        write_manifest(&dir, &redeclared);
        let before = fingerprint(&dir);
        let out = verify(&s, &dir);
        assert_eq!(out.status.code(), Some(5), "{out:?}");
        assert!(stderr(&out).contains("index"), "{out:?}");
        assert_eq!(fingerprint(&dir), before, "verify wrote nothing");
    }

    /// T6 review t6-2 (SS-19) and t6-1 (SS-18): a pack of a retired database carries the
    /// Tantivy component, and a `--pack` read recalls through that captured directory (read
    /// only: no lock file, no reader mark, nothing in the pack changes); the same pack
    /// relabelled as FTS5 with the component removed claims an index the snapshot cannot
    /// hold, so verify and the read refuse it.
    #[test]
    fn a_retired_pack_is_recalled_through_its_captured_index_and_never_as_fts5() {
        let s = Scratch::new("kaban-pack", "tantivy-read");
        remember(&s, &s.db(), "the seeded note");
        mark_tantivy(&s.db());
        let note = remember(&s, &s.db(), "quokka read from the immutable pack");
        let id = note["id"].as_str().unwrap().to_owned();
        let dir = s.0.join("pack");
        let manifest = build_ok(&s, &dir);
        assert_eq!(
            manifest["versions"]["schema"],
            kaban_store_sqlite::LATEST_SCHEMA_VERSION,
            "the source retired FTS5 and is at the current schema: {manifest}"
        );
        assert_eq!(manifest["text_index"], "tantivy", "{manifest}");

        // 1. the read: lexical and fuzzy recall answer from the captured index
        let before = fingerprint(&dir);
        let pack_recall = |args: &[&str]| -> Output {
            kaban(&s)
                .args(["--json", "--pack", dir.to_str().unwrap(), "recall"])
                .args(args)
                .output()
                .unwrap()
        };
        let out = pack_recall(&["quokka read"]);
        assert_eq!(out.status.code(), Some(0), "{out:?}");
        let hits = document(&out)["results"].as_array().unwrap().clone();
        assert!(hits.iter().any(|hit| hit["id"] == id.as_str()), "{hits:?}");
        let out = pack_recall(&["--fuzzy", "quoka immutabel"]);
        assert_eq!(out.status.code(), Some(0), "{out:?}");
        let hits = document(&out)["results"].as_array().unwrap().clone();
        assert!(hits.iter().any(|hit| hit["id"] == id.as_str()), "{hits:?}");
        assert_eq!(fingerprint(&dir), before, "a pack read writes nothing");
        assert_eq!(verify(&s, &dir).status.code(), Some(0));

        // 2. relabelled as FTS5 at the retired schema, the component removed: no file is
        // undeclared and every hash holds, yet the manifest claims an index the snapshot
        // cannot hold
        let index_dir = dir.join("index/tantivy");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::set_permissions(dir.join("index"), std::fs::Permissions::from_mode(0o700))
            .unwrap();
        std::fs::set_permissions(&index_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        for entry in std::fs::read_dir(&index_dir).unwrap() {
            let path = entry.unwrap().path();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        std::fs::remove_dir_all(dir.join("index")).unwrap();
        let mut relabelled = read_manifest(&dir);
        relabelled["text_index"] = Value::from("fts5");
        relabelled.as_object_mut().unwrap().remove("index");
        relabelled["files"]
            .as_object_mut()
            .unwrap()
            .retain(|name, _| !name.starts_with("index/"));
        write_manifest(&dir, &relabelled);
        let before = fingerprint(&dir);
        let out = verify(&s, &dir);
        assert_eq!(out.status.code(), Some(5), "{out:?}");
        assert!(stderr(&out).contains("text_index"), "{out:?}");
        let out = pack_recall(&["quokka read"]);
        assert_ne!(out.status.code(), Some(0), "{out:?}");
        assert!(stderr(&out).contains("text_index"), "{out:?}");
        assert_eq!(fingerprint(&dir), before, "neither wrote anything");
    }

    #[test]
    fn a_fork_searches_through_the_copied_index_without_a_rebuild() {
        let s = Scratch::new("kaban-pack", "tantivy-fork");
        remember(&s, &s.db(), "the seeded note");
        mark_tantivy(&s.db());
        let note = remember(&s, &s.db(), "quokka inherited by the fork");
        let pack = s.0.join("pack");
        let manifest = build_ok(&s, &pack);
        let child = s.0.join("child.db");
        let report = fork_ok(&s, &pack, &child);
        let child_index = TantivyIndex::index_dir(&child);
        assert!(child_index.is_dir(), "the index came with the fork");
        let payload = index_payload(&child_index);
        assert_eq!(
            payload["instance_id"], manifest["instance_id"],
            "the parent's payload: no rebuild stamped the child's instance"
        );
        assert_ne!(payload["instance_id"], report["instance_id"]);
        // searched at once, through the copied index, and extended by the fork's own writes
        let id = note["id"].as_str().unwrap().to_owned();
        assert!(recall(&s, &child, "quokka inherited").contains(&id));
        assert_eq!(
            index_payload(&child_index)["instance_id"],
            manifest["instance_id"],
            "the search rebuilt nothing"
        );
        let own = remember(&s, &child, "quokka written by the fork");
        assert!(
            recall(&s, &child, "quokka written fork")
                .contains(&own["id"].as_str().unwrap().to_owned())
        );
        assert!(recall(&s, &child, "quokka inherited").contains(&id));
        // the parent and its pack are untouched
        assert_eq!(verify(&s, &pack).status.code(), Some(0));
        assert!(
            !recall(&s, &s.db(), "quokka written fork")
                .contains(&own["id"].as_str().unwrap().to_owned())
        );
    }

    /// T4 review t4-5 (SS-19): payload equality is not component verification. A `meta.json`
    /// reduced to the declared payload (rehashed in the manifest) is no Tantivy index, and a
    /// referenced segment file removed and undeclared leaves a `meta.json` naming a file the
    /// pack lacks. Both are rejected by verify, which writes nothing.
    #[test]
    fn an_index_that_is_only_a_payload_or_lacks_a_referenced_segment_is_rejected() {
        let s = Scratch::new("kaban-pack", "tantivy-component");
        remember(&s, &s.db(), "the seeded note");
        mark_tantivy(&s.db());
        remember(&s, &s.db(), "quokka captured in the pack");
        let unseal = |path: &Path, mode: u32| {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
        };

        // 1. meta.json reduced to its payload, its true hash declared
        let dir = s.0.join("pack");
        build_ok(&s, &dir);
        let index_dir = dir.join("index/tantivy");
        let meta_path = index_dir.join("meta.json");
        let meta: Value = serde_json::from_slice(&std::fs::read(&meta_path).unwrap()).unwrap();
        let reduced =
            serde_json::to_vec(&serde_json::json!({ "payload": meta["payload"] })).unwrap();
        unseal(&index_dir, 0o700);
        unseal(&meta_path, 0o600);
        std::fs::write(&meta_path, &reduced).unwrap();
        unseal(&meta_path, 0o400);
        let mut manifest = read_manifest(&dir);
        manifest["files"]["index/tantivy/meta.json"] = serde_json::json!({
            "sha256": kaban_policy::sha256_hex(&reduced),
            "bytes": reduced.len(),
        });
        write_manifest(&dir, &manifest);
        let before = fingerprint(&dir);
        let out = verify(&s, &dir);
        assert_eq!(out.status.code(), Some(5), "{out:?}");
        assert!(stderr(&out).contains("index"), "{out:?}");
        assert_eq!(fingerprint(&dir), before, "verify wrote nothing");

        // 2. a referenced segment file removed and undeclared
        let dir = s.0.join("pack2");
        build_ok(&s, &dir);
        let index_dir = dir.join("index/tantivy");
        let segment = std::fs::read_dir(&index_dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| p.extension().is_some_and(|ext| ext == "term"))
            .expect("a segment's term dictionary");
        let name = segment.file_name().unwrap().to_string_lossy().into_owned();
        unseal(&index_dir, 0o700);
        std::fs::remove_file(&segment).unwrap();
        let mut manifest = read_manifest(&dir);
        manifest["files"]
            .as_object_mut()
            .unwrap()
            .remove(&format!("index/tantivy/{name}"))
            .expect("the file was declared");
        write_manifest(&dir, &manifest);
        let before = fingerprint(&dir);
        let out = verify(&s, &dir);
        assert_eq!(out.status.code(), Some(5), "{out:?}");
        assert!(stderr(&out).contains("index"), "{out:?}");
        assert_eq!(fingerprint(&dir), before, "verify wrote nothing");
    }

    /// Rewrite the pack's sealed snapshot in place through `edit` (the write-stamp triggers
    /// call the connection-local `kaban_writer()`, so a stand-in is bound), reseal it and
    /// declare its true hash in the manifest: every file hash holds, so only a check that
    /// reads the bodies can tell.
    fn rewrite_snapshot(dir: &Path, edit: impl FnOnce(&rusqlite::Connection)) {
        let db = dir.join("kaban.db");
        std::fs::set_permissions(&db, std::fs::Permissions::from_mode(0o600)).unwrap();
        {
            let conn = rusqlite::Connection::open(&db).unwrap();
            conn.create_scalar_function(
                "kaban_writer",
                0,
                rusqlite::functions::FunctionFlags::SQLITE_UTF8,
                |_| Ok("test".to_owned()),
            )
            .unwrap();
            // bytes change in place: the change-log row the update trigger appends is not
            // a canonical write, so the sequence stays the manifest's
            let head: i64 = conn
                .query_row("SELECT COALESCE(max(seq), 0) FROM changes", [], |r| {
                    r.get(0)
                })
                .unwrap();
            edit(&conn);
            conn.execute("DELETE FROM changes WHERE seq > ?1", [head])
                .unwrap();
        }
        std::fs::set_permissions(&db, std::fs::Permissions::from_mode(0o400)).unwrap();
        let bytes = std::fs::read(&db).unwrap();
        let mut manifest = read_manifest(dir);
        manifest["files"]["kaban.db"] = serde_json::json!({
            "sha256": kaban_policy::sha256_hex(&bytes),
            "bytes": bytes.len(),
        });
        write_manifest(dir, &manifest);
    }

    /// T7 review t7-3 (SS-17, SS-19): verification decodes what the file hashes cannot see.
    /// A pack of a retired database carries encoded bodies; the same pack with one body's
    /// frame garbled, and one whose snapshot records a dictionary this binary does not ship,
    /// are rejected by verify and by the `--pack` read with every file hash true and nothing
    /// written.
    #[test]
    fn a_pack_with_a_corrupt_body_or_a_foreign_dictionary_is_rejected_with_true_hashes() {
        let s = Scratch::new("kaban-pack", "tantivy-codec");
        remember(&s, &s.db(), "the seeded note");
        mark_tantivy(&s.db());
        // a body long enough to encode, whose frame is not a few repeated bytes
        let mut text = String::new();
        for i in 0..300_u64 {
            use std::fmt::Write as _;
            let _ = writeln!(
                text,
                "quokka line {i:03} at {:x}",
                (i * 2_654_435_761) % 0xFFFF_FFFF
            );
        }
        let note = remember(&s, &s.db(), &text);
        let id = note["id"].as_str().unwrap().to_owned();
        let pack_recall = |dir: &Path| -> Output {
            kaban(&s)
                .args([
                    "--json",
                    "--pack",
                    dir.to_str().unwrap(),
                    "recall",
                    "quokka line",
                ])
                .output()
                .unwrap()
        };

        // 1. one garbled frame
        let dir = s.0.join("pack");
        let manifest = build_ok(&s, &dir);
        assert_eq!(manifest["versions"]["codec"], "zstd-dict-v1", "{manifest}");
        assert_eq!(verify(&s, &dir).status.code(), Some(0));
        rewrite_snapshot(&dir, |conn| {
            let mut frame: Vec<u8> = conn
                .query_row("SELECT text FROM node WHERE id = ?1", [&id], |r| r.get(0))
                .expect("an encoded BLOB body");
            assert!(
                frame.starts_with(&[0, b'K', b'Z']) && frame.len() > 64,
                "{}",
                frame.len()
            );
            for b in &mut frame[24..40] {
                *b ^= 0x5A;
            }
            conn.execute(
                "UPDATE node SET text = ?1 WHERE id = ?2",
                rusqlite::params![frame, id],
            )
            .unwrap();
        });
        let before = fingerprint(&dir);
        let out = verify(&s, &dir);
        assert_eq!(out.status.code(), Some(5), "{out:?}");
        assert!(stderr(&out).contains("body"), "{out:?}");
        let out = pack_recall(&dir);
        assert_eq!(out.status.code(), Some(5), "{out:?}");
        assert!(stderr(&out).contains("body"), "{out:?}");
        assert_eq!(fingerprint(&dir), before, "neither wrote anything");

        // 2. a snapshot bound to another dictionary
        let dir = s.0.join("pack2");
        build_ok(&s, &dir);
        assert_eq!(verify(&s, &dir).status.code(), Some(0));
        rewrite_snapshot(&dir, |conn| {
            conn.execute(
                "UPDATE index_meta SET value = ?1 WHERE key = 'body_dictionary_sha256'",
                ["f".repeat(64)],
            )
            .unwrap();
        });
        let before = fingerprint(&dir);
        let out = verify(&s, &dir);
        assert_eq!(out.status.code(), Some(5), "{out:?}");
        let out = pack_recall(&dir);
        assert_eq!(out.status.code(), Some(5), "{out:?}");
        assert_eq!(fingerprint(&dir), before, "neither wrote anything");
    }

    /// T4 review t4-3 (SS-20): two forks of a pack with an index racing for one destination
    /// leave exactly one complete fork, the winner's database beside its index directory (the
    /// parent's payload, searched at once, never rebuilt), and nothing of the loser, which
    /// removes only what it owns.
    #[test]
    fn two_concurrent_forks_with_an_index_leave_one_complete_fork() {
        let s = Scratch::new("kaban-pack", "tantivy-fork-race");
        remember(&s, &s.db(), "the seeded note");
        mark_tantivy(&s.db());
        let note = remember(&s, &s.db(), "quokka raced by two forks");
        seed_many(&s, 200);
        let pack = s.0.join("pack");
        let manifest = build_ok(&s, &pack);
        let child = s.0.join("child.db");
        let attempts: Vec<Child> = (0..2)
            .map(|_| {
                kaban(&s)
                    .args(["--json", "pack", "fork"])
                    .arg(&pack)
                    .arg(&child)
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
                    .unwrap()
            })
            .collect();
        let outputs: Vec<Output> = attempts
            .into_iter()
            .map(|attempt| attempt.wait_with_output().unwrap())
            .collect();
        let mut codes: Vec<Option<i32>> = outputs.iter().map(|out| out.status.code()).collect();
        codes.sort_unstable();
        assert_eq!(codes, vec![Some(0), Some(4)], "{outputs:?}");
        let child_index = TantivyIndex::index_dir(&child);
        assert!(child.is_file(), "the winner published");
        assert!(
            child_index.join("meta.json").is_file(),
            "the winner's index directory is intact"
        );
        assert_eq!(
            index_payload(&child_index)["instance_id"],
            manifest["instance_id"],
            "the copied index, not a rebuilt one"
        );
        assert_eq!(fork_rows(&child), 1);
        assert_eq!(
            staging_copies(&child),
            Vec::<PathBuf>::new(),
            "neither attempt left its staging behind"
        );
        let id = note["id"].as_str().unwrap().to_owned();
        assert!(recall(&s, &child, "quokka raced").contains(&id));
        assert_eq!(verify(&s, &pack).status.code(), Some(0));
    }

    /// T4 review t4-7 (SS-20): an attempt interrupted while copying its index leaves only
    /// what it owns, the initialized staging copy `.<name>.forking.<pid>.<id>` and the index
    /// staging `.index/` beside it, never the destination's index root. The retry is not
    /// refused by that leftover, publishes one complete fork, and removes nothing it does not
    /// own.
    #[test]
    fn a_fork_interrupted_while_copying_its_index_does_not_refuse_the_retry() {
        let s = Scratch::new("kaban-pack", "tantivy-fork-retry");
        remember(&s, &s.db(), "the seeded note");
        mark_tantivy(&s.db());
        let note = remember(&s, &s.db(), "quokka retried after an interruption");
        let pack = s.0.join("pack");
        let manifest = build_ok(&s, &pack);
        let child = s.0.join("child.db");
        // what a kill between the identity commit and publication leaves behind
        let leftover =
            s.0.join(".child.db.forking.4242.01a14192-0000-7000-8000-000000000000");
        std::fs::write(&leftover, b"the initialized copy of an interrupted attempt").unwrap();
        let mut leftover_index = leftover.as_os_str().to_owned();
        leftover_index.push(".index");
        let leftover_index = PathBuf::from(leftover_index).join("tantivy");
        std::fs::create_dir_all(&leftover_index).unwrap();
        std::fs::write(leftover_index.join("partial.term"), b"half a segment").unwrap();

        let out = kaban(&s)
            .args(["--json", "pack", "fork"])
            .arg(&pack)
            .arg(&child)
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(0), "{out:?}");
        let child_index = TantivyIndex::index_dir(&child);
        assert!(child.is_file() && child_index.join("meta.json").is_file());
        assert_eq!(
            index_payload(&child_index)["instance_id"],
            manifest["instance_id"],
            "the copied index, not a rebuilt one"
        );
        let id = note["id"].as_str().unwrap().to_owned();
        assert!(recall(&s, &child, "quokka retried").contains(&id));
        assert!(
            leftover.is_file() && leftover_index.join("partial.term").is_file(),
            "the retry removes nothing it does not own"
        );
    }
}
