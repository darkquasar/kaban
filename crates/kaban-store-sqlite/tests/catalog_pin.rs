//! Pins and the container (spec 11 T5 Step 4, KER-03, KER-11, INV-3, INV-5, PC-8): extents
//! appended outside the writer lock and published by a commit read back through a pin, a
//! later pin reads an extent appended after an earlier pin mapped the container, and a reader
//! racing a reclamation either retries or reads intact bytes, never a zeroed range.
//!
//! Runs with the `failpoints` feature (the test class and the failpoints live behind it).

#![cfg(feature = "failpoints")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::Scratch;
use kaban_core::StoreError;
use kaban_core::query_budget::QueryBudget;
use kaban_store_sqlite::SqliteStore;
use kaban_store_sqlite::catalog_testing::{self, IoFaults, PinView, Window};
use std::io::{BufRead, Write};
use std::process::{Child, ChildStdin, Command as Process, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

/// The deadline of every pin a test takes: acquisition and execution share it (PC-17).
const PIN_DEADLINE: Duration = Duration::from_secs(10);

fn budget() -> QueryBudget {
    QueryBudget::new(u64::MAX, Some(PIN_DEADLINE))
}

/// `len` bytes of the nonzero `fill`.
fn pattern(fill: u8, len: usize) -> Vec<u8> {
    assert_ne!(
        fill, 0,
        "a pattern is never zero: zero is what reclamation writes"
    );
    vec![fill; len]
}

#[test]
fn an_extent_appended_and_published_reads_back_through_a_later_pin() {
    // Arrange
    let s = Scratch::new("kaban-t5-pin", "append-publish");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let before = catalog_testing::pin(&store, &budget()).unwrap();

    // Act
    let first = catalog_testing::append_and_publish(&store, &pattern(7, 4096))
        .expect("the extent was not appended and published");
    let second = catalog_testing::append_and_publish(&store, &pattern(9, 100))
        .expect("the extent was not appended and published");
    let pin = catalog_testing::pin(&store, &budget()).unwrap();

    // Assert: the earlier pin sees neither, the later one both, intact and in id order
    assert_eq!(before.view().extents(), Vec::<u64>::new());
    let view = pin.view();
    assert_eq!(view.extents(), vec![first, second]);
    assert!(
        view.generation() > before.view().generation(),
        "a publication did not publish a new generation"
    );
    assert_eq!(view.read(first).unwrap(), Some(pattern(7, 4096)));
    assert_eq!(view.read(second).unwrap(), Some(pattern(9, 100)));
}

#[test]
fn an_extent_appended_after_a_pin_mapped_the_container_is_readable_by_a_later_pin() {
    // Arrange: a pin that has mapped the container to read the first extent, still held
    let s = Scratch::new("kaban-t5-pin", "append-after-map");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let first = catalog_testing::append_and_publish(&store, &pattern(3, 8192)).unwrap();
    let mapped = catalog_testing::pin(&store, &budget()).unwrap();
    assert_eq!(mapped.view().read(first).unwrap(), Some(pattern(3, 8192)));

    // Act: the container grows past that mapping
    let second = catalog_testing::append_and_publish(&store, &pattern(5, 70_000)).unwrap();
    let later = catalog_testing::pin(&store, &budget()).unwrap();

    // Assert: the later pin reads the new extent, the earlier one still reads its own
    assert_eq!(later.view().read(second).unwrap(), Some(pattern(5, 70_000)));
    assert_eq!(later.view().read(first).unwrap(), Some(pattern(3, 8192)));
    assert_eq!(mapped.view().read(first).unwrap(), Some(pattern(3, 8192)));
    assert_eq!(mapped.view().read(second).unwrap(), None);
}

#[test]
fn concurrent_appends_reserve_disjoint_ranges_and_the_container_never_shrinks() {
    // Arrange: four writers on their own stores of one database
    let s = Scratch::new("kaban-t5-pin", "concurrent-append");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let kbx = s.0.join("kaban.kbx");
    let writers: Vec<_> = (1..=4_u8)
        .map(|w| {
            let (db, keys) = (s.db(), s.keys());
            std::thread::spawn(move || {
                let store = SqliteStore::open(&db, &keys).unwrap();
                (0..25_u8)
                    .map(|i| {
                        let fill = w * 50 + i + 1;
                        let len = 1000 + usize::from(i) * 37;
                        let id = catalog_testing::append_and_publish(&store, &pattern(fill, len))
                            .unwrap();
                        (id, fill, len)
                    })
                    .collect::<Vec<_>>()
            })
        })
        .collect();
    let mut written = Vec::new();
    for w in writers {
        written.extend(w.join().expect("a writer panicked"));
    }

    // Act
    let pin = catalog_testing::pin(&store, &budget()).unwrap();

    // Assert: every extent reads back as written (no two ranges overlap), and the container
    // is at least as long as everything appended
    let view = pin.view();
    assert_eq!(view.extents().len(), written.len());
    for (id, fill, len) in &written {
        assert_eq!(
            view.read(*id).unwrap(),
            Some(pattern(*fill, *len)),
            "extent {id} was overwritten"
        );
    }
    let total: usize = written.iter().map(|(_, _, len)| len).sum();
    let on_disk = std::fs::metadata(&kbx).unwrap().len();
    assert!(on_disk >= total as u64, "{on_disk} < {total}");
}

// The race suite (T5 Step 4, KER-03): a reader stops in one window of the pin protocol while a
// reclamation (T6's two-phase rule, zero-filling) retires and reclaims the extent the reader
// may hold. Each window runs `ITERATIONS` times with the reclamation on a thread of this
// process and again in a child process. Every wait is bounded (`STEP`), every loop by
// `RACE_DEADLINE`, and the child is killed and reaped however the test ends. Both sides count
// their syncs without performing them (`IoFaults::skip_syncs`): what a pin reads is served by
// the page cache whether or not a reclamation's release reached the device, so the race needs
// the release, not a device sync per iteration; that a release is synced, and that a failed
// sync is fatal, is `catalog_io.rs`'s subject.
//
//   window                 reader (this process)            reclamation (thread or child)
//   ReaderCandidate        candidate g read, no lock  ~>    retire, reclaim (to completion)
//   ReaderLocked           holds pins/<g>, no txn     ~>    retire, reclaim (concurrently)
//   ReaderInTransaction    txn open, nothing copied   ~>    retire, reclaim (to completion)
//   ReclaimFloorCommitted  pins (whole acquisition)   <~    floor committed, re-scan waits

/// Iterations per window and mode (recorded for the Step 7 capture).
const ITERATIONS: usize = 1000;
/// The bound of one race test's loop: past it the test fails with the count it reached.
const RACE_DEADLINE: Duration = Duration::from_secs(45);
/// The bound of any one wait on the other side of a race.
const STEP: Duration = Duration::from_secs(10);
/// The length of every extent of the race.
const EXTENT_LEN: usize = 4096;
/// The database the child reclamation opens (set only for the child process).
const RACE_DB: &str = "KABAN_T5_RACE_DB";
/// The prefix of the child reclamation's replies (the test harness writes to stdout too).
const REPLY: &str = "race-reply ";

/// The nonzero fill of iteration `i`'s extent (the keeper's is 255).
fn fill(i: usize) -> u8 {
    u8::try_from(i % 254).unwrap() + 1
}

/// What the reader asks the reclamation for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Command {
    /// Retire the extent, then reclaim.
    Reclaim(u64),
    /// As `Reclaim`, but stop after the floor commit until `Resume`.
    ReclaimPaused(u64),
    /// Continue a paused reclamation.
    Resume,
    /// End.
    Exit,
}

