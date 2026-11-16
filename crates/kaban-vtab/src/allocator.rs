//! The request-scoped SQLite allocator adapter (spec 11 QRY-04, QRY-06, PC-17).
//!
//! The adapter wraps the allocator SQLite had before it ([`wrap`] records it from
//! `SQLITE_CONFIG_GETMALLOC`) and forwards every hook to it. Each block it hands to SQLite
//! carries a [`Header`] of [`HEADER_BYTES`] in front: the [`Owner`] (budget and lease) of a block
//! allocated while a budget scope was active on the thread, or `None` for an unscoped block. The
//! owner travels with the block through `xRealloc` and is dropped by `xFree` on whatever thread
//! frees it, so a charge always returns to the budget that paid it, even after its query ended.
//!
//! Layout of one block (`base` comes from the original allocator, 8-byte aligned):
//!
//! ```text
//! base                       base + 24 (pointer SQLite sees, 8-byte aligned)
//! | Header (24 bytes)        | usable bytes (original xSize - 24)            |
//! ```
//!
//! A scoped block is charged `QueryBudget::allocation_charge(usable + 8, 8)`: the usable size
//! plus the whole header, i.e. exactly what the original allocator holds for it. A scoped grow
//! is a replacement charged before the old block is released, never a destructive resize. The
//! callbacks never call into SQLite, never allocate through SQLite and never unwind into C: every
//! callback body runs under `catch_unwind` and fails closed (null, zero or nothing) if it panics.
//! The wrapper's own state takes no lock (the scope slot is a thread-local; the recorded hooks
//! are read with a lock-free `OnceLock::get`); a refused charge records the budget's first breach
//! through a `OnceLock`, which can wait briefly for a concurrent recorder of the same budget, and
//! the forwarded original hooks lock as that allocator does (docs/developer/SAFETY.md).

use std::cell::RefCell;
use std::ffi::{c_int, c_void};
use std::marker::PhantomData;
use std::mem;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr;
use std::sync::OnceLock;

use kaban_core::query_budget::{ALLOCATION_METADATA_BYTES, BudgetLease, QueryBudget};
use rusqlite::ffi;

use crate::InitError;

/// SQLite's alignment requirement for allocator blocks (and the original allocator's guarantee).
const SQLITE_ALIGN: usize = 8;

/// The bytes in front of every block handed to SQLite.
const HEADER_BYTES: usize = 24;

/// [`HEADER_BYTES`] in SQLite's size type.
const HEADER_INT: c_int = 24;

/// Header bytes beyond the `ALLOCATION_METADATA_BYTES` that `QueryBudget::allocation_charge`
/// adds to every charge; they are charged as if they were usable bytes, so a block's charge is
/// exactly what the original allocator holds for it.
const HEADER_EXTRA: u64 = HEADER_BYTES as u64 - ALLOCATION_METADATA_BYTES;

/// Who pays for a scoped block: the budget, kept so a grow can reserve the replacement block
/// against it, and the block's charge.
struct Owner {
    budget: QueryBudget,
    lease: BudgetLease,
}

/// Ownership record in front of every block: the owner, or `None` for an unscoped block.
#[repr(C)]
struct Header {
    owner: Option<Owner>,
}

// The header must fit its slot without disturbing SQLite's alignment, and the slot must hold at
// least the metadata `QueryBudget::allocation_charge` charges beside each allocation.
const _: () = {
    assert!(mem::size_of::<Header>() <= HEADER_BYTES);
    assert!(mem::align_of::<Header>() <= SQLITE_ALIGN);
    assert!(HEADER_BYTES.is_multiple_of(SQLITE_ALIGN));
    assert!(HEADER_BYTES as u64 >= ALLOCATION_METADATA_BYTES);
    assert!(HEADER_INT as usize == HEADER_BYTES);
};

/// The original allocator's opaque `pAppData`.
#[derive(Clone, Copy)]
struct AppData(*mut c_void);

// SAFETY: the pointer is opaque to kaban; it is only handed back to the original allocator's own
// `xInit`/`xShutdown`, which SQLite may call from any thread, exactly as SQLite itself would.
unsafe impl Send for AppData {}
// SAFETY: as above; kaban never dereferences it, so sharing the address is harmless.
unsafe impl Sync for AppData {}

