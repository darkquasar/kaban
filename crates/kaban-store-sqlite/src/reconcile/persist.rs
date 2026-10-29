//! Claim rows and statuses (engine-spec EN-F8, EN-F8a, EN-F12a): the immutable `assertion`
//! insert (a re-submitted key writes nothing), the `assertion_status` row every claim owns,
//! the group loader the pass decides over, and the `forget`/purge paths over a node's claims.
//! Every function runs inside the caller's transaction; none opens one.

use super::Group;
use crate::rows::bad_enum;
use crate::sql_error::SqlResultExt;
use kaban_core::{
    Assertion, AssertionKey, AssertionStatus, CanonicalJson, ClaimClass, Fixed, Layer, Method,
    Rank, Result, StoreError, Subject, SupersedesClass, WorkspaceId, group_key, same_group,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;

/// Unix seconds now, the `ts` of a claim row and the `updated_at` of a status.
pub(crate) fn now_unix() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs()),
    )
    .unwrap_or(i64::MAX)
}

/// What `insert_claim` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Inserted {
    /// A new row (and its `pending` status row) was written.
    Row(i64),
    /// A row with this key exists: nothing written, no `ChangeLog` row (EN-F12a N5).
    Existing(i64),
}

/// The columns of an `assertion` row in the order [`row_claim`] decodes them.
const CLAIM_COLS: &str = "a.id, a.key, a.workspace_id, a.subject_kind, a.subject_rid, a.chunk_rid, a.offset, a.len, a.claim_class, a.predicate, a.object_rid, a.value, a.layer, a.method, a.extractor_id, a.extractor_version, a.source_id, a.lineage_id, a.raw_score, a.rank, a.rank_reason, a.calibration_version, (SELECT s.key FROM assertion s WHERE s.id = a.supersedes), a.ts";

/// Insert the claim unless a row with its key exists; a new row gets its `pending` status
/// with the claim's group key (EN-F8). The key must already be computed (`with_key`).
pub(crate) fn insert_claim(conn: &Connection, a: &Assertion) -> Result<Inserted> {
    if let Some(id) = existing_claim(conn, a)? {
        return Ok(Inserted::Existing(id));
    }
    let (subject_kind, subject_rid, chunk_rid, offset, len) = match &a.subject {
        Subject::Span {
            chunk_rid,
            offset,
            len,
        } => ("span", None, Some(*chunk_rid), Some(*offset), Some(*len)),
        Subject::Node(rid) => ("node", Some(*rid), None, None, None),
        Subject::Record(rid) => ("record", Some(*rid), None, None, None),
        Subject::Edge { src, .. } => ("edge", Some(*src), None, None, None),
        // `Subject` is `non_exhaustive`: the CHECK set of 0004 knows these four
        _ => return Err(crate::sql_error::backend_error(bad_enum(3, "subject"))),
    };
    let supersedes: Option<i64> = match &a.supersedes {
        None => None,
        Some(key) => Some(superseded_row(conn, a, key)?),
    };
    let written = conn
        .execute(
            "INSERT INTO assertion(key, workspace_id, subject_kind, subject_rid, chunk_rid, offset, len,
                 claim_class, predicate, object_rid, value, layer, method, extractor_id, extractor_version,
                 source_id, lineage_id, raw_score, rank, rank_reason, calibration_version, supersedes, ts)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23)
             ON CONFLICT(key) DO NOTHING",
            params![
                a.key.0,
                a.workspace_id.as_str(),
                subject_kind,
                subject_rid,
                chunk_rid,
                offset,
                len,
                a.claim_class.as_str(),
                a.predicate,
                a.object_rid,
                a.value.as_str(),
                a.layer.as_str(),
                a.method.as_str(),
                a.extractor_id,
                a.extractor_version,
                a.source_id,
                a.lineage_id,
                a.raw_score.0,
                a.rank.as_str(),
                a.rank_reason,
                a.calibration_version,
                supersedes,
                a.ts
            ],
        )
        .sql()?;
    if written == 0 {
        let id: i64 = conn
            .query_row("SELECT id FROM assertion WHERE key = ?", [&a.key.0], |r| {
                r.get(0)
            })
            .sql()?;
        return Ok(Inserted::Existing(id));
    }
    let id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO assertion_status(assertion_id, disposition, group_key, updated_at) VALUES (?1, 'pending', ?2, ?3)",
        params![id, group_key(a), a.ts],
    )
    .sql()?;
    Ok(Inserted::Row(id))
}

