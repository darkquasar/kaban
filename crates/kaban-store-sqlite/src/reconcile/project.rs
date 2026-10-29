//! The projection of a verdict (engine-spec EN-F9, EN-E9; engine-plan E2.2): statuses for
//! every supporter of the group, the one `edge` row per triple for an `edge` winner (the
//! `INSERT ... ON CONFLICT(src,dst,rel) DO UPDATE` statement lives here and nowhere else), the
//! graph output of a `span` winner (`materialise`, EN-F13; E2.5), a `node_candidate` row for a
//! `Candidate`, nothing but statuses for `Unresolved` and `Rejected`; an `identity` group's
//! cluster projection (`er::reproject_identity`, E2.10b) runs whatever the verdict. The two fixed-point to
//! `REAL` conversions of the write path (`confidence`, `score`) happen here, after the decision
//! (engine-plan §4: no `f64` in a decision).

use super::{Group, Setup, disposition_of, er, materialise, persist, v1};
use crate::candidates;
use crate::sql_error::SqlResultExt;
use kaban_core::{
    AssertionStatus, CandidateReason, ClaimClass, Layer, Method, Owner, ProtectionSet, Result,
    Subject, UnresolvedReason, Verdict, new_id, now_iso,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Map, Value, json};

/// One `edge` row as the store writes it: the structural edges of a producer batch carry no
/// scoring columns; a reconciled edge carries the winner's.
pub(crate) struct EdgeWrite<'a> {
    pub(crate) src: i64,
    pub(crate) dst: i64,
    pub(crate) rel: &'a str,
    pub(crate) confidence: Option<f64>,
    /// The stored props object, `props.owner` included (EN-E4).
    pub(crate) props: &'a Value,
    pub(crate) layer: Option<Layer>,
    pub(crate) method: Option<Method>,
    pub(crate) score: Option<f64>,
    pub(crate) source_id: Option<&'a str>,
    pub(crate) offset: Option<u32>,
    pub(crate) len: Option<u32>,
}

/// The row as stored, for the "unchanged writes nothing" rule (N5).
type Stored = (
    Option<f64>,
    String,
    Option<String>,
    Option<String>,
    Option<f64>,
    Option<String>,
    Option<u32>,
    Option<u32>,
);

/// Write the edge unless the stored row already holds these values. Returns `Some(true)` for
/// a new row, `Some(false)` for a refreshed one, `None` when nothing was written.
pub(crate) fn write_edge(conn: &Connection, e: &EdgeWrite<'_>) -> Result<Option<bool>> {
    let props = serde_json::to_string(e.props)?;
    if !super::engine_tables(conn)? {
        return write_edge_0002(conn, e, &props);
    }
    let stored: Option<Stored> = conn
        .query_row(
            "SELECT confidence, props, layer, method, score, source_id, offset, len
             FROM edge WHERE src=?1 AND dst=?2 AND rel=?3",
            params![e.src, e.dst, e.rel],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    r.get(7)?,
                ))
            },
        )
        .optional()
        .sql()?;
    let layer = e.layer.map(Layer::as_str);
    let method = e.method.map(Method::as_str);
    if let Some((c, p, l, m, s, sid, off, len)) = &stored
        && *c == e.confidence
        && *p == props
        && l.as_deref() == layer
        && m.as_deref() == method
        && *s == e.score
        && sid.as_deref() == e.source_id
        && *off == e.offset
        && *len == e.len
    {
        return Ok(None);
    }
    // the edge is tenant-scoped like its source node (F17)
    conn.execute(
        "INSERT INTO edge(id,src,dst,rel,confidence,created_at,props,workspace_id,layer,method,score,source_id,offset,len)
         VALUES (?1,?2,?3,?4,?5,?6,?7,(SELECT workspace_id FROM node WHERE rid=?2),?8,?9,?10,?11,?12,?13)
         ON CONFLICT(src,dst,rel) DO UPDATE SET confidence=excluded.confidence, props=excluded.props,
             layer=excluded.layer, method=excluded.method, score=excluded.score,
             source_id=excluded.source_id, offset=excluded.offset, len=excluded.len",
        params![
            new_id(),
            e.src,
            e.dst,
            e.rel,
            e.confidence,
            now_iso(),
            props,
            layer,
            method,
            e.score,
            e.source_id,
            e.offset,
            e.len
        ],
    )
    .sql()?;
    Ok(Some(stored.is_none()))
}

