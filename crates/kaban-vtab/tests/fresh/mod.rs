//! Fresh-process test cases and the raw SQLite calls they observe the allocator through.
//!
//! SQLite's allocator can be configured once per process, before SQLite initializes, so every
//! case runs in its own child process: the test re-executes its own binary with `--exact <name>`
//! and an environment marker naming the case, and the child runs the body.
#![allow(dead_code, unused_macros, unused_imports)] // each test binary uses a subset
#![allow(clippy::panic)] // a child past its time limit fails the case

use std::ffi::c_void;
use std::process::{Command, Output, Stdio};
use std::ptr;
use std::time::{Duration, Instant};

use kaban_vtab::{Runtime, initialize_for_embedding};
use rusqlite::ffi;

const MARKER: &str = "KABAN_VTAB_FRESH_CASE";

/// Declare `#[test] fn $name` whose `$body` runs in a fresh child process.
macro_rules! fresh_test {
    ($(#[$meta:meta])* fn $name:ident() $body:block) => {
        $(#[$meta])*
        #[test]
        fn $name() {
            fresh::run_fresh(stringify!($name), || $body);
        }
    };
}
pub(crate) use fresh_test;

/// In the child named by the marker, run `body`; otherwise spawn that child and require that it
/// ran exactly this case and passed.
pub(crate) fn run_fresh(name: &str, body: fn()) {
    if is_child(name) {
        body();
        return;
    }
    let output = spawn_child(name);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success() && stdout.contains("1 passed"),
        "fresh child `{name}` failed ({}):\n--- stdout\n{stdout}\n--- stderr\n{stderr}",
        output.status
    );
}

/// Whether this process is the child running case `name`.
pub(crate) fn is_child(name: &str) -> bool {
    std::env::var(MARKER).is_ok_and(|case| case == name)
}

/// How long a fresh child may run before it is killed and its case fails.
const CHILD_LIMIT: Duration = Duration::from_secs(60);

/// Re-execute this test binary as the child for case `name` and collect its output. A child
/// still running after [`CHILD_LIMIT`] is killed and fails the case: the wait never hangs.
pub(crate) fn spawn_child(name: &str) -> Output {
    let exe = std::env::current_exe().unwrap();
    let mut child = Command::new(exe)
        .args(["--exact", name, "--nocapture", "--test-threads", "1"])
        .env(MARKER, name)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let started = Instant::now();
    while child.try_wait().unwrap().is_none() {
        if started.elapsed() > CHILD_LIMIT {
            child.kill().unwrap();
            let output = child.wait_with_output().unwrap();
            panic!(
                "fresh child `{name}` ran past {CHILD_LIMIT:?} and was killed:\n--- stdout\n{}\n--- stderr\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    child.wait_with_output().unwrap()
}

/// The installed runtime: the startup bootstrap's when `process-bootstrap` is on, otherwise
/// installed here through the embedding initializer.
pub(crate) fn installed_runtime() -> Runtime {
    Runtime::get().unwrap_or_else(|| {
        // SAFETY: called first thing in a fresh child, on one thread, before any SQLite entry;
        // nothing replaces the allocator or shuts SQLite down afterwards.
        unsafe { initialize_for_embedding() }.unwrap()
    })
}

fn empty_methods() -> ffi::sqlite3_mem_methods {
    ffi::sqlite3_mem_methods {
        xMalloc: None,
        xFree: None,
        xRealloc: None,
        xSize: None,
        xRoundup: None,
        xInit: None,
        xShutdown: None,
        pAppData: ptr::null_mut(),
    }
}

/// `SQLITE_CONFIG_GETMALLOC`: the configured allocator, or the refusal code.
pub(crate) fn configured_allocator() -> Result<ffi::sqlite3_mem_methods, i32> {
    let mut methods = empty_methods();
    // SAFETY: GETMALLOC writes one `sqlite3_mem_methods` through the pointer, which is valid and
    // exclusively borrowed for the call.
    let rc = unsafe { ffi::sqlite3_config(ffi::SQLITE_CONFIG_GETMALLOC, &raw mut methods) };
    if rc == ffi::SQLITE_OK {
        Ok(methods)
    } else {
        Err(rc)
    }
}

/// `sqlite3_initialize`, asserting success.
pub(crate) fn initialize_sqlite() {
    // SAFETY: plain FFI call with no arguments.
    let rc = unsafe { ffi::sqlite3_initialize() };
    assert_eq!(rc, ffi::SQLITE_OK);
}

/// One live block from SQLite's public allocator (`sqlite3_malloc64`), freed with [`Block::free`]
/// (dropping without `free` leaks it on purpose, as an allocation that outlives its query would).
#[derive(Debug)]
pub(crate) struct Block(ptr::NonNull<c_void>);

// SAFETY: SQLite's allocator is thread-safe in this build (SQLITE_THREADSAFE=1); a block may be
// resized, measured or freed on any thread, and `Block` is its only owner.
unsafe impl Send for Block {}

impl Block {
    /// `sqlite3_malloc64(bytes)`, or `None` when SQLite returns null.
    pub(crate) fn malloc(bytes: u64) -> Option<Self> {
        // SAFETY: any size is accepted; failure returns null.
        ptr::NonNull::new(unsafe { ffi::sqlite3_malloc64(bytes) }).map(Self)
    }

    /// `sqlite3_realloc64(self, bytes)`; on null the old block stays live in `self`.
    pub(crate) fn realloc(&mut self, bytes: u64) -> bool {
        // SAFETY: `self.0` is a live block from SQLite's allocator, owned by `self`.
        match ptr::NonNull::new(unsafe { ffi::sqlite3_realloc64(self.0.as_ptr(), bytes) }) {
            Some(moved) => {
                self.0 = moved;
                true
            }
            None => false,
        }
    }

    /// `sqlite3_msize(self)`.
    pub(crate) fn size(&self) -> u64 {
        // SAFETY: `self.0` is a live block from SQLite's allocator.
        unsafe { ffi::sqlite3_msize(self.0.as_ptr()) }
    }

    /// The block's address.
    pub(crate) fn addr(&self) -> usize {
        self.0.as_ptr() as usize
    }

    /// Fill the first `len` bytes (at most [`Block::size`]) with `byte`.
    pub(crate) fn fill(&mut self, len: usize, byte: u8) {
        assert!(len as u64 <= self.size());
        // SAFETY: the block is live and has at least `len` usable bytes (asserted above).
        unsafe { ptr::write_bytes(self.0.as_ptr().cast::<u8>(), byte, len) }
    }

    /// Whether the first `len` bytes (at most [`Block::size`], previously filled) equal `byte`.
    pub(crate) fn holds(&self, len: usize, byte: u8) -> bool {
        assert!(len as u64 <= self.size());
        // SAFETY: the block is live with at least `len` usable bytes, initialized by `fill`.
        let bytes = unsafe { std::slice::from_raw_parts(self.0.as_ptr().cast::<u8>(), len) };
        bytes.iter().all(|&b| b == byte)
    }

    /// `sqlite3_free(self)`.
    pub(crate) fn free(self) {
        // SAFETY: `self.0` is a live block owned by `self`, consumed here, so freed exactly once.
        unsafe { ffi::sqlite3_free(self.0.as_ptr()) }
    }
}

/// The flags a request's in-memory query connection opens with.
pub(crate) fn query_flags() -> rusqlite::OpenFlags {
    use rusqlite::OpenFlags;
    OpenFlags::SQLITE_OPEN_READ_WRITE
        | OpenFlags::SQLITE_OPEN_CREATE
        | OpenFlags::SQLITE_OPEN_NO_MUTEX
        | OpenFlags::SQLITE_OPEN_URI
}

/// Run `body` on a request's in-memory query connection with `functions` registered, inside an
/// unbounded request scope; returns the budget so callers can check what stays charged.
pub(crate) fn with_query(
    functions: Vec<std::sync::Arc<dyn kaban_vtab::TableFunction>>,
    body: impl FnOnce(&rusqlite::Connection),
) -> kaban_core::query_budget::QueryBudget {
    let runtime = installed_runtime();
    let budget = kaban_core::query_budget::QueryBudget::new(u64::MAX, None);
    let scope = kaban_vtab::RequestScope::enter(runtime, &budget).unwrap();
    let conn = scope
        .open_query_connection(std::path::Path::new(":memory:"), query_flags())
        .unwrap();
    for f in functions {
        kaban_vtab::register(&conn, f).unwrap();
    }
    body(&conn);
    conn.close().unwrap();
    drop(scope);
    budget
}
