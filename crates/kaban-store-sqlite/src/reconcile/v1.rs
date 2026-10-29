//! Weighted vote v1. Repeated borrowed scans trade time for zero scratch allocation.
//! The only allocations are returned verdict data and E1.1 validator internals.
use kaban_core::{
    Assertion, AssertionKey, CandidateReason, ClaimClass, Fixed, Layer, Rank, ReconcileCtx,
    Reconciler, UnresolvedReason, Verdict, same_group, validate, votes,
};
use std::cmp::Ordering;

/// Rels whose active edge needs a human whatever the loaded ontology declares (EN-S10): a
/// group on one of them is routed to review unless a human claim leads it.
const GOVERNED_SEMANTICS_RELS: [&str; 4] = ["GOVERNS", "SUPERSEDES", "INVALIDATES", "CONTRADICTS"];

/// Pure store-owned implementation, invoked only through `Reconciler`.
pub(crate) struct WeightedVoteV1;

impl Reconciler for WeightedVoteV1 {
    fn decide(&self, group: &[Assertion], ctx: &ReconcileCtx<'_>) -> Verdict {
        debug_assert!(!mixed_workspaces(group), "mixed workspaces");
        decide_inner(group, ctx)
    }
}

fn mixed_workspaces(group: &[Assertion]) -> bool {
    group
        .first()
        .is_some_and(|first| group.iter().any(|a| a.workspace_id != first.workspace_id))
}

/// Borrowed ascending-key traversal; duplicate identities count once. If an input
/// repeats a key with different scores, the largest score is the deterministic representative.
pub(crate) fn supporters(group: &[Assertion]) -> impl Iterator<Item = &Assertion> {
    let mut previous: Option<&AssertionKey> = None;
    std::iter::from_fn(move || {
        let next = group
            .iter()
            .filter(|a| previous.is_none_or(|key| a.key > *key))
            .min_by(|a, b| {
                a.key
                    .cmp(&b.key)
                    .then_with(|| b.raw_score.cmp(&a.raw_score))
            });
        previous = next.map(|a| &a.key);
        next
    })
}

/// The supporters that vote (EN-F12b rank partition): `preferred` only when one exists, never
/// `deprecated`, never a claim another member supersedes.
pub(crate) fn voting(group: &[Assertion]) -> impl Iterator<Item = &Assertion> {
    supporters(group).filter(move |a| votes(a, group))
}

fn reject(reason: &str) -> Verdict {
    Verdict::Rejected {
        reason: reason.into(),
    }
}

fn weight(a: &Assertion, ctx: &ReconcileCtx<'_>) -> i64 {
    let prior = ctx
        .priors
        .lookup(a.layer, a.method, a.claim_class)
        .unwrap_or(Fixed::ZERO);
    i64::from(prior.0) * i64::from(a.raw_score.0) / 10_000
}

/// Whether two supporters back the same value (every `edge` claim of a triple does).
pub(crate) fn same_value(a: &Assertion, b: &Assertion) -> bool {
    if a.claim_class == ClaimClass::Edge {
        return true;
    }
    if a.claim_class == ClaimClass::CanonicalId
        && (a.object_rid.is_some() || b.object_rid.is_some())
    {
        return a.object_rid == b.object_rid;
    }
    if a.claim_class == ClaimClass::Identity {
        // the verdict on the pair (E2.10; the pair itself is the group, E2.10b BR-2): two
        // `match: true` claims with different `W` back one value, a `match: false` claim
        // backs the other
        return a.value.as_value().get("match") == b.value.as_value().get("match");
    }
    a.value.as_str() == b.value.as_str()
}

// Greater means the stronger supporter, including the final stable-key tie break.
pub(crate) fn compare_support(a: &Assertion, b: &Assertion, ctx: &ReconcileCtx<'_>) -> Ordering {
    weight(a, ctx)
        .cmp(&weight(b, ctx))
        .then_with(|| b.layer.cmp(&a.layer))
        .then_with(|| b.value.as_str().cmp(a.value.as_str()))
        .then_with(|| b.key.cmp(&a.key))
}