impl Command {
    fn encode(self) -> String {
        match self {
            Self::Reclaim(id) => format!("reclaim {id}"),
            Self::ReclaimPaused(id) => format!("paused {id}"),
            Self::Resume => "resume".into(),
            Self::Exit => "exit".into(),
        }
    }
    fn decode(line: &str) -> Self {
        let mut words = line.split_whitespace();
        match (words.next(), words.next().map(|id| id.parse().unwrap())) {
            (Some("reclaim"), Some(id)) => Self::Reclaim(id),
            (Some("paused"), Some(id)) => Self::ReclaimPaused(id),
            (Some("resume"), None) => Self::Resume,
            (Some("exit"), None) => Self::Exit,
            _ => panic!("not a command: {line:?}"),
        }
    }
}

/// What the reclamation answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reply {
    /// A paused reclamation committed its floor and waits.
    Floor,
    /// A reclamation finished.
    Done { punched: usize, race_retry: bool },
}

impl Reply {
    fn encode(self) -> String {
        match self {
            Self::Floor => format!("{REPLY}floor"),
            Self::Done {
                punched,
                race_retry,
            } => format!("{REPLY}done {punched} {}", u8::from(race_retry)),
        }
    }
    fn decode(line: &str) -> Option<Self> {
        let rest = line.strip_prefix(REPLY)?;
        let words: Vec<&str> = rest.split_whitespace().collect();
        Some(match words.as_slice() {
            ["floor"] => Self::Floor,
            ["done", punched, race] => Self::Done {
                punched: punched.parse().unwrap(),
                race_retry: *race == "1",
            },
            _ => panic!("not a reply: {line:?}"),
        })
    }
}