/// The edge write of a store below schema 0004 (the 0003 fixtures and benches): no scoring
/// columns, no claims; the pre-E2.2 statement.
fn write_edge_0002(conn: &Connection, e: &EdgeWrite<'_>, props: &str) -> Result<Option<bool>> {
    let stored: Option<(Option<f64>, String)> = conn
        .query_row(
            "SELECT confidence, props FROM edge WHERE src=?1 AND dst=?2 AND rel=?3",
            params![e.src, e.dst, e.rel],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .sql()?;
    if let Some((c, p)) = &stored
        && *c == e.confidence
        && p == props
    {
        return Ok(None);
    }
    conn.execute(
        "INSERT INTO edge(id,src,dst,rel,confidence,created_at,props,workspace_id)
         VALUES (?1,?2,?3,?4,?5,?6,?7,(SELECT workspace_id FROM node WHERE rid=?2))
         ON CONFLICT(src,dst,rel) DO UPDATE SET confidence=excluded.confidence, props=excluded.props",
        params![new_id(), e.src, e.dst, e.rel, e.confidence, now_iso(), props],
    )
    .sql()?;
    Ok(Some(stored.is_none()))
}

/// The `props.owner` tag of an edge a batch writes (EN-E4): the producer's value when it
/// set one, else the batch's owner.
pub(crate) fn owned_props(props: Option<&Value>, owner: Owner) -> Value {
    let mut map = match props {
        Some(Value::Object(m)) => m.clone(),
        _ => Map::new(),
    };
    map.entry("owner")
        .or_insert_with(|| Value::String(owner.as_str().to_owned()));
    Value::Object(map)
}

/// Remove the materialised edge of a triple when its group no longer has a winner; a
/// structural row of the same triple (no `layer`) is a producer's and stays.
fn drop_reconciled_edge(conn: &Connection, src: i64, dst: i64, rel: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM edge WHERE src=?1 AND dst=?2 AND rel=?3 AND layer IS NOT NULL",
        params![src, dst, rel],
    )
    .sql()?;
    Ok(())
}

/// The node a group's review row lands on, the one rid both `review` and `release_review`
/// use (`None` for a subject shape this store does not know: `Subject` is `non_exhaustive`).
/// A span group's row is the claimed entity's (OWN116): the first `object` in key order, the
/// target EN-F13 selection names, never the source chunk, which stays visible (EN-F49). A span
/// group without an object holds no row; `candidates::review_rows` lists it under its chunk
/// from the claims' `candidate` statuses.
fn review_rid(group: &Group, subject: &Subject) -> Option<i64> {
    match subject {
        Subject::Span { .. } => group.claims.iter().find_map(|a| a.object_rid),
        Subject::Node(rid) | Subject::Record(rid) => Some(*rid),
        Subject::Edge { src, .. } => Some(*src),
        _ => None,
    }
}

/// The `node_candidate.reason` CHECK set of 0004 is narrower than [`CandidateReason`]; the
/// margin and confidence gates and the governed and proposed reviews share `below_margin`.
fn review_reason(reason: CandidateReason) -> &'static str {
    match reason {
        CandidateReason::DiscoveredKind => "discovered_kind",
        CandidateReason::GovernedSemantics => "governed_semantics",
        CandidateReason::Merge => "merge",
        // `BelowMargin`, `BelowMinConfidence`, `Governed`, `Proposed`, and (the enum is
        // `non_exhaustive`) any reason this store does not know yet: a review
        _ => "below_margin",
    }
}

#[allow(clippy::cast_precision_loss)] // 0..=10000 is exact in f64
pub(super) fn real(fixed: kaban_core::Fixed) -> f64 {
    f64::from(fixed.0) / 10_000.0
}

/// The `props.owner` of a projected edge (EN-E4): a function of the winning claim alone, so a
/// re-decide (purge, replay) writes the row the batch wrote. A human claim's edge is the
/// human's; structured ingest winners retain their producer ownership for scoped finish.
/// Other layer winners belong to the reconciler. Ordinary batch diffs protect every
/// claim-backed projection regardless of its producer tag.
pub(super) fn edge_owner(winner: &kaban_core::Assertion) -> Owner {
    if winner.layer == Layer::Human {
        Owner::Human
    } else if winner.claim_class == ClaimClass::Edge
        && winner.method == Method::IngestSubmit
        && matches!(winner.layer, Layer::Llm | Layer::Gliner | Layer::Script)
    {
        Owner::KabanParse
    } else {
        Owner::KabanReconcile
    }
}

