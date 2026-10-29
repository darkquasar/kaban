//! The explanation of a stored conflict group (engine-spec EN-F14; engine-plan E2.4) and the
//! evaluation counters of EN-F16. `explain_group` is the read-only twin of the decision pass
//! (`reconcile::decide_groups_without`): the same loader, the same protection set, the same
//! context and the same `Reconciler::decide`, then a rendering instead of `project::apply`.
//! Nothing here opens a transaction or writes a row, except the lazily created `priors_obs`
//! table and its counters.
//!
//! ```text
//!   explain_group(key)
//!     load_group -> protection_of -> setup.ctx -> reconciler.decide -> human_protected
//!       |                                                                  |
//!       v                                                                  v
//!   candidates in decision order (ordered_candidates), each with its lineages
//!   (supporters of the value grouped by lineage_id, best by compare_support), the margin,
//!   the gates (tau by class, min_confidence by kind or rel), tie_break, unresolved_reason,
//!   protected_by (the human winner)   ->   Explanation (four-decimal strings)
//! ```

use crate::reconcile::{self, Setup, persist, v1};
use crate::rows::{NODE_COLS, row_node};
use crate::sql_error::SqlResultExt;
use crate::sqlite::WS_COL;
use kaban_core::{
    Assertion, AssertionStatus, CandidateView, ClaimClass, Explanation, Fixed, LineageView,
    PriorsObs, PriorsOutcome, ReconcileCtx, Result, StoreError, SupporterView, Verdict,
    WEIGHTED_VOTE_V1,
};
use kaban_core::{ClaimClass as Class, Layer, Method};
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::BTreeMap;

/// The group key of the claim `key` in every workspace (`MemoryStore::claim_group`).
pub(crate) fn claim_group(conn: &Connection, key: &str) -> Result<String> {
    claim_group_where(conn, key, None)
}

/// The group key of the claim `key` when it belongs to `ws` (`MemoryStore::claim_group_in`,
/// core-spec S6): a key of another workspace is `NotFound`, as an unknown key is, so the
/// caller learns nothing of a foreign group (the wave-2 review's RW2-1).
pub(crate) fn claim_group_in(conn: &Connection, ws: &str, key: &str) -> Result<String> {
    claim_group_where(conn, key, Some(ws))
}

/// One lookup for both: `ws` is the workspace the claim must belong to, or `None` for any.
fn claim_group_where(conn: &Connection, key: &str, ws: Option<&str>) -> Result<String> {
    if !reconcile::engine_tables(conn)? {
        return Err(StoreError::NotFound(key.to_owned()));
    }
    conn.query_row(
        "SELECT s.group_key FROM assertion a JOIN assertion_status s ON s.assertion_id = a.id
         WHERE a.key = ?1 AND (?2 IS NULL OR a.workspace_id = ?2)",
        params![key, ws],
        |r| r.get(0),
    )
    .optional()
    .sql()?
    .ok_or_else(|| StoreError::NotFound(key.to_owned()))
}

/// The explanation of the group `key` (`MemoryStore::explain_group`).
pub(crate) fn explain_group(conn: &Connection, setup: &Setup, key: &str) -> Result<Explanation> {
    if !reconcile::engine_tables(conn)? {
        return Err(StoreError::NotFound(key.to_owned()));
    }
    let group = persist::load_group(conn, key)?;
    if group.claims.is_empty() {
        return Err(StoreError::NotFound(key.to_owned()));
    }
    let protection = reconcile::protection_of(conn, setup, key, &group)?;
    let ctx = setup.ctx(&protection)?;
    let verdict = reconcile::er::identity_conflict(
        reconcile::human_protected(setup.reconciler.decide(&group.claims, &ctx), &group),
        &group,
    );
    Ok(render(&group.claims, key, &verdict, &ctx))
}

