//! Files the `kaban` binary creates (core-spec S5; C2.5b review P-6): the database and key
//! files are exactly `0600` whatever the process `umask`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::Scratch;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn sidecar(db: &Path, suffix: &str) -> PathBuf {
    let mut s = db.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

/// Drive one `remember` through the MCP server so the database and its key get written.
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
    // produces an audit row signed with the key
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
}