/// Apply `verdict` to `group` (whose key is `key`) under `protection` (EN-F12c) and return
/// the disposition counted for the batch report: one UPDATE per changed status, so the
/// `assertion_status_au` trigger writes exactly one `status` row per change. A protected
/// group's edge row is touched by a human claim's new win only: a non-human verdict (and the
/// human's own repeated win) leaves it byte-identical; under a human winner the supporters
/// of another value are `unresolved`, the agreeing ones `dissent`. `lost_member` says the
/// group is decided without a supporter it had (a masked or retired claim, EN-S9): the row
/// is then re-projected whatever the protection says, so bytes copied from that supporter
/// never outlive it (a surviving winner rewrites the same bytes; a lost one loses the row).
/// `setup` names the ontology a span winner's kind is checked against (EN-F13: an undeclared
/// kind materialises nothing).
#[allow(clippy::too_many_arguments)] // the one projection entry: the group, its verdict and the transaction's facts
pub(crate) fn apply(
    conn: &Connection,
    setup: &Setup,
    group: &Group,
    key: &str,
    verdict: &Verdict,
    protection: &ProtectionSet,
    lost_member: bool,
    now: i64,
) -> Result<AssertionStatus> {
    let Some(first) = group.claims.first() else {
        return Ok(AssertionStatus::Rejected);
    };
    let winner = match verdict {
        Verdict::Winner { assertion_key, .. } => group.claim_of(assertion_key),
        _ => None,
    };
    // the human claim an `Unresolved{human_protected}` keeps as the group's winner
    let kept_human = match verdict {
        Verdict::Unresolved {
            reason: UnresolvedReason::HumanProtected,
            ..
        } => protection
            .human_winner
            .as_ref()
            .and_then(|k| group.claim_of(k)),
        _ => None,
    };
    let lead = winner.or(kept_human);
    // a verdict naming a key outside the group (an injected reconciler's) decides nothing:
    // the group is left for review as `rejected`, never projected
    let others = if matches!(verdict, Verdict::Winner { .. }) && winner.is_none() {
        AssertionStatus::Rejected
    } else {
        disposition_of(verdict)
    };
    for (id, claim) in group.ids.iter().zip(&group.claims) {
        let disposition = match lead {
            Some(w) if w.key == claim.key => AssertionStatus::Winner,
            Some(w) if w.layer == Layer::Human => {
                if v1::same_value(claim, w) {
                    AssertionStatus::Dissent
                } else {
                    AssertionStatus::Unresolved
                }
            }
            _ => others,
        };
        persist::set_status(conn, *id, disposition, now)?;
    }
    // an identity group's physical effect is the cluster projection of the final verdicts
    // (E2.10b BR-4): the redirect, or its removal, follows the statuses just set
    if first.claim_class == ClaimClass::Identity
        && let (Subject::Node(lo), Some(hi)) = (&first.subject, first.object_rid)
    {
        er::reproject_identity(conn, *lo, hi, now)?;
    }
    // a protected row moves for a human claim's news only: a new human winner, or a human
    // claim that joined the group since its last decision; a machine joining it, agreeing or
    // not, leaves the row byte-identical (its dissent lives in `assertion_status`)
    let row_protected = protection.accepted_record_edge || protection.human_winner.is_some();
    let human_news = winner.is_some_and(|w| {
        w.layer == Layer::Human
            && (protection.human_winner.as_ref() != Some(&w.key) || group.fresh_human())
    });
    let missing_human_projection =
        if let Some(winner) = winner.filter(|claim| claim.layer == Layer::Human) {
            if let Subject::Edge { src, rel, dst } = &winner.subject {
                !conn
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM edge WHERE src=?1 AND dst=?2 AND rel=?3)",
                        rusqlite::params![src, dst, rel],
                        |r| r.get::<_, bool>(0),
                    )
                    .sql()?
            } else {
                false
            }
        } else {
            false
        };
    let touch_edge = !row_protected || human_news || missing_human_projection || lost_member;
    let rid = review_rid(group, &first.subject);
    match (verdict, winner) {
        (
            Verdict::Winner {
                confidence,
                dissent,
                ..
            },
            Some(winner),
        ) => {
            if let Subject::Edge { src, rel, dst } = &winner.subject
                && touch_edge
            {
                // the stable claim keys (EN-F12a), never storage row ids: the row is then
                // byte-identical whatever order the claims arrived in
                let provenance: Vec<&str> = group
                    .claims
                    .iter()
                    .filter(|a| v1::same_value(a, winner))
                    .map(|a| a.key.0.as_str())
                    .collect();
                let dissent: Vec<&str> = dissent
                    .iter()
                    .filter(|k| group.claim_of(k).is_some())
                    .map(|k| k.0.as_str())
                    .collect();
                let mut props = match winner.value.as_value() {
                    Value::Object(m) => m.clone(),
                    _ => Map::new(),
                };
                props.insert("owner".into(), json!(edge_owner(winner).as_str()));
                props.insert("provenance".into(), json!(provenance));
                props.insert("dissent".into(), json!(dissent));
                write_edge(
                    conn,
                    &EdgeWrite {
                        src: *src,
                        dst: *dst,
                        rel,
                        confidence: Some(real(*confidence)),
                        props: &Value::Object(props),
                        layer: Some(winner.layer),
                        method: Some(winner.method),
                        score: Some(real(winner.raw_score)),
                        source_id: winner.source_id.as_deref(),
                        offset: None,
                        len: None,
                    },
                )?;
                candidates::refresh_mean_score(conn, *src)?;
            } else if let Subject::Span {
                chunk_rid,
                offset,
                len,
            } = &winner.subject
                && winner.claim_class == ClaimClass::Type
                && touch_edge
            {
                // EN-F13 (E2.5): the span winner's graph output, the `MENTIONS` edge to the
                // resolved target or the `proposed` record with its `EVIDENCE_FOR`
                let dissent: Vec<kaban_core::AssertionKey> = dissent
                    .iter()
                    .filter(|k| group.claim_of(k).is_some())
                    .cloned()
                    .collect();
                materialise::span_winner(
                    conn,
                    setup,
                    &materialise::SpanWin {
                        group,
                        winner,
                        confidence: *confidence,
                        dissent: &dissent,
                        chunk_rid: *chunk_rid,
                        offset: *offset,
                        len: *len,
                    },
                )?;
            }
            if let Some(rid) = rid {
                candidates::release_review(conn, rid, key)?;
            }
            Ok(AssertionStatus::Winner)
        }
        (Verdict::Candidate { reason, .. }, _) => {
            if let Subject::Edge { src, rel, dst } = &first.subject
                && touch_edge
            {
                drop_reconciled_edge(conn, *src, *dst, rel)?;
            }
            // span cleanup belongs to the class that owns the materialisation (EN-F13: a `type`
            // winner's `MENTIONS` row); a group of another class deciding on the same
            // coordinates owns no row there (RW1-1)
            if let Subject::Span {
                chunk_rid,
                offset,
                len,
            } = &first.subject
                && first.claim_class == ClaimClass::Type
                && touch_edge
            {
                materialise::drop_span_edges(conn, *chunk_rid, *offset, *len, None)?;
            }
            // OWN-118: an uncertain span never hides an entity already admitted (one holding no
            // review row); review lists its group under that entity from the statuses instead
            let admitted = match (&first.subject, rid) {
                (Subject::Span { .. }, Some(object)) => !candidates::holds_row(conn, object)?,
                _ => false,
            };
            if let Some(rid) = rid.filter(|_| !admitted) {
                candidates::review(
                    conn,
                    rid,
                    first.workspace_id.as_str(),
                    review_reason(*reason),
                    key,
                )?;
            }
            Ok(AssertionStatus::Candidate)
        }
        _ => {
            // a human kept as the winner under `Unresolved{human_protected}` keeps its row
            // (its bytes are the human's own); a forced touch drops only an orphaned row
            if let Subject::Edge { src, rel, dst } = &first.subject
                && touch_edge
                && kept_human.is_none()
            {
                drop_reconciled_edge(conn, *src, *dst, rel)?;
            }
            if let Subject::Span {
                chunk_rid,
                offset,
                len,
            } = &first.subject
                && first.claim_class == ClaimClass::Type
                && touch_edge
                && kept_human.is_none()
            {
                materialise::drop_span_edges(conn, *chunk_rid, *offset, *len, None)?;
            }
            if let Some(rid) = rid {
                candidates::release_review(conn, rid, key)?;
            }
            Ok(others)
        }
    }
}
