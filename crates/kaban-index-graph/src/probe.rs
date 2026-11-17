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

/// The owner's cancellation rule (END-MAJ-02/03, POST-MAJ-01, POST-MED-03/04): every internal
/// loop over input-sized data checks the budget from inside. Each row names one loop (its
/// [`Phase`]), cancels the budget at a unit of that loop of one invocation, and requires that
/// the same invocation refuses from that loop's own check, attempts no further unit and leaves
/// nothing charged. Removing the loop's metering makes its row fail: either the loop is never
/// seen (no unit of it is counted) or a later loop's check refuses instead.
mod cancellation {
    #![allow(clippy::unwrap_used)] // test code: failures are the assertion

    use kaban_core::query_budget::QueryBudget;
    use tantivy::directory::OwnedBytes;

    use super::watch;
    use crate::charge::{Grow, Phase};
    use crate::error::GraphError;
    use crate::id::{Direction, LocalId, RelId, RelSelection};
    use crate::op::{Attr, EdgeOp, FactOp, NodeOp, OpKind, Records};
    use crate::paths::shortest_path;
    use crate::sediment::{EMPTY, PreparedSediment};
    use crate::structure::{topo_order, wcc};
    use crate::traverse::{Caps, RowCap, expand};
    use crate::view::View;
    use crate::{Base, Layer, build_base, seal};

    /// Records per fixture: every loop is many check intervals long.
    const N: u32 = 4_000;

    fn node(id: u32) -> NodeOp {
        NodeOp {
            node: LocalId(id),
            kind: OpKind::Add,
        }
    }

    fn edge(src: u32, dst: u32, kind: OpKind) -> EdgeOp {
        EdgeOp {
            src: LocalId(src),
            rel: RelId(0),
            dst: LocalId(dst),
            kind,
            attr: Attr::default(),
        }
    }

    fn fact(id: u32) -> FactOp {
        FactOp {
            node: LocalId(id),
            key: 0,
            kind: OpKind::Add,
            payload: b"x".to_vec(),
        }
    }

    /// A chain `by + n > ... > by` in descending order, so a run has to sort it.
    fn descending(by: u32, n: u32) -> Records {
        Records {
            nodes: (0..=n).rev().map(|m| node(by + m)).collect(),
            edges: (1..=n)
                .rev()
                .map(|m| edge(by + m, by + m - 1, OpKind::Add))
                .collect(),
            ..Records::default()
        }
    }

    fn base(r: &Records, domain: u32) -> Base {
        let free = QueryBudget::new(u64::MAX, None);
        Base::open(OwnedBytes::new(build_base(r, domain)), &free).unwrap()
    }

    /// `layers` layers sealed one over the other from `r`, over `under`.
    fn layers(under: Option<&Base>, r: &Records, count: usize) -> Vec<Layer> {
        let free = QueryBudget::new(u64::MAX, None);
        let mut layers: Vec<Layer> = Vec::new();
        for _ in 0..count {
            let bytes = seal(&View::new(under, &layers, &EMPTY, &free).unwrap(), r);
            layers.push(Layer::open(OwnedBytes::new(bytes), &free).unwrap());
        }
        layers
    }

    fn drain<T>(it: impl Iterator<Item = Result<T, GraphError>>) -> Result<(), GraphError> {
        for item in it {
            item?;
        }
        Ok(())
    }

    /// Where a row cancels: at the n-th unit of its loop, or `k` units before the loop's last
    /// unit (counted by one unbudgeted run of the same call).
    #[derive(Clone, Copy)]
    enum At {
        Unit(u64),
        FromEnd(u64),
    }

    type Call<'a> = Box<dyn Fn(&QueryBudget) -> Result<(), GraphError> + 'a>;