/// A stored immutable key is a replay before its old supersession reference is validated;
/// tenant membership is still checked before taking that no-op shortcut.
pub(crate) fn existing_claim(conn: &Connection, claim: &Assertion) -> Result<Option<i64>> {
    let row: Option<(i64, String)> = conn
        .query_row(
            "SELECT id,workspace_id FROM assertion WHERE key=?1",
            [claim.key.0.as_str()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .sql()?;
    match row {
        Some((_, workspace)) if workspace != claim.workspace_id.as_str() => {
            Err(StoreError::SupersedesInvalid(SupersedesClass::NotFound))
        }
        Some((id, _)) => Ok(Some(id)),
        None => Ok(None),
    }
}

/// The row id of the stored claim `key` names as the one `a` supersedes (OWN-86 (3), EN-F8):
/// a claim of `a`'s workspace (another tenant's key is `supersedes_not_found`, as an unknown
/// one is) and of `a`'s conflict group (`supersedes_cross_group` otherwise).
pub(crate) fn superseded_row(conn: &Connection, a: &Assertion, key: &AssertionKey) -> Result<i64> {
    let target = conn
        .prepare_cached(&format!(
            "SELECT {CLAIM_COLS} FROM assertion a WHERE a.key = ?1 AND a.workspace_id = ?2"
        ))
        .sql()?
        .query_row(params![key.0, a.workspace_id.as_str()], row_claim)
        .optional()
        .sql()?;
    match target {
        None => Err(StoreError::SupersedesInvalid(SupersedesClass::NotFound)),
        Some((_, target)) if !same_group(&target, a) => {
            Err(StoreError::SupersedesInvalid(SupersedesClass::CrossGroup))
        }
        Some((id, _)) => Ok(id),
    }
}

/// Every stored claim of a group with its current status, in ascending `key` order (EN-F12a
/// group order).
pub(crate) fn load_group(conn: &Connection, group: &str) -> Result<Group> {
    let mut st = conn
        .prepare_cached(&format!(
            "SELECT {CLAIM_COLS}, s.disposition FROM assertion a JOIN assertion_status s ON s.assertion_id = a.id
             WHERE s.group_key = ?1 ORDER BY a.key"
        ))
        .sql()?;
    let mut out = Group::default();
    let rows = st
        .query_map([group], |r| {
            let (id, claim) = row_claim(r)?;
            let disposition: String = r.get(24)?;
            let status =
                AssertionStatus::parse(&disposition).ok_or_else(|| bad_enum(24, &disposition))?;
            Ok((id, claim, status))
        })
        .sql()?;
    for row in rows {
        let (id, claim, status) = row.sql()?;
        out.ids.push(id);
        out.claims.push(claim);
        out.statuses.push(status);
    }
    Ok(out)
}

/// Recover disposable statuses from immutable claims at indexed conflict coordinates.
/// A new incoming claim is not inserted yet, so it cannot become historical authority.
pub(crate) fn restore_group(
    conn: &Connection,
    seed: &Assertion,
    now: i64,
) -> Result<(Group, bool)> {
    use rusqlite::types::Value as SqlValue;
    let mut values = vec![
        SqlValue::Text(seed.workspace_id.to_string()),
        SqlValue::Text(seed.claim_class.as_str().into()),
    ];
    let coordinates = match &seed.subject {
        Subject::Span {
            chunk_rid,
            offset,
            len,
        } => {
            values.extend([
                SqlValue::Integer(*chunk_rid),
                SqlValue::Integer(i64::from(*offset)),
                SqlValue::Integer(i64::from(*len)),
            ]);
            "a.subject_kind='span' AND a.chunk_rid=?3 AND a.offset=?4 AND a.len=?5"
        }
        Subject::Edge { src, rel, dst } => {
            values.extend([
                SqlValue::Integer(*src),
                SqlValue::Text(rel.clone()),
                SqlValue::Integer(*dst),
            ]);
            "a.subject_kind='edge' AND a.subject_rid=?3 AND a.predicate=?4 AND a.object_rid=?5"
        }
        Subject::Node(rid) => {
            values.push(SqlValue::Integer(*rid));
            "a.subject_kind='node' AND a.subject_rid=?3"
        }
        Subject::Record(rid) => {
            values.push(SqlValue::Integer(*rid));
            "a.subject_kind='record' AND a.subject_rid=?3"
        }
        _ => return Err(StoreError::EdgeRequiresAssertion),
    };
    let mut statement = conn.prepare_cached(&format!("SELECT {CLAIM_COLS} FROM assertion a WHERE a.workspace_id=?1 AND a.claim_class=?2 AND {coordinates} ORDER BY a.key")).sql()?;
    let rows = statement
        .query_map(rusqlite::params_from_iter(values), row_claim)
        .sql()?
        .collect::<rusqlite::Result<Vec<_>>>()
        .sql()?;
    let key = group_key(seed);
    let mut restored = false;
    for (id, claim) in rows {
        if !same_group(seed, &claim) {
            continue;
        }
        restored |= conn.execute("INSERT INTO assertion_status(assertion_id,disposition,group_key,updated_at) VALUES(?1,'pending',?2,?3) ON CONFLICT(assertion_id) DO NOTHING", params![id,key,now]).sql()? != 0;
    }
    Ok((load_group(conn, &key)?, restored))
}

/// The canonical accepted state, independent of the disposable edge projection.
pub(crate) fn accepted_endpoint(conn: &Connection, src: i64, dst: i64) -> Result<bool> {
    conn.query_row("SELECT EXISTS(SELECT 1 FROM node WHERE rid IN (?1,?2) AND governance='accepted' AND lifecycle='active')", params![src,dst], |r| r.get(0)).sql()
}

/// One stored claim by its row id (`None` when the row is gone: a `delete` `ChangeLog` row
/// replayed after the purge).
pub(crate) fn load_claim(conn: &Connection, id: i64) -> Result<Option<Assertion>> {
    let mut st = conn
        .prepare_cached(&format!(
            "SELECT {CLAIM_COLS} FROM assertion a WHERE a.id = ?1"
        ))
        .sql()?;
    let row = st.query_row([id], row_claim).optional().sql()?;
    Ok(row.map(|(_, claim)| claim))
}

/// A fresh producer generation may replace producer-set retirement, never an explicit
/// human deprecation or policy/forget retirement of the same underlying claim identity.
pub(crate) fn carry_gazetteer_retirement(conn: &Connection, claim: &mut Assertion) -> Result<()> {
    if claim.layer != Layer::Gazetteer
        || !claim.extractor_version.as_deref().is_some_and(|version| {
            version
                .rsplit_once(":set:")
                .is_some_and(|(_, generation)| generation.parse::<u64>().is_ok())
        })
    {
        return Ok(());
    }
    let base_key = without_set_generation(claim.clone()).with_key()?.key;
    let ids: Vec<i64> = conn.prepare_cached("SELECT id FROM assertion WHERE lineage_id=?1 AND workspace_id=?2 AND layer='gazetteer' AND rank='deprecated' ORDER BY id").sql()?.query_map(params![claim.lineage_id,claim.workspace_id.as_str()],|r|r.get(0)).sql()?.collect::<rusqlite::Result<_>>().sql()?;
    for id in ids {
        if let Some(prior) = load_claim(conn, id)? {
            if prior.rank_reason.as_deref() == Some("producer-set:retired")
                && crate::sqlite::artifacts::machine_retired(
                    conn,
                    &prior.key.0,
                    prior.workspace_id.as_str(),
                )?
            {
                continue;
            }
            let reason = prior.rank_reason.clone();
            if without_set_generation(prior).with_key()?.key == base_key {
                claim.rank = Rank::Deprecated;
                claim.rank_reason = reason;
                break;
            }
        }
    }
    Ok(())
}

fn without_set_generation(mut claim: Assertion) -> Assertion {
    let version = claim.extractor_version.as_deref().unwrap_or("1");
    claim.extractor_version = Some(
        version
            .rsplit_once(":set:")
            .filter(|(_, generation)| generation.parse::<u64>().is_ok())
            .map_or(version, |(producer, _)| producer)
            .to_owned(),
    );
    claim
}

/// The `pending` status row of a claim whose status is missing (a replay from zero,
/// EN-F8a): nothing is written when the row exists, so a replay is idempotent.
pub(crate) fn ensure_status(conn: &Connection, id: i64, group: &str, now: i64) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO assertion_status(assertion_id, disposition, group_key, updated_at)
         VALUES (?1, 'pending', ?2, ?3)",
        params![id, group, now],
    )
    .sql()?;
    Ok(())
}

