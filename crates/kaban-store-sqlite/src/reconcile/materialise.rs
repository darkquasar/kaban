//! The graph output of a span winner (engine-spec EN-F13 "span-winner materialisation",
//! OWN-81 close; engine-plan E2.5 item 2): a `Winner` of a `span` group of class `type` whose
//! value is a declared kind writes, in the projection's transaction, either the one
//! `MENTIONS{method: reconciled, score: P}` edge from the chunk to the target the winning
//! value's supporters resolved (the first `object` in ascending key order) or, with no target,
//! the `proposed` record (kind the winning kind, text the span bytes, identity by the
//! `remember` formula so a re-decide finds it) and the `EVIDENCE_FOR` edge from the chunk to
//! it. Everything goes through [`write_edge`], so an unchanged row writes nothing (N5) and a
//! reversed arrival order leaves the same bytes. A `Candidate` or `Unresolved` span, and a
//! winner of an undeclared `x:` kind, materialise nothing (`kaban never writes "orange is a
//! fruit" on its own`); a span that loses its winner loses the `MENTIONS` row it wrote, and a
//! winner that moves (another target, a record, no output) takes the reconciler's previous
//! `MENTIONS` row with it (RW1-2). Cleanup is the `type` class's and never touches a human's
//! row (EN-F12c; RW1-1): that row leaves through `forget` or the human's own re-decision.
//!
//! ```text
//!   Verdict::Winner on Subject::Span, class type, declared kind
//!        |
//!        +-- first object among the winning value's supporters (key order)
//!        |        |
//!        |   Some(target) ==> drop_span_edges(keep: target)             [other reconciler rows go]
//!        |                ==> write_edge(chunk -MENTIONS-> target)      [N5: unchanged = no write]
//!        |        |
//!        |   None ==> drop_span_edges(keep: none)
//!        |        ==> proposed_record(chunk text[offset..offset+len])
//!        |              active row with that identity: reused; a retired one: nothing
//!        |              else INSERT node(governance proposed, props.proposed_by kaban-reconcile)
//!        |           ==> write_edge(chunk -EVIDENCE_FOR-> record)
//!        v
//!   candidates::refresh_mean_score(chunk)
//! ```

use super::project::{EdgeWrite, edge_owner, real, write_edge};
use super::{Group, Setup, v1};
use crate::candidates;
use crate::sql_error::SqlResultExt;
use kaban_core::ontology::meta::Acceptance;
use kaban_core::{
    Assertion, AssertionKey, Fixed, Layer, Method, NewEvent, Result, SignerKind, new_id, now_iso,
    stored_name,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Map, Value, json};

/// The `props.proposed_by` of a record the reconciler wrote (EN-F13): the one marker a read
/// surface or a human action tells a reconciler's proposal by.
pub(crate) const PROPOSED_BY: &str = "kaban-reconcile";

/// A span winner as the projection hands it over.
pub(super) struct SpanWin<'a> {
    pub(super) group: &'a Group,
    pub(super) winner: &'a Assertion,
    /// The verdict's confidence, `P` in EN-F12a units.
    pub(super) confidence: Fixed,
    /// The verdict's dissent, filtered to the group's keys by the caller.
    pub(super) dissent: &'a [AssertionKey],
    pub(super) chunk_rid: i64,
    pub(super) offset: u32,
    pub(super) len: u32,
}

/// Whether `kind` is a kind the reconciler's ontology declares (an `x:` label is not).
fn declared(setup: &Setup, kind: &str) -> bool {
    setup.ontology.kinds.keys().any(|q| stored_name(q) == kind)
}