/// The next command, blocking (bounded by its source).
type Next = Arc<dyn Fn() -> Command + Send + Sync>;
/// Send one reply.
type Answer = Arc<dyn Fn(Reply) + Send + Sync>;

/// Serve the reclamation side on `store` until `Exit`.
fn serve(store: &SqliteStore, next: &Next, answer: &Answer) {
    let paused = Arc::new(AtomicBool::new(false));
    let (pause, wait, tell) = (Arc::clone(&paused), Arc::clone(next), Arc::clone(answer));
    catalog_testing::set_failpoint(
        store,
        Window::ReclaimFloorCommitted,
        Some(Arc::new(move || {
            if pause.swap(false, Ordering::AcqRel) {
                tell(Reply::Floor);
                assert_eq!(
                    wait(),
                    Command::Resume,
                    "a paused reclamation was not resumed"
                );
            }
        })),
    );
    loop {
        let (id, pause) = match next() {
            Command::Reclaim(id) => (id, false),
            Command::ReclaimPaused(id) => (id, true),
            Command::Exit => return,
            Command::Resume => panic!("resume outside a paused reclamation"),
        };
        paused.store(pause, Ordering::Release);
        catalog_testing::retire(store, &[id]).unwrap();
        let r = catalog_testing::reclaim(store).unwrap();
        answer(Reply::Done {
            punched: r.punched.len(),
            race_retry: r.pin_race_retry,
        });
    }
}

/// The reclamation side of a child process: commands on stdin, replies on stdout. A no-op
/// unless `KABAN_T5_RACE_DB` is set.
#[test]
#[ignore = "the reclamation role of the cross-process race cases"]
fn pin_race_reclaimer_role() {
    let Ok(db) = std::env::var(RACE_DB) else {
        return;
    };
    let db = std::path::PathBuf::from(db);
    let faults = IoFaults::install(db.parent().unwrap());
    faults.skip_syncs();
    let store = SqliteStore::open(&db, &db.with_file_name("keys")).unwrap();
    let next: Next = Arc::new(|| {
        let mut line = String::new();
        // end of input (the test is gone) ends the role
        match std::io::stdin().lock().read_line(&mut line).unwrap() {
            0 => Command::Exit,
            _ => Command::decode(&line),
        }
    });
    let answer: Answer = Arc::new(|reply| {
        let mut out = std::io::stdout().lock();
        writeln!(out, "{}", reply.encode()).unwrap();
        out.flush().unwrap();
    });
    serve(&store, &next, &answer);
}

/// Where the reclamation runs.
#[derive(Debug, Clone, Copy)]
enum Mode {
    InProcess,
    CrossProcess,
}

/// The reader's handle on the reclamation: commands out, replies in (each wait bounded).
struct Reclaimer {
    send: Box<dyn Fn(Command) + Send + Sync>,
    replies: Mutex<Receiver<Reply>>,
    /// The thread (in-process) to join, or the child to kill and reap, on drop.
    worker: Worker,
}

enum Worker {
    Thread(Option<std::thread::JoinHandle<()>>),
    Child(Child),
}

impl Reclaimer {
    /// The reclamation on a thread of this process, with its own store of `s`'s database.
    fn thread(s: &Scratch) -> Self {
        let (commands, inbox): (Sender<Command>, Receiver<Command>) = channel();
        let (replies_tx, replies) = channel();
        let (db, keys) = (s.db(), s.keys());
        let handle = std::thread::spawn(move || {
            let store = SqliteStore::open(&db, &keys).unwrap();
            let inbox = Mutex::new(inbox);
            let next: Next = Arc::new(move || {
                inbox
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .recv_timeout(STEP)
                    .unwrap_or(Command::Exit)
            });
            let answer: Answer = Arc::new(move |reply| {
                let _ = replies_tx.send(reply);
            });
            serve(&store, &next, &answer);
        });
        Self {
            send: Box::new(move |c| {
                let _ = commands.send(c);
            }),
            replies: Mutex::new(replies),
            worker: Worker::Thread(Some(handle)),
        }
    }

