//! Shared black-box harness for the `kaban` binary over stdio: a private scratch home and a
//! server whose stdout is drained by a reader thread, so responses can be awaited with a
//! deadline and matched by id regardless of arrival order.

#![allow(dead_code)] // each test binary uses a subset
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Private scratch home (db + keys), removed on drop.
pub(crate) struct Scratch(pub(crate) PathBuf);
impl Scratch {
    pub(crate) fn new(prefix: &str, name: &str) -> Self {
        let d = std::env::temp_dir().join(format!("{prefix}-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        Self(d)
    }
    pub(crate) fn db(&self) -> PathBuf {
        self.0.join("kaban.db")
    }
    /// `$KABAN_HOME` of servers started on this scratch (`home/keys/<db-id>.key`).
    pub(crate) fn home(&self) -> PathBuf {
        self.0.join("home")
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A kaban server over stdio.
pub(crate) struct Server {
    child: Child,
    rx: mpsc::Receiver<Value>,
    pending: Vec<Value>,
    next_id: u64,
    /// Collects stderr when the server was started with [`Self::start_traced`].
    stderr: Option<std::thread::JoinHandle<String>>,
}

impl Server {
    /// Start `kaban --db <scratch db> [args..]` with `TOKIO_WORKER_THREADS=1` and complete the
    /// MCP handshake (10 s deadline).
    pub(crate) fn start(s: &Scratch) -> Self {
        Self::start_with(s, &[], Duration::from_secs(10)).0
    }

    /// Start in a private repository without changing this test process's directory.
    pub(crate) fn start_at(s: &Scratch, root: &std::path::Path) -> Self {
        Self::spawn_at(
            s,
            &[],
            Duration::from_secs(10),
            false,
            &json!({}),
            Some(root),
        )
        .0
    }

    /// [`Self::start`] with extra arguments and an explicit `initialize` deadline; also returns
    /// how long the `initialize` round trip took.
    pub(crate) fn start_with(
        s: &Scratch,
        args: &[&str],
        init_deadline: Duration,
    ) -> (Self, Duration) {
        Self::spawn(s, args, init_deadline, false, &json!({}))
    }

    /// [`Self::start`] with extra arguments, `RUST_LOG=trace` and stderr captured (read it back
    /// with [`Self::finish_with_stderr`]).
    pub(crate) fn start_traced(s: &Scratch, args: &[&str]) -> Self {
        Self::spawn(s, args, Duration::from_secs(10), true, &json!({})).0
    }

    fn spawn(
        s: &Scratch,
        args: &[&str],
        init_deadline: Duration,
        traced: bool,
        capabilities: &Value,
    ) -> (Self, Duration) {
        Self::spawn_at(s, args, init_deadline, traced, capabilities, None)
    }

    fn spawn_at(
        s: &Scratch,
        args: &[&str],
        init_deadline: Duration,
        traced: bool,
        capabilities: &Value,
        root: Option<&std::path::Path>,
    ) -> (Self, Duration) {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_kaban"));
        if let Some(root) = root {
            cmd.current_dir(root);
        }
        cmd.env("KABAN_HOME", s.home())
            .env("TOKIO_WORKER_THREADS", "1")
            .args(["--db", s.db().to_str().unwrap()])
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped());
        if traced {
            cmd.env("RUST_LOG", "trace").stderr(Stdio::piped());
        } else {
            cmd.stderr(Stdio::null());
        }
        let mut child = cmd.spawn().unwrap();
        let stderr = child.stderr.take().map(|mut err| {
            std::thread::spawn(move || {
                let mut text = String::new();
                let _ = std::io::Read::read_to_string(&mut err, &mut text);
                text
            })
        });
        let out = BufReader::new(child.stdout.take().unwrap());
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in out.lines() {
                let Ok(line) = line else { break };
                if let Ok(v) = serde_json::from_str::<Value>(&line)
                    && tx.send(v).is_err()
                {
                    break;
                }
            }
        });
        let mut srv = Self {
            child,
            rx,
            pending: Vec::new(),
            next_id: 0,
            stderr,
        };
        let sent = Instant::now();
        let id = srv.request(
            "initialize",
            &json!({"protocolVersion": "2025-11-25", "capabilities": capabilities, "clientInfo": {"name": "t", "version": "0"}}),
        );
        let init = srv
            .response(id, init_deadline)
            .unwrap_or_else(|| panic!("initialize not answered within {init_deadline:?}"));
        let took = sent.elapsed();
        assert_eq!(init["result"]["serverInfo"]["name"], json!("kaban"));
        srv.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        (srv, took)
    }
    pub(crate) fn send(&mut self, msg: &Value) {
        let stdin = self.child.stdin.as_mut().unwrap();
        stdin.write_all(msg.to_string().as_bytes()).unwrap();
        stdin.write_all(b"\n").unwrap();
        stdin.flush().unwrap();
    }
    /// Send a request and return its id (the response is collected with [`Self::response`]).
    pub(crate) fn request(&mut self, method: &str, params: &Value) -> u64 {
        self.next_id += 1;
        self.send(
            &json!({"jsonrpc": "2.0", "id": self.next_id, "method": method, "params": params}),
        );
        self.next_id
    }
    /// Wait up to `timeout` for the response with `id` (a message without `method`; the
    /// server's own requests may reuse the number); other messages are kept for later.
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
                Ok(v) if pred(&v) => return Some(v),
                Ok(v) => self.pending.push(v),
                Err(_) => return None,
            }
        }
    }
    /// Close stdin and wait for the process to exit; returns its exit status and how long the
    /// shutdown took.
    pub(crate) fn finish(mut self) -> (std::process::ExitStatus, Duration) {
        drop(self.child.stdin.take());
        let t = Instant::now();
        let status = self.child.wait().unwrap();
        (status, t.elapsed())
    }
    /// [`Self::finish`] for a traced server: also returns everything it wrote to stderr.
    pub(crate) fn finish_with_stderr(mut self) -> (std::process::ExitStatus, String) {
        drop(self.child.stdin.take());
        let status = self.child.wait().unwrap();
        let stderr = self
            .stderr
            .take()
            .map(|h| h.join().unwrap())
            .unwrap_or_default();
        (status, stderr)
    }
}
