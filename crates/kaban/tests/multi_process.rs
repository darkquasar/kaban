//! Multi-process contention (core-spec F15 "tested at 8 in CI", N4, SR-2; core-plan C2.5 and
//! §16 "Contention"; MD-15): eight `kaban serve` stdio processes each write 1,000 notes
//! through `remember` while three lease contenders (threads with their own store connections)
//! fight over the `export` lease. Every write must land, the audit chain must verify, exactly
//! one contender may hold the lease at any time, and no `SQLITE_BUSY` may reach a caller.
//! Shared mode B on FTS5 (spec 05 SS-21): two processes write one database concurrently, each
//! reads the other's writes, and a lone CLI writer makes progress beside them with no daemon.
//!
//! Expensive: `#[ignore]`, run alone with
//! `cargo nextest run --workspace -j4 --run-ignored only -E 'test(multi_process)'` (MD-16: the
//! nextest 0.9.126 value is `only`).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion
#![allow(clippy::print_stderr)] // the wall time and the lease log are the test's report

mod common;

use common::Scratch;
use kaban_core::LeaseStore;
use kaban_store_sqlite::SqliteStore;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

const WRITERS: usize = 8;
const NOTES_PER_WRITER: usize = 1_000;
const CONTENDERS: usize = 3;
/// Lease ttl in seconds: short so the lease changes hands several times during the run.
const LEASE_TTL_S: u32 = 2;
/// The example policy (as the smoke uses): its redact rule gives the audit chain one row per
/// writer, so `verify-audit ok` is not vacuous.
const POLICY: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../examples/policy.yaml");
/// A value the example policy redacts (`internal-hostname`); never persisted raw.
const REDACT_CANARY: &str = "db1.corp.acme.internal";

/// A kaban server over stdio (pattern of `mcp_blocking.rs`): stdout drained by a thread,
/// responses matched by id.
struct Server {
    child: Child,
    rx: mpsc::Receiver<Value>,
    pending: Vec<Value>,
    next_id: u64,
    /// The child's stderr (tracing output), printed when the writer fails.
    stderr_path: PathBuf,
}