    /// The reclamation in a child process of this test binary on `s`'s database.
    fn child(s: &Scratch) -> Self {
        let stderr = std::fs::File::create(s.0.join("reclaimer.stderr")).unwrap();
        let mut child = Process::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "pin_race_reclaimer_role",
                "--ignored",
                "--nocapture",
            ])
            .env(RACE_DB, s.db())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(stderr)
            .spawn()
            .unwrap();
        let stdin: Mutex<ChildStdin> = Mutex::new(child.stdin.take().unwrap());
        let stdout = child.stdout.take().unwrap();
        let (replies_tx, replies) = channel();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(stdout).lines() {
                let Ok(line) = line else { return };
                if let Some(reply) = Reply::decode(&line)
                    && replies_tx.send(reply).is_err()
                {
                    return;
                }
            }
        });
        Self {
            send: Box::new(move |c| {
                let mut stdin = stdin.lock().unwrap_or_else(PoisonError::into_inner);
                let _ = writeln!(stdin, "{}", c.encode()).and_then(|()| stdin.flush());
            }),
            replies: Mutex::new(replies),
            worker: Worker::Child(child),
        }
    }

    fn command(&self, c: Command) {
        (self.send)(c);
    }

    fn reply(&self) -> Reply {
        self.replies
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .recv_timeout(STEP)
            .unwrap_or_else(|e| panic!("the reclamation did not answer within {STEP:?}: {e}"))
    }

    /// Wait for the end of a reclamation.
    fn done(&self) -> Reply {
        let reply = self.reply();
        assert!(matches!(reply, Reply::Done { .. }), "{reply:?}");
        reply
    }
}