#[derive(Clone, Copy)]
pub(crate) struct Candidate<'a> {
    pub(crate) representative: &'a Assertion,
    pub(crate) sum: i64,
    pub(crate) best_lineage: i64,
}

fn candidate<'a>(
    a: &'a Assertion,
    group: &'a [Assertion],
    ctx: &ReconcileCtx<'_>,
) -> Candidate<'a> {
    let representative = voting(group)
        .filter(|b| same_value(a, b))
        .max_by(|a, b| compare_support(a, b, ctx))
        .unwrap_or(a);
    let best_lineage = weight(representative, ctx);
    let sum = if matches!(a.claim_class, ClaimClass::Edge | ClaimClass::PropValue) {
        best_lineage
    } else {
        // Each lineage contributes its strongest voting supporter once, visited by stable key.
        voting(group)
            .filter(|b| same_value(a, b))
            .filter(|b| {
                !voting(group).any(|c| {
                    same_value(a, c)
                        && b.lineage_id == c.lineage_id
                        && compare_support(c, b, ctx) == Ordering::Greater
                })
            })
            .map(|b| weight(b, ctx))
            .sum()
    };
    Candidate {
        representative,
        sum,
        best_lineage,
    }
}

/// A `kind_of_record` claim votes only when its props satisfy the kind's schema, except a
/// human one: a person's acceptance is the authority over the record (EN-F12c, E2.3b),
/// whatever the loaded ontology says about its props. Machine claims stay gated.
fn record_valid(a: &Assertion, ctx: &ReconcileCtx<'_>) -> bool {
    if a.claim_class != ClaimClass::KindOfRecord || a.layer == Layer::Human {
        return true;
    }
    let Some((kind, _)) = a.kind_name().and_then(|name| ctx.kind(name)) else {
        return false;
    };
    let Some(props) = a.value.as_value().get("props") else {
        return false;
    };
    validate(&kind.props_schema, props).is_empty()
}

pub(crate) fn candidates<'a>(
    group: &'a [Assertion],
    ctx: &'a ReconcileCtx<'_>,
) -> impl Iterator<Item = Candidate<'a>> {
    voting(group)
        .filter(|a| record_valid(a, ctx))
        .filter_map(|a| {
            let c = candidate(a, group, ctx);
            (c.representative.key == a.key).then_some(c)
        })
}

fn compare_candidates(a: Candidate<'_>, b: Candidate<'_>, total: i64) -> Ordering {
    // Rational probabilities stay in i128 until rendering the returned confidence.
    (i128::from(a.sum) * i128::from(total))
        .cmp(&(i128::from(b.sum) * i128::from(total)))
        .then_with(|| a.best_lineage.cmp(&b.best_lineage))
        .then_with(|| b.representative.layer.cmp(&a.representative.layer))
        .then_with(|| {
            let discovered = |c: Candidate<'_>| {
                c.representative
                    .kind_name()
                    .is_some_and(|k| k.starts_with("x:"))
            };
            discovered(b).cmp(&discovered(a))
        })
        .then_with(|| {
            b.representative
                .value
                .as_str()
                .cmp(a.representative.value.as_str())
        })
        .then_with(|| b.representative.key.cmp(&a.representative.key))
}

pub(crate) fn ordered_candidates<'a>(
    group: &'a [Assertion],
    ctx: &'a ReconcileCtx<'_>,
    total: i64,
) -> impl Iterator<Item = Candidate<'a>> {
    let mut previous = None;
    std::iter::from_fn(move || {
        let next = candidates(group, ctx)
            .filter(|c| previous.is_none_or(|p| compare_candidates(*c, p, total) == Ordering::Less))
            .max_by(|a, b| compare_candidates(*a, *b, total));
        previous = next;
        next
    })
}

pub(crate) fn probability(sum: i64, total: i64) -> Fixed {
    let n = i128::from(sum) * 10_000;
    let d = i128::from(total);
    let q = n / d;
    let r = n % d;
    let rounded = q + i128::from(r * 2 > d || (r * 2 == d && q % 2 != 0));
    Fixed(u32::try_from(rounded).unwrap_or(0))
}

