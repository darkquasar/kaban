//! Reclamation under readers and the scheduled checkpoint (spec 11 T6 Steps 2 and 3, PC-8,
//! KER-06, KER-10): a persistent reader keeps what it pins and lets it go once it moves on, a
//! retired extent is destroyed only once the WAL is backfilled through the commit that retired
//! it, and checkpoints complete while other processes query continuously.
//!
//! Runs with the `failpoints` feature (the test class lives behind it).
//!
//! Every wait is bounded: the threads and the child process stop by themselves after
//! [`STEP`] or [`ROLE_DEADLINE`], and the test kills the child however it ends.

#![cfg(feature = "failpoints")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::Scratch;
use kaban_core::query_budget::QueryBudget;
use kaban_store_sqlite::SqliteStore;
use kaban_store_sqlite::catalog_testing;
use std::io::{BufRead, Read, Write};
use std::os::unix::fs::FileExt;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

/// The bound of any one wait on another thread or process.
const STEP: Duration = Duration::from_secs(10);
/// How long the querying child may run before it stops by itself.
const ROLE_DEADLINE: Duration = Duration::from_secs(30);
/// The length of every extent.
const LEN: usize = 4096;
/// The database the querying child opens (set only for the child process).
const QUERY_DB: &str = "KABAN_T6_QUERY_DB";
/// The prefix of the querying child's replies (the test harness writes to stdout too).
const REPLY: &str = "query-reply ";

fn budget() -> QueryBudget {
    QueryBudget::new(u64::MAX, Some(STEP))
}

/// `LEN` bytes of the nonzero `fill`.
fn pattern(fill: u8) -> Vec<u8> {
    vec![fill; LEN]
}

/// A reader on its own store of `s`'s database that re-pins every 10 ms (the new pin taken
/// before the old one drops) and checks every extent its pin holds reads as one nonzero byte
/// repeated. It stops when told, or by itself after [`ROLE_DEADLINE`].
struct Repinner {
    stop: Arc<AtomicBool>,
    /// The generation of the reader's current pin.
    pinned: Arc<AtomicU64>,
    done: mpsc::Receiver<Result<u64, String>>,
}

