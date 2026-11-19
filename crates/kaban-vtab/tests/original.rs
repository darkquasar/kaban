//! The allocator adapter over an original allocator that hands out more than it is asked for, and
//! whose `xRoundup` refuses some sizes (spec 11 T12, QRY-06, MAJ-04, MED-01). The test installs
//! its own original with `SQLITE_CONFIG_MALLOC` before the embedding initializer records it, so
//! these cases need the default build (no startup bootstrap). Each runs in a fresh process.
#![cfg(not(feature = "process-bootstrap"))]
#![allow(clippy::unwrap_used)]

mod fresh;

use std::alloc::{Layout, alloc, dealloc};
use std::ffi::{c_int, c_void};
use std::ptr;
use std::sync::atomic::{AtomicUsize, Ordering};

use fresh::{Block, fresh_test, initialize_sqlite};
use kaban_core::query_budget::QueryBudget;
use kaban_vtab::{RequestScope, Runtime, initialize_for_embedding};
use rusqlite::ffi;

/// Bytes the test original reports beyond each request.
const SLACK: usize = 4096;
/// Bytes it reserves beyond the reported size: a grow that fits stays at the same address.
const HIDDEN: usize = 8192;
/// The prefix holding the reported size and the capacity (keeps 8-byte alignment).
const PREFIX: usize = 16;
/// `xRoundup` refuses (returns 0 for) every size from this one on.
const REFUSED_FROM: c_int = 50_000;

/// Calls of the original's `xMalloc` and `xRealloc`.
static ATTEMPTS: AtomicUsize = AtomicUsize::new(0);

fn layout(capacity: usize) -> Layout {
    Layout::from_size_align(capacity + PREFIX, 8).unwrap()
}

/// A fresh block reporting `n + SLACK` usable bytes, with `HIDDEN` more behind them.
fn place(n: usize) -> *mut c_void {
    let capacity = n + SLACK + HIDDEN;
    // SAFETY: the layout has a nonzero size.
    let base = unsafe { alloc(layout(capacity)) }.cast::<usize>();
    if base.is_null() {
        return ptr::null_mut();
    }
    // SAFETY: the block is at least `PREFIX` (two `usize`) bytes long and 8-byte aligned.
    unsafe {
        base.write(n + SLACK);
        base.add(1).write(capacity);
        base.cast::<u8>().add(PREFIX).cast()
    }
}

/// The prefix of a block from [`place`].
///
/// # Safety
/// `p` is a live block from [`place`].
unsafe fn prefix(p: *mut c_void) -> *mut usize {
    // SAFETY: the caller passes a live block, which starts `PREFIX` bytes after its prefix.
    unsafe { p.cast::<u8>().sub(PREFIX).cast() }
}

unsafe extern "C" fn test_malloc(n: c_int) -> *mut c_void {
    ATTEMPTS.fetch_add(1, Ordering::SeqCst);
    usize::try_from(n)
        .ok()
        .filter(|&n| n > 0)
        .map_or(ptr::null_mut(), place)
}

unsafe extern "C" fn test_free(p: *mut c_void) {
    if p.is_null() {
        return;
    }
    // SAFETY: SQLite frees a live block of this allocator once.
    unsafe {
        let base = prefix(p);
        dealloc(base.cast(), layout(base.add(1).read()));
    }
}

unsafe extern "C" fn test_size(p: *mut c_void) -> c_int {
    if p.is_null() {
        return 0;
    }
    // SAFETY: SQLite measures a live block of this allocator.
    c_int::try_from(unsafe { prefix(p).read() }).unwrap_or(c_int::MAX)
}

unsafe extern "C" fn test_realloc(p: *mut c_void, n: c_int) -> *mut c_void {
    ATTEMPTS.fetch_add(1, Ordering::SeqCst);
    let Some(n) = usize::try_from(n).ok().filter(|&n| n > 0) else {
        return ptr::null_mut();
    };
    // SAFETY: SQLite resizes a live block of this allocator.
    unsafe {
        let base = prefix(p);
        let (reported, capacity) = (base.read(), base.add(1).read());
        if n + SLACK <= capacity {
            base.write(n + SLACK);
            return p;
        }
        let moved = place(n);
        if !moved.is_null() {
            ptr::copy_nonoverlapping(p.cast::<u8>(), moved.cast::<u8>(), reported.min(n));
            test_free(p);
        }
        moved
    }
}

unsafe extern "C" fn test_roundup(n: c_int) -> c_int {
    if n >= REFUSED_FROM { 0 } else { (n + 7) & !7 }
}

/// Configure the test original, install the adapter over it and initialize SQLite unscoped.
fn install() -> Runtime {
    let methods = ffi::sqlite3_mem_methods {
        xMalloc: Some(test_malloc),
        xFree: Some(test_free),
        xRealloc: Some(test_realloc),
        xSize: Some(test_size),
        xRoundup: Some(test_roundup),
        xInit: None,
        xShutdown: None,
        pAppData: ptr::null_mut(),
    };
    // SAFETY: fresh process, one thread, no SQLite entry yet; SQLite copies the struct.
    let rc = unsafe { ffi::sqlite3_config(ffi::SQLITE_CONFIG_MALLOC, &raw const methods) };
    assert_eq!(rc, ffi::SQLITE_OK);
    // SAFETY: as above; nothing replaces the allocator or shuts SQLite down afterwards.
    let runtime = unsafe { initialize_for_embedding() }.unwrap();
    initialize_sqlite();
    runtime
}

