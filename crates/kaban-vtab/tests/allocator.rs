//! The allocator adapter charges scoped SQLite allocations to the budget that owns them and keeps
//! SQLite's allocator contract (spec 11 T12 allocator RED/GREEN, QRY-04, QRY-06). Each case runs
//! in a fresh process with the adapter installed before any SQLite entry.
#![allow(clippy::unwrap_used)]

mod fresh;

use std::sync::{Arc, Barrier};
use std::thread;

use fresh::{Block, configured_allocator, fresh_test, initialize_sqlite};
use kaban_core::query_budget::QueryBudget;
use kaban_vtab::{initialize_for_embedding, test_hooks};
use rusqlite::Connection;

const SIZES: &[u64] = &[1, 7, 8, 9, 24, 100, 4096, 100_000];

/// Install the adapter, then initialize SQLite outside any scope so its process-wide state is
/// never charged to a test budget.
fn install() {
    // SAFETY: the case runs in a fresh process, on one thread, before any SQLite entry, and
    // nothing replaces the allocator or shuts SQLite down afterwards.
    unsafe { initialize_for_embedding() }.unwrap();
    initialize_sqlite();
}

/// What one live block of `size` usable bytes must be charged.
fn charge(size: u64) -> u64 {
    QueryBudget::allocation_charge(size, 8).unwrap()
}

fn round8(n: u64) -> u64 {
    n.next_multiple_of(8)
}

/// Open a connection, write and read some rows, close it.
fn workload(rows: i64) {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch("CREATE TABLE t(x INTEGER, y TEXT)").unwrap();
    for i in 0..rows {
        conn.execute("INSERT INTO t VALUES (?1, printf('%0100d', ?1))", [i])
            .unwrap();
    }
    let count: i64 = conn
        .query_row("SELECT count(*) FROM t WHERE y LIKE '%1%'", [], |row| row.get(0))
        .unwrap();
    assert!(count > 0);
}

fresh_test! {
    fn scoped_blocks_are_aligned_sized_and_charged_exactly() {
        install();
        let budget = QueryBudget::new(u64::MAX, None);
        let _scope = test_hooks::scope(&budget);
        for &n in SIZES {
            let block = Block::malloc(n).unwrap();
            assert_eq!(block.addr() % 8, 0, "block of {n} bytes is not 8-byte aligned");
            assert_eq!(block.size(), round8(n), "usable size of a {n}-byte request");
            assert_eq!(budget.live_bytes(), charge(block.size()), "charge for {n} bytes");
            block.free();
            assert_eq!(budget.live_bytes(), 0, "free released the {n}-byte charge");
        }
    }
}

fresh_test! {
    fn the_wrapper_keeps_the_original_roundup_and_size_contract() {
        let original = configured_allocator().unwrap();
        // SAFETY: fresh process, single thread, no SQLite entry yet.
        unsafe { initialize_for_embedding() }.unwrap();
        let wrapper = configured_allocator().unwrap();
        let (malloc, free, size, roundup) = (
            wrapper.xMalloc.unwrap(),
            wrapper.xFree.unwrap(),
            wrapper.xSize.unwrap(),
            wrapper.xRoundup.unwrap(),
        );
        for n in [1, 7, 8, 9, 24, 100, 4096, 100_000] {
            // SAFETY: the hooks are SQLite's allocator methods; each block is measured while live
            // and freed once.
            unsafe {
                let rounded = roundup(n);
                assert!(rounded >= n);
                assert_eq!(rounded, original.xRoundup.unwrap()(n), "roundup of {n}");
                let block = malloc(rounded);
                assert!(!block.is_null());
                assert_eq!(block as usize % 8, 0);
                assert_eq!(size(block), rounded, "xSize of a block of xRoundup({n})");
                free(block);
            }
        }
    }
}

fresh_test! {
    fn a_failed_grow_returns_null_and_keeps_the_old_block_and_its_charge() {
        install();
        let budget = QueryBudget::new(charge(64) + 100, None);
        let _scope = test_hooks::scope(&budget);
        let mut block = Block::malloc(64).unwrap();
        block.fill(64, 0xA5);
        let (addr, size, live) = (block.addr(), block.size(), budget.live_bytes());
        assert_eq!(live, charge(64));

        assert!(!block.realloc(10_000), "a grow past the budget must fail");

        assert_eq!((block.addr(), block.size()), (addr, size));
        assert!(block.holds(64, 0xA5), "the old block's bytes changed");
        assert_eq!(budget.live_bytes(), live, "a failed grow changed the charge");
        block.free();
        assert_eq!(budget.live_bytes(), 0);
    }
}

fresh_test! {
    fn a_grow_charges_the_difference_and_moves_the_charge_with_the_block() {
        install();
        let budget = QueryBudget::new(u64::MAX, None);
        let _scope = test_hooks::scope(&budget);
        let mut block = Block::malloc(100).unwrap();
        block.fill(100, 0x3C);
        assert!(block.realloc(50_000));
        assert!(block.holds(100, 0x3C));
        assert_eq!(block.size(), round8(50_000));
        assert_eq!(budget.live_bytes(), charge(block.size()));
        block.free();
        assert_eq!(budget.live_bytes(), 0);
    }
}