/// Set one status; writes nothing when it already holds (the `assertion_status_au` trigger
/// then emits no `status` row). Returns whether the row changed.
pub(crate) fn set_status(
    conn: &Connection,
    id: i64,
    disposition: AssertionStatus,
    now: i64,
) -> Result<bool> {
    let changed = conn
        .execute(
            "UPDATE assertion_status SET disposition = ?1, updated_at = ?2
             WHERE assertion_id = ?3 AND disposition <> ?1",
            params![disposition.as_str(), now, id],
        )
        .sql()?;
    Ok(changed == 1)
}

/// The human claim that currently wins `group` (EN-F12c's `human_winner`): the smallest key
/// among the `layer = human` supporters whose status is `winner`, if any. Only eligible
/// authority protects (RE4-7): a human claim that is `deprecated` (a human retired it) or
/// that another claim `supersedes` is out of the vote (`votes()`) and out of this set, so the
/// decision after its retirement runs without it and its projection does not outlive it. A
/// `normal`-rank human winner (a store that predates the rank) still protects.
pub(crate) fn human_winner(conn: &Connection, group: &str) -> Result<Option<AssertionKey>> {
    let key: Option<String> = conn
        .prepare_cached(
            "SELECT a.key FROM assertion a JOIN assertion_status s ON s.assertion_id = a.id
             WHERE s.group_key = ?1 AND a.layer = 'human' AND s.disposition = 'winner'
               AND a.rank <> 'deprecated'
               AND NOT EXISTS (SELECT 1 FROM assertion b WHERE b.supersedes = a.id)
             ORDER BY a.key LIMIT 1",
        )
        .sql()?
        .query_row([group], |r| r.get(0))
        .optional()
        .sql()?;
    Ok(key.map(AssertionKey))
}

