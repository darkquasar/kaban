//! `QueryBudget` accounting (spec 11 QRY-04, QRY-06, PC-17): one test per behaviour.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use kaban_core::query_budget::{ALLOCATION_METADATA_BYTES, GrowError, Limit, QueryBudget};

#[test]
fn a_lease_charges_live_bytes_and_drop_releases_them() {
    let budget = QueryBudget::new(1_000, None);
    let lease = budget.reserve(100).unwrap();
    assert_eq!(lease.bytes(), 100);
    assert_eq!(budget.live_bytes(), 100);
    drop(lease);
    assert_eq!(budget.live_bytes(), 0);
}

#[test]
fn a_reservation_past_the_ceiling_is_refused_and_charges_nothing() {
    let budget = QueryBudget::new(100, None);
    let held = budget.reserve(60).unwrap();
    let err = budget.reserve(41).unwrap_err();
    assert_eq!(err.limit, Limit::Memory);
    assert_eq!(err.value, 101);
    assert_eq!(budget.live_bytes(), 60);
    let exact = budget.reserve(40).unwrap();
    assert_eq!(budget.live_bytes(), 100);
    drop((held, exact));
    assert_eq!(budget.live_bytes(), 0);
}

#[test]
fn arithmetic_overflow_is_refused_without_changing_the_count() {
    let budget = QueryBudget::new(u64::MAX, None);
    let held = budget.reserve(10).unwrap();
    let err = budget.reserve(u64::MAX).unwrap_err();
    assert_eq!(err.limit, Limit::Memory);
    assert_eq!(err.value, u64::MAX);
    assert_eq!(budget.live_bytes(), 10);
    drop(held);
    assert_eq!(budget.live_bytes(), 0);
}

#[test]
fn grow_reserves_the_delta_before_allocating_and_commits_on_success() {
    let budget = QueryBudget::new(100, None);
    let mut lease = budget.reserve(40).unwrap();
    let seen_during_alloc = lease
        .grow_with(30, || Ok::<_, ()>(budget.live_bytes()))
        .unwrap();
    assert_eq!(
        seen_during_alloc, 70,
        "the delta is charged before the allocation runs"
    );
    assert_eq!(lease.bytes(), 70);
    assert_eq!(budget.live_bytes(), 70);
    drop(lease);
    assert_eq!(budget.live_bytes(), 0);
}

#[test]
fn a_failed_grow_allocation_keeps_the_old_charge_and_releases_the_attempt() {
    let budget = QueryBudget::new(100, None);
    let mut lease = budget.reserve(40).unwrap();
    let err = lease.grow_with(30, || Err::<(), _>("oom")).unwrap_err();
    assert_eq!(err, GrowError::Alloc("oom"));
    assert_eq!(lease.bytes(), 40);
    assert_eq!(budget.live_bytes(), 40);

    let refused = lease.grow_with(61, || Ok::<_, ()>(())).unwrap_err();
    assert!(matches!(refused, GrowError::Budget(e) if e.limit == Limit::Memory));
    assert_eq!(lease.bytes(), 40);
    assert_eq!(budget.live_bytes(), 40);
    drop(lease);
    assert_eq!(budget.live_bytes(), 0);
}

#[test]
fn an_allocation_is_charged_for_capacity_alignment_and_metadata() {
    // capacity 100 rounded up to 64-byte alignment is 128, plus the metadata
    assert_eq!(
        QueryBudget::allocation_charge(100, 64).unwrap(),
        128 + ALLOCATION_METADATA_BYTES
    );
    assert_eq!(
        QueryBudget::allocation_charge(0, 8).unwrap(),
        ALLOCATION_METADATA_BYTES
    );
    assert_eq!(
        QueryBudget::allocation_charge(10, 0).unwrap_err().limit,
        Limit::Memory
    );
    assert_eq!(
        QueryBudget::allocation_charge(u64::MAX, 8)
            .unwrap_err()
            .limit,
        Limit::Memory
    );
    assert_eq!(
        QueryBudget::allocation_charge(u64::MAX - 8, 1)
            .unwrap_err()
            .limit,
        Limit::Memory,
        "the metadata addition is checked too"
    );
}

#[test]
fn check_reports_time_once_the_deadline_passes_and_keeps_the_first_limit() {
    let budget = QueryBudget::new(100, Some(Duration::ZERO));
    std::thread::sleep(Duration::from_millis(2));
    let first = budget.check().unwrap_err();
    assert_eq!(first.limit, Limit::Time);
    assert!(first.value >= 2, "elapsed milliseconds: {}", first.value);
    // a later memory breach does not replace the first exceeded limit
    let _ = budget.reserve(1_000);
    assert_eq!(budget.check().unwrap_err(), first);
    assert_eq!(
        budget.reserve(1).unwrap_err(),
        first,
        "a past deadline refuses reservations"
    );
    assert_eq!(budget.live_bytes(), 0);
}