/// The document itself, from the claims, the verdict and the frozen context.
fn render(
    claims: &[Assertion],
    key: &str,
    verdict: &Verdict,
    ctx: &ReconcileCtx<'_>,
) -> Explanation {
    let total: i64 = v1::candidates(claims, ctx).map(|c| c.sum).sum();
    let ordered: Vec<v1::Candidate<'_>> = if total == 0 {
        Vec::new()
    } else {
        v1::ordered_candidates(claims, ctx, total).collect()
    };
    let candidates = ordered
        .iter()
        .map(|c| CandidateView {
            value: c.representative.value.as_value().clone(),
            p: v1::vote_share(c.sum, total).to_string(),
            lineages: lineages_of(c.representative, claims, ctx),
        })
        .collect();
    let (best, second) = (ordered.first(), ordered.get(1));
    let margin = match (best, second) {
        (Some(best), Some(second)) => Fixed(
            u32::try_from(
                (i128::from(best.sum) - i128::from(second.sum)) * 10_000 / i128::from(total),
            )
            .unwrap_or(0),
        ),
        (Some(_), None) => Fixed::ONE,
        (None, _) => Fixed::ZERO,
    };
    let tie_break = match (best, second) {
        (Some(a), Some(b)) if a.sum == b.sum => Some(tie_level(*a, *b).to_owned()),
        _ => None,
    };
    let class = claims.first().map_or(ClaimClass::Type, |a| a.claim_class);
    let threshold = match class {
        ClaimClass::Type => ctx.tau_type,
        ClaimClass::CanonicalId => ctx.tau_id,
        _ => Fixed::ZERO,
    };
    let leader = best.map(|c| c.representative).or_else(|| claims.first());
    let min_confidence = leader.map_or(Fixed::ZERO, |a| {
        if a.claim_class == ClaimClass::Edge {
            a.predicate
                .as_deref()
                .and_then(|rel| ctx.rel(rel))
                .map_or(Fixed::ZERO, |(_, min)| min)
        } else {
            a.kind_name()
                .and_then(|name| ctx.kind(name))
                .map_or(Fixed::ZERO, |(_, min)| min)
        }
    });
    let (status, unresolved_reason) = match verdict {
        Verdict::Winner { .. } => (AssertionStatus::Winner, None),
        Verdict::Candidate { .. } => (AssertionStatus::Candidate, None),
        Verdict::Unresolved { reason, .. } => (AssertionStatus::Unresolved, Some(*reason)),
        // `Verdict` is `non_exhaustive`: an unknown verdict projects nothing (`disposition_of`)
        Verdict::Rejected { .. } | _ => (AssertionStatus::Rejected, None),
    };
    Explanation {
        rule: WEIGHTED_VOTE_V1.to_owned(),
        group_key: key.to_owned(),
        verdict: status,
        candidates,
        margin: margin.to_string(),
        threshold: threshold.to_string(),
        min_confidence: min_confidence.to_string(),
        tie_break,
        unresolved_reason,
        protected_by: ctx.protection.human_winner.clone(),
        // EN-F51 (4): the leader's stored waterfall, signed four-decimal, identity groups only
        er_contributions: leader.and_then(reconcile::er::contributions_of),
    }
}

/// The `compare_candidates` level that split two candidates of equal sum.
fn tie_level(a: v1::Candidate<'_>, b: v1::Candidate<'_>) -> &'static str {
    let discovered = |c: v1::Candidate<'_>| {
        c.representative
            .kind_name()
            .is_some_and(|k| k.starts_with("x:"))
    };
    if a.best_lineage != b.best_lineage {
        "lineage"
    } else if a.representative.layer != b.representative.layer {
        "layer"
    } else if discovered(a) != discovered(b) {
        "discovered"
    } else if a.representative.value.as_str() != b.representative.value.as_str() {
        "value"
    } else {
        "key"
    }
}