/// The same `P` under the name the explanation templates use (EN-F14: a share of the group's
/// weight, never a statement about the world; the templates' grep test forbids the other word).
pub(crate) fn vote_share(sum: i64, total: i64) -> Fixed {
    probability(sum, total)
}

fn review_reason(
    best: Candidate<'_>,
    group: &[Assertion],
    ctx: &ReconcileCtx<'_>,
    margin: Fixed,
) -> Option<CandidateReason> {
    let a = best.representative;
    if a.claim_class == ClaimClass::KindOfRecord {
        return Some(CandidateReason::Proposed);
    }
    if a.claim_class == ClaimClass::Identity {
        // E2.10 (EN-F51 (3)): a `{match: true}` claim wins (the identity row is the
        // store's), a `Possible` (`match: false`) is the `merge` review
        return (a
            .value
            .as_value()
            .get("match")
            .and_then(serde_json::Value::as_bool)
            != Some(true))
        .then_some(CandidateReason::Merge);
    }
    let tau = match a.claim_class {
        ClaimClass::Type => ctx.tau_type,
        ClaimClass::CanonicalId => ctx.tau_id,
        _ => Fixed::ZERO,
    };
    if margin < tau {
        return Some(CandidateReason::BelowMargin);
    }
    if let Some((kind, min)) = a.kind_name().and_then(|name| ctx.kind(name)) {
        if best.sum < i64::from(min.0) {
            return Some(CandidateReason::BelowMinConfidence);
        }
        if kind.governed.is_some() {
            return Some(CandidateReason::Governed);
        }
    }
    // Multiple governed targets require review even if a non-governed candidate leads.
    if candidates(group, ctx)
        .filter(|c| {
            c.representative
                .kind_name()
                .and_then(|n| ctx.kind(n))
                .is_some_and(|(k, _)| k.governed.is_some())
        })
        .take(2)
        .count()
        > 1
    {
        return Some(CandidateReason::Governed);
    }
    if a.kind_name().is_some_and(|name| name.starts_with("x:")) {
        return Some(CandidateReason::DiscoveredKind);
    }
    if governed_rel(a, ctx) {
        return Some(CandidateReason::GovernedSemantics);
    }
    if let Some((_, min)) = a.predicate.as_deref().and_then(|name| ctx.rel(name))
        && best.sum < i64::from(min.0)
    {
        return Some(CandidateReason::BelowMinConfidence);
    }
    None
}

/// Whether an `edge` claim's rel carries governed semantics (EN-S10): one of the four named
/// rels, or any rel the loaded ontology flags `governed_semantics: true`.
fn governed_rel(a: &Assertion, ctx: &ReconcileCtx<'_>) -> bool {
    a.claim_class == ClaimClass::Edge
        && a.predicate.as_deref().is_some_and(|rel| {
            GOVERNED_SEMANTICS_RELS.contains(&rel)
                || ctx.rel(rel).is_some_and(|(r, _)| r.governed_semantics)
        })
}

/// Whether two eligible `preferred` supporters back different values from different
/// lineages (EN-F12b): a disagreement between two humans no vote resolves. Judged over
/// every voting `preferred` supporter, not over the two leading representatives (E2 interim
/// review RE4-8): a value's representative may share the leader's lineage while another
/// preferred supporter of that value, or of a third value, comes from a different one.
fn preferred_conflict(group: &[Assertion], ctx: &ReconcileCtx<'_>) -> bool {
    let preferred: Vec<&Assertion> = voting(group)
        .filter(|a| a.rank == Rank::Preferred && record_valid(a, ctx))
        .collect();
    preferred.iter().enumerate().any(|(i, a)| {
        preferred[i + 1..]
            .iter()
            .any(|b| a.lineage_id != b.lineage_id && !same_value(a, b))
    })
}

