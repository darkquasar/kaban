//! The request-scoped SQLite allocator adapter (spec 11 QRY-04, QRY-06, PC-17).
//!
//! The adapter wraps the allocator SQLite had before it ([`wrap`] records it from
//! `SQLITE_CONFIG_GETMALLOC`) and forwards every hook to it. Each block it hands to SQLite
//! carries a [`Header`] of [`HEADER_BYTES`] in front: the owner's [`BudgetLease`] for a block
//! allocated while a budget scope was active on the thread, or `None` for an unscoped block. The
//! lease travels with the block through `xRealloc` and is dropped by `xFree` on whatever thread
//! frees it, so a charge always returns to the budget that paid it, even after its query ended.
//!
//! Layout of one block (`base` comes from the original allocator, 8-byte aligned):
//!
//! ```text
//! base                       base + 16 (pointer SQLite sees, 8-byte aligned)
//! | Header (16 bytes)        | usable bytes (original xSize - 16)            |
//! ```
//!
//! A scoped block is charged `QueryBudget::allocation_charge(usable, 8)`, which is the usable
//! size plus `ALLOCATION_METADATA_BYTES` (the header), i.e. exactly what the original allocator
//! holds for it. The callbacks never call into SQLite, never allocate through SQLite, take no
//! lock (the scope slot is a thread-local; the recorded hooks are read with a lock-free
//! `OnceLock::get`) and never unwind into C: every callback body runs under `catch_unwind` and
//! fails closed (null, zero or nothing) if it panics.

use std::cell::RefCell;
use std::ffi::{c_int, c_void};
use std::marker::PhantomData;
use std::mem::{self, ManuallyDrop};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr;
use std::sync::OnceLock;

use kaban_core::query_budget::{ALLOCATION_METADATA_BYTES, BudgetLease, QueryBudget};
use rusqlite::ffi;

use crate::InitError;

/// SQLite's alignment requirement for allocator blocks (and the original allocator's guarantee).
const SQLITE_ALIGN: usize = 8;

/// The bytes in front of every block handed to SQLite.
const HEADER_BYTES: usize = 16;

/// [`HEADER_BYTES`] in SQLite's size type.
const HEADER_INT: c_int = 16;

/// Ownership record in front of every block: the owner's lease, or `None` for an unscoped block.
#[repr(C)]
struct Header {
    lease: Option<BudgetLease>,
}

// The header must fit its slot without disturbing SQLite's alignment, and the slot must be exactly
// the metadata `QueryBudget::allocation_charge` charges beside each allocation.
const _: () = {
    assert!(mem::size_of::<Header>() <= HEADER_BYTES);
    assert!(mem::align_of::<Header>() <= SQLITE_ALIGN);
    assert!(HEADER_BYTES.is_multiple_of(SQLITE_ALIGN));
    assert!(HEADER_BYTES as u64 == ALLOCATION_METADATA_BYTES);
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

/// While alive, SQLite allocations made on this thread are charged to one [`QueryBudget`].
/// Dropping it restores the scope that was active before; guards on one thread drop in reverse
/// order of creation. It is bound to the thread that created it.
#[derive(Debug)]
pub struct ScopeGuard {
    previous: Option<QueryBudget>,
    _thread_bound: PhantomData<*const ()>,
}

/// Charge SQLite allocations made on this thread to `budget` until the guard drops.
///
/// This is the seam the request-scoped budget scope drives; blocks keep the budget they were
/// charged to after the guard drops.
pub(crate) fn enter_scope(budget: &QueryBudget) -> ScopeGuard {
    let previous = CURRENT.with_borrow_mut(|slot| slot.replace(budget.clone()));
    ScopeGuard {
        previous,
        _thread_bound: PhantomData,
    }
}

/// Charge SQLite allocations made on this thread to `budget` until the guard drops, unless a
/// scope is already active on the thread (or its slot is unavailable): then `None`, and nothing
/// changes. A request scope enters only through this, so request scopes never nest.
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
        previous: None,
        _thread_bound: PhantomData,
    })
}