/// Materialise the span winner `w` (EN-F13): the `MENTIONS` edge to the resolved target, or
/// the `proposed` record plus its `EVIDENCE_FOR` edge; nothing for an undeclared kind, a value
/// that names no kind or a span the chunk text does not cover. Before the write, the span's
/// previous reconciler-owned `MENTIONS` destination is reconciled against the new output
/// (RW1-2): a row to another target goes (the winner moved, or now writes a record or nothing),
/// the row to the same target is refreshed in place (N5), a human's row and every
/// `EVIDENCE_FOR` edge stay.
pub(super) fn span_winner(conn: &Connection, setup: &Setup, w: &SpanWin<'_>) -> Result<()> {
    let kind = w.winner.kind_name().filter(|k| declared(setup, k));
    let output = match kind {
        Some(kind) => span_output(conn, setup, w, kind)?,
        None => None,
    };
    let keep = match output {
        Some((dst, "MENTIONS")) => Some(dst),
        _ => None,
    };
    drop_span_edges(conn, w.chunk_rid, w.offset, w.len, keep)?;
    let (Some(kind), Some((dst, rel))) = (kind, output) else {
        return Ok(());
    };
    // the stable claim keys (EN-F12a), never storage row ids, as an `edge` winner's row
    let provenance: Vec<&str> = w
        .group
        .claims
        .iter()
        .filter(|a| v1::same_value(a, w.winner))
        .map(|a| a.key.0.as_str())
        .collect();
    let dissent: Vec<&str> = w.dissent.iter().map(|k| k.0.as_str()).collect();
    let mut props = Map::new();
    props.insert("kind".into(), json!(kind));
    props.insert("owner".into(), json!(edge_owner(w.winner).as_str()));
    props.insert("provenance".into(), json!(provenance));
    props.insert("dissent".into(), json!(dissent));
    write_edge(
        conn,
        &EdgeWrite {
            src: w.chunk_rid,
            dst,
            rel,
            confidence: Some(real(w.confidence)),
            props: &Value::Object(props),
            layer: Some(w.winner.layer),
            method: Some(Method::Reconciled),
            score: Some(real(w.confidence)),
            source_id: w.winner.source_id.as_deref(),
            offset: Some(w.offset),
            len: Some(w.len),
        },
    )?;
    candidates::refresh_mean_score(conn, w.chunk_rid)?;
    Ok(())
}

/// The graph output of the span winner `w` of the declared `kind`: `(dst, rel)` of the
/// `MENTIONS` edge to the resolved target or of the `EVIDENCE_FOR` edge to the `proposed`
/// record; `None` when nothing may be written (see [`proposed_record`]).
fn span_output(
    conn: &Connection,
    setup: &Setup,
    w: &SpanWin<'_>,
    kind: &str,
) -> Result<Option<(i64, &'static str)>> {
    // target selection (EN-F13, default (a)): the first `object` among the winning value's
    // supporters in ascending key order (`group.claims` is in that order)
    let target = w
        .group
        .claims
        .iter()
        .filter(|a| v1::same_value(a, w.winner))
        .find_map(|a| a.object_rid);
    match target {
        Some(dst) => Ok(Some((dst, "MENTIONS"))),
        None => Ok(proposed_record(conn, setup, w, kind)?.map(|rid| (rid, "EVIDENCE_FOR"))),
    }
}

