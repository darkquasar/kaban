//! Fresh-process test cases and the raw SQLite calls they observe the allocator through.
//!
//! SQLite's allocator can be configured once per process, before SQLite initializes, so every
//! case runs in its own child process: the test re-executes its own binary with `--exact <name>`
//! and an environment marker naming the case, and the child runs the body.
#![allow(dead_code)] // each test binary uses a subset

use std::ffi::c_void;
use std::process::Command;
use std::ptr;

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
    if std::env::var(MARKER).is_ok_and(|case| case == name) {
        body();
        return;
    }
    let exe = std::env::current_exe().unwrap();
    let output = Command::new(exe)
        .args(["--exact", name, "--nocapture", "--test-threads", "1"])
        .env(MARKER, name)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success() && stdout.contains("1 passed"),
        "fresh child `{name}` failed ({}):\n--- stdout\n{stdout}\n--- stderr\n{stderr}",
        output.status
    );
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