impl Drop for ScopeGuard {
    fn drop(&mut self) {
        let previous = self.previous.take();
        // During thread teardown the slot may already be gone; nothing is left to restore then.
        let _ = CURRENT.try_with(|slot| {
            if let Ok(mut slot) = slot.try_borrow_mut() {
                *slot = previous;
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
fn charge_current(bytes: u64) -> Result<Option<BudgetLease>, ()> {
    let Some(budget) = current_budget() else {
        return Ok(None);
    };
    #[cfg(feature = "test-hooks")]
    assert!(
        PANIC_NEXT_CHARGE.try_with(|flag| flag.replace(false)) != Ok(true),
        "injected panic in an allocator charge"
    );
    budget.reserve(bytes).map(Some).map_err(drop)
}

/// The charge for a block with `usable` bytes, or `None` on a negative size or overflow.
fn charge_for(usable: c_int) -> Option<u64> {
    let usable = u64::try_from(usable).ok()?;
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
    let (Some(total), Some(predicted)) = (bytes.checked_add(HEADER_INT), charge_for(bytes)) else {
        return ptr::null_mut();
    };
    let Ok(mut lease) = charge_current(predicted) else {
        return ptr::null_mut();
    };
    // SAFETY: the original xMalloc accepts any positive size and returns null on failure.
    let base = unsafe { (original.malloc)(total) }.cast::<Header>();
    if base.is_null() {
        return ptr::null_mut(); // dropping `lease` releases the charge
    }
    if let Some(lease) = lease.as_mut() {
        // SAFETY: `base` is the live block just allocated.
        let actual = unsafe { usable(original, base) };
        if !reconcile(lease, actual) {
            // SAFETY: `base` is live and not yet handed to SQLite.
            unsafe { (original.free)(base.cast()) };
            return ptr::null_mut();
        }
    }
    // SAFETY: `base` is live, at least `HEADER_BYTES` long and 8-byte aligned (the original
    // allocator's contract, and `Header`'s alignment is at most 8); the slot is uninitialized, so
    // writing does not drop anything.
    unsafe { base.write(Header { lease }) };
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
/// # Safety
/// `block` is a live block handed out by the wrapper.
unsafe fn realloc(block: *mut c_void, bytes: c_int) -> *mut c_void {
    let Some(original) = ORIGINAL.get() else {
        return ptr::null_mut();
    };
    let Some(total) = bytes.checked_add(HEADER_INT) else {
        return ptr::null_mut();
    };
    let base = header_of(block);
    // SAFETY: `block` is live, so `base` holds an initialized `Header`. The owner is moved out
    // here and written back to exactly one live block on every path below; `ManuallyDrop` keeps
    // an unwind from dropping it while the header bytes still name it.
    let mut owner = ManuallyDrop::new(unsafe { base.read() });
    let Some(lease) = owner.lease.as_mut() else {
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
        // SAFETY: `base` is still the live block; its header slot holds the bytes read above.
        unsafe { base.write(ManuallyDrop::into_inner(owner)) };
        return block;
    }
    let grown = charge_for(bytes).ok_or(()).and_then(|predicted| {
        let extra = predicted.saturating_sub(lease.bytes());
        lease
            .grow_with(extra, || {
                // SAFETY: `base` is the live original block; on null it is left untouched,
                // otherwise it moved (header bytes included) to the returned block.
                let moved = unsafe { (original.realloc)(base.cast(), total) };
                if moved.is_null() {
                    Err(())
                } else {
                    Ok(moved.cast::<Header>())
                }
            })
            .map_err(drop)
    });
    if let Ok(moved) = grown {
        // An original allocator that hands out more than requested is charged for it; if the
        // budget refuses that slack, the block keeps the charge for the requested size.
        // SAFETY: `moved` is the live, moved block.
        let actual = unsafe { usable(original, moved) };
        let _ = reconcile(lease, actual);
        // SAFETY: `moved` is live and aligned; its header slot holds a stale bitwise copy of
        // the owner moved out above, which is overwritten without being dropped.
        unsafe { moved.write(ManuallyDrop::into_inner(owner)) };
        block_of(moved)
    } else {
        // SAFETY: the grow was refused or the original realloc failed, so `base` is still the
        // live old block; restore its owner (the lease kept its old charge).
        unsafe { base.write(ManuallyDrop::into_inner(owner)) };
        ptr::null_mut()
    }
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
/// `xSize(xMalloc(xRoundup(n))) == xRoundup(n)` whenever the original keeps that contract.
///
/// # Safety
/// Called by SQLite as its `xRoundup` hook.
unsafe extern "C" fn x_roundup(bytes: c_int) -> c_int {
    contained(bytes, || {
        match (ORIGINAL.get(), bytes.checked_add(HEADER_INT)) {
            (Some(original), Some(total)) => {
                // SAFETY: the original xRoundup is a pure size computation.
                let rounded = unsafe { (original.roundup)(total) };
                rounded.saturating_sub(HEADER_INT).max(bytes)
            }
            _ => bytes,
        }
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