/// What the adapter charges a live block of `size` usable bytes: its 24-byte header is 16 bytes
/// of accounting metadata plus the owner's budget handle, charged as 8 more bytes.
fn charge(size: u64) -> u64 {
    QueryBudget::allocation_charge(size + 8, 8).unwrap()
}

/// Grow `block` to `bytes` and check the promised outcome: either it fails and the block, its
/// bytes and its charge stay as they were, or it succeeds and the charge is exact for the new
/// block, which holds the old bytes. Returns whether it grew.
fn grow_checked(block: &mut Block, bytes: u64, budget: &QueryBudget) -> bool {
    let (addr, size, live) = (block.addr(), block.size(), budget.live_bytes());
    block.fill(64, 0xC3);
    let grew = block.realloc(bytes);
    if grew {
        assert!(block.size() >= bytes);
        assert_eq!(
            budget.live_bytes(),
            charge(block.size()),
            "a grow left the block undercharged"
        );
    } else {
        assert_eq!((block.addr(), block.size()), (addr, size));
        assert_eq!(
            budget.live_bytes(),
            live,
            "a refused grow changed the charge"
        );
    }
    assert!(block.holds(64, 0xC3), "the block's bytes changed");
    grew
}

fresh_test! {
    fn a_moving_grow_whose_slack_is_refused_fails_and_keeps_the_block() {
        let runtime = install();
        let start = charge(64 + SLACK as u64);
        let target = 20_000;
        let budget = QueryBudget::new(start + charge(target) + 1000, None);
        let scope = RequestScope::enter(runtime, &budget).unwrap();
        let mut block = Block::malloc(64).unwrap();
        assert_eq!(budget.live_bytes(), charge(block.size()));
        assert!(!grow_checked(&mut block, target, &budget), "the refused slack was handed out");
        block.free();
        assert_eq!(budget.live_bytes(), 0);
        drop(scope);
    }
}

fresh_test! {
    fn an_in_place_grow_whose_slack_is_refused_fails_and_keeps_the_block() {
        let runtime = install();
        let start = charge(64 + SLACK as u64);
        let target = 6000; // past the reported size, within the original's hidden capacity
        let budget = QueryBudget::new(start + charge(target) + 1000, None);
        let scope = RequestScope::enter(runtime, &budget).unwrap();
        let mut block = Block::malloc(64).unwrap();
        assert!(!grow_checked(&mut block, target, &budget), "the refused slack was handed out");
        block.free();
        assert_eq!(budget.live_bytes(), 0);
        drop(scope);
    }
}

fresh_test! {
    fn grows_with_room_are_charged_exactly_and_released_on_free() {
        let runtime = install();
        let budget = QueryBudget::new(u64::MAX, None);
        let scope = RequestScope::enter(runtime, &budget).unwrap();
        let mut block = Block::malloc(64).unwrap();
        assert!(grow_checked(&mut block, 6000, &budget));
        assert!(grow_checked(&mut block, 40_000, &budget));
        drop(scope);
        // Released on free, after the request and on another thread.
        std::thread::spawn(move || block.free()).join().unwrap();
        assert_eq!(budget.live_bytes(), 0);
    }
}

fresh_test! {
    fn a_size_the_original_roundup_refuses_is_never_allocated() {
        let runtime = install();
        let budget = QueryBudget::new(u64::MAX, None);
        let scope = RequestScope::enter(runtime, &budget).unwrap();
        let attempts = ATTEMPTS.load(Ordering::SeqCst);
        assert!(Block::malloc(60_000).is_none(), "a refused size was allocated");
        assert_eq!(ATTEMPTS.load(Ordering::SeqCst), attempts, "the original was asked anyway");
        assert_eq!(budget.live_bytes(), 0);
        drop(scope);
        // Unscoped too.
        assert!(Block::malloc(60_000).is_none());
        assert_eq!(ATTEMPTS.load(Ordering::SeqCst), attempts);
    }
}

fresh_test! {
    fn a_grow_to_a_refused_size_fails_and_keeps_the_block_and_its_charge() {
        let runtime = install();
        let budget = QueryBudget::new(u64::MAX, None);
        let scope = RequestScope::enter(runtime, &budget).unwrap();
        let mut block = Block::malloc(64).unwrap();
        let attempts = ATTEMPTS.load(Ordering::SeqCst);
        assert!(!grow_checked(&mut block, 60_000, &budget));
        assert_eq!(ATTEMPTS.load(Ordering::SeqCst), attempts, "the original was asked anyway");
        block.free();
        assert_eq!(budget.live_bytes(), 0);
        drop(scope);
    }
}