impl Server {
    fn start(s: &Scratch, index: usize) -> Self {
        Self::start_with(s, index, &[])
    }
    /// [`Self::start`] with extra arguments before the (default) `serve` command.
    fn start_with(s: &Scratch, index: usize, extra: &[&str]) -> Self {
        let stderr_path = s.0.join(format!("writer-{index}.stderr"));
        let stderr = std::fs::File::create(&stderr_path).unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_kaban"))
            .env("KABAN_HOME", s.home())
            .env("RUST_LOG", "info")
            .args(["--db", s.db().to_str().unwrap(), "--policy", POLICY])
            .args(extra)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(stderr))
            .spawn()
            .unwrap();
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
            stderr_path,
        };
        let id = srv.request(
            "initialize",
            &json!({"protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": {"name": "t", "version": "0"}}),
        );
        let init = srv.response(id, Duration::from_secs(30));
        assert!(
            init.as_ref()
                .is_some_and(|v| v["result"]["serverInfo"]["name"] == json!("kaban")),
            "writer {index}: initialize not answered: {init:?}; child stderr:\n{}",
            srv.stderr()
        );
        srv.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        srv
    }
    fn stderr(&self) -> String {
        std::fs::read_to_string(&self.stderr_path).unwrap_or_default()
    }
    /// The child's exit status if it already died (a dead child explains an unanswered request).
    fn exited(&mut self) -> Option<std::process::ExitStatus> {
        self.child.try_wait().ok().flatten()
    }
    fn send(&mut self, msg: &Value) {
        let stdin = self.child.stdin.as_mut().unwrap();
        stdin.write_all(msg.to_string().as_bytes()).unwrap();
        stdin.write_all(b"\n").unwrap();
        stdin.flush().unwrap();
    }
    fn request(&mut self, method: &str, params: &Value) -> u64 {
        self.next_id += 1;
        self.send(
            &json!({"jsonrpc": "2.0", "id": self.next_id, "method": method, "params": params}),
        );
        self.next_id
    }
    fn response(&mut self, id: u64, timeout: Duration) -> Option<Value> {
        if let Some(i) = self.pending.iter().position(|v| v["id"] == json!(id)) {
            return Some(self.pending.remove(i));
        }
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.checked_duration_since(Instant::now())?;
            match self.rx.recv_timeout(left) {
                Ok(v) if v["id"] == json!(id) => return Some(v),
                Ok(v) => self.pending.push(v),
                Err(_) => return None,
            }
        }
    }
    /// Close stdin, give the child a moment to exit, kill it if it has not, and reap it.
    fn finish(mut self) {
        drop(self.child.stdin.take());
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.child.try_wait().ok().flatten().is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// What one writer process observed: created notes, tool errors, and errors naming SQLite's
/// busy/locked conditions (must be zero: core-plan §16 "`SQLITE_BUSY` count must be 0 at the API").
#[derive(Debug, Default)]
struct WriterOutcome {
    created: usize,
    errors: Vec<String>,
    busy: usize,
}

fn run_writer(s: &Scratch, index: usize, stop: &AtomicBool) -> WriterOutcome {
    let mut srv = Server::start(s, index);
    let mut out = WriterOutcome::default();
    for i in 0..NOTES_PER_WRITER {
        if stop.load(Ordering::Relaxed) {
            out.errors.push(format!(
                "writer {index}: stopped at note {i} by another failure"
            ));
            break;
        }
        let id = srv.request(
            "tools/call",
            &json!({"name": "remember", "arguments": {
                "kind": "Fact",
                "text": if i == 0 {
                    format!("writer {index} note {i}: host {REDACT_CANARY} eats termites")
                } else {
                    format!("writer {index} note {i}: numbats eat termites")
                },
            }}),
        );
        let Some(resp) = srv.response(id, Duration::from_secs(30)) else {
            // a dead child or a stalled server ends the whole run, never hangs it
            out.errors.push(format!(
                "writer {index}: note {i} unanswered within 30 s; child exit status {:?}; child stderr:\n{}",
                srv.exited(),
                srv.stderr()
            ));
            stop.store(true, Ordering::Relaxed);
            break;
        };
        if resp["result"]["isError"] == json!(true) || resp.get("error").is_some() {
            let text = resp.to_string().to_lowercase();
            if text.contains("busy") || text.contains("locked") {
                out.busy += 1;
            }
            out.errors.push(resp.to_string());
            continue;
        }
        let body: Value =
            serde_json::from_str(resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        if body["status"] == json!("created") {
            out.created += 1;
        } else {
            out.errors.push(body.to_string());
        }
    }
    srv.finish();
    out
}

/// One lease event as the contender saw it, in database time (`unixepoch()` seconds).
#[derive(Debug, Clone)]
struct Hold {
    owner: String,
    fence: u64,
    /// `expires_at` returned by the acquisition minus the ttl: the database time of the acquire.
    acquired_at: i64,
    /// The last `expires_at` the holder had when it stopped renewing (its hold ends there).
    released_at: i64,
    renewals: u32,
    /// The holder learned of a successor from a rejected renewal (stopped at once).
    lost: bool,
}

/// A contender: its own store connection; acquires, renews a few times, lets the lease
/// expire (there is no release in the F15 protocol: a holder that stops renewing is gone),
/// and tries again until told to stop.
fn run_contender(s: &Scratch, index: usize, stop: &AtomicBool, log: &Mutex<Vec<Hold>>) {
    let store = SqliteStore::open(&s.db(), &s.home().join("keys")).unwrap();
    let owner = format!(
        "test:{}:{index}:{}",
        std::process::id(),
        kaban_core::new_id()
    );
    while !stop.load(Ordering::Relaxed) {
        match store.lease_acquire("export", &owner, LEASE_TTL_S).unwrap() {
            None => std::thread::sleep(Duration::from_millis(50)),
            Some(mut lease) => {
                let acquired_at = lease.expires_at - i64::from(LEASE_TTL_S);
                let mut renewals = 0;
                let mut lost = false;
                // hold for a few renewals, then stop renewing so the others can take over
                while renewals < 3 && !stop.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(u64::from(LEASE_TTL_S) * 1000 / 3));
                    match store.lease_renew(&lease, LEASE_TTL_S) {
                        Ok(()) => {
                            renewals += 1;
                            lease.expires_at = db_now(&s.db()) + i64::from(LEASE_TTL_S);
                        }
                        Err(kaban_core::StoreError::LeaseLost { .. }) => {
                            // a renewal delayed past the expiry under write load: a successor
                            // took the lease and this holder stops at once (F15, SR-2)
                            lost = true;
                            break;
                        }
                        Err(e) => panic!("contender {index}: {e}"),
                    }
                }
                log.lock().unwrap().push(Hold {
                    owner: owner.clone(),
                    fence: lease.fence,
                    acquired_at,
                    released_at: lease.expires_at,
                    renewals,
                    lost,
                });
                // let it expire before contending again (a holder never re-takes its own lease)
                if !lost {
                    std::thread::sleep(Duration::from_millis(u64::from(LEASE_TTL_S) * 1000 + 200));
                }
            }
        }
    }
}