fresh_test! {
    fn a_shrink_is_charged_exactly_for_the_block_it_leaves() {
        install();
        let budget = QueryBudget::new(u64::MAX, None);
        let _scope = test_hooks::scope(&budget);
        let mut block = Block::malloc(10_000).unwrap();
        block.fill(100, 0x5A);
        assert!(block.realloc(100));
        assert!(block.size() >= 100);
        assert!(block.holds(100, 0x5A));
        assert_eq!(budget.live_bytes(), charge(block.size()), "shrunk block's charge");
        block.free();
        assert_eq!(budget.live_bytes(), 0);
    }
}

fresh_test! {
    fn a_block_freed_on_another_thread_releases_its_owners_charge() {
        install();
        let owner = QueryBudget::new(u64::MAX, None);
        let scope = test_hooks::scope(&owner);
        let block = Block::malloc(1000).unwrap();
        drop(scope);
        assert_eq!(owner.live_bytes(), charge(1000));

        let bystander = QueryBudget::new(u64::MAX, None);
        let in_thread = bystander.clone();
        thread::spawn(move || {
            let _other_scope = test_hooks::scope(&in_thread);
            block.free();
        })
        .join()
        .unwrap();

        assert_eq!(owner.live_bytes(), 0, "the owner's charge was not released");
        assert_eq!(bystander.live_bytes(), 0, "the freeing thread's budget was touched");
    }
}

fresh_test! {
    fn a_block_outliving_its_scope_stays_owned_by_its_budget() {
        install();
        let owner = QueryBudget::new(u64::MAX, None);
        let scope = test_hooks::scope(&owner);
        let mut block = Block::malloc(200).unwrap();
        drop(scope);

        let later = QueryBudget::new(u64::MAX, None);
        let later_scope = test_hooks::scope(&later);
        assert!(block.realloc(20_000));
        assert_eq!(owner.live_bytes(), charge(block.size()), "growth after the scope");
        assert_eq!(later.live_bytes(), 0, "the current scope took a block it does not own");
        drop(later_scope);

        block.free();
        assert_eq!(owner.live_bytes(), 0);
    }
}

fresh_test! {
    fn closing_a_scoped_connection_releases_everything_it_charged() {
        install();
        let budget = QueryBudget::new(u64::MAX, None);
        let _scope = test_hooks::scope(&budget);
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE t(x); INSERT INTO t VALUES (1), (2), (3);").unwrap();
        assert!(budget.live_bytes() > 0);
        drop(conn);
        assert_eq!(budget.live_bytes(), 0, "a closed connection left charged blocks");
    }
}

fresh_test! {
    fn simultaneous_queries_and_writers_keep_separate_accounting() {
        install();
        let barrier = Arc::new(Barrier::new(3));
        let query = |size: u64, count: usize, barrier: Arc<Barrier>| {
            thread::spawn(move || {
                let budget = QueryBudget::new(u64::MAX, None);
                let _scope = test_hooks::scope(&budget);
                workload(200);
                let after_close = budget.live_bytes();
                let blocks: Vec<Block> =
                    (0..count).map(|_| Block::malloc(size).unwrap()).collect();
                barrier.wait();
                let held = budget.live_bytes();
                for block in blocks {
                    block.free();
                }
                (after_close, held, budget.live_bytes())
            })
        };
        let a = query(40, 50, Arc::clone(&barrier));
        let b = query(4000, 7, Arc::clone(&barrier));
        let writer_barrier = Arc::clone(&barrier);
        let writer = thread::spawn(move || {
            // An unscoped writer: forwarded, charged to nobody.
            workload(300);
            writer_barrier.wait();
        });
        let (a_closed, a_held, a_end) = a.join().unwrap();
        let (b_closed, b_held, b_end) = b.join().unwrap();
        writer.join().unwrap();

        assert_eq!((a_closed, b_closed), (0, 0), "closed connections left charges");
        assert_eq!(a_held, 50 * charge(40));
        assert_eq!(b_held, 7 * charge(4000));
        assert_eq!((a_end, b_end), (0, 0));
    }
}

fresh_test! {
    fn unscoped_allocations_are_forwarded_and_uncharged() {
        install();
        let budget = QueryBudget::new(u64::MAX, None);
        let elsewhere = budget.clone();
        // A scope on another thread does not reach this one.
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        let other = thread::spawn(move || {
            let _scope = test_hooks::scope(&elsewhere);
            ready_tx.send(()).unwrap();
            done_rx.recv().unwrap();
        });
        ready_rx.recv().unwrap();
        for &n in SIZES {
            let mut block = Block::malloc(n).unwrap();
            assert_eq!(block.addr() % 8, 0);
            assert_eq!(block.size(), round8(n));
            assert!(block.realloc(n + 1000));
            assert_eq!(block.size(), round8(n + 1000));
            assert!(block.realloc(1));
            block.free();
        }
        assert_eq!(budget.live_bytes(), 0);
        done_tx.send(()).unwrap();
        other.join().unwrap();
    }
}

#[cfg(feature = "test-hooks")]
fresh_test! {
    fn a_panicking_charge_fails_the_allocation_without_unwinding_into_sqlite() {
        install();
        let budget = QueryBudget::new(u64::MAX, None);
        let _scope = test_hooks::scope(&budget);
        let prior = Block::malloc(16).unwrap();
        let prior_live = budget.live_bytes();

        test_hooks::panic_on_next_charge();
        let refused = Block::malloc(64);

        assert!(refused.is_none(), "an allocation whose charge panicked must fail");
        assert_eq!(budget.live_bytes(), prior_live);
        let after = Block::malloc(64).unwrap();
        assert_eq!(budget.live_bytes(), prior_live + charge(64));
        after.free();
        prior.free();
        assert_eq!(budget.live_bytes(), 0);
    }
}