/// The original allocator's hooks, recorded once before the wrapper is installed.
#[derive(Clone, Copy)]
struct Original {
    malloc: unsafe extern "C" fn(c_int) -> *mut c_void,
    free: unsafe extern "C" fn(*mut c_void),
    realloc: unsafe extern "C" fn(*mut c_void, c_int) -> *mut c_void,
    size: unsafe extern "C" fn(*mut c_void) -> c_int,
    roundup: unsafe extern "C" fn(c_int) -> c_int,
    init: Option<unsafe extern "C" fn(*mut c_void) -> c_int>,
    shutdown: Option<unsafe extern "C" fn(*mut c_void)>,
    app_data: AppData,
}

static ORIGINAL: OnceLock<Original> = OnceLock::new();

thread_local! {
    /// The budget SQLite allocations on this thread are charged to, if any.
    static CURRENT: RefCell<Option<QueryBudget>> = const { RefCell::new(None) };
}

#[cfg(feature = "test-hooks")]
thread_local! {
    static PANIC_NEXT_CHARGE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Record `original` and return the wrapper's methods, which forward to it.
///
/// Runs at most once per process (the runtime's `OnceLock`); a second call is refused.
pub(crate) fn wrap(
    original: &ffi::sqlite3_mem_methods,
) -> Result<ffi::sqlite3_mem_methods, InitError> {
    let missing = |hook| InitError::OriginalIncomplete { hook };
    let recorded = Original {
        malloc: original.xMalloc.ok_or(missing("xMalloc"))?,
        free: original.xFree.ok_or(missing("xFree"))?,
        realloc: original.xRealloc.ok_or(missing("xRealloc"))?,
        size: original.xSize.ok_or(missing("xSize"))?,
        roundup: original.xRoundup.ok_or(missing("xRoundup"))?,
        init: original.xInit,
        shutdown: original.xShutdown,
        app_data: AppData(original.pAppData),
    };
    ORIGINAL
        .set(recorded)
        .map_err(|_| InitError::AlreadyInitialized)?;
    Ok(ffi::sqlite3_mem_methods {
        xMalloc: Some(x_malloc),
        xFree: Some(x_free),
        xRealloc: Some(x_realloc),
        xSize: Some(x_size),
        xRoundup: Some(x_roundup),
        xInit: Some(x_init),
        xShutdown: Some(x_shutdown),
        pAppData: ptr::null_mut(),
    })
}

/// While alive, SQLite allocations made on this thread are charged to one [`QueryBudget`];
/// dropping it leaves the thread unscoped. At most one exists per thread ([`try_enter_scope`]).
/// It is bound to the thread that created it.
#[derive(Debug)]
pub(crate) struct ScopeGuard {
    _thread_bound: PhantomData<*const ()>,
}

/// Charge SQLite allocations made on this thread to `budget` until the guard drops, unless a
/// scope is already active on the thread (or its slot is unavailable): then `None`, and nothing
/// changes. The request scope is the only caller, so scopes never nest.
pub(crate) fn try_enter_scope(budget: &QueryBudget) -> Option<ScopeGuard> {
    let entered = CURRENT.try_with(|slot| {
        let mut slot = slot.try_borrow_mut().ok()?;
        if slot.is_some() {
            return None;
        }
        *slot = Some(budget.clone());
        Some(())
    });
    entered.ok().flatten().map(|()| ScopeGuard {
        _thread_bound: PhantomData,
    })
}

impl Drop for ScopeGuard {
    fn drop(&mut self) {
        // During thread teardown the slot may already be gone; nothing is left to clear then.
        let _ = CURRENT.try_with(|slot| {
            if let Ok(mut slot) = slot.try_borrow_mut() {
                *slot = None;
            }
        });
    }
}

/// Make the next charge attempt on this thread panic (fault injection for the no-unwind test).
#[cfg(feature = "test-hooks")]
pub(crate) fn inject_panic() {
    PANIC_NEXT_CHARGE.set(true);
}

/// The budget of this thread's active scope. Never panics: a slot that is gone (thread teardown)
/// or borrowed reads as unscoped.
pub(crate) fn current_budget() -> Option<QueryBudget> {
    CURRENT
        .try_with(|slot| slot.try_borrow().ok().and_then(|budget| budget.clone()))
        .ok()
        .flatten()
}

/// Charge `bytes` to this thread's scope: `Ok(None)` when unscoped, `Err` when refused.
fn charge_current(bytes: u64) -> Result<Option<Owner>, ()> {
    let Some(budget) = current_budget() else {
        return Ok(None);
    };
    #[cfg(feature = "test-hooks")]
    assert!(
        PANIC_NEXT_CHARGE.try_with(|flag| flag.replace(false)) != Ok(true),
        "injected panic in an allocator charge"
    );
    let lease = budget.reserve(bytes).map_err(drop)?;
    Ok(Some(Owner { budget, lease }))
}

/// The charge for a block with `usable` bytes, or `None` on a negative size or overflow.
fn charge_for(usable: c_int) -> Option<u64> {
    let usable = u64::try_from(usable).ok()?.checked_add(HEADER_EXTRA)?;
    QueryBudget::allocation_charge(usable, SQLITE_ALIGN as u64).ok()
}

/// Run a callback body; a panic fails closed with `fallback` instead of unwinding into C. The
/// panic payload is leaked rather than dropped, since its destructor could panic again.
fn contained<T>(fallback: T, body: impl FnOnce() -> T) -> T {
    catch_unwind(AssertUnwindSafe(body)).unwrap_or_else(|payload| {
        mem::forget(payload);
        fallback
    })
}

/// The header slot of a block handed to SQLite (address arithmetic only, no access).
fn header_of(block: *mut c_void) -> *mut Header {
    block.cast::<u8>().wrapping_sub(HEADER_BYTES).cast()
}

/// The pointer handed to SQLite for an original block (address arithmetic only, no access).
fn block_of(base: *mut Header) -> *mut c_void {
    base.cast::<u8>().wrapping_add(HEADER_BYTES).cast()
}

/// The usable bytes of the original block at `base`.
///
/// # Safety
/// `base` is a live block of the original allocator that the wrapper allocated.
unsafe fn usable(original: &Original, base: *mut Header) -> c_int {
    // SAFETY: the caller passes a live original block.
    let size = unsafe { (original.size)(base.cast()) };
    size.saturating_sub(HEADER_INT).max(0)
}

/// The original block size for a request of `bytes` usable bytes: `bytes` plus the header, or
/// `None` when the request is not positive, overflows with the header, or the original's
/// `xRoundup` refuses that size (returns 0, or less than it was asked for). A refused size is
/// never forwarded to the original allocator.
fn original_total(original: &Original, bytes: c_int) -> Option<c_int> {
    if bytes <= 0 {
        return None;
    }
    let total = bytes.checked_add(HEADER_INT)?;
    // SAFETY: the original xRoundup is a pure size computation.
    let rounded = unsafe { (original.roundup)(total) };
    (rounded >= total).then_some(total)
}

/// Grow `lease` to the charge of `usable` bytes if it holds less; `false` when refused.
fn reconcile(lease: &mut BudgetLease, usable: c_int) -> bool {
    match charge_for(usable).map(|charge| charge.saturating_sub(lease.bytes())) {
        Some(0) => true,
        Some(extra) => lease.grow_with(extra, || Ok::<(), ()>(())).is_ok(),
        None => false,
    }
}

/// `xMalloc`.
///
/// # Safety
/// Called by SQLite as its `xMalloc` hook, after [`wrap`] recorded the original allocator.
unsafe extern "C" fn x_malloc(bytes: c_int) -> *mut c_void {
    contained(ptr::null_mut(), || {
        // SAFETY: SQLite calls this as its xMalloc hook.
        unsafe { malloc(bytes) }
    })
}

/// The body of [`x_malloc`].
///
/// # Safety
/// As for [`x_malloc`].
unsafe fn malloc(bytes: c_int) -> *mut c_void {
    let Some(original) = ORIGINAL.get() else {
        return ptr::null_mut();
    };
    let (Some(total), Some(predicted)) = (original_total(original, bytes), charge_for(bytes)) else {
        return ptr::null_mut();
    };
    let Ok(mut owner) = charge_current(predicted) else {
        return ptr::null_mut();
    };
    // SAFETY: the original xMalloc accepts any positive size and returns null on failure.
    let base = unsafe { (original.malloc)(total) }.cast::<Header>();
    if base.is_null() {
        return ptr::null_mut(); // dropping `owner` releases the charge
    }
    if let Some(owner) = owner.as_mut() {
        // SAFETY: `base` is the live block just allocated.
        let actual = unsafe { usable(original, base) };
        if !reconcile(&mut owner.lease, actual) {
            // SAFETY: `base` is live and not yet handed to SQLite.
            unsafe { (original.free)(base.cast()) };
            return ptr::null_mut();
        }
    }
    // SAFETY: `base` is live, at least `HEADER_BYTES` long and 8-byte aligned (the original
    // allocator's contract, and `Header`'s alignment is at most 8); the slot is uninitialized, so
    // writing does not drop anything.
    unsafe { base.write(Header { owner }) };
    block_of(base)
}

/// `xFree`.
///
/// # Safety
/// Called by SQLite as its `xFree` hook with a block from [`x_malloc`]/[`x_realloc`] (or null).
unsafe extern "C" fn x_free(block: *mut c_void) {
    contained((), || {
        let Some(original) = ORIGINAL.get() else {
            return;
        };
        if block.is_null() {
            return;
        }
        let base = header_of(block);
        // SAFETY: `block` came from the wrapper, so `base` holds an initialized `Header` that this
        // call now owns; the block is freed next and its header never read again.
        let header = unsafe { base.read() };
        // SAFETY: `base` is the live original block.
        unsafe { (original.free)(base.cast()) };
        drop(header); // releases the owner's charge, whichever thread frees
    });
}

/// `xRealloc`.
///
/// # Safety
/// Called by SQLite as its `xRealloc` hook with a live block from the wrapper (or null).
unsafe extern "C" fn x_realloc(block: *mut c_void, bytes: c_int) -> *mut c_void {
    contained(ptr::null_mut(), || {
        if block.is_null() {
            // SAFETY: a realloc of null is an xMalloc.
            return unsafe { malloc(bytes) };
        }
        // SAFETY: SQLite passes a live wrapper block.
        unsafe { realloc(block, bytes) }
    })
}

/// The body of [`x_realloc`] for a live block.
///
/// An unscoped block is forwarded to the original `xRealloc`. A scoped block that fits keeps its
/// place (its charge already covers its full size). A scoped grow is a replacement, never a
/// destructive resize: the replacement is reserved against the owner's budget while the old block
/// keeps its own charge (the copy's peak), allocated, charged for every byte the original handed
/// out, then filled, and only then is the old block freed and its charge released. Any refusal
/// frees the replacement and leaves the old block, its bytes and its charge as they were.
///
/// # Safety
/// `block` is a live block handed out by the wrapper.
unsafe fn realloc(block: *mut c_void, bytes: c_int) -> *mut c_void {
    let Some(original) = ORIGINAL.get() else {
        return ptr::null_mut();
    };
    let Some(total) = original_total(original, bytes) else {
        return ptr::null_mut(); // the old block and its charge stay as they were
    };
    let base = header_of(block);
    // SAFETY: `block` is live, so `base` holds an initialized `Header`; SQLite does not use the
    // block concurrently with its own realloc, so nothing else reads or writes the header while
    // this shared borrow lives (it ends before the header is moved out below).
    let Some(owner) = (unsafe { &*base }).owner.as_ref() else {
        // Unscoped: forward unchanged. The header bytes (`None`) move with the block.
        // SAFETY: `base` is the live original block; on null it is left untouched.
        let moved = unsafe { (original.realloc)(base.cast(), total) }.cast::<Header>();
        return if moved.is_null() {
            ptr::null_mut()
        } else {
            block_of(moved)
        };
    };
    // SAFETY: `base` is live.
    let old_usable = unsafe { usable(original, base) };
    if bytes <= old_usable {
        // A charged shrink keeps the block in place: SQLite then sees (and the lease charges) its
        // full size, so the charge stays exact without reserving under an exhausted budget.
        return block;
    }
    let Some(predicted) = charge_for(bytes) else {
        return ptr::null_mut();
    };
    let Ok(mut lease) = owner.budget.reserve(predicted) else {
        return ptr::null_mut();
    };
    // SAFETY: the original xMalloc accepts any positive size and returns null on failure.
    let fresh = unsafe { (original.malloc)(total) }.cast::<Header>();
    if fresh.is_null() {
        return ptr::null_mut(); // dropping `lease` releases the reservation
    }
    // SAFETY: `fresh` is the live block just allocated.
    let actual = unsafe { usable(original, fresh) };
    if !reconcile(&mut lease, actual) {
        // SAFETY: `fresh` is live and was never handed to SQLite.
        unsafe { (original.free)(fresh.cast()) };
        return ptr::null_mut();
    }
    let budget = owner.budget.clone();
    let keep = usize::try_from(old_usable.min(actual)).unwrap_or(0);
    // SAFETY: both blocks are live and distinct; the old one has `old_usable` usable bytes and the
    // fresh one `actual`, so `keep` bytes are in bounds of each.
    unsafe { ptr::copy_nonoverlapping(block.cast::<u8>(), block_of(fresh).cast::<u8>(), keep) };
    // SAFETY: `fresh` is live, at least `HEADER_BYTES` long and 8-byte aligned; its header slot is
    // uninitialized, so writing drops nothing.
    unsafe {
        fresh.write(Header {
            owner: Some(Owner { budget, lease }),
        });
    };
    // SAFETY: the shared borrow of the old header ended above; `base` still holds its
    // initialized `Header`, which this read takes ownership of. The old block is freed next and
    // its header never read again.
    let old = unsafe { base.read() };
    // SAFETY: `base` is the live old original block, replaced by `fresh`.
    unsafe { (original.free)(base.cast()) };
    drop(old); // releases the old block's charge to its owner
    block_of(fresh)
}

/// `xSize`.
///
/// # Safety
/// Called by SQLite as its `xSize` hook with a live block from the wrapper (or null).
unsafe extern "C" fn x_size(block: *mut c_void) -> c_int {
    contained(0, || match ORIGINAL.get() {
        Some(original) if !block.is_null() => {
            // SAFETY: SQLite passes a live wrapper block, so its header slot starts the live
            // original block.
            unsafe { usable(original, header_of(block)) }
        }
        _ => 0,
    })
}

/// `xRoundup`: the original rounding of the request plus the header, minus the header, so that
/// `xSize(xMalloc(xRoundup(n))) == xRoundup(n)` whenever the original keeps that contract. A
/// refusal is preserved: 0 when the original's rounding refuses the size (returns 0 or less than
/// asked), when the request is not positive or overflows with the header, or on a panic. SQLite
/// then calls `xMalloc(0)` or `xRealloc(p, 0)`, which fail without forwarding anything.
///
/// # Safety
/// Called by SQLite as its `xRoundup` hook.
unsafe extern "C" fn x_roundup(bytes: c_int) -> c_int {
    contained(0, || {
        let Some(original) = ORIGINAL.get() else {
            return 0;
        };
        let Some(total) = (bytes > 0).then(|| bytes.checked_add(HEADER_INT)).flatten() else {
            return 0;
        };
        // SAFETY: the original xRoundup is a pure size computation.
        let rounded = unsafe { (original.roundup)(total) };
        if rounded < total { 0 } else { rounded - HEADER_INT }
    })
}

/// `xInit`: forwards to the original with its own `pAppData`.
///
/// # Safety
/// Called by SQLite as its `xInit` hook.
unsafe extern "C" fn x_init(_app_data: *mut c_void) -> c_int {
    contained(ffi::SQLITE_ERROR, || match ORIGINAL.get() {
        Some(Original {
            init: Some(init),
            app_data,
            ..
        }) => {
            // SAFETY: SQLite calls xInit exactly where it would call the original's.
            unsafe { init(app_data.0) }
        }
        Some(_) => ffi::SQLITE_OK,
        None => ffi::SQLITE_ERROR,
    })
}

/// `xShutdown`: forwards to the original with its own `pAppData`.
///
/// # Safety
/// Called by SQLite as its `xShutdown` hook.
unsafe extern "C" fn x_shutdown(_app_data: *mut c_void) {
    contained((), || {
        if let Some(Original {
            shutdown: Some(shutdown),
            app_data,
            ..
        }) = ORIGINAL.get()
        {
            // SAFETY: SQLite calls xShutdown exactly where it would call the original's.
            unsafe { shutdown(app_data.0) };
        }
    });
}