/// The supporters of `rep`'s value grouped by lineage (ascending lineage id; within a lineage
/// the strongest by `compare_support` is `best`, the rest are counted).
fn lineages_of(rep: &Assertion, claims: &[Assertion], ctx: &ReconcileCtx<'_>) -> Vec<LineageView> {
    let mut by_lineage: BTreeMap<&str, Vec<&Assertion>> = BTreeMap::new();
    for a in v1::supporters(claims).filter(|a| v1::same_value(a, rep)) {
        by_lineage.entry(&a.lineage_id).or_default().push(a);
    }
    by_lineage
        .into_iter()
        .filter_map(|(lineage_id, members)| {
            let best = members
                .iter()
                .copied()
                .max_by(|a, b| v1::compare_support(a, b, ctx))?;
            let prior = ctx
                .priors
                .lookup(best.layer, best.method, best.claim_class)
                .unwrap_or(Fixed::ZERO);
            Some(LineageView {
                lineage_id: lineage_id.to_owned(),
                rank: best.rank,
                best: SupporterView {
                    assertion_key: best.key.clone(),
                    layer: best.layer,
                    method: best.method,
                    raw_score: best.raw_score.to_string(),
                    prior: prior.to_string(),
                },
                others: u32::try_from(members.len().saturating_sub(1)).unwrap_or(u32::MAX),
            })
        })
        .collect()
}

/// One page of the active `proposed` records of `ws` in id order
/// (`MemoryStore::proposed_nodes_in`): one query over the governance column, no kind list.
pub(crate) fn proposed_nodes_in(
    conn: &Connection,
    ws: &str,
    offset: usize,
    limit: usize,
) -> Result<Vec<kaban_core::Node>> {
    let limit = i64::try_from(limit.min(500)).unwrap_or(500);
    let offset = i64::try_from(offset).unwrap_or(i64::MAX);
    let mut st = conn
        .prepare_cached(&format!(
            "SELECT {NODE_COLS} FROM node n WHERE {WS_COL} = ?1 AND n.governance = 'proposed'
             AND n.lifecycle = 'active' ORDER BY n.id LIMIT ?2 OFFSET ?3"
        ))
        .sql()?;
    let rows = st
        .query_map(params![ws, limit, offset], row_node)
        .sql()?
        .collect::<rusqlite::Result<Vec<_>>>()
        .sql()?;
    Ok(rows)
}

/// The most chunks of one path a `<path>:<offset>` lookup examines
/// (`MemoryStore::chunk_covering`, RW2-6): the path's chunks in id order up to this many are
/// the candidates, so one document's size bounds the cost and the other documents never
/// enter it. A document split into more chunks than this answers from its first
/// `MAX_SPAN_SCAN` only.
pub(crate) const MAX_SPAN_SCAN: usize = 4096;

/// The active chunk of `ws` from `path` covering byte `offset`
/// (`MemoryStore::chunk_covering`): the path's first `MAX_SPAN_SCAN` chunks by id are the
/// candidates (the inner select), the covering one with the lowest id is decoded; no other
/// row is read into a `Node`.
pub(crate) fn chunk_covering(
    conn: &Connection,
    ws: &str,
    path: &str,
    offset: u64,
) -> Result<Option<kaban_core::Node>> {
    let offset = i64::try_from(offset).unwrap_or(i64::MAX);
    let bound = i64::try_from(MAX_SPAN_SCAN).unwrap_or(i64::MAX);
    let mut st = conn
        .prepare_cached(&format!(
            "SELECT {NODE_COLS} FROM (
               SELECT n.* FROM node n WHERE n.kind = 'Chunk' AND {WS_COL} = ?1 AND n.lifecycle = 'active'
                 AND json_extract(n.props, '$.path') = ?2 ORDER BY n.id LIMIT ?4) n
             WHERE json_extract(n.props, '$.byte_start') <= ?3 AND json_extract(n.props, '$.byte_end') > ?3
             ORDER BY n.id LIMIT 1"
        ))
        .sql()?;
    st.query_row(params![ws, path, offset, bound], row_node)
        .optional()
        .sql()
}

/// The group keys of `ws` with an `unresolved` claim (`IngestStore::unresolved_groups`).
pub(crate) fn unresolved_groups(conn: &Connection, ws: &str) -> Result<Vec<String>> {
    if !reconcile::engine_tables(conn)? {
        return Ok(Vec::new());
    }
    let mut st = conn
        .prepare_cached(
            "SELECT DISTINCT s.group_key FROM assertion_status s JOIN assertion a ON a.id = s.assertion_id
             WHERE s.disposition = 'unresolved' AND a.workspace_id = ?1 ORDER BY s.group_key",
        )
        .sql()?;
    let rows = st
        .query_map([ws], |r| r.get::<_, String>(0))
        .sql()?
        .collect::<rusqlite::Result<Vec<_>>>()
        .sql()?;
    Ok(rows)
}

