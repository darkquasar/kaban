//! Spec 05 SS-10 on a database marked for the Tantivy index (feature `text-index-tantivy`):
//! the public entry points distinguish a pre-commit rollback (`rejected`, its own status), a
//! searchable success (`committed`, 0: a lone writer publishes its own commit inside the
//! barrier) and a post-commit pending outcome (`committed_not_yet_searchable`, status 6 on the
//! CLI, the same receipt over MCP, a stderr note and exit 0 from a hook) while another
//! process holds the writer lock stopped; a `set -e` script handles 6 explicitly; and the
//! pending write is never replayed: once the lock is free it is published as it stands.

#![cfg(feature = "text-index-tantivy")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion
#![allow(clippy::print_stderr)] // the per-write latencies are the test's report

mod common;

use common::{Scratch, Server};
use kaban_core::config::Recall;
use kaban_core::text_index::{TextIndex, Visibility};
use kaban_core::{ChangeLogStore, MemoryStore};
use kaban_index_tantivy::TantivyIndex;
use kaban_index_tantivy::projector::{Halt, Projector};
use kaban_store_sqlite::SqliteStore;
use kaban_store_sqlite::index_rows::IndexRows;
use serde_json::{Value, json};
use std::io::Write;
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

const DEADLINE: Duration = Duration::from_secs(20);
const WORKSPACE: &str = "default";
const UNKNOWN_ID: &str = "01a17a03-5cbc-7000-8000-000000000000";
const HOLDER_TTL_S: u32 = 2;

fn kaban(s: &Scratch) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_kaban"));
    cmd.env("KABAN_HOME", s.home())
        .env("RUST_LOG", "warn")
        .args(["--db", s.db().to_str().unwrap(), "--workspace", WORKSPACE])
        .stdin(Stdio::null());
    cmd
}

fn run(s: &Scratch, args: &[&str]) -> Output {
    kaban(s).args(args).output().unwrap()
}

/// stdout of a run as its one JSON document (the caller checks the status).
fn document(out: &Output) -> Value {
    let text = String::from_utf8(out.stdout.clone()).unwrap();
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("one JSON document: {e}: {text}"))
}

fn store(s: &Scratch) -> SqliteStore {
    SqliteStore::open(&s.db(), &s.home().join("keys")).unwrap()
}

fn adapter(s: &Scratch) -> TantivyIndex {
    TantivyIndex::open(
        &TantivyIndex::index_dir(&s.db()),
        &kaban_core::shipped::resolved().unwrap(),
        &Recall::default(),
        Box::new(IndexRows::open(&s.db()).unwrap()),
    )
    .unwrap()
}

/// Mark the database for the Tantivy index (the marker lane T5's migration sets).
fn mark_tantivy(s: &Scratch) {
    let conn = rusqlite::Connection::open(s.db()).unwrap();
    conn.execute(
        "INSERT OR REPLACE INTO index_meta(key, value) VALUES ('text_index', 'tantivy')",
        [],
    )
    .unwrap();
}

/// The published watermark, `None` while the index serves no payload.
fn visible_through(index: &dyn TextIndex) -> Option<i64> {
    match index.visible().unwrap() {
        Visibility::Through(seq) => Some(seq),
        Visibility::Unavailable => None,
        Visibility::Current => panic!("an index with its own storage reports a watermark"),
    }
}