    /// Why the row `what` fails, if it does: the loop `phase` must be entered, cancelled
    /// there, refuse from its own check, attempt nothing more and release every lease.
    fn stops_inside(what: &str, phase: Phase, at: At, call: &Call<'_>) -> Option<String> {
        let nth = match at {
            At::Unit(n) => n,
            At::FromEnd(k) => {
                let free = QueryBudget::new(u64::MAX, None);
                let (got, report) = watch(&free, None, || call(&free));
                let units = report.units_in(phase);
                if got.is_err() || units <= k {
                    return Some(format!(
                        "{what}: the unbudgeted run gave {got:?} with {units} units of {phase:?}"
                    ));
                }
                units - k
            }
        };
        let budget = QueryBudget::new(u64::MAX, None);
        let (got, report) = watch(&budget, Some((phase, nth)), || call(&budget));
        let cancelled = matches!(
            got,
            Err(GraphError::BudgetExceeded {
                limit: "cancelled",
                ..
            })
        );
        drop(got);
        if report.cancelled_in != Some(phase) {
            Some(format!(
                "{what}: the {phase:?} loop never reached unit {nth} (units {:?})",
                report.units
            ))
        } else if !cancelled {
            Some(format!("{what}: the call did not refuse"))
        } else if report.refused_in != Some(phase) {
            Some(format!(
                "{what}: refused by {:?}, not inside the cancelled {phase:?} loop",
                report.refused_in
            ))
        } else if report.after_refusal != 0 {
            Some(format!(
                "{what}: {} units attempted after the refusal",
                report.after_refusal
            ))
        } else if budget.live_bytes() != 0 {
            Some(format!(
                "{what}: {} bytes still charged after the refusal",
                budget.live_bytes()
            ))
        } else {
            None
        }
    }