/// The `priors_obs` table (EN-F16), created on first use: no migration, no `schema_version`
/// move; a store that never counted has none and reads as all zeros.
const PRIORS_OBS_DDL: &str = "CREATE TABLE IF NOT EXISTS priors_obs(
    layer TEXT NOT NULL, method TEXT NOT NULL, claim_class TEXT NOT NULL,
    accepted INTEGER NOT NULL DEFAULT 0, rejected INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY(layer, method, claim_class))";

/// Count `outcome` once per `winner`/`candidate` non-human claim of the node's groups
/// (`IngestStore::priors_obs_bump`), inside the caller's transaction.
pub(crate) fn priors_obs_bump(conn: &Connection, rid: i64, outcome: PriorsOutcome) -> Result<u32> {
    if !reconcile::engine_tables(conn)? {
        return Ok(0);
    }
    conn.execute_batch(PRIORS_OBS_DDL).sql()?;
    let (accepted, rejected) = match outcome {
        PriorsOutcome::Accepted => (1_i64, 0_i64),
        PriorsOutcome::Rejected => (0, 1),
    };
    let mut claims = conn
        .prepare_cached(
            "SELECT a.layer, a.method, a.claim_class FROM assertion a JOIN assertion_status s ON s.assertion_id = a.id
             WHERE s.group_key = ?1 AND s.disposition IN ('winner', 'candidate') AND a.layer <> 'human'
             ORDER BY a.key",
        )
        .sql()?;
    let mut bump = conn
        .prepare_cached(
            "INSERT INTO priors_obs(layer, method, claim_class, accepted, rejected) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(layer, method, claim_class) DO UPDATE SET
               accepted = accepted + excluded.accepted, rejected = rejected + excluded.rejected",
        )
        .sql()?;
    let mut counted = 0_u32;
    for group in persist::groups_of_node(conn, rid)? {
        let rows = claims
            .query_map([&group], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })
            .sql()?
            .collect::<rusqlite::Result<Vec<_>>>()
            .sql()?;
        for (layer, method, class) in rows {
            bump.execute(params![layer, method, class, accepted, rejected])
                .sql()?;
            counted += 1;
        }
    }
    Ok(counted)
}

/// Every configured prior with its counters (`IngestStore::priors_obs`): the vocabulary order
/// of `(layer, method, claim_class)`, zeros where nothing was counted or no table exists yet.
pub(crate) fn priors_obs(conn: &Connection, setup: &Setup) -> Result<Vec<PriorsObs>> {
    let has_table: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'priors_obs')",
            [],
            |r| r.get(0),
        )
        .sql()?;
    let mut counts: BTreeMap<(String, String, String), (u64, u64)> = BTreeMap::new();
    if has_table {
        let mut st = conn
            .prepare("SELECT layer, method, claim_class, accepted, rejected FROM priors_obs")
            .sql()?;
        let rows = st
            .query_map([], |r| {
                Ok((
                    (r.get(0)?, r.get(1)?, r.get(2)?),
                    (
                        u64::try_from(r.get::<_, i64>(3)?).unwrap_or(0),
                        u64::try_from(r.get::<_, i64>(4)?).unwrap_or(0),
                    ),
                ))
            })
            .sql()?;
        for row in rows {
            let (key, value) = row.sql()?;
            counts.insert(key, value);
        }
    }
    let mut out = Vec::new();
    for &layer in Layer::ALL {
        for &method in Method::ALL {
            for &class in Class::ALL {
                let Some(prior) = setup.priors.lookup(layer, method, class) else {
                    continue;
                };
                let key = (
                    layer.as_str().to_owned(),
                    method.as_str().to_owned(),
                    class.as_str().to_owned(),
                );
                let (accepted, rejected) = counts.get(&key).copied().unwrap_or((0, 0));
                out.push(PriorsObs {
                    layer,
                    method,
                    claim_class: class,
                    prior,
                    accepted,
                    rejected,
                });
            }
        }
    }
    Ok(out)
}
