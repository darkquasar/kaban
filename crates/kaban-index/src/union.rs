//! The Go two-run union of implementation relationships (stage1-spec G1-F3, RS-5; stage1-plan
//! CG3.2). scip-go 0.2.7 drops some `is_implementation` relationships depending on whether
//! tests are compiled (the sibling-package implementer is missing from the run with tests, the
//! test-only implementer from the run without), so `kaban-index` runs it twice and takes
//! `rel(A) ∪ rel(B)` as a set of `(src scip, dst scip, edge type)` triples. Documents, symbols,
//! occurrences and every other relationship come from A only. The set is ordered, so run order
//! cannot change the bytes; the reason is recorded in `meta` by the writer.
//!
//! The union reads [`Admitted`] views only (RU1-2). A triple whose endpoint the policy rejected
//! in either run is dropped and counted (RU1-4: a skipped row never returns as a union endpoint
//! or a placeholder); a triple whose endpoint fails the grammar is dropped and counted too
//! (RU1-5), in the [`Union`] the writer folds into `counts:*`.

use std::collections::{BTreeSet, HashMap};

use kaban_codegraph::EdgeType;
use kaban_codegraph::symbol;
use scip::types::symbol_information::Kind;

use crate::derive::{derive_edge_type, label_of};
use crate::gate::Admitted;

/// `(src scip, dst scip, IMPLEMENTS | IMPLEMENTS_MEMBER)` triples, borrowed from the admitted
/// indexes they came from, in byte order.
pub type ImplementsSet<'a> = BTreeSet<(&'a str, &'a str, EdgeType)>;

/// The reason text written to `meta.union_reason` when B is present.
pub const UNION_REASON: &str = "scip-go 0.2.7 test-variant defect: IMPLEMENTS and \
                                IMPLEMENTS_MEMBER are the union of run A (tests compiled) and \
                                run B (--skip-tests); everything else is from A (G1-F3)";

/// The union's result: the triple set and what was dropped on the way.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Union<'a> {
    /// The implementation triples that survived.
    pub set: ImplementsSet<'a>,
    /// Triples dropped because an endpoint is a policy-rejected identity of either run
    /// (`counts:policy_skipped_row`, RU1-4).
    pub policy_skipped: u64,
    /// Triples dropped because an endpoint fails the grammar
    /// (`counts:grammar_skipped_relationships`, RU1-5).
    pub grammar_skipped: u64,
}

/// The implementation relationship set of run `a`, unioned with run `b` when given. Labels come
/// from each run's own symbol table (`effective_label` when the producer emitted
/// `UnspecifiedKind`); a target without a `SymbolInformation` is labelled from its descriptor
/// alone. Self-loops are dropped silently (G1-F3); a rejected or ungrammatical endpoint drops
/// the triple with a count.
#[must_use]
pub fn implements<'a>(a: &Admitted<'a>, b: Option<&Admitted<'a>>) -> Union<'a> {
    let mut out = Union::default();
    let mut candidates: BTreeSet<(&'a str, &'a str, EdgeType)> = BTreeSet::new();
    let mut grammar_failed: BTreeSet<(&'a str, &'a str)> = BTreeSet::new();
    for run in std::iter::once(a).chain(b) {
        relationships_of(run, &mut candidates, &mut grammar_failed);
    }
    out.grammar_skipped = u64::try_from(grammar_failed.len()).unwrap_or(u64::MAX);
    let rejected = |sym: &str| a.is_rejected(sym) || b.is_some_and(|b| b.is_rejected(sym));
    for (src, dst, ty) in candidates {
        if rejected(src) || rejected(dst) {
            out.policy_skipped += 1;
        } else {
            out.set.insert((src, dst, ty));
        }
    }
    out
}

/// Every implementation triple of one run into `candidates`; the `(src, dst)` pairs an
/// endpoint's grammar failure dropped into `grammar_failed` (a pair, so the same relationship
/// seen in both runs counts once).
fn relationships_of<'a>(
    index: &Admitted<'a>,
    candidates: &mut BTreeSet<(&'a str, &'a str, EdgeType)>,
    grammar_failed: &mut BTreeSet<(&'a str, &'a str)>,
) {
    // every symbol's effective label, first row in document order wins
    let mut labels: HashMap<&'a str, Kind> = HashMap::new();
    for s in index.symbols() {
        if let Ok(sym) = symbol::parse(s.symbol()) {
            labels.entry(s.symbol()).or_insert(label_of(s.kind(), &sym));
        }
    }
    let label = |scip: &'a str| -> Option<Kind> {
        labels.get(scip).copied().or_else(|| {
            symbol::parse(scip)
                .ok()
                .map(|sym| label_of(Kind::UnspecifiedKind, &sym))
        })
    };
    for s in index.symbols() {
        let src_label = label(s.symbol());
        for r in s.relationships() {
            if !r.flags().is_implementation() || r.target() == s.symbol() {
                continue;
            }
            let (Some(src_label), Some(dst_label)) = (src_label, label(r.target())) else {
                grammar_failed.insert((s.symbol(), r.target()));
                continue;
            };
            if let Some(ty @ (EdgeType::Implements | EdgeType::ImplementsMember)) =
                derive_edge_type(src_label, dst_label, r.flags())
            {
                candidates.insert((s.symbol(), r.target(), ty));
            }
        }
    }
}