    #[test]
    fn cancellation_stops_every_internal_loop() {
        let free = QueryBudget::new(u64::MAX, None);
        let all = RelSelection::All;
        let none = RelSelection::only(Vec::new());

        // Sediment inputs.
        let blob = descending(0, N).encode();
        let batch = [descending(0, N)];
        let empty_batches = vec![Records::default(); N as usize];
        let fact_batches: Vec<Records> = (0..N)
            .map(|i| Records {
                facts: vec![fact(i)],
                ..Records::default()
            })
            .collect();
        let duplicates = [Records {
            nodes: (0..N).map(|_| node(1)).collect(),
            ..Records::default()
        }];
        let one_run = PreparedSediment::prepare(&batch, &free).unwrap();
        // An equal-sized next batch: the extension merges it with the existing run.
        let next = [descending(N + 1, N)];
        // Three runs that stay apart (each under half the size of the one before).
        let mut runs = PreparedSediment::prepare(&batch, &free).unwrap();
        for (by, n) in [(N + 1, N / 4), (2 * N + 2, N / 16)] {
            runs = runs.extend(&[descending(by, n)], &free).unwrap();
        }
        assert_eq!(runs.runs(), 3, "the sealing row merges several runs");
        let parent = View::new(None, &[], &runs, &free).unwrap();

        // Extents.
        let base_bytes = OwnedBytes::new(build_base(&descending(0, N), 0));
        // 300 empty layers: empty presence and edge indexes, a union over each.
        let empty_chain = layers(None, &Records::default(), 300);
        let empty_view = View::new(None, &empty_chain, &EMPTY, &free).unwrap();
        // 32 layers tombstoning the same 1,000 edges of node 0: every key is in every source.
        let hub_base = base(
            &Records {
                nodes: (0..=1_000).map(node).collect(),
                ..Records::default()
            },
            0,
        );
        let tombstones = Records {
            edges: (1..=1_000).map(|m| edge(0, m, OpKind::Remove)).collect(),
            ..Records::default()
        };
        let overlapping_layers = layers(Some(&hub_base), &tombstones, 32);
        let overlapping = View::new(Some(&hub_base), &overlapping_layers, &EMPTY, &free).unwrap();
        // A present hub whose N edges all filter out: absent endpoints in the base, re-added
        // by a layer that tombstones every edge.
        let filtered_base = base(
            &Records {
                nodes: vec![node(0)],
                edges: (1..=N).map(|m| edge(0, m, OpKind::Add)).collect(),
                ..Records::default()
            },
            0,
        );
        let filtered_layers = layers(
            Some(&filtered_base),
            &Records {
                nodes: (1..=N).map(node).collect(),
                edges: (1..=N).map(|m| edge(0, m, OpKind::Remove)).collect(),
                ..Records::default()
            },
            1,
        );
        let filtered = View::new(Some(&filtered_base), &filtered_layers, &EMPTY, &free).unwrap();
        // A bitmap presence whose population sits at the top of the domain: a long run of
        // empty words before the first entry.
        let words_base = base(
            &Records {
                nodes: ((1 << 18) - 10_000..1 << 18).map(node).collect(),
                ..Records::default()
            },
            1 << 18,
        );
        let words = View::new(Some(&words_base), &[], &EMPTY, &free).unwrap();

        // Structural inputs, as prepared sediment.
        let isolated = PreparedSediment::prepare(
            &[Records {
                nodes: (0..N).map(node).collect(),
                ..Records::default()
            }],
            &free,
        )
        .unwrap();
        let isolated = View::new(None, &[], &isolated, &free).unwrap();
        let sources: Vec<LocalId> = (0..N).rev().map(LocalId).collect();
        let twice: Vec<LocalId> = (0..N).flat_map(|m| [LocalId(m), LocalId(m)]).collect();
        let wide = Caps {
            depth: 1,
            rows: 10 * N,
        };
        // 0 fans out to 1..=N, each of which reaches N + 1.
        let fan = PreparedSediment::prepare(
            &[Records {
                nodes: (0..=N + 1).map(node).collect(),
                edges: (1..=N)
                    .flat_map(|m| [edge(0, m, OpKind::Add), edge(m, N + 1, OpKind::Add)])
                    .collect(),
                ..Records::default()
            }],
            &free,
        )
        .unwrap();
        let fan = View::new(None, &[], &fan, &free).unwrap();
        // A chain 0 > ... > N - 1 that releases, then a two-node cycle N <> N + 1 left over:
        // the cycle search scans N released in-degrees first.
        let cyclic = PreparedSediment::prepare(
            &[Records {
                nodes: (0..N + 2).map(node).collect(),
                edges: (1..N)
                    .map(|m| edge(m - 1, m, OpKind::Add))
                    .chain([edge(N, N + 1, OpKind::Add), edge(N + 1, N, OpKind::Add)])
                    .collect(),
                ..Records::default()
            }],
            &free,
        )
        .unwrap();
        let cyclic = View::new(None, &[], &cyclic, &free).unwrap();
        let cyclic_sources: Vec<LocalId> = (0..N + 2).map(LocalId).collect();
        // Nodes 0..=N in a chain whose union-find forest becomes a path N > N-1 > ... > 0
        // (each helper h_k links k-1 then k, in descending k); the last node z links to N, and
        // its find walks the whole path: the tail of wcc's work.
        let helper = |k: u32| N + 1 + (N - k);
        let z = 2 * N + 1;
        let forest = PreparedSediment::prepare(
            &[Records {
                nodes: (0..=N)
                    .chain((1..=N).map(helper))
                    .chain([z])
                    .map(node)
                    .collect(),
                edges: (1..=N)
                    .flat_map(|k| {
                        [
                            edge(helper(k), k - 1, OpKind::Add),
                            edge(helper(k), k, OpKind::Add),
                        ]
                    })
                    .chain([edge(z, N, OpKind::Add)])
                    .collect(),
                ..Records::default()
            }],
            &free,
        )
        .unwrap();
        let forest = View::new(None, &[], &forest, &free).unwrap();

        let first = At::Unit(1);
        let rows: Vec<(&str, Phase, At, Call<'_>)> = vec![
            (
                "decoding",
                Phase::Decode,
                first,
                Box::new(|b| Records::decode(&blob, b).map(|_| ())),
            ),
            (
                "extent validation",
                Phase::Validate,
                first,
                Box::new(|b| Base::open(base_bytes.clone(), b).map(|_| ())),
            ),
            (
                "counting the records of empty batches",
                Phase::CountBatches,
                first,
                Box::new(|b| PreparedSediment::prepare(&empty_batches, b).map(|_| ())),
            ),
            (
                "sizing fact payloads",
                Phase::SizePayload,
                first,
                Box::new(|b| PreparedSediment::prepare(&fact_batches, b).map(|_| ())),
            ),
            (
                "sizing the payloads of empty batches",
                Phase::SizePayload,
                first,
                Box::new(|b| PreparedSediment::prepare(&empty_batches, b).map(|_| ())),
            ),
            (
                "copying empty batches",
                Phase::Copy,
                first,
                Box::new(|b| PreparedSediment::prepare(&empty_batches, b).map(|_| ())),
            ),
            (
                "copying records into a run",
                Phase::Copy,
                first,
                Box::new(|b| PreparedSediment::prepare(&batch, b).map(|_| ())),
            ),
            (
                "sorting a run",
                Phase::Sort,
                first,
                Box::new(|b| PreparedSediment::prepare(&batch, b).map(|_| ())),
            ),
            (
                "compacting a run",
                Phase::Compact,
                first,
                Box::new(|b| PreparedSediment::prepare(&duplicates, b).map(|_| ())),
            ),
            (
                "counting a run's barriers",
                Phase::CountBarriers,
                first,
                Box::new(|b| PreparedSediment::prepare(&batch, b).map(|_| ())),
            ),
            (
                "copying a run's barriers",
                Phase::Barriers,
                first,
                Box::new(|b| PreparedSediment::prepare(&batch, b).map(|_| ())),
            ),
            (
                "transposing a run's edges",
                Phase::Transpose,
                first,
                Box::new(|b| PreparedSediment::prepare(&batch, b).map(|_| ())),
            ),
            (
                "an extension's run merge",
                Phase::Merge,
                first,
                Box::new(|b| one_run.extend(&next, b).map(|_| ())),
            ),
            (
                "sealing's merge of several runs",
                Phase::Merge,
                first,
                Box::new(|b| runs.seal(&parent, runs.batches(), b).map(|_| ())),
            ),
            (
                "union coverage over empty edge indexes",
                Phase::UnionCoverage,
                first,
                Box::new(|b| View::new(None, &empty_chain, &EMPTY, b).map(|_| ())),
            ),
            (
                "preparing a cursor per layer",
                Phase::Cursor,
                first,
                Box::new(|b| drain(empty_view.nodes(b))),
            ),
            (
                "setting up a merge's heads over many layers",
                Phase::MergeSetup,
                first,
                Box::new(|b| drain(empty_view.nodes(b))),
            ),
            (
                "priming a merge over many empty layers",
                Phase::MergePrime,
                first,
                Box::new(|b| drain(empty_view.nodes(b))),
            ),
            (
                "the accessor's merge over 33 overlapping sources",
                Phase::Merge,
                first,
                Box::new(|b| drain(overlapping.neighbours(LocalId(0), &all, Direction::Out, b))),
            ),
            (
                "an all-filtered neighbour scan",
                Phase::Merge,
                first,
                Box::new(|b| drain(filtered.neighbours(LocalId(0), &all, Direction::Out, b))),
            ),
            (
                "a low-output traversal over an all-filtered hub",
                Phase::Merge,
                first,
                Box::new(|b| {
                    expand(&filtered, &[LocalId(0)], &all, Direction::Out, wide, b).map(|_| ())
                }),
            ),
            (
                "a presence scan over empty bitmap words",
                Phase::Merge,
                first,
                Box::new(|b| drain(words.nodes(b))),
            ),
            (
                "moving a growing buffer",
                Phase::Grow,
                At::FromEnd(400),
                Box::new(|b| {
                    let mut v: Grow<'_, [u64; 8]> = Grow::new(b)?;
                    for i in 0..1u64 << 16 {
                        v.push([i; 8])?;
                    }
                    Ok(())
                }),
            ),
            (
                "a search's frontier",
                Phase::Search,
                first,
                Box::new(|b| {
                    expand(&isolated, &sources, &none, Direction::Out, wide, b).map(|_| ())
                }),
            ),
            (
                "deduplicating a search's sources",
                Phase::Dedup,
                first,
                Box::new(|b| expand(&isolated, &twice, &none, Direction::Out, wide, b).map(|_| ())),
            ),
            (
                "shortest-path initialisation",
                Phase::Init,
                first,
                Box::new(|b| {
                    let caps = Caps {
                        depth: 3,
                        rows: 10 * N,
                    };
                    shortest_path(&fan, LocalId(0), LocalId(N + 1), &all, caps, b).map(|_| ())
                }),
            ),
            (
                "the union-find forest initialisation",
                Phase::Init,
                first,
                Box::new(|b| wcc(&isolated, &none, RowCap(1), b).map(|_| ())),
            ),
            (
                "seeding a topological order",
                // The in-degree loop counts one unit per node first.
                Phase::Topo,
                At::Unit(u64::from(N) + 1),
                Box::new(|b| topo_order(&isolated, &sources, &none, wide, b).map(|_| ())),
            ),
            (
                "searching for an unreleased node",
                Phase::Cycle,
                first,
                Box::new(
                    |b| match topo_order(&cyclic, &cyclic_sources, &all, wide, b) {
                        Err(GraphError::Cycle { .. }) => Ok(()),
                        other => other.map(|_| ()),
                    },
                ),
            ),
            (
                "the union-find walk",
                Phase::UnionFind,
                At::FromEnd(1_000),
                Box::new(|b| wcc(&forest, &all, RowCap(1), b).map(|_| ())),
            ),
        ];
        let failures: Vec<String> = rows
            .iter()
            .filter_map(|(what, phase, at, call)| stops_inside(what, *phase, *at, call))
            .collect();
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }
}
