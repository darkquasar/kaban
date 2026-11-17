//! Operation-local test instrumentation of the metered loops (PC-17).
//!
//! [`watch`] runs one call with a probe installed for one budget on the calling thread: every
//! unit a ticker on that budget counts is attributed to its [`Phase`], the probe can cancel the
//! budget at the n-th unit of a named phase, and it records which phase's check first refused
//! and how many units were still attempted after that refusal. Work on other threads, or on
//! other budgets, is invisible to it, so concurrent tests (a threaded harness) cannot disturb a
//! cancellation trigger or a count.

use std::cell::RefCell;
use std::collections::BTreeMap;

use kaban_core::query_budget::QueryBudget;

use crate::charge::Phase;

/// What one watched call did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Report {
    /// Units counted per phase.
    pub(crate) units: BTreeMap<Phase, u64>,
    /// The phase in which the probe cancelled the budget, if it did.
    pub(crate) cancelled_in: Option<Phase>,
    /// The phase whose budget check first refused, if one did.
    pub(crate) refused_in: Option<Phase>,
    /// Units attempted on a ticker after it had refused.
    pub(crate) after_refusal: u64,
}

impl Report {
    /// Units counted in `phase`.
    pub(crate) fn units_in(&self, phase: Phase) -> u64 {
        self.units.get(&phase).copied().unwrap_or(0)
    }

    /// Units counted over every phase.
    pub(crate) fn total(&self) -> u64 {
        self.units.values().sum()
    }
}

struct Watch {
    /// The address of the watched budget: only its tickers are counted.
    budget: usize,
    cancel_at: Option<(Phase, u64)>,
    report: Report,
}

thread_local! {
    static WATCH: RefCell<Option<Watch>> = const { RefCell::new(None) };
}

fn address(budget: &QueryBudget) -> usize {
    std::ptr::from_ref(budget) as usize
}

/// Run `call` with a probe on `budget`; with `cancel_at = Some((phase, n))` the probe cancels
/// `budget` right after the n-th unit (counting from 1) of `phase`.
pub(crate) fn watch<T>(
    budget: &QueryBudget,
    cancel_at: Option<(Phase, u64)>,
    call: impl FnOnce() -> T,
) -> (T, Report) {
    WATCH.with_borrow_mut(|w| {
        *w = Some(Watch {
            budget: address(budget),
            cancel_at,
            report: Report::default(),
        });
    });
    let out = call();
    let report = WATCH
        .with_borrow_mut(Option::take)
        .map(|w| w.report)
        .unwrap_or_default();
    (out, report)
}

fn with(budget: Option<&QueryBudget>, f: impl FnOnce(&mut Watch)) {
    let Some(budget) = budget else { return };
    WATCH.with_borrow_mut(|w| {
        if let Some(w) = w
            && w.budget == address(budget)
        {
            f(w);
        }
    });
}

/// A unit of `phase` was counted.
pub(crate) fn unit(budget: Option<&QueryBudget>, phase: Phase) {
    with(budget, |w| {
        let n = w.report.units.entry(phase).or_insert(0);
        *n += 1;
        if w.cancel_at == Some((phase, *n))
            && let Some(budget) = budget
        {
            budget.cancel();
            w.report.cancelled_in = Some(phase);
        }
    });
}

/// The check before a unit of `phase` refused.
pub(crate) fn refused(budget: &QueryBudget, phase: Phase) {
    with(Some(budget), |w| {
        w.report.refused_in.get_or_insert(phase);
    });
}

/// A unit of `phase` was attempted on a ticker that had already refused.
pub(crate) fn after_refusal(budget: Option<&QueryBudget>, _phase: Phase) {
    with(budget, |w| w.report.after_refusal += 1);
}

mod tests {
    #![allow(clippy::unwrap_used)] // test code: failures are the assertion

    use super::*;
    use crate::charge::Ticker;
    use crate::error::GraphError;

    #[test]
    fn a_watch_counts_only_its_own_budget_and_cancels_at_the_named_unit() {
        let watched = QueryBudget::new(u64::MAX, None);
        let other = QueryBudget::new(u64::MAX, None);
        let (got, report) = watch(&watched, Some((Phase::Sort, 300)), || {
            let t = Ticker::new(&watched);
            let u = Ticker::new(&other);
            for _ in 0..10 {
                u.tick(Phase::Sort)?;
                t.tick(Phase::Copy)?;
            }
            for _ in 0..1000 {
                t.tick(Phase::Sort)?;
            }
            Ok::<_, GraphError>(())
        });
        assert!(matches!(
            got,
            Err(GraphError::BudgetExceeded {
                limit: "cancelled",
                ..
            })
        ));
        assert_eq!(
            report.units_in(Phase::Copy),
            10,
            "the other budget is not counted"
        );
        assert_eq!(report.cancelled_in, Some(Phase::Sort));
        // Cancelled after Sort unit 300 (ticker unit 310): the check before ticker unit 512.
        assert_eq!(report.units_in(Phase::Sort), 502);
        assert_eq!(report.total(), 512);
        assert_eq!(report.refused_in, Some(Phase::Sort));
        assert_eq!(report.after_refusal, 0);
        assert!(
            other.check().is_ok(),
            "only the watched budget is cancelled"
        );
    }
}