impl Repinner {
    fn start(s: &Scratch) -> Self {
        let (stop, pinned) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicU64::new(0)));
        let (tx, done) = mpsc::channel();
        let (db, keys, halt, at) = (s.db(), s.keys(), Arc::clone(&stop), Arc::clone(&pinned));
        std::thread::spawn(move || {
            let store = SqliteStore::open(&db, &keys).unwrap();
            let started = Instant::now();
            let mut pins = 0_u64;
            let mut held = None;
            let outcome = loop {
                if halt.load(Ordering::Acquire) || started.elapsed() > ROLE_DEADLINE {
                    break Ok(pins);
                }
                let pin = catalog_testing::pin(&store, &budget()).unwrap();
                let bad = pin.view().extents().into_iter().find(|id| {
                    let bytes = pin.view().read(*id).unwrap().unwrap();
                    bytes[0] == 0 || bytes.iter().any(|b| *b != bytes[0])
                });
                if let Some(id) = bad {
                    break Err(format!("pin {pins} read a zeroed or torn extent {id}"));
                }
                at.store(pin.view().generation(), Ordering::Release);
                held = Some(pin);
                pins += 1;
                std::thread::sleep(Duration::from_millis(10));
            };
            drop(held);
            let _ = tx.send(outcome);
        });
        Self {
            stop,
            pinned,
            done,
        }
    }

    /// Wait (bounded) until the reader's pin is at `generation` or later.
    fn wait_pinned_at(&self, generation: u64) {
        let started = Instant::now();
        while self.pinned.load(Ordering::Acquire) < generation {
            assert!(
                started.elapsed() < STEP,
                "the reader never re-pinned at {generation}"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Stop the reader and return how many pins it took.
    fn stop(self) -> u64 {
        self.stop.store(true, Ordering::Release);
        self.done
            .recv_timeout(STEP)
            .expect("the reader did not stop in time")
            .unwrap_or_else(|e| panic!("{e}"))
    }
}

#[test]
fn a_persistent_reader_keeps_its_pinned_extent_and_lets_it_go_once_it_moves_on() {
    // Arrange: an extent pinned at g, and a reader re-pinning every 10 ms
    let s = Scratch::new("kaban-t6-reclaim", "persistent-reader");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let keeper = catalog_testing::append_and_publish(&store, &pattern(255)).unwrap();
    let doomed = catalog_testing::append_and_publish(&store, &pattern(31)).unwrap();
    let held = catalog_testing::pin(&store, &budget()).unwrap();
    let reader = Repinner::start(&s);

    // Act: retire the extent and reclaim while the pin at g lives
    let retired = catalog_testing::retire(&store, &[doomed]).unwrap();
    let while_held = catalog_testing::reclaim_retired(&store).unwrap();

    // Assert: nothing pinned is destroyed and the pin still reads it intact
    assert_eq!(while_held.reclaimed, Vec::<u64>::new(), "{while_held:?}");
    assert_eq!(while_held.pending, vec![doomed], "{while_held:?}");
    assert_eq!(held.view().read(doomed).unwrap(), Some(pattern(31)));

    // Act: the pin at g is released once the reader has moved past the retirement
    reader.wait_pinned_at(retired);
    drop(held);
    let released = catalog_testing::reclaim_retired(&store).unwrap();

    // Assert: the next reclamation destroys it, its bytes counted once, punched or zero-filled
    assert_eq!(released.reclaimed, vec![doomed], "{released:?}");
    assert_eq!(
        released.released_bytes + released.zero_filled_bytes,
        LEN as u64,
        "{released:?}"
    );
    if released.space_reclaim_unsupported {
        assert_eq!(released.released_bytes, 0, "{released:?}");
    }
    let pins = reader.stop();
    assert!(pins > 0, "the reader never pinned");
    let pin = catalog_testing::pin(&store, &budget()).unwrap();
    assert_eq!(pin.view().extents(), vec![keeper]);
}

#[test]
fn nothing_is_destroyed_until_the_wal_is_backfilled_through_the_retiring_commit() {
    // Arrange: an extent, and a raw reader whose snapshot predates its retirement, so no
    // checkpoint can backfill the WAL past it
    let s = Scratch::new("kaban-t6-reclaim", "barrier");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let doomed = catalog_testing::append_and_publish(&store, &pattern(41)).unwrap();
    let span = catalog_testing::pin(&store, &budget())
        .unwrap()
        .view()
        .span(doomed)
        .unwrap();
    let raw = rusqlite::Connection::open(s.db()).unwrap();
    raw.execute_batch("BEGIN").unwrap();
    let _: i64 = raw
        .query_row("SELECT count(*) FROM cat_extent", [], |r| r.get(0))
        .unwrap();
    catalog_testing::retire(&store, &[doomed]).unwrap();

    // Act
    let blocked = catalog_testing::reclaim_retired(&store).unwrap();

    // Assert: the barrier is pending, nothing is destroyed and the bytes are intact
    assert!(blocked.barrier_pending, "{blocked:?}");
    assert_eq!(blocked.reclaimed, Vec::<u64>::new(), "{blocked:?}");
    assert_eq!(blocked.pending, vec![doomed], "{blocked:?}");
    let container = std::fs::File::open(s.0.join("kaban.kbx")).unwrap();
    let mut bytes = vec![0_u8; usize::try_from(span.1).unwrap()];
    container.read_exact_at(&mut bytes, span.0).unwrap();
    assert_eq!(bytes, pattern(41));

    // Act: the old snapshot ends
    raw.execute_batch("COMMIT").unwrap();
    let passed = catalog_testing::reclaim_retired(&store).unwrap();

    // Assert: the barrier passes and the extent is destroyed
    assert!(!passed.barrier_pending, "{passed:?}");
    assert_eq!(passed.reclaimed, vec![doomed], "{passed:?}");
}

/// The querying role: two stores of the database each run bounded snapshot queries that hold
/// their transaction 15 ms, staggered so one is always open, until stdin closes or
/// [`ROLE_DEADLINE`] passes; prints `ready` once both run, then `queries <n>` at the end. A
/// no-op unless `KABAN_T6_QUERY_DB` is set.
#[test]
#[ignore = "the querying role of the cross-process checkpoint case"]
fn continuous_query_role() {
    let Ok(db) = std::env::var(QUERY_DB) else {
        return;
    };
    let db = std::path::PathBuf::from(db);
    let stop = Arc::new(AtomicBool::new(false));
    let count = Arc::new(AtomicU64::new(0));
    let workers: Vec<_> = (0..2_u64)
        .map(|i| {
            let (db, stop, count) = (db.clone(), Arc::clone(&stop), Arc::clone(&count));
            std::thread::spawn(move || {
                let store = SqliteStore::open(&db, &db.with_file_name("keys")).unwrap();
                std::thread::sleep(Duration::from_millis(7 * i));
                let started = Instant::now();
                while !stop.load(Ordering::Acquire) && started.elapsed() < ROLE_DEADLINE {
                    catalog_testing::with_snapshot(&store, &budget(), |view| {
                        std::thread::sleep(Duration::from_millis(15));
                        Ok(view.generation())
                    })
                    .unwrap();
                    count.fetch_add(1, Ordering::AcqRel);
                }
            })
        })
        .collect();
    let started = Instant::now();
    while count.load(Ordering::Acquire) < 2 {
        assert!(started.elapsed() < STEP, "the queries never ran");
        std::thread::sleep(Duration::from_millis(5));
    }
    println!("\n{REPLY}ready");
    std::io::stdout().flush().unwrap();
    // end of input (the test is done or gone) stops the queries
    let mut rest = Vec::new();
    let _ = std::io::stdin().read_to_end(&mut rest);
    stop.store(true, Ordering::Release);
    for w in workers {
        w.join().unwrap();
    }
    println!("\n{REPLY}queries {}", count.load(Ordering::Acquire));
}

/// A child process of this binary, killed and reaped however the test ends.
struct Supervised(std::process::Child);

impl Drop for Supervised {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn checkpoints_complete_while_another_process_queries_continuously() {
    // Arrange: a writer, and a child process whose snapshot queries always overlap
    let s = Scratch::new("kaban-t6-reclaim", "checkpoint");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    catalog_testing::append_and_publish(&store, &pattern(1)).unwrap();
    let stderr = std::fs::File::create(s.0.join("query.stderr")).unwrap();
    let mut child = Supervised(
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "continuous_query_role", "--ignored", "--nocapture"])
            .env(QUERY_DB, s.db())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(stderr)
            .spawn()
            .unwrap(),
    );
    let stdin = child.0.stdin.take().unwrap();
    let stdout = child.0.stdout.take().unwrap();
    let (lines, said) = mpsc::channel();
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(stdout)
            .lines()
            .map_while(Result::ok)
        {
            if let Some((_, reply)) = line.split_once(REPLY)
                && lines.send(reply.to_owned()).is_err()
            {
                return;
            }
        }
    });
    let ready = said
        .recv_timeout(STEP)
        .expect("the querying child never started");
    assert_eq!(ready, "ready");

    // Act: writes, each followed by a scheduled checkpoint, while the queries run
    let ticks: Vec<catalog_testing::Checkpoint> = (0..10_u8)
        .map(|i| {
            catalog_testing::append_and_publish(&store, &pattern(i + 2)).unwrap();
            catalog_testing::checkpoint_tick(&store).unwrap()
        })
        .collect();
    drop(stdin);
    let done = said
        .recv_timeout(STEP)
        .expect("the querying child did not finish");
    let queries: u64 = done
        .strip_prefix("queries ")
        .unwrap_or_else(|| panic!("not a count: {done:?}"))
        .parse()
        .unwrap();

    // Assert: every tick drained the bounded readers and backfilled the whole WAL, and the
    // queries kept running throughout
    for (i, tick) in ticks.iter().enumerate() {
        assert!(tick.drained && tick.backfilled, "tick {i}: {tick:?}");
        assert_eq!(tick.checkpointed_frames, tick.wal_frames, "tick {i}: {tick:?}");
    }
    assert!(queries > 2, "the child stopped querying: {queries}");
}
