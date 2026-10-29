//! The first `code_*` call of a session is warm (backlog G6; GO-1 defect 5): on a real 83 MB
//! snapshot the first call paid 500 ms (snapshot admission: `PRAGMA quick_check` once per
//! process and file generation, then the head fetch), later calls 3 to 4 ms. `kaban serve`
//! now resolves the current snapshot once the transport is up, so the first tool call after
//! the warm-up answers like every later one.
//!
//! The test needs a real snapshot and is `#[ignore]`: `KABAN_TEST_GO_REPO` (the indexed
//! checkout, the server's cwd) and `KABAN_TEST_GO_HOME` (its `KABAN_HOME`, holding
//! `keys/codegraph.key` and `codegraph/<repo-id>/<commit>.cg.db`), for example the GO-1
//! dogfood clone and home. Run it as
//! `cargo nextest run -p kaban --run-ignored only --test mcp_code_warm`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion
#![allow(clippy::print_stderr)] // the visible `NOT RUN` marker and the timing line

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// The bound a warm first call must meet (the brief's line; cold calls measured 520-830 ms).
const WARM_BOUND: Duration = Duration::from_millis(50);

/// How long the server gets to warm the snapshot after `initialize` (cold admission measured
/// at about half a second; the bound is generous so the test is about the warm-up, not the box).
const WARMUP_GRACE: Duration = Duration::from_secs(3);

const DEADLINE: Duration = Duration::from_secs(30);

fn env_path(var: &str) -> Option<PathBuf> {
    let p = PathBuf::from(std::env::var_os(var)?);
    p.is_dir().then_some(p)
}

/// One `kaban serve` over stdio in `repo` with `KABAN_HOME = home`, `RUST_LOG=info` on a piped
/// stderr; a private database so nothing in `home` is written.
struct Session {
    child: std::process::Child,
    out: BufReader<std::process::ChildStdout>,
    next_id: u64,
    db_dir: PathBuf,
}

impl Session {
    fn start(repo: &std::path::Path, home: &std::path::Path) -> Self {
        let db_dir = std::env::temp_dir().join(format!("kaban-code-warm-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&db_dir);
        std::fs::create_dir_all(&db_dir).unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_kaban"))
            .current_dir(repo)
            .env("KABAN_HOME", home)
            .env("RUST_LOG", "info")
            .env("TOKIO_WORKER_THREADS", "1")
            .args(["--db", db_dir.join("kaban.db").to_str().unwrap(), "serve"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let out = BufReader::new(child.stdout.take().unwrap());
        let mut s = Self {
            child,
            out,
            next_id: 0,
            db_dir,
        };
        let init = s.request(
            "initialize",
            &json!({"protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": {"name": "warm", "version": "0"}}),
        );
        assert_eq!(
            init["result"]["serverInfo"]["name"],
            json!("kaban"),
            "{init}"
        );
        s.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        s
    }

    fn send(&mut self, msg: &Value) {
        let stdin = self.child.stdin.as_mut().unwrap();
        stdin.write_all(msg.to_string().as_bytes()).unwrap();
        stdin.write_all(b"\n").unwrap();
        stdin.flush().unwrap();
    }

    /// Send a request and block for its response (the server answers in order on one worker).
    fn request(&mut self, method: &str, params: &Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        let deadline = Instant::now() + DEADLINE;
        let mut line = String::new();
        while Instant::now() < deadline {
            line.clear();
            assert_ne!(
                self.out.read_line(&mut line).unwrap(),
                0,
                "server closed stdout before answering {method}"
            );
            if let Ok(v) = serde_json::from_str::<Value>(&line)
                && v["id"] == json!(id)
            {
                return v;
            }
        }
        panic!("{method}: no response within {DEADLINE:?}");
    }

    /// Time one `tools/call` round trip; the result text must not be an error.
    fn timed_call(&mut self, tool: &str, args: &Value) -> Duration {
        let started = Instant::now();
        let resp = self.request("tools/call", &json!({"name": tool, "arguments": args}));
        let elapsed = started.elapsed();
        assert_ne!(resp["result"]["isError"], json!(true), "{tool}: {resp}");
        elapsed
    }

    /// Close stdin, wait, and return everything the server wrote to stderr.
    fn finish(mut self) -> String {
        drop(self.child.stdin.take());
        let mut err = self.child.stderr.take().unwrap();
        let mut text = String::new();
        let _ = std::io::Read::read_to_string(&mut err, &mut text);
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.db_dir);
        text
    }
}

/// G6: after the server has had a moment to warm the snapshot, the first `code_definition` of
/// the session answers within [`WARM_BOUND`], like the second; the server's stderr says the
/// warm-up ran.
#[test]
#[ignore = "requires an indexed checkout: KABAN_TEST_GO_REPO and KABAN_TEST_GO_HOME"]
fn first_code_call_after_warm_up_is_under_fifty_ms() {
    let (Some(repo), Some(home)) = (
        env_path("KABAN_TEST_GO_REPO"),
        env_path("KABAN_TEST_GO_HOME"),
    ) else {
        eprintln!("NOT RUN: KABAN_TEST_GO_REPO or KABAN_TEST_GO_HOME is unset or not a directory");
        return;
    };
    let mut s = Session::start(&repo, &home);
    std::thread::sleep(WARMUP_GRACE);
    let target = json!({"target": {"label": "Function", "name": "NewCmdRoot"}});
    let first = s.timed_call("code_definition", &target);
    let second = s.timed_call("code_definition", &target);
    let stderr = s.finish();
    eprintln!("first {first:?}, second {second:?}");
    assert!(
        stderr.contains("code graph warmed"),
        "the warm-up ran and said so: {stderr}"
    );
    assert!(
        first < WARM_BOUND,
        "first call {first:?} is not under {WARM_BOUND:?} (second {second:?})"
    );
}