/// The `proposed` record of a span winner without a target (EN-F13, default (b)): the row
/// whose identity is `content_hash(workspace, kind, span bytes)`, reused when active, never
/// re-proposed when a human retired it, inserted otherwise under the chunk's policy version
/// (the span bytes passed the gate with the chunk). `None` when nothing may be written: the
/// chunk text does not cover the span (or the span splits a character), the bytes are blank,
/// or the identity belongs to a retired row. OWN-84 (2) (E2.5g): under the loaded
/// `discovered.auto_accept_when` (the last namespace's block, as the registry reads it), a
/// record whose kind's `kind_stats` row passes all three and whose verdict is at or above
/// `min_mean_score` is written `accepted` with one `system` event (`kaban-auto-accept`,
/// reason `auto_accept_when`, `path_scope ["**"]`) and no EN-F12c claim; the identity, the
/// props and the `EVIDENCE_FOR` edge are the same. A kind at the `human` tier (OWN-84 (1),
/// `GovernedSet::tier`) is never written `accepted` here, whatever its counts (RW3-2).
fn proposed_record(
    conn: &Connection,
    setup: &Setup,
    w: &SpanWin<'_>,
    kind: &str,
) -> Result<Option<i64>> {
    let Some((ws, chunk_id, chunk_text, policy_version)) = conn
        .query_row(
            "SELECT workspace_id, id, text, policy_version FROM node WHERE rid = ?1",
            [w.chunk_rid],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    crate::body_codec::read_column(r, 2)?,
                    r.get::<_, String>(3)?,
                ))
            },
        )
        .optional()
        .sql()?
    else {
        return Ok(None);
    };
    let start = w.offset as usize;
    let Some(text) = start
        .checked_add(w.len as usize)
        .and_then(|end| chunk_text.get(start..end))
    else {
        return Ok(None);
    };
    if text.trim().is_empty() {
        return Ok(None);
    }
    let hash = crate::sqlite::content_hash(&ws, kind, text);
    let existing: Option<(i64, String)> = conn
        .query_row(
            "SELECT rid, lifecycle FROM node WHERE content_hash = ?1
             ORDER BY (lifecycle = 'active') DESC, rid LIMIT 1",
            [&hash],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .sql()?;
    match existing {
        Some((rid, lifecycle)) if lifecycle == "active" => return Ok(Some(rid)),
        // a human retired (or archived) the record: the verdict stands, the proposal does not
        // come back on its own
        Some(_) => return Ok(None),
        None => {}
    }
    let now = now_iso();
    let props = json!({
        "proposed_by": PROPOSED_BY,
        "span": {"chunk": chunk_id, "offset": w.offset, "len": w.len},
        "score": real(w.confidence),
    });
    let auto_accept = setup
        .ontology
        .namespaces
        .iter()
        .filter_map(|ns| setup.ontology.discovered.get(ns))
        .next_back()
        .and_then(|block| block.auto_accept_when.as_ref());
    // OWN-84 (1) over (2) (wave-3 review RW3-2): the thresholds write `accepted` at the
    // `agent` tier only; a kind declaring `acceptance: human` (its own key or the `discovered`
    // block's) stays `proposed` however the counts read (fail closed: a count is no person).
    // The tier rule is `GovernedSet::tier`'s, read from the same resolved ontology.
    let tier = kaban_core::GovernedSet::from_ontology(&setup.ontology).tier(kind);
    let accepted = match (tier, auto_accept) {
        (Acceptance::Agent, Some(when)) => {
            real(w.confidence) >= when.min_mean_score
                && candidates::stats_pass(conn, &ws, kind, when)?
        }
        (Acceptance::Human, _) | (Acceptance::Agent, None) => false,
    };
    let id = new_id();
    let bodies = crate::body_codec::Writer::for_connection(conn)?;
    // the column list of `nodes.rs` `insert_node`; `agent_id` names the writer for the
    // `n_sources` counter, the record governs no path and belongs to no repository
    conn.execute(
        "INSERT INTO node(id,kind,text,summary,tags,confidence,importance,lifecycle,governance,store_scope,path_scope,dup_key,repo,created_at,updated_at,valid_from,session_id,agent_id,workspace_id,props,content_hash,policy_version)
         VALUES (?1,?2,?3,NULL,'[]',?4,NULL,'active',?11,'project','[]',NULL,NULL,?5,?5,NULL,NULL,?6,?7,?8,?9,?10)",
        params![
            id,
            kind,
            bodies.stored(text),
            real(w.confidence),
            now,
            PROPOSED_BY,
            ws,
            serde_json::to_string(&props)?,
            hash,
            policy_version,
            if accepted { "accepted" } else { "proposed" }
        ],
    )
    .sql()?;
    let rid = conn.last_insert_rowid();
    if accepted {
        crate::events::append(
            conn,
            &id,
            &ws,
            "accepted",
            &NewEvent {
                reason: "auto_accept_when".into(),
                path_scope: vec!["**".into()],
                signer: Some("kaban-auto-accept".into()),
                signer_kind: SignerKind::System,
                ..Default::default()
            },
        )?;
    }
    candidates::touch_stats(conn, &ws, kind)?;
    Ok(Some(rid))
}

/// Remove the `MENTIONS` rows the reconciler materialised for a `type` span at these
/// coordinates, except the one to `keep` (the destination the span's winner writes now; `None`
/// when it writes none): the span was decided without a winner, or its winner moved to another
/// target or to a `proposed` record (EN-F13: as an `edge` winner's row is dropped; RW1-2). The
/// record and its `EVIDENCE_FOR` edge stay for the human. A producer's structural row (no
/// `layer`) stays, and so does a human's row (`layer = human`): it leaves through `forget` or
/// the human's own re-decision, never through another verdict's cleanup (EN-F12c; RW1-1).
pub(super) fn drop_span_edges(
    conn: &Connection,
    chunk_rid: i64,
    offset: u32,
    len: u32,
    keep: Option<i64>,
) -> Result<()> {
    conn.execute(
        "DELETE FROM edge WHERE src = ?1 AND rel = 'MENTIONS' AND layer IS NOT NULL AND layer != ?4
           AND offset = ?2 AND len = ?3 AND (?5 IS NULL OR dst != ?5)",
        params![chunk_rid, offset, len, Layer::Human.as_str(), keep],
    )
    .sql()?;
    Ok(())
}
