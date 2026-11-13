//! A cwd-aware black-box stdio client for the acceptance tests (stage1-plan CG10.2; the plan's
//! `tests/common/mcp_client.rs`). `common/mod.rs`'s `Server` spawns the binary in the test's
//! working directory; the code-graph provider resolves the repository from the server's
//! `cwd` (`main.rs` `repo_root`), so the acceptance tests need a server started inside the
//! scratch fixture repository. This client also keeps every byte the server wrote to stdout
//! (not only the JSON it parsed), and it computes the snapshot directory the server will read
//! with the same key and rule as `kaban serve` (`codegraph.rs`).
//!
//! Included from a test file with `#[path = "common/mcp_client.rs"] mod mcp_client;` beside
//! `mod common;` (`common/mod.rs` is not owned by CG10.2 and declares no submodule).

#![allow(dead_code)] // each test binary uses a subset
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

use kaban_codegraph::{RepoIdKey, repo_id, resolve_common_dir};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// The deadline of one request.
pub(crate) const DEADLINE: Duration = Duration::from_secs(20);

/// How to start the server.
pub(crate) struct Spawn<'a> {
    /// `KABAN_HOME` (the key directory and the `codegraph/` directory live here).
    pub(crate) home: &'a Path,
    /// `--db`.
    pub(crate) db: &'a Path,
    /// The server's working directory (the fixture repository).
    pub(crate) cwd: &'a Path,
    /// `--policy`, when set.
    pub(crate) policy: Option<&'a Path>,
    /// `--workspace`.
    pub(crate) workspace: &'a str,
}

/// One line the server wrote to stdout: the raw text and, when it parsed, the message.
struct Line {
    raw: String,
    msg: Option<Value>,
}

/// A kaban server over stdio started in a chosen working directory, traced
/// (`RUST_LOG=trace`, stderr captured).
pub(crate) struct McpClient {
    child: Child,
    rx: mpsc::Receiver<Line>,
    pending: Vec<Value>,
    next_id: u64,
    /// Every stdout line in arrival order (the canary sweep's stdout channel).
    transcript: Vec<String>,
    /// Every `tools/call` response (the sweep's "every MCP response" channel).
    responses: Vec<Value>,
    stderr: std::thread::JoinHandle<String>,
}

/// What a finished server left behind.
pub(crate) struct Finished {
    pub(crate) status: std::process::ExitStatus,
    /// Every stdout line, joined with `\n`.
    pub(crate) stdout: String,
    /// Everything written to stderr.
    pub(crate) stderr: String,
    /// Every `tools/call` response, serialised.
    pub(crate) responses: Vec<Value>,
}