/// What EN-F12c's protection makes of a non-human leader (`None` when the group is not
/// protected or the leader is human): a value against the protected one is `Unresolved`,
/// agreement with the human winner hands the win back to the human's claim.
fn protected(
    best: Candidate<'_>,
    group: &[Assertion],
    ctx: &ReconcileCtx<'_>,
    confidence: Fixed,
    margin: Fixed,
    total: i64,
) -> Option<Verdict> {
    let rep = best.representative;
    if rep.layer == Layer::Human {
        return None;
    }
    if ctx.protection.accepted_record_edge {
        return Some(unresolved(
            group,
            ctx,
            total,
            UnresolvedReason::AcceptedRecordProtected,
        ));
    }
    let human = ctx
        .protection
        .human_winner
        .as_ref()
        .and_then(|key| group.iter().find(|a| a.key == *key))?;
    if !same_value(rep, human) {
        return Some(unresolved(
            group,
            ctx,
            total,
            UnresolvedReason::HumanProtected,
        ));
    }
    Some(Verdict::Winner {
        assertion_key: human.key.clone(),
        confidence,
        margin,
        dissent: supporters(group)
            .filter(|a| a.key != human.key)
            .map(|a| a.key.clone())
            .collect(),
    })
}

fn unresolved(
    group: &[Assertion],
    ctx: &ReconcileCtx<'_>,
    total: i64,
    reason: UnresolvedReason,
) -> Verdict {
    Verdict::Unresolved {
        ranked: ordered_candidates(group, ctx, total)
            .map(|c| (c.representative.key.clone(), probability(c.sum, total)))
            .collect(),
        reason,
    }
}

fn decide_inner(group: &[Assertion], ctx: &ReconcileCtx<'_>) -> Verdict {
    if mixed_workspaces(group) {
        return reject("mixed_workspaces");
    }
    let Some(first) = group.first() else {
        return reject("empty_group");
    };
    if group.iter().any(|a| !same_group(first, a)) {
        return reject("mixed_groups");
    }
    if group.iter().any(|a| a.raw_score.0 > 10_000) {
        return reject("raw_score_out_of_range");
    }
    if group.iter().any(|a| {
        ctx.priors
            .lookup(a.layer, a.method, a.claim_class)
            .is_none()
    }) {
        return reject("missing_prior");
    }
    // the rank partition (EN-F12b) is inside `voting`: the vote below sees the voters only
    let total: i64 = candidates(group, ctx).map(|c| c.sum).sum();
    if total == 0 {
        return reject("no_valid_support");
    }
    let mut ordered = ordered_candidates(group, ctx, total);
    let Some(best) = ordered.next() else {
        return reject("no_valid_support");
    };
    let second = ordered.next();
    let margin = second.map_or(Fixed::ONE, |second| {
        Fixed(
            u32::try_from(
                (i128::from(best.sum) - i128::from(second.sum)) * 10_000 / i128::from(total),
            )
            .unwrap_or(0),
        )
    });
    // two `preferred` values from different lineages: only a human resolves it (EN-F12b)
    if preferred_conflict(group, ctx) {
        return unresolved(group, ctx, total, UnresolvedReason::PreferredConflict);
    }
    let confidence = if matches!(first.claim_class, ClaimClass::Edge | ClaimClass::PropValue) {
        Fixed(u32::try_from(best.sum).unwrap_or(0))
    } else {
        probability(best.sum, total)
    };
    if let Some(verdict) = protected(best, group, ctx, confidence, margin, total) {
        return verdict;
    }
    if let Some(reason) = review_reason(best, group, ctx, margin) {
        let ranked = ordered_candidates(group, ctx, total)
            .map(|c| (c.representative.key.clone(), probability(c.sum, total)))
            .collect();
        return Verdict::Candidate { ranked, reason };
    }
    Verdict::Winner {
        assertion_key: best.representative.key.clone(),
        confidence,
        margin,
        // every supporter of another value, plus the agreeing ones that did not vote
        // (`deprecated`, `normal` beside a `preferred`, superseded): listed, never counted
        dissent: supporters(group)
            .filter(|a| !same_value(a, best.representative) || !votes(a, group))
            .map(|a| a.key.clone())
            .collect(),
    }
}

/// Exercise the release rejection path on debug hosts without disabling assertions globally.
#[cfg(test)]
#[allow(dead_code)] // driven by `tests/reconcile_props.rs`, which includes this file by path
pub(crate) fn decide_without_debug(group: &[Assertion], ctx: &ReconcileCtx<'_>) -> Verdict {
    decide_inner(group, ctx)
}