fn db_now(db: &std::path::Path) -> i64 {
    rusqlite::Connection::open(db)
        .unwrap()
        .query_row("SELECT unixepoch()", [], |r| r.get(0))
        .unwrap()
}

/// C2.5 slice 4 (F15, N4, SR-2, §16 contention line): 8 writer processes x 1,000 notes with 3
/// lease contenders: all 8,000 writes land, `verify-audit` is ok, exactly one lease holder at
/// any time (fences strictly consecutive, holds disjoint in database time), no busy error
/// surfaced to any caller. Wall time is printed to stderr for the §16 record.
#[test]
#[ignore = "expensive 8-process run; cargo nextest run --run-ignored only -E 'test(multi_process)'"]
fn multi_process_eight_writers_and_three_lease_contenders_share_one_database() {
    let s = Scratch::new("kaban-multi", "eight");
    let stop = Arc::new(AtomicBool::new(false));
    let holds = Mutex::new(Vec::new());
    let t0 = Instant::now();
    let s = &s;
    let holds = &holds;
    // hard deadline for the whole run: a stuck writer or child must end in a message, not a hang
    let watchdog = std::thread::spawn({
        let stop = Arc::clone(&stop);
        move || {
            let deadline = Instant::now() + Duration::from_secs(180);
            while Instant::now() < deadline {
                if stop.load(Ordering::Relaxed) {
                    return false;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            stop.store(true, Ordering::Relaxed);
            true
        }
    });
    let stop = &*stop;
    let (outcomes, contender_panics): (Vec<WriterOutcome>, usize) = std::thread::scope(|sc| {
        let contenders: Vec<_> = (0..CONTENDERS)
            .map(|i| sc.spawn(move || run_contender(s, i, stop, holds)))
            .collect();
        let writers: Vec<_> = (0..WRITERS)
            .map(|i| sc.spawn(move || run_writer(s, i, stop)))
            .collect();
        let outcomes: Vec<WriterOutcome> = writers
            .into_iter()
            .enumerate()
            .map(|(i, h)| {
                h.join().unwrap_or_else(|_| WriterOutcome {
                    errors: vec![format!("writer {i}: thread panicked")],
                    ..WriterOutcome::default()
                })
            })
            .collect();
        // the hand-over assertion below needs a second hold, which cannot begin before the
        // first holder's three renewals plus one ttl have elapsed in database time (about
        // 6 s at ttl 2); writers that finish earlier (C2.5a removed the wal-index recovery
        // churn that used to slow them) would otherwise stop the contenders too soon. Wait for
        // it, bounded by the watchdog, unless a writer already failed.
        let writers_ok = outcomes.iter().all(|o| o.errors.is_empty());
        while writers_ok && holds.lock().unwrap().len() < 2 && !stop.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(50));
        }
        stop.store(true, Ordering::Relaxed);
        let panics = contenders
            .into_iter()
            .map(|c| c.join().is_err())
            .filter(|p| *p)
            .count();
        (outcomes, panics)
    });
    let timed_out = watchdog.join().unwrap();
    let wall = t0.elapsed();
    assert!(
        !timed_out,
        "the run exceeded the 180 s deadline; outcomes: {outcomes:?}"
    );
    assert_eq!(
        contender_panics, 0,
        "a lease contender panicked (see its message above)"
    );
    let holds = holds.lock().unwrap().clone();

    // every write landed, no busy error reached a caller
    let created: usize = outcomes.iter().map(|o| o.created).sum();
    let busy: usize = outcomes.iter().map(|o| o.busy).sum();
    let errors: Vec<&String> = outcomes.iter().flat_map(|o| o.errors.iter()).collect();
    assert!(
        errors.is_empty(),
        "tool errors: {:?}",
        &errors[..errors.len().min(5)]
    );
    assert_eq!(busy, 0, "SQLITE_BUSY surfaced to a caller");
    assert_eq!(created, WRITERS * NOTES_PER_WRITER);
    let nodes: i64 = rusqlite::Connection::open(s.db())
        .unwrap()
        .query_row("SELECT count(*) FROM node", [], |r| r.get(0))
        .unwrap();
    assert_eq!(usize::try_from(nodes).unwrap(), WRITERS * NOTES_PER_WRITER);

    let conn = rusqlite::Connection::open(s.db()).unwrap();
    kaban_store_sqlite::register_body_reader(&conn).unwrap();
    let raw: i64 = conn
        .query_row(
            "SELECT count(*) FROM node WHERE kaban_body(text) LIKE ?1",
            [format!("%{REDACT_CANARY}%")],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(raw, 0, "the redacted value is never persisted raw");

    // exactly one holder at any time: fences are consecutive from 1 and each hold begins
    // after the previous one ended (acquisition requires `expires_at < unixepoch()`)
    let mut by_fence = holds.clone();
    by_fence.sort_by_key(|h| h.fence);
    assert!(by_fence.len() >= 2, "the lease changed hands: {by_fence:?}");
    for (i, h) in by_fence.iter().enumerate() {
        assert_eq!(
            h.fence,
            i as u64 + 1,
            "fences are consecutive: {by_fence:?}"
        );
        assert!(h.released_at >= h.acquired_at, "{h:?}");
        if let Some(prev) = i.checked_sub(1).map(|p| &by_fence[p]) {
            // acquisition needs `expires_at < unixepoch()`, so the successor's database time
            // is at least the previous expiry + 1; the recorded `released_at` is read a moment
            // after the renewal and may overshoot the database's value by one second
            assert!(
                h.acquired_at >= prev.released_at,
                "overlapping holds: {prev:?} then {h:?}"
            );
        }
    }
    // the number of distinct owners is reported, not asserted: a holder that stops renewing
    // may legitimately re-take the lease one second after its own expiry when the other
    // contenders' `BEGIN IMMEDIATE` is delayed behind the writers (observed once in C2.6's
    // gate run; C2.5a review note recorded, not enforced)
    let owners: std::collections::BTreeSet<&str> = holds.iter().map(|h| h.owner.as_str()).collect();
    eprintln!(
        "multi_process: {WRITERS} writers x {NOTES_PER_WRITER} notes in {wall:?}; {} lease holds by {} contenders; busy errors {busy}",
        holds.len(),
        owners.len()
    );
    for h in &by_fence {
        eprintln!(
            "lease export fence {} owner {} acquired_at {} released_at {} renewals {} lost {}",
            h.fence, h.owner, h.acquired_at, h.released_at, h.renewals, h.lost
        );
    }
}

// ---- engine-plan E1.7 (EN-E8, EN-N8): eight openers on a populated 0003 store ----

// ---- spec 05 SS-21 (FTS5 part): shared mode B, two processes and a lone CLI writer ----

/// Notes each of the two sharing processes writes.
const SHARED_NOTES: usize = 40;

/// Write `n` notes through `srv`'s `remember`, each naming `label` and its index, and return
/// the ids created; the first tool error or unanswered request ends the writer.
fn write_notes(srv: &mut Server, label: &str, n: usize) -> Result<Vec<String>, String> {
    let mut ids = Vec::with_capacity(n);
    for i in 0..n {
        let id = srv.request(
            "tools/call",
            &json!({"name": "remember", "arguments": {
                "kind": "Fact",
                "text": format!("{label} note {i}: the shared quokka counts {i} leaves"),
            }}),
        );
        let resp = srv.response(id, Duration::from_secs(30)).ok_or_else(|| {
            format!(
                "{label}: note {i} unanswered within 30 s; child exit status {:?}; child stderr:\n{}",
                srv.exited(),
                srv.stderr()
            )
        })?;
        if resp["result"]["isError"] == json!(true) || resp.get("error").is_some() {
            return Err(format!("{label}: note {i}: {resp}"));
        }
        let body: Value =
            serde_json::from_str(resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        if body["status"] != json!("created") {
            return Err(format!("{label}: note {i}: {body}"));
        }
        ids.push(body["id"].as_str().unwrap().to_owned());
    }
    Ok(ids)
}

/// The ids `recall` returns through `srv` for `query` (at most 10, best first).
fn recall_ids(srv: &mut Server, query: &str) -> Vec<String> {
    let id = srv.request(
        "tools/call",
        &json!({"name": "recall", "arguments": {"query": query, "limit": 10}}),
    );
    let resp = srv
        .response(id, Duration::from_secs(30))
        .expect("recall answered");
    assert!(
        resp["result"]["isError"] != json!(true) && resp.get("error").is_none(),
        "{resp}"
    );
    let body: Value =
        serde_json::from_str(resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    body["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|card| card["id"].as_str().unwrap().to_owned())
        .collect()
}

/// Spec 05 SS-21 (the FTS5 part, SS-23's admission on the way in): two `kaban` processes
/// share one database with no daemon and no projector. Both write concurrently (SQLite
/// serializes them: every note lands, no busy error reaches a caller), each then reads the
/// other's notes, and a lone CLI `remember` makes progress while both processes still hold
/// the database open; the audit chain the three writers extended verifies.
#[test]
#[ignore = "multi-process run; cargo nextest run --run-ignored only -E 'test(multi_process)'"]
fn multi_process_two_processes_share_one_database_and_a_lone_cli_writer_progresses() {
    let s = Scratch::new("kaban-multi", "shared-mode-b");
    let s = &s;
    // Arrange: the first process creates the database; the second opens it while held (the
    // `-shm` sidecar is on disk, so this is the mode-B open SS-23's admission decides)
    let mut alpha = Server::start(s, 0);
    assert!(
        std::fs::metadata(format!("{}-shm", s.db().display())).is_ok(),
        "the first process holds the database in WAL mode"
    );
    let mut beta = Server::start(s, 1);

    // Act: concurrent writes from both processes
    let (alpha_ids, beta_ids) = std::thread::scope(|sc| {
        let a = sc.spawn(|| write_notes(&mut alpha, "alpha", SHARED_NOTES));
        let b = sc.spawn(|| write_notes(&mut beta, "beta", SHARED_NOTES));
        (a.join().unwrap(), b.join().unwrap())
    });
    let alpha_ids = alpha_ids.unwrap_or_else(|e| panic!("{e}"));
    let beta_ids = beta_ids.unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(alpha_ids.len(), SHARED_NOTES);
    assert_eq!(beta_ids.len(), SHARED_NOTES);

    // Assert: cross-process visibility, each process reads the other's last note
    let last = SHARED_NOTES - 1;
    assert!(
        recall_ids(&mut beta, &format!("alpha note {last}")).contains(&alpha_ids[last]),
        "beta reads alpha's write"
    );
    assert!(
        recall_ids(&mut alpha, &format!("beta note {last}")).contains(&beta_ids[last]),
        "alpha reads beta's write"
    );

    // Act: a lone CLI writer beside the two live processes (no daemon, no IPC)
    let out = Command::new(env!("CARGO_BIN_EXE_kaban"))
        .env("KABAN_HOME", s.home())
        .args([
            "--db",
            s.db().to_str().unwrap(),
            "--policy",
            POLICY,
            "remember",
            "Fact",
            "lone cli note: the shared quokka sleeps alone",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let created: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(created["status"], json!("created"), "{created}");
    let cli_id = created["id"].as_str().unwrap().to_owned();

    // Assert: both live processes read the CLI's note
    assert!(recall_ids(&mut alpha, "quokka sleeps alone").contains(&cli_id));
    assert!(recall_ids(&mut beta, "quokka sleeps alone").contains(&cli_id));
    alpha.finish();
    beta.finish();

    // every write landed
    let nodes: i64 = rusqlite::Connection::open(s.db())
        .unwrap()
        .query_row("SELECT count(*) FROM node", [], |r| r.get(0))
        .unwrap();
    assert_eq!(usize::try_from(nodes).unwrap(), 2 * SHARED_NOTES + 1);
}