/// Whether the triple has a materialised `edge` row and one of its endpoints is an accepted
/// governed record (the row half of EN-F12c's `accepted_record_edge`; the rel half is the
/// caller's).
pub(crate) fn edge_touches_accepted_record(
    conn: &Connection,
    src: i64,
    dst: i64,
    rel: &str,
) -> Result<bool> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM edge WHERE src = ?1 AND dst = ?2 AND rel = ?3)
            AND EXISTS(SELECT 1 FROM node WHERE rid IN (?1, ?2) AND governance = 'accepted')",
        params![src, dst, rel],
        |r| r.get(0),
    )
    .sql()
}

/// The group keys of every claim naming `rid` as subject, span chunk or object.
pub(crate) fn groups_of_node(conn: &Connection, rid: i64) -> Result<Vec<String>> {
    let mut st = conn
        .prepare_cached(
            "SELECT DISTINCT s.group_key FROM assertion_status s JOIN assertion a ON a.id = s.assertion_id
             WHERE a.subject_rid = ?1 OR a.chunk_rid = ?1 OR a.object_rid = ?1 ORDER BY s.group_key",
        )
        .sql()?;
    let rows = st
        .query_map([rid], |r| r.get::<_, String>(0))
        .sql()?
        .collect::<rusqlite::Result<Vec<_>>>()
        .sql()?;
    Ok(rows)
}

/// The `rank_reason` of a claim the engine retired because its node was forgotten (RE4-1):
/// a machine reason, distinct from any human deprecation's free text.
pub(crate) const FORGOTTEN_REASON: &str = "forgotten";
/// The `rank_reason` prefix of a claim the policy rescan retired: `policy:<rule ids>`.
pub(crate) const POLICY_REASON_PREFIX: &str = "policy:";
/// The `index_meta` key of the policy rescan's high-water mark: the greatest `assertion.id`
/// the last pass reached (RE4-3), beside the `assertion_policy_hash` version row.
pub(crate) const ASSERTION_POLICY_HW: &str = "assertion_policy_hw";

