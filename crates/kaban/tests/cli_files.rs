//! Files the `kaban` binary creates or is pointed at (core-spec S5, S3; C2.5b review P-2 and
//! P-6): the database, key and audit-export files are exactly `0600` whatever the process
//! `umask`, and `--audit-export` refuses a path that aliases the database or its sidecars.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::Scratch;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// CLI-files view of the scratch home: the audit export target.
trait ScratchExt {
    fn export(&self) -> PathBuf;
}
impl ScratchExt for Scratch {
    fn export(&self) -> PathBuf {
        self.0.join("audit-export.jsonl")
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

/// `kaban serve` with `--audit-export export`, stdin closed: the server exits as soon as the
/// client is gone, so only the startup checks run.
fn serve_with_export(s: &Scratch, export: &Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_kaban"))
        .env("KABAN_HOME", s.home())
        .arg("--db")
        .arg(s.db())
        .arg("--audit-export")
        .arg(export)
        .arg("serve")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap()
}

fn assert_refused_as_config_error(out: &std::process::Output, what: &str) {
    assert_eq!(
        out.status.code(),
        Some(4),
        "{what}: expected exit 4 (config error), got {:?}; stderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("audit-export"),
        "{what}: stderr names the option: {err}"
    );
    assert!(out.stdout.is_empty(), "{what}: stdout stays empty on error");
}

/// P-2: `--audit-export` pointed straight at the database (or a sidecar name) is a
/// configuration error, before any write.
#[test]
fn audit_export_pointing_at_the_database_or_a_sidecar_is_refused() {
    let s = Scratch::new("kaban-cli-files", "export-direct");
    let out = serve_with_export(&s, &s.db());
    assert_refused_as_config_error(&out, "export = db");
    assert!(
        !s.db().exists(),
        "the database is not even created when the configuration is refused"
    );
    let out = serve_with_export(&s, &sidecar(&s.db(), "-wal"));
    assert_refused_as_config_error(&out, "export = db-wal");
    let out = serve_with_export(&s, &sidecar(&s.db(), "-shm"));
    assert_refused_as_config_error(&out, "export = db-shm");
    // the same file reached through a `..` component is still the database
    let dotted = s.0.join("sub").join("..").join("kaban.db");
    std::fs::create_dir_all(s.0.join("sub")).unwrap();
    let out = serve_with_export(&s, &dotted);
    assert_refused_as_config_error(&out, "export = sub/../db");
}

/// P-2: a symlink is refused whatever it points at, and a hard link to the database is
/// recognised by its `(dev, inode)` identity.
#[test]
fn audit_export_through_a_symlink_or_hard_link_to_the_database_is_refused() {
    let s = Scratch::new("kaban-cli-files", "export-links");
    // create the database first, so links to it can exist
    let out = serve_with_export(&s, &s.export());
    assert!(
        s.db().exists(),
        "fixture: a plain export path starts the server (exit {:?}, stderr {})",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    let link = s.0.join("export-link.jsonl");
    std::os::unix::fs::symlink(s.0.join("harmless.jsonl"), &link).unwrap();
    let out = serve_with_export(&s, &link);
    assert_refused_as_config_error(&out, "export = symlink to a harmless file");
    assert!(
        !s.0.join("harmless.jsonl").exists(),
        "the symlink target was not created"
    );
    let hard = s.0.join("export-hard.jsonl");
    std::fs::hard_link(s.db(), &hard).unwrap();
    let out = serve_with_export(&s, &hard);
    assert_refused_as_config_error(&out, "export = hard link to the database");
    assert_eq!(
        std::fs::metadata(s.db()).unwrap().len(),
        std::fs::metadata(&hard).unwrap().len(),
        "the database bytes were not appended to"
    );
}

/// Drive one `remember` through the MCP server so the audit export file gets written.
fn remember_once(s: &Scratch, mut cmd: Command) {
    let mut child = cmd
        .env("KABAN_HOME", s.home())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut out = BufReader::new(child.stdout.take().unwrap());
    let mut stdin = child.stdin.take().unwrap();
    let mut send = |v: Value| {
        stdin.write_all(v.to_string().as_bytes()).unwrap();
        stdin.write_all(b"\n").unwrap();
        stdin.flush().unwrap();
    };
    let recv = |out: &mut BufReader<std::process::ChildStdout>| -> Value {
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut line = String::new();
        while Instant::now() < deadline {
            line.clear();
            assert!(
                out.read_line(&mut line).unwrap() != 0,
                "server closed stdout"
            );
            if let Ok(v) = serde_json::from_str::<Value>(&line)
                && v.get("id").is_some()
            {
                return v;
            }
        }
        panic!("no response within the deadline");
    };
    send(
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": {"name": "t", "version": "0"}}}),
    );
    assert_eq!(
        recv(&mut out)["result"]["serverInfo"]["name"],
        json!("kaban")
    );
    send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    // the shipped example policy redacts `hunter2` (rule `old-password`, not a secret), which
    // produces the audit row that the export file carries
    send(
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "remember", "arguments": {"kind": "Fact", "text": "a note with hunter2 written under a restrictive umask"}}}),
    );
    let resp = recv(&mut out);
    assert!(resp.get("result").is_some(), "remember answered: {resp}");
    drop(stdin);
    let status = child.wait().unwrap();
    assert!(status.success(), "server exit {status:?}");
}

/// P-6: `OpenOptions::mode(0o600)` is filtered by the process umask, so under `umask 0277`
/// a file would be created `0400` (unreadable for re-open as a database, and not the exact
/// S5 mode). Every file kaban creates is `fchmod`-ed to exactly `0600` before its descriptor is
/// dropped. The umask is inherited from `sh`, so the test process's own umask is untouched.
#[test]
fn files_created_under_a_restrictive_umask_are_exactly_0600() {
    let s = Scratch::new("kaban-cli-files", "umask");
    let policy = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/policy.yaml");
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg("umask 0277; exec \"$0\" \"$@\"")
        .arg(env!("CARGO_BIN_EXE_kaban"))
        .arg("--db")
        .arg(s.db())
        .arg("--policy")
        .arg(policy)
        .arg("--audit-export")
        .arg(s.export())
        .arg("serve");
    remember_once(&s, cmd);
    assert_eq!(mode(&s.db()), 0o600, "database file mode under umask 0277");
    for suffix in ["-wal", "-shm"] {
        let f = sidecar(&s.db(), suffix);
        if f.exists() {
            assert_eq!(mode(&f), 0o600, "{suffix} file mode under umask 0277");
        }
    }
    let key = std::fs::read_dir(s.home().join("keys"))
        .unwrap()
        .next()
        .expect("one key file")
        .unwrap()
        .path();
    assert_eq!(mode(&key), 0o600, "audit key file mode under umask 0277");
    assert!(s.export().exists(), "fixture: the export was written");
    assert_eq!(
        mode(&s.export()),
        0o600,
        "audit export file mode under umask 0277"
    );
}