/// Wait until the holder role owns the `text-index` lease: since the store publishes its own
/// commits (spec 05 OWN-M), a row written before the holder is elected would be published by
/// the writer itself, and the head would say nothing about who holds the lock.
fn wait_for_holder(s: &Scratch) {
    let conn = rusqlite::Connection::open(s.db()).unwrap();
    let start = Instant::now();
    while start.elapsed() < DEADLINE {
        let owner: Option<String> = conn
            .query_row(
                "SELECT owner FROM leases WHERE name = 'text-index'",
                [],
                |r| r.get(0),
            )
            .ok();
        if owner.as_deref() == Some("holder") {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("the holder never took the lease");
}

/// Wait until the index publishes the head of the log (the holder caught up).
fn wait_for_head(index: &dyn TextIndex, store: &SqliteStore) {
    let start = Instant::now();
    while start.elapsed() < DEADLINE {
        if visible_through(index) == Some(i64::try_from(store.changes_head().unwrap()).unwrap()) {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("the holder never published the head");
}

/// The holder role of the two-process case: in a child process (`KABAN_BARRIER_HOLDER` set
/// to the scratch directory) hold lease and lock over that database until the lease is lost
/// or the process is killed. A no-op elsewhere.
#[test]
#[ignore = "a role of search_barrier's two-process case"]
fn search_barrier_holder_role() {
    let Ok(dir) = std::env::var("KABAN_BARRIER_HOLDER") else {
        return;
    };
    let s = Scratch(std::path::PathBuf::from(dir));
    let store = store(&s);
    let index = adapter(&s);
    let projector = Projector::new(&index, &store, &store, "holder", HOLDER_TTL_S);
    // a writer's own post-commit projection holds the lease for a moment (spec 05 OWN-M): the
    // role retries its election like the serving loop does
    let progress = loop {
        let progress = projector.hold(&AtomicBool::new(false)).unwrap();
        if !matches!(progress.halt, Some(Halt::LeaseHeld | Halt::WriterHeld)) {
            break progress;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    std::mem::forget(s); // the parent owns the directory
    assert_eq!(progress.halt, Some(Halt::LeaseLost), "{progress:?}");
}

/// The lock holder, in a child process.
struct Holder(Child);

impl Holder {
    fn start(s: &Scratch) -> Self {
        let child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "search_barrier_holder_role", "--ignored"])
            .env("KABAN_BARRIER_HOLDER", &s.0)
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        Self(child)
    }
    fn signal(&self, signal: &str) {
        let status = Command::new("kill")
            .args([signal, &self.0.id().to_string()])
            .status()
            .unwrap();
        assert!(status.success(), "kill {signal}");
    }
}

impl Drop for Holder {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A Claude Code `PostToolUseFailure` envelope for a shell command.
fn failure_envelope(command: &str) -> Value {
    json!({
        "hook_event_name": "PostToolUseFailure",
        "session_id": "s-barrier",
        "tool_name": "Bash",
        "tool_input": {"command": command},
        "error": "exit status 1",
    })
}

fn hook(s: &Scratch, envelope: &Value) -> Output {
    let mut child = kaban(s)
        .args([
            "hook",
            "post-tool-use-failure",
            "--host",
            "claude-code",
            "--stdin-json",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(envelope.to_string().as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn a_stopped_lock_holder_makes_committed_writes_pending_on_every_transport() {
    let s = Scratch::new("kaban-barrier", "pending");
    // FTS5 first: the database exists, then it is marked and its index built
    let seeded = run(&s, &["remember", "Fact", "a seeded note"]);
    assert_eq!(seeded.status.code(), Some(0), "{seeded:?}");
    mark_tantivy(&s);
    let store = store(&s);
    let index = adapter(&s);
    index.rebuild().unwrap();

    // a lone writer publishes its own commit inside the barrier: committed, exit 0, and the
    // note is a recall hit through the adapter at once
    let out = run(&s, &["remember", "Fact", "quokka by the lone writer"]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(document(&out)["receipt"]["outcome"], json!("committed"));
    let found = document(&run(&s, &["recall", "quokka lone writer"]));
    assert_eq!(
        found["results"].as_array().map(Vec::len),
        Some(1),
        "{found}"
    );

    // a stopped holder: lease expired, lock still held. The holder is known to hold the lock
    // once it owns the lease and published a row this process wrote beside it.
    let holder = Holder::start(&s);
    wait_for_holder(&s);
    store
        .remember(
            kaban_core::NewNode {
                kind: "Fact".into(),
                text: "quokka published by the holder".into(),
                ..Default::default()
            },
            &[],
            &kaban_core::WriteCtx::new(
                "v1",
                kaban_core::WorkspaceId::new(WORKSPACE),
                kaban_core::Actor::Agent,
            ),
        )
        .unwrap();
    wait_for_head(&index, &store);
    holder.signal("-STOP");
    std::thread::sleep(Duration::from_secs(u64::from(HOLDER_TTL_S) + 1));

    // CLI: committed and pending, status 6, the document still printed whole
    let out = run(&s, &["remember", "Fact", "quokka while stopped"]);
    assert_eq!(out.status.code(), Some(6), "{out:?}");
    let doc = document(&out);
    assert_eq!(doc["status"], json!("created"), "{doc}");
    assert_eq!(
        doc["receipt"]["outcome"],
        json!("committed_not_yet_searchable"),
        "{doc}"
    );
    assert!(doc["receipt"]["seq"].as_u64().is_some_and(|seq| seq > 0));

    // a `set -e` caller handles 6 explicitly and never replays
    let script = r#"set -e
status=0
"$KABAN" --db "$DB" --workspace default remember Fact "quokka from a script" > "$OUT" || status=$?
case "$status" in
  0) echo committed ;;
  6) echo handled-pending ;;
  *) echo "failed $status"; exit "$status" ;;
esac
"#;
    let shell = Command::new("sh")
        .arg("-c")
        .arg(script)
        .env("KABAN_HOME", s.home())
        .env("KABAN", env!("CARGO_BIN_EXE_kaban"))
        .env("DB", s.db())
        .env("OUT", s.0.join("script.json"))
        .output()
        .unwrap();
    assert_eq!(shell.status.code(), Some(0), "{shell:?}");
    assert_eq!(
        String::from_utf8_lossy(&shell.stdout).trim(),
        "handled-pending"
    );

    // MCP: the same receipt object, no error
    let mut srv = Server::start_with(&s, &["--workspace", WORKSPACE], DEADLINE).0;
    let id = srv.request(
        "tools/call",
        &json!({"name": "remember", "arguments": {"kind": "Fact", "text": "quokka over mcp"}}),
    );
    let reply = srv.response(id, DEADLINE).unwrap()["result"].clone();
    srv.finish();
    assert_ne!(reply["isError"], json!(true), "{reply}");
    assert_eq!(
        reply["structuredContent"]["receipt"]["outcome"],
        json!("committed_not_yet_searchable"),
        "{reply}"
    );

    // a hook: exit 0 kept, the pending status said on stderr, never claimed searchable
    let started = Instant::now();
    let out = hook(&s, &failure_envelope("cargo test -p quokka"));
    let elapsed = started.elapsed();
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("not yet searchable, do not replay"),
        "after {elapsed:?}: {out:?}"
    );

    // a pre-commit rollback keeps its own status and the rejected receipt
    let out = run(&s, &["forget", UNKNOWN_ID]);
    assert_eq!(out.status.code(), Some(3), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("\"outcome\":\"rejected\""));

    // the lock dies with the holder: the pending writes are published as they stand (never
    // replayed) and a new write is committed again
    holder.signal("-KILL");
    drop(holder);
    let out = run(&s, &["remember", "Fact", "quokka after the kill"]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(document(&out)["receipt"]["outcome"], json!("committed"));
    let found = document(&run(&s, &["recall", "quokka"]));
    let texts: Vec<&str> = found["results"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|card| card["text"].as_str())
        .collect();
    for expected in [
        "quokka while stopped",
        "quokka from a script",
        "quokka over mcp",
        "quokka after the kill",
    ] {
        assert_eq!(
            texts.iter().filter(|t| **t == expected).count(),
            1,
            "{expected} once: {texts:?}"
        );
    }
}

/// Run `args` with `body` on stdin (`--stdin-json` commands).
fn run_with_stdin(s: &Scratch, cwd: &std::path::Path, args: &[&str], body: &Value) -> Output {
    let mut child = kaban(s)
        .current_dir(cwd)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(body.to_string().as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

/// The texts of a recall's cards.
fn recalled(s: &Scratch, query: &str) -> Vec<String> {
    document(&run(s, &["recall", query]))["results"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|card| card["text"].as_str().map(str::to_owned))
        .collect()
}

/// Every public write path runs the barrier under activation (T2 review t2-3, SS-10): an
/// ordinary `ingest` (the CLI and MCP `index_docs` entry point) and an `accept` return the
/// typed receipt, and their rows are recall hits at once (a lone writer publishes inside
/// the barrier) or the receipt says pending. Before, both returned without a receipt and the
/// rows stayed unsearchable indefinitely.
#[test]
fn an_ingest_and_an_acceptance_carry_a_receipt_and_are_searchable_under_activation() {
    let s = Scratch::new("kaban-barrier", "entry-points");
    let seeded = run(&s, &["remember", "Fact", "a seeded note"]);
    assert_eq!(seeded.status.code(), Some(0), "{seeded:?}");
    mark_tantivy(&s);
    adapter(&s).rebuild().unwrap();
    let root = s.0.join("repo");
    std::fs::create_dir_all(root.join(".git")).unwrap();
    std::fs::write(
        root.join("alpha.md"),
        "# Alpha\n\n## Storage\n\nThe nightjar service uses WAL mode for concurrent readers.\n",
    )
    .unwrap();

    let out = kaban(&s)
        .current_dir(&root)
        .args(["ingest", "alpha.md"])
        .output()
        .unwrap();
    let doc = document(&out);
    let outcome = doc["receipt"]["outcome"].as_str().map(str::to_owned);
    match (out.status.code(), outcome.as_deref()) {
        (Some(0), Some("committed")) => {
            // the ingested rows (the document's section, its chunk) are hits at once
            let texts = recalled(&s, "nightjar WAL");
            assert!(!texts.is_empty(), "an ingested row is a hit once committed");
        }
        (Some(6), Some("committed_not_yet_searchable")) => {}
        other => panic!("a typed receipt and its status: {other:?}: {doc}"),
    }

    let created = document(&run(&s, &["remember", "Convention", "ship on mondays"]));
    let id = created["id"].as_str().unwrap().to_owned();
    let out = run_with_stdin(
        &s,
        &root,
        &["accept", "--stdin-json"],
        &json!({"id": id, "reason": "agreed in review", "path_scope": ["src/"]}),
    );
    let doc = document(&out);
    let outcome = doc["receipt"]["outcome"].as_str().map(str::to_owned);
    match (out.status.code(), outcome.as_deref()) {
        (Some(0), Some("committed")) => {
            let texts = recalled(&s, "ship mondays");
            assert!(
                texts.iter().any(|t| t == "ship on mondays"),
                "the accepted record is a hit once committed: {texts:?}"
            );
        }
        (Some(6), Some("committed_not_yet_searchable")) => {}
        other => panic!("a typed receipt and its status: {other:?}: {doc}"),
    }
}

/// The budget of one write under activation (fix11: a lone writer or a serving process
/// publishes its own commit well inside the 5 s barrier; before, every write slept to the
/// deadline).
const ONE_WRITE: Duration = Duration::from_secs(2);
/// The budget of fifty writes in a loop.
const FIFTY_WRITES: Duration = Duration::from_secs(10);

/// A database the production open activates (SS-13b, the path users hit): one note on the
/// latest schema and the adapter's projection built beside it at the canonical head, so the
/// next binary open serves from the adapter.
fn seed_installed(s: &Scratch) {
    let seeded = run(s, &["remember", "Fact", "a seeded note"]);
    assert_eq!(seeded.status.code(), Some(0), "{seeded:?}");
    adapter(s).rebuild().unwrap();
}

/// Median and p95 of per-write latencies, printed for the hand-back.
fn report(label: &str, mut samples: Vec<Duration>) {
    samples.sort_unstable();
    let n = samples.len();
    let median = samples[n / 2];
    let p95 = samples[(n * 95 / 100).min(n - 1)];
    let total: Duration = samples.iter().sum();
    eprintln!("{label}: n={n} median={median:?} p95={p95:?} total={total:?}");
}

/// A lone CLI writer under activation (SS-10, SS-21; fix11): the production open activates
/// the installed index, the first `remember` is `committed` and a recall hit within 2 s,
/// and fifty more land within 10 s in all. Before the fix each write slept the whole 5 s
/// barrier.
#[test]
fn a_lone_cli_writer_publishes_each_write_well_inside_the_barrier() {
    let s = Scratch::new("kaban-barrier", "cli-latency");
    seed_installed(&s);

    let started = Instant::now();
    let out = run(&s, &["remember", "Fact", "quokka first under activation"]);
    let first = started.elapsed();
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(document(&out)["receipt"]["outcome"], json!("committed"));
    assert!(
        visible_through(&adapter(&s)).is_some_and(|seq| seq > 0),
        "the writer published its own commits"
    );
    let texts = recalled(&s, "quokka first activation");
    assert!(
        texts.iter().any(|t| t == "quokka first under activation"),
        "{texts:?}"
    );
    assert!(first < ONE_WRITE, "the first write took {first:?}");

    let mut samples = Vec::new();
    let loop_started = Instant::now();
    for i in 0..50 {
        let text = format!("wombat note {i} by the lone writer");
        let started = Instant::now();
        let out = run(&s, &["remember", "Fact", &text]);
        samples.push(started.elapsed());
        assert_eq!(out.status.code(), Some(0), "{i}: {out:?}");
        assert_eq!(
            document(&out)["receipt"]["outcome"],
            json!("committed"),
            "{i}"
        );
    }
    let total = loop_started.elapsed();
    report("cli", samples);
    let texts = recalled(&s, "wombat note lone writer");
    assert!(!texts.is_empty(), "the loop's notes are hits");
    assert!(total < FIFTY_WRITES, "fifty writes took {total:?}");
}

/// A serving process under activation (the harness drives `kaban --db ...` over MCP): the
/// mode-B loop or the drain publishes each `remember` inside the barrier; one write within
/// 2 s, fifty within 10 s, every receipt `committed`.
#[test]
fn a_serving_process_publishes_each_write_well_inside_the_barrier() {
    let s = Scratch::new("kaban-barrier", "mcp-latency");
    seed_installed(&s);
    let mut srv = Server::start_with(&s, &["--workspace", WORKSPACE], DEADLINE).0;
    let mut remember = |text: &str| -> (Value, Duration) {
        let started = Instant::now();
        let id = srv.request(
            "tools/call",
            &json!({"name": "remember", "arguments": {"kind": "Fact", "text": text}}),
        );
        let reply = srv.response(id, DEADLINE).unwrap()["result"].clone();
        (reply, started.elapsed())
    };

    let (reply, first) = remember("quokka first over mcp");
    assert_ne!(reply["isError"], json!(true), "{reply}");
    assert_eq!(
        reply["structuredContent"]["receipt"]["outcome"],
        json!("committed"),
        "{reply}"
    );
    assert!(first < ONE_WRITE, "the first write took {first:?}");

    let mut samples = Vec::new();
    let loop_started = Instant::now();
    for i in 0..50 {
        let (reply, took) = remember(&format!("wombat note {i} over mcp"));
        samples.push(took);
        assert_eq!(
            reply["structuredContent"]["receipt"]["outcome"],
            json!("committed"),
            "{i}: {reply}"
        );
    }
    let total = loop_started.elapsed();
    report("mcp", samples);
    srv.finish();
    assert!(
        visible_through(&adapter(&s)).is_some_and(|seq| seq > 0),
        "the writer published its own commits"
    );
    let texts = recalled(&s, "quokka first mcp");
    assert!(
        texts.iter().any(|t| t == "quokka first over mcp"),
        "{texts:?}"
    );
    assert!(total < FIFTY_WRITES, "fifty writes took {total:?}");
}