impl Drop for Reclaimer {
    fn drop(&mut self) {
        (self.send)(Command::Exit);
        match &mut self.worker {
            Worker::Thread(handle) => {
                // the worker ends on `Exit`, or by itself within `STEP` of its last command
                if let Some(handle) = handle.take() {
                    let _ = handle.join();
                }
            }
            Worker::Child(child) => {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
}

/// What one race saw.
#[derive(Debug, Default)]
struct Tally {
    iterations: usize,
    /// Iterations whose pin held the extent the reclamation retired (and read it intact).
    held_doomed: usize,
    /// Extents the reclamation zeroed.
    punched: usize,
    /// Reclamations whose re-scan found a live pin below the floor.
    race_retries: usize,
    /// Reader retries (a candidate the floor had passed).
    reader_retries: u64,
}

impl Tally {
    fn count(&mut self, reply: Reply) {
        if let Reply::Done {
            punched,
            race_retry,
        } = reply
        {
            self.punched += punched;
            self.race_retries += usize::from(race_retry);
        }
    }
}

/// Read the keeper and, when `view` holds it, the `doomed` extent: each must be one nonzero
/// byte repeated over [`EXTENT_LEN`], never a zeroed or torn range. Returns whether the pin
/// holds `doomed`.
fn read_intact(view: PinView<'_>, keeper: u64, doomed: u64, at: usize) -> bool {
    let held = view.extents().contains(&doomed);
    for id in std::iter::once(keeper).chain(held.then_some(doomed)) {
        let bytes = view
            .read(id)
            .unwrap()
            .unwrap_or_else(|| panic!("iteration {at}: the pin lost extent {id}"));
        assert_eq!(bytes.len(), EXTENT_LEN, "iteration {at}: extent {id}");
        let first = bytes[0];
        assert!(
            first != 0 && bytes.iter().all(|b| *b == first),
            "iteration {at}: the pin read a zeroed or torn range in extent {id} (doomed {doomed})"
        );
    }
    held
}

/// Race the reader against the reclamation `ITERATIONS` times in `window`.
fn race(window: Window, mode: Mode) -> Tally {
    let s = Scratch::new("kaban-t5-pin", &format!("race-{window:?}-{mode:?}"));
    let faults = IoFaults::install(&s.0);
    faults.skip_syncs();
    let reader = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    // the keeper (never retired), then one extent per iteration, published up front (one sync)
    let mut extents = vec![pattern(255, EXTENT_LEN)];
    extents.extend((0..ITERATIONS).map(|i| pattern(fill(i), EXTENT_LEN)));
    let ids = catalog_testing::append_and_publish_all(&reader, &extents).unwrap();
    let keeper = ids[0];
    let reclaimer = Arc::new(match mode {
        Mode::InProcess => Reclaimer::thread(&s),
        Mode::CrossProcess => Reclaimer::child(&s),
    });
    // the reader's hook: armed once per iteration (a retry passes the window again)
    let armed = Arc::new(AtomicBool::new(false));
    let doomed = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let sync = !matches!(window, Window::ReaderLocked);
    let in_window = Arc::new(Mutex::new(Vec::new()));
    if !matches!(window, Window::ReclaimFloorCommitted) {
        let (armed, doomed, reclaimer, replies) = (
            Arc::clone(&armed),
            Arc::clone(&doomed),
            Arc::clone(&reclaimer),
            Arc::clone(&in_window),
        );
        catalog_testing::set_failpoint(
            &reader,
            window,
            Some(Arc::new(move || {
                if armed.swap(false, Ordering::AcqRel) {
                    reclaimer.command(Command::Reclaim(doomed.load(Ordering::Acquire)));
                    if sync {
                        // the reclamation runs to completion inside the window
                        let reply = reclaimer.done();
                        replies
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .push(reply);
                    }
                }
            })),
        );
    }
    let started = Instant::now();
    let mut tally = Tally::default();
    for i in 0..ITERATIONS {
        assert!(
            started.elapsed() < RACE_DEADLINE,
            "{window:?} {mode:?}: {i} iterations in {:?}",
            started.elapsed()
        );
        let id = ids[i + 1];
        doomed.store(id, Ordering::Release);
        let held = match window {
            Window::ReaderCandidate | Window::ReaderLocked => {
                armed.store(true, Ordering::Release);
                let pin = catalog_testing::pin(&reader, &budget()).unwrap();
                if !sync {
                    tally.count(reclaimer.done());
                }
                read_intact(pin.view(), keeper, id, i)
            }
            Window::ReaderInTransaction => {
                armed.store(true, Ordering::Release);
                catalog_testing::with_snapshot(&reader, &budget(), |view| {
                    Ok(read_intact(view, keeper, id, i))
                })
                .unwrap()
            }
            Window::ReclaimFloorCommitted => {
                reclaimer.command(Command::ReclaimPaused(id));
                assert_eq!(reclaimer.reply(), Reply::Floor);
                let pin = catalog_testing::pin(&reader, &budget()).unwrap();
                reclaimer.command(Command::Resume);
                tally.count(reclaimer.done());
                read_intact(pin.view(), keeper, id, i)
            }
        };
        assert!(
            !armed.load(Ordering::Acquire),
            "iteration {i}: the window was never reached"
        );
        tally.held_doomed += usize::from(held);
        tally.iterations += 1;
    }
    // with no pin left every retired extent is reclaimed: nothing stale blocks it
    catalog_testing::set_failpoint(&reader, window, None);
    for reply in in_window
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .drain(..)
    {
        tally.count(reply);
    }
    reclaimer.command(Command::Reclaim(0));
    tally.count(reclaimer.done());
    tally.reader_retries = catalog_testing::pin_retries(&reader);
    let pin = catalog_testing::pin(&reader, &budget()).unwrap();
    assert_eq!(
        pin.view().extents(),
        vec![keeper],
        "{window:?} {mode:?}: live extents"
    );
    assert!(read_intact(pin.view(), keeper, keeper, ITERATIONS));
    drop(pin);
    drop(reclaimer);
    let _ = writeln!(
        std::io::stderr(),
        "{window:?} {mode:?}: {tally:?} in {:?}",
        started.elapsed()
    );
    tally
}

#[test]
fn a_reader_whose_candidate_a_reclamation_passed_retries_and_reads_intact_bytes_in_process() {
    let tally = race(Window::ReaderCandidate, Mode::InProcess);
    assert!(
        tally.reader_retries >= ITERATIONS as u64,
        "a candidate below the committed floor was pinned: {tally:?}"
    );
    assert_eq!(tally.punched, ITERATIONS, "{tally:?}");
}

#[test]
fn a_reader_whose_candidate_a_reclamation_passed_retries_and_reads_intact_bytes_across_processes() {
    let tally = race(Window::ReaderCandidate, Mode::CrossProcess);
    assert!(
        tally.reader_retries >= ITERATIONS as u64,
        "a candidate below the committed floor was pinned: {tally:?}"
    );
    assert_eq!(tally.punched, ITERATIONS, "{tally:?}");
}

#[test]
fn a_reader_holding_its_pin_lock_before_its_transaction_never_reads_a_zeroed_range_in_process() {
    let tally = race(Window::ReaderLocked, Mode::InProcess);
    assert_eq!(tally.punched, ITERATIONS, "{tally:?}");
}

#[test]
fn a_reader_holding_its_pin_lock_before_its_transaction_never_reads_a_zeroed_range_across_processes()
 {
    let tally = race(Window::ReaderLocked, Mode::CrossProcess);
    assert_eq!(tally.punched, ITERATIONS, "{tally:?}");
}

#[test]
fn a_reader_whose_transaction_is_open_across_a_reclamation_reads_intact_bytes_in_process() {
    let tally = race(Window::ReaderInTransaction, Mode::InProcess);
    assert_eq!(
        tally.held_doomed, ITERATIONS,
        "the race never held the retired extent: {tally:?}"
    );
    assert_eq!(tally.punched, ITERATIONS, "{tally:?}");
}

#[test]
fn a_reader_whose_transaction_is_open_across_a_reclamation_reads_intact_bytes_across_processes() {
    let tally = race(Window::ReaderInTransaction, Mode::CrossProcess);
    assert_eq!(
        tally.held_doomed, ITERATIONS,
        "the race never held the retired extent: {tally:?}"
    );
    assert_eq!(tally.punched, ITERATIONS, "{tally:?}");
}

#[test]
fn a_reader_pinning_between_the_floor_commit_and_the_rescan_reads_intact_bytes_in_process() {
    let tally = race(Window::ReclaimFloorCommitted, Mode::InProcess);
    assert_eq!(tally.punched, ITERATIONS, "{tally:?}");
}

#[test]
fn a_reader_pinning_between_the_floor_commit_and_the_rescan_reads_intact_bytes_across_processes() {
    let tally = race(Window::ReclaimFloorCommitted, Mode::CrossProcess);
    assert_eq!(tally.punched, ITERATIONS, "{tally:?}");
}

#[test]
fn a_detached_pin_outlives_its_store_and_still_guards_its_extents() {
    // Arrange: a detached pin of an extent, its store dropped
    let s = Scratch::new("kaban-t5-pin", "detached-outlives");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let id = catalog_testing::append_and_publish(&store, &pattern(11, EXTENT_LEN)).unwrap();
    let pin = catalog_testing::pin(&store, &budget()).unwrap();
    drop(store);
    let other = SqliteStore::open(&s.db(), &s.keys()).unwrap();

    // Act: another store retires and reclaims the extent while the pin lives
    catalog_testing::retire(&other, &[id]).unwrap();
    let held = catalog_testing::reclaim(&other).unwrap();

    // Assert: nothing the pin holds is reclaimed and its bytes stay intact; once the pin is
    // released the extent is reclaimed
    assert_eq!(held.punched, Vec::<u64>::new(), "{held:?}");
    assert_eq!(pin.view().read(id).unwrap(), Some(pattern(11, EXTENT_LEN)));
    drop(pin);
    let released = catalog_testing::reclaim(&other).unwrap();
    assert_eq!(released.punched, vec![id], "{released:?}");
}

#[test]
fn bytes_from_a_pin_keep_their_extent_from_reclamation_after_the_pin_is_dropped() {
    // Arrange: bytes of a pinned extent, a clone and a slice of them; the pin and the original
    // bytes dropped, the extent retired
    let s = Scratch::new("kaban-t5-pin", "bytes-outlive-pin");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let written = pattern(13, EXTENT_LEN);
    let id = catalog_testing::append_and_publish(&store, &written).unwrap();
    let pin = catalog_testing::pin(&store, &budget()).unwrap();
    let bytes = pin
        .view()
        .bytes(id)
        .unwrap()
        .expect("the pin holds the extent");
    let clone = bytes.clone();
    let slice = bytes.slice(1..EXTENT_LEN - 1);
    drop(pin);
    drop(bytes);
    catalog_testing::retire(&store, &[id]).unwrap();

    // Act / Assert: nothing is released while the clone or the slice lives, and both stay intact
    let held = catalog_testing::reclaim(&store).unwrap();
    assert_eq!(
        held.punched,
        Vec::<u64>::new(),
        "released under a live clone and slice: {held:?}"
    );
    assert_eq!(clone.as_slice(), written.as_slice());
    drop(clone);
    let held = catalog_testing::reclaim(&store).unwrap();
    assert_eq!(
        held.punched,
        Vec::<u64>::new(),
        "released under a live slice: {held:?}"
    );
    assert_eq!(slice.as_slice(), &written[1..EXTENT_LEN - 1]);

    // Act: the last holder is dropped on another thread
    let (done, finished) = channel();
    std::thread::spawn(move || {
        drop(slice);
        done.send(()).unwrap();
    });
    finished
        .recv_timeout(STEP)
        .expect("the other thread did not drop the slice in time");

    // Assert: with no holder left the extent is reclaimed
    let released = catalog_testing::reclaim(&store).unwrap();
    assert_eq!(released.punched, vec![id], "{released:?}");
}

#[test]
fn a_pin_refuses_the_bytes_of_an_extent_it_does_not_hold() {
    // Arrange: an earlier pin, and a later one that also holds an extent published after it
    let s = Scratch::new("kaban-t5-pin", "foreign-extent");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let shared = catalog_testing::append_and_publish(&store, &pattern(21, 100)).unwrap();
    let earlier = catalog_testing::pin(&store, &budget()).unwrap();
    let foreign = catalog_testing::append_and_publish(&store, &pattern(23, 100)).unwrap();
    let later = catalog_testing::pin(&store, &budget()).unwrap();

    // Act
    let refused = earlier.view().bytes_of(later.view(), foreign);
    let accepted = earlier.view().bytes_of(later.view(), shared);

    // Assert: the extent the earlier pin does not hold is refused by name; one it holds reads
    let err = refused.expect_err("a pin read an extent it does not hold");
    let StoreError::Backend { source } = &err else {
        panic!("not a backend error: {err:?}");
    };
    let text = source.driver().to_string();
    assert!(text.contains("an extent this pin does not hold"), "{text}");
    assert_eq!(
        accepted.unwrap().map(|b| b.as_slice().to_vec()),
        Some(pattern(21, 100))
    );
}

/// The database the pin-holding child opens (set only for that child process).
const HOLD_DB: &str = "KABAN_T5_HOLD_DB";

/// A child that pins, says so on stdout and then waits to be killed (bounded, in case nothing
/// kills it). A no-op unless `KABAN_T5_HOLD_DB` is set.
#[test]
#[ignore = "the pin holder of the killed-holder case"]
fn killed_pin_holder_role() {
    let Ok(db) = std::env::var(HOLD_DB) else {
        return;
    };
    let db = std::path::PathBuf::from(db);
    let faults = IoFaults::install(db.parent().unwrap());
    faults.skip_syncs();
    let store = SqliteStore::open(&db, &db.with_file_name("keys")).unwrap();
    let pin = catalog_testing::pin(&store, &budget()).unwrap();
    let mut out = std::io::stdout().lock();
    writeln!(out, "{REPLY}holding {}", pin.view().generation()).unwrap();
    out.flush().unwrap();
    std::thread::sleep(Duration::from_secs(30));
}

/// The pin lock files in `s`'s pins directory.
fn lock_files(s: &Scratch) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(s.0.join("kaban.kbx.pins"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn a_pin_lock_left_by_a_killed_process_neither_blocks_reclamation_nor_survives_it() {
    // Arrange: an extent, pinned by a child process
    let s = Scratch::new("kaban-t5-pin", "killed-holder");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let id = catalog_testing::append_and_publish(&store, &pattern(13, EXTENT_LEN)).unwrap();
    let stderr = std::fs::File::create(s.0.join("holder.stderr")).unwrap();
    let mut child = Process::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "killed_pin_holder_role",
            "--ignored",
            "--nocapture",
        ])
        .env(HOLD_DB, s.db())
        .stdout(Stdio::piped())
        .stderr(stderr)
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let (lines, holding) = channel();
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(stdout)
            .lines()
            .map_while(Result::ok)
        {
            if line.starts_with(REPLY) && lines.send(line).is_err() {
                return;
            }
        }
    });
    let said = holding.recv_timeout(STEP);
    let killed = |child: &mut Child| {
        let _ = child.kill();
        let _ = child.wait();
    };
    let Ok(said) = said else {
        killed(&mut child);
        panic!("the holder never pinned within {STEP:?}");
    };
    assert!(said.starts_with(&format!("{REPLY}holding")), "{said}");

    // Act: retire and reclaim while it lives, then kill it and reclaim again
    catalog_testing::retire(&store, &[id]).unwrap();
    let alive = catalog_testing::reclaim(&store);
    killed(&mut child);
    let left = lock_files(&s);
    let after = catalog_testing::reclaim(&store).unwrap();

    // Assert: the live holder kept its extent; its lock outlived it as a file but not as a
    // lock, so the next reclamation took the extent and removed the file
    let alive = alive.unwrap();
    assert_eq!(
        alive.punched,
        Vec::<u64>::new(),
        "a live pin's extent was reclaimed: {alive:?}"
    );
    assert_eq!(
        left.len(),
        1,
        "the killed holder left no lock file: {left:?}"
    );
    assert_eq!(after.punched, vec![id], "{after:?}");
    assert!(!after.pin_race_retry, "{after:?}");
    assert_eq!(
        lock_files(&s),
        Vec::<String>::new(),
        "a stale lock file survived"
    );
}

#[test]
fn a_reader_locking_a_stale_candidate_during_the_rescan_makes_the_reclamation_retry() {
    // Arrange: a reader that read its candidate g, then a reclamation that retired the extent
    // and committed floor g + 1 before the reader locks g, and re-scans while it holds g
    let s = Scratch::new("kaban-t5-pin", "rescan-race");
    let reader = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let ids = catalog_testing::append_and_publish_all(
        &reader,
        &[pattern(255, EXTENT_LEN), pattern(17, EXTENT_LEN)],
    )
    .unwrap();
    let (keeper, doomed) = (ids[0], ids[1]);
    let reclaimer = Arc::new(Reclaimer::thread(&s));
    let rescanned = Arc::new(Mutex::new(None));
    let (paused, resumed) = (
        Arc::new(AtomicBool::new(true)),
        Arc::new(AtomicBool::new(true)),
    );
    let (once, r) = (Arc::clone(&paused), Arc::clone(&reclaimer));
    catalog_testing::set_failpoint(
        &reader,
        Window::ReaderCandidate,
        Some(Arc::new(move || {
            if once.swap(false, Ordering::AcqRel) {
                r.command(Command::ReclaimPaused(doomed));
                assert_eq!(r.reply(), Reply::Floor);
            }
        })),
    );
    let (once, r, seen) = (
        Arc::clone(&resumed),
        Arc::clone(&reclaimer),
        Arc::clone(&rescanned),
    );
    catalog_testing::set_failpoint(
        &reader,
        Window::ReaderLocked,
        Some(Arc::new(move || {
            if once.swap(false, Ordering::AcqRel) {
                r.command(Command::Resume);
                *seen.lock().unwrap_or_else(PoisonError::into_inner) = Some(r.done());
            }
        })),
    );

    // Act
    let pin = catalog_testing::pin(&reader, &budget()).unwrap();

    // Assert: the re-scan saw the stale lock below its floor and zeroed nothing; the reader
    // retried past the floor and holds only intact bytes; with the pin gone the extent goes
    let rescanned = rescanned
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take();
    assert_eq!(
        rescanned,
        Some(Reply::Done {
            punched: 0,
            race_retry: true
        })
    );
    assert_eq!(catalog_testing::pin_retries(&reader), 1);
    assert!(!read_intact(pin.view(), keeper, doomed, 0));
    drop(pin);
    for window in [Window::ReaderCandidate, Window::ReaderLocked] {
        catalog_testing::set_failpoint(&reader, window, None);
    }
    reclaimer.command(Command::Reclaim(0));
    assert_eq!(
        reclaimer.done(),
        Reply::Done {
            punched: 1,
            race_retry: false
        }
    );
}