impl McpClient {
    /// Start `kaban --db <db> [--policy <p>] --workspace <ws>` in `cwd`
    /// and complete the MCP handshake.
    pub(crate) fn start(spawn: &Spawn<'_>) -> Self {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_kaban"));
        cmd.current_dir(spawn.cwd)
            .env("KABAN_HOME", spawn.home)
            .env("TOKIO_WORKER_THREADS", "1")
            .env("RUST_LOG", "trace")
            .args(["--db", spawn.db.to_str().unwrap()])
            .args(["--workspace", spawn.workspace])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(p) = spawn.policy {
            cmd.args(["--policy", p.to_str().unwrap()]);
        }
        let mut child = cmd.spawn().unwrap();
        let mut err = child.stderr.take().unwrap();
        let stderr = std::thread::spawn(move || {
            let mut text = String::new();
            let _ = std::io::Read::read_to_string(&mut err, &mut text);
            text
        });
        let out = BufReader::new(child.stdout.take().unwrap());
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in out.lines() {
                let Ok(raw) = line else { break };
                let msg = serde_json::from_str::<Value>(&raw).ok();
                if tx.send(Line { raw, msg }).is_err() {
                    break;
                }
            }
        });
        let mut c = Self {
            child,
            rx,
            pending: Vec::new(),
            next_id: 0,
            transcript: Vec::new(),
            responses: Vec::new(),
            stderr,
        };
        let id = c.request(
            "initialize",
            &json!({"protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": {"name": "t", "version": "0"}}),
        );
        let init = c
            .response(id, DEADLINE)
            .unwrap_or_else(|| panic!("initialize not answered within {DEADLINE:?}"));
        assert_eq!(init["result"]["serverInfo"]["name"], json!("kaban"));
        c.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        c
    }

    fn send(&mut self, msg: &Value) {
        let stdin = self.child.stdin.as_mut().unwrap();
        stdin.write_all(msg.to_string().as_bytes()).unwrap();
        stdin.write_all(b"\n").unwrap();
        stdin.flush().unwrap();
    }

    /// Send a request and return its id.
    pub(crate) fn request(&mut self, method: &str, params: &Value) -> u64 {
        self.next_id += 1;
        self.send(
            &json!({"jsonrpc": "2.0", "id": self.next_id, "method": method, "params": params}),
        );
        self.next_id
    }

    /// Wait for the response with `id`; other messages are kept for later.
    pub(crate) fn response(&mut self, id: u64, timeout: Duration) -> Option<Value> {
        self.wait_for(timeout, |v| {
            v.get("method").is_none() && v["id"] == json!(id)
        })
    }

    fn wait_for(&mut self, timeout: Duration, pred: impl Fn(&Value) -> bool) -> Option<Value> {
        if let Some(i) = self.pending.iter().position(&pred) {
            return Some(self.pending.remove(i));
        }
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.checked_duration_since(Instant::now())?;
            match self.rx.recv_timeout(left) {
                Ok(line) => {
                    self.transcript.push(line.raw);
                    match line.msg {
                        Some(v) if pred(&v) => return Some(v),
                        Some(v) => self.pending.push(v),
                        None => {}
                    }
                }
                Err(_) => return None,
            }
        }
    }

    /// `tools/call` and its whole response (recorded for the sweep).
    pub(crate) fn call_raw(&mut self, tool: &str, args: &Value) -> Value {
        let id = self.request("tools/call", &json!({"name": tool, "arguments": args}));
        let resp = self
            .response(id, DEADLINE)
            .unwrap_or_else(|| panic!("{tool}: no response within {DEADLINE:?}"));
        self.responses.push(resp.clone());
        resp
    }

    /// `tools/call` expecting success: the parsed structured result.
    pub(crate) fn call(&mut self, tool: &str, args: &Value) -> Value {
        let resp = self.call_raw(tool, args);
        assert_ne!(resp["result"]["isError"], json!(true), "{tool}: {resp}");
        serde_json::from_str(resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
    }

    /// `tools/call` expecting a tool error: its text.
    pub(crate) fn call_err(&mut self, tool: &str, args: &Value) -> String {
        let resp = self.call_raw(tool, args);
        assert_eq!(resp["result"]["isError"], json!(true), "{tool}: {resp}");
        resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    /// Close stdin, wait for the exit and hand back every channel.
    pub(crate) fn finish(mut self) -> Finished {
        drop(self.child.stdin.take());
        let status = self.child.wait().unwrap();
        // drain what the reader thread still holds
        while let Ok(line) = self.rx.try_recv() {
            self.transcript.push(line.raw);
        }
        let stderr = self.stderr.join().unwrap();
        Finished {
            status,
            stdout: self.transcript.join("\n"),
            stderr,
            responses: self.responses,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// the provider's side: key and snapshot directory, as `kaban serve` derives them

/// Write a 32-byte `<home>/keys/codegraph.key` (0600; `keys/` 0700) and return the key.
pub(crate) fn write_codegraph_key(home: &Path) -> RepoIdKey {
    let keys = home.join("keys");
    std::fs::create_dir_all(&keys).unwrap();
    std::fs::set_permissions(&keys, std::fs::Permissions::from_mode(0o700)).unwrap();
    let bytes: [u8; 32] = std::array::from_fn(|i| u8::try_from(i * 7 + 3).unwrap());
    let path = keys.join("codegraph.key");
    std::fs::write(&path, bytes).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    RepoIdKey::new(bytes)
}

/// `<home>/codegraph/<repo-id>` for the repository at `repo`, created 0700 (both levels),
/// with `<repo-id>` from `kaban_codegraph::{resolve_common_dir, repo_id}` over the output
/// of `git rev-parse --git-common-dir`, exactly as `kaban/src/codegraph.rs` derives it.
pub(crate) fn snapshot_dir(home: &Path, key: &RepoIdKey, repo: &Path) -> PathBuf {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "--git-common-dir"])
        .output()
        .unwrap();
    assert!(out.status.success(), "git rev-parse --git-common-dir");
    let text = String::from_utf8(out.stdout).unwrap();
    let (_handle, canonical) = resolve_common_dir(repo, &text).expect("common dir resolves");
    let id = repo_id(key, canonical.to_str().unwrap());
    let cg = home.join("codegraph");
    let dir = cg.join(id);
    std::fs::create_dir_all(&dir).unwrap();
    for p in [&cg, &dir] {
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    dir
}