/// Whether the engine retired this claim (RE4-1, D3): `deprecated` under one of the machine
/// reasons. Such a row is out of every vote and projection and its status stays `rejected`;
/// a human deprecation (free-text reason) is a non-voting supporter whose status follows
/// the verdict.
pub(crate) fn retired(a: &Assertion) -> bool {
    a.rank == Rank::Deprecated
        && a.rank_reason.as_deref().is_some_and(|r| {
            r == FORGOTTEN_REASON
                || r == "layer:forgotten"
                || r.starts_with(POLICY_REASON_PREFIX)
                || r.starts_with("producer-set:")
        })
}

/// The row ids of every claim naming `rid` as subject, span chunk or object.
pub(crate) fn claim_ids_of_node(conn: &Connection, rid: i64) -> Result<Vec<i64>> {
    let mut st = conn
        .prepare_cached(
            "SELECT id FROM assertion WHERE subject_rid = ?1 OR chunk_rid = ?1 OR object_rid = ?1 ORDER BY id",
        )
        .sql()?;
    let rows = st
        .query_map([rid], |r| r.get::<_, i64>(0))
        .sql()?
        .collect::<rusqlite::Result<Vec<_>>>()
        .sql()?;
    Ok(rows)
}

/// `forget` (tombstone) deletes no claim: every status of the node's claims becomes
/// `rejected`, one `status` `ChangeLog` row each (EN-F8a), and every claim not already
/// `deprecated` is retired with the reason `forgotten` (RE4-1: the retirement lives on the
/// row, so no later decide or replay lets the claim vote; a human deprecation keeps its own
/// reason). Returns how many statuses changed.
pub(crate) fn reject_claims_of_node(conn: &Connection, rid: i64, now: i64) -> Result<usize> {
    conn.execute(
        "UPDATE assertion SET rank = 'deprecated', rank_reason = ?2
         WHERE rank <> 'deprecated' AND (subject_rid = ?1 OR chunk_rid = ?1 OR object_rid = ?1)",
        params![rid, FORGOTTEN_REASON],
    )
    .sql()?;
    let changed = conn
        .execute(
            "UPDATE assertion_status SET disposition = 'rejected', updated_at = ?2
             WHERE disposition <> 'rejected' AND assertion_id IN (
                 SELECT id FROM assertion WHERE subject_rid = ?1 OR chunk_rid = ?1 OR object_rid = ?1)",
            params![rid, now],
        )
        .sql()?;
    Ok(changed)
}

/// `Op::Reactivate` brings a forgotten node's claims back: every claim retired with the
/// reason `forgotten` whose named nodes are all visible again returns to `rank = normal`
/// (reason cleared); a policy-retired or human-deprecated claim stays as it is. The caller
/// re-decides the node's groups. Returns how many rows came back.
pub(crate) fn restore_forgotten_claims_of_node(conn: &Connection, rid: i64) -> Result<usize> {
    conn.execute(
        "UPDATE assertion SET rank = 'normal', rank_reason = NULL
         WHERE rank = 'deprecated' AND rank_reason = ?2
           AND (subject_rid = ?1 OR chunk_rid = ?1 OR object_rid = ?1)
           AND NOT EXISTS (SELECT 1 FROM node n
                           WHERE n.rid IN (assertion.subject_rid, assertion.chunk_rid, assertion.object_rid)
                             AND n.lifecycle IN ('tombstoned', 'purged'))",
        params![rid, FORGOTTEN_REASON],
    )
    .sql()
}