#[test]
fn check_is_ok_within_deadline_and_ceiling() {
    let budget = QueryBudget::new(100, Some(Duration::from_secs(3600)));
    let _lease = budget.reserve(100).unwrap();
    assert_eq!(budget.check(), Ok(()));
}

#[test]
fn check_reports_the_memory_breach_as_the_first_limit() {
    let budget = QueryBudget::new(10, None);
    let err = budget.reserve(11).unwrap_err();
    assert_eq!(budget.check().unwrap_err(), err);
    assert_eq!(
        err,
        kaban_core::query_budget::BudgetExceeded {
            limit: Limit::Memory,
            value: 11
        }
    );
}

#[test]
fn each_refusal_reports_its_own_attempt_while_check_keeps_the_first() {
    let budget = QueryBudget::new(10, None);
    let first = budget.reserve(11).unwrap_err();
    let second = budget.reserve(25).unwrap_err();
    assert_eq!((first.value, second.value), (11, 25));
    assert_eq!(budget.check().unwrap_err(), first);
}

#[test]
fn cancellation_is_observed_by_every_clone_and_charges_no_more_bytes() {
    let budget = QueryBudget::new(100, None);
    let held = budget.reserve(10).unwrap();
    let worker_view = budget.clone();
    budget.cancel();
    let err = worker_view.check().unwrap_err();
    assert_eq!(err.limit, Limit::Cancelled);
    // a cancelled query cannot reserve, so it cannot go on to produce a row
    assert_eq!(worker_view.reserve(1).unwrap_err().limit, Limit::Cancelled);
    assert_eq!(budget.live_bytes(), 10);
    drop(held);
    assert_eq!(
        budget.live_bytes(),
        0,
        "leases taken before cancellation still release"
    );
}

#[test]
fn concurrent_reservations_never_exceed_the_ceiling() {
    const CEILING: u64 = 1_000;
    let budget = QueryBudget::new(CEILING, None);
    let peak = Arc::new(AtomicU64::new(0));
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let budget = budget.clone();
            let peak = Arc::clone(&peak);
            std::thread::spawn(move || {
                let mut held = Vec::new();
                for _ in 0..500 {
                    if let Ok(lease) = budget.reserve(10) {
                        peak.fetch_max(budget.live_bytes(), Ordering::SeqCst);
                        held.push(lease);
                    }
                }
                held
            })
        })
        .collect();
    let held: Vec<_> = handles
        .into_iter()
        .flat_map(|h| h.join().unwrap())
        .collect();
    assert!(
        peak.load(Ordering::SeqCst) <= CEILING,
        "peak {}",
        peak.load(Ordering::SeqCst)
    );
    assert_eq!(
        held.len(),
        100,
        "exactly the ceiling's worth of leases was granted"
    );
    assert_eq!(budget.live_bytes(), CEILING);
    drop(held);
    assert_eq!(budget.live_bytes(), 0);
}

#[test]
fn a_lease_dropped_on_another_thread_releases_exactly_once() {
    let budget = QueryBudget::new(100, None);
    let lease = budget.reserve(64).unwrap();
    std::thread::spawn(move || drop(lease)).join().unwrap();
    assert_eq!(budget.live_bytes(), 0);
    let again = budget.reserve(100).unwrap();
    assert_eq!(
        budget.live_bytes(),
        100,
        "no double release left room beyond the ceiling"
    );
    drop(again);
    assert_eq!(budget.live_bytes(), 0);
}

#[test]
fn repeated_grow_failures_and_cleanup_keep_exact_accounting() {
    let budget = QueryBudget::new(10_000, None);
    let mut lease = budget.reserve(100).unwrap();
    for _ in 0..1_000 {
        let _ = lease.grow_with(50, || Err::<(), _>(()));
        let _ = lease.grow_with(1_000_000, || Ok::<_, ()>(()));
        assert_eq!(budget.live_bytes(), 100);
    }
    lease.grow_with(25, || Ok::<_, ()>(())).unwrap();
    assert_eq!(budget.live_bytes(), 125);
    let leases: Vec<_> = (0..50).map(|_| budget.reserve(7).unwrap()).collect();
    assert_eq!(budget.live_bytes(), 125 + 350);
    drop(leases);
    drop(lease);
    assert_eq!(budget.live_bytes(), 0);
}