/// The S5 purge of a node's engine rows (EN-F8a): what the `ON DELETE CASCADE` of a node row
/// would take goes explicitly, because the purged node keeps its id row as a `purged` ghost
/// (core-spec F5): every claim naming the node (three FKs; the `assertion_ad` trigger writes
/// one `delete` row each and `assertion_status` cascades), its `node_candidate`,
/// `node_identity` (a canonical reference is set to NULL, as the FK says) and
/// `derived_artifact` rows. Returns the group keys that lost a member, so the caller
/// re-decides them in the same transaction. The one DELETE on `assertion`: `id` is an
/// `INTEGER PRIMARY KEY` without AUTOINCREMENT, so removing the highest rows could let a
/// later insert reuse an id below the rescan's high-water mark; the mark is lowered to the
/// greatest surviving id (RE4-3) so the next pass judges such a row.
pub(crate) fn purge_claims_of_node(conn: &Connection, rid: i64) -> Result<Vec<String>> {
    let groups = groups_of_node(conn, rid)?;
    conn.execute(
        "DELETE FROM assertion WHERE subject_rid = ?1 OR chunk_rid = ?1 OR object_rid = ?1",
        [rid],
    )
    .sql()?;
    conn.execute(
        "UPDATE index_meta SET value = CAST(min(CAST(value AS INTEGER),
             (SELECT COALESCE(max(id), 0) FROM assertion)) AS TEXT) WHERE key = ?1",
        [ASSERTION_POLICY_HW],
    )
    .sql()?;
    conn.execute("DELETE FROM node_candidate WHERE rid = ?1", [rid])
        .sql()?;
    conn.execute("DELETE FROM node_identity WHERE rid = ?1", [rid])
        .sql()?;
    conn.execute(
        "UPDATE node_identity SET canonical_rid = NULL WHERE canonical_rid = ?1",
        [rid],
    )
    .sql()?;
    conn.execute("DELETE FROM derived_artifact WHERE source_rid = ?1", [rid])
        .sql()?;
    Ok(groups)
}

/// Decode one `assertion` row ([`CLAIM_COLS`] order) into its row id and the claim.
fn row_claim(r: &rusqlite::Row<'_>) -> rusqlite::Result<(i64, Assertion)> {
    let subject_kind: String = r.get(3)?;
    let subject_rid: Option<i64> = r.get(4)?;
    let chunk_rid: Option<i64> = r.get(5)?;
    let offset: Option<u32> = r.get(6)?;
    let len: Option<u32> = r.get(7)?;
    let claim_class: String = r.get(8)?;
    let predicate: Option<String> = r.get(9)?;
    let object_rid: Option<i64> = r.get(10)?;
    let value: Option<String> = r.get(11)?;
    let layer: String = r.get(12)?;
    let method: String = r.get(13)?;
    let rank: String = r.get(19)?;
    let supersedes: Option<String> = r.get(22)?;
    let subject = match subject_kind.as_str() {
        "span" => Subject::Span {
            chunk_rid: chunk_rid.ok_or_else(|| bad_enum(5, "NULL"))?,
            offset: offset.unwrap_or(0),
            len: len.unwrap_or(0),
        },
        "node" => Subject::Node(subject_rid.ok_or_else(|| bad_enum(4, "NULL"))?),
        "record" => Subject::Record(subject_rid.ok_or_else(|| bad_enum(4, "NULL"))?),
        "edge" => Subject::Edge {
            src: subject_rid.ok_or_else(|| bad_enum(4, "NULL"))?,
            rel: predicate.clone().unwrap_or_default(),
            dst: object_rid.ok_or_else(|| bad_enum(10, "NULL"))?,
        },
        other => return Err(bad_enum(3, other)),
    };
    let value: Value = match value {
        None => Value::Null,
        Some(text) => serde_json::from_str(&text).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(11, rusqlite::types::Type::Text, Box::new(e))
        })?,
    };
    let raw: u32 = r.get(18)?;
    let ws: String = r.get(2)?;
    Ok((
        r.get(0)?,
        Assertion {
            key: AssertionKey(r.get(1)?),
            workspace_id: WorkspaceId::new(&ws),
            subject,
            claim_class: ClaimClass::parse(&claim_class)
                .ok_or_else(|| bad_enum(8, &claim_class))?,
            predicate,
            object_rid,
            value: CanonicalJson::new(value),
            layer: Layer::parse(&layer).ok_or_else(|| bad_enum(12, &layer))?,
            method: Method::parse(&method).ok_or_else(|| bad_enum(13, &method))?,
            extractor_id: r.get(14)?,
            extractor_version: r.get(15)?,
            source_id: r.get(16)?,
            lineage_id: r.get(17)?,
            raw_score: Fixed(raw),
            rank: Rank::parse(&rank).ok_or_else(|| bad_enum(19, &rank))?,
            rank_reason: r.get(20)?,
            calibration_version: r.get(21)?,
            supersedes: supersedes.map(AssertionKey),
            ts: r.get(23)?,
        },
    ))
}
