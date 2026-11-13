//! Discovered labels and the candidate state (engine-spec EN-F5, EN-F5a, EN-F6, EN-F49;
//! kept as a bridge until the discovered kinds go with PC-14 (T3 slice B);
//! engine-plan E1.8): the placement state machine of a record's kind, the per-workspace cap,
//! the `kind_stats` counters, the read-path exclusion predicate, and `promote`/`prune`.
//!
//! # The placement state machine
//!
//! ```text
//!   kind as written ("Vendor", "x:Vendor", "user:Widget", "Decision", ...)
//!        |  normalise (E1.7a: a qualified spelling of a bare namespace is its bare name)
//!        v
//!   +---------------------+  declared in schema_type   +--------------------------+
//!   | place_kind          | =========================> | Placement::Declared(kind) |
//!   +---------------------+                            +--------------------------+
//!        |  undeclared bare or `x:` label
//!        |  1. identifier regex       -> Err(invalid_identifier)      nothing stored
//!        |  2. shadows a governed kind -> Err(kind_not_allowed)       nothing stored
//!        |  3. policy deny            -> Err(kind_not_allowed)       nothing stored
//!        v
//!   +----------------------------------------------+
//!   | Placement::Discovered { label: "x:Vendor",   |
//!   |                         policy: review|allow }|
//!   +----------------------------------------------+
//!        |  admit_discovered: a new `x:` kind beyond the workspace cap
//!        |     -> Admission::CapReached (discovered_overflow.n_rejected += 1, nothing else)
//!        v  node row inserted by the caller (governance none under review, proposed under allow)
//!   record_discovered: schema_type(ns = 'x') row, kind_stats upsert,
//!                      node_candidate(reason = 'discovered_kind', group_key = 'x:Vendor') under review
//! ```
//!
//! States of a discovered node afterwards: **candidate** (a `node_candidate` row; excluded by
//! every read surface unless `ReadFilters::include_candidates`) until a human **accepts** it
//! (`GovernanceStore::transition` to `accepted`: the row is deleted in the acceptance
//! transaction), the kind is **promoted** (`promote_kind`: nodes renamed to the declared
//! kind, every `discovered_kind` row of the group deleted, one `events` row, one
//! transaction) or the kind is **pruned** (`prune_kinds`: the candidate nodes tombstoned, the
//! rows deleted, the cap released). `node_candidate` is written by this module only, from
//! `apply_batch` and `remember` (the "maintained only by `apply_batch`" of EN-F5a, where
//! `remember` is the one-record batch). `assertion_status.disposition = 'candidate'` is E2.2's
//! per-claim view and is not touched here: `node_candidate` is the per-node truth.
//!
//! # Knob precedence
//!
//! The policy name comes from the loaded ontologies ([`kaban_core::DiscoveredRules`], set by
//! the service: the last loaded namespace declaring a `discovered` block, `review` for the
//! shipped set). `cap`, `prune_after_days` and `promote_when` are the values of that
//! policy-declaring block when it sets them, else the `[discovered]` register of
//! `EngineConfig` (engine-spec §15.1: the register is the default, not the only source). The
//! store receives the resolved values and reads no config itself.
//!
//! # Counting semantics of `kind_stats` (EN-F6; E1.9a RE3-7)
//!
//! A row exists for every discovered kind of a workspace from its first node on, and for a
//! declared kind only once a promotion moved a row under its name. While a row exists it
//! mirrors the workspace's **active** nodes of that stored kind: `n_nodes` is their count,
//! `n_sources` the distinct writer identities among them (`coalesce(session_id, agent_id,
//! repo, '')`, provisional until E2 defines provenance), `last_seen` the last insert or
//! content update, `first_seen` the first. The write paths keep it so: an insert or an
//! `apply_batch` update of a counted kind refreshes the counts and `last_seen`
//! ([`touch_stats`]); a retirement (`forget`, a batch tombstone, an `on_link` archive or
//! tombstone, `prune`) recounts without touching `last_seen` ([`recount_stats`]). A
//! promotion recounts under the declared name from the nodes, so the merged `n_sources` is
//! the distinct union of the writer identities and `n_nodes` includes the records remembered
//! under the declared name before. A row whose nodes are all retired keeps `n_nodes = 0`
//! until `prune` lists and removes it (the cap slot is released then, never automatically).
//!
//! # Identity after a promotion (F1 dedup identity; E1.9a RE3-3)
//!
//! `node.content_hash` is a record's dedup identity: `remember` hashes `(workspace, kind,
//! trimmed text)`, `apply_batch` hashes the written kind, the id and every content field under
//! the `kaban-batch/v1` tag, a rescan rewrite the masked strings under `kaban-redacted/v1`.
//! `promote_kind` recomputes the identity of every renamed row with the **`remember`**
//! formula under the target kind (the one formula a later `remember` compares against), so
//! `remember infra:Vendor/T` finds the promoted record and `remember Vendor/T` starts a new
//! candidate; a new identity equal to an existing active row's refuses the whole promotion
//! (`promote_conflict`). A batch-written row therefore changes identity on promotion: a later
//! batch upsert of its id under the old written kind is `KindMismatch` (kind is immutable per
//! id, WD-2) and one under the promoted kind is an `Updated` row.
//!
//! SQL lives in this crate only (engine-plan §4 placement).

use crate::ontology::{self, TypeTable, valid_bare_label};
use crate::sql_error::SqlResultExt;
use kaban_core::config::PromoteWhen;
use kaban_core::ontology::meta::{Acceptance, DiscoveredPolicyName};
use kaban_core::{
    DiscoveredRules, GovernedSet, KindStat, OntologyError, PromoteReport, PruneReport, ReadFilters,
    Result, StoreError, split_stored,
};
use rusqlite::{Connection, OptionalExtension, params};
use std::time::Duration;

/// The namespace of discovered kinds (engine-spec §3.1: reserved, never a parseable `Ns`).
pub(crate) const DISCOVERED_NS: &str = "x";
/// The stored-name prefix of a discovered kind.
const DISCOVERED_PREFIX: &str = "x:";
/// The `node_candidate.reason` of a discovered-label candidate.
const REASON_DISCOVERED: &str = "discovered_kind";

/// Where a record's kind lands.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Placement {
    /// A declared kind, in its canonical stored spelling.
    Declared(String),
    /// An undeclared label admitted under the policy, as the `x:` stored name.
    Discovered {
        /// The stored name (`x:Vendor`).
        label: String,
        /// `review` (candidate) or `allow` (proposed); `deny` never reaches here.
        policy: DiscoveredPolicyName,
        /// OWN-84 (2): the loaded `discovered.auto_accept_when` (the tenant's governed set);
        /// a `review` placement whose `kind_stats` row passes it writes no queue item.
        auto_accept: Option<kaban_core::ontology::meta::PromoteWhen>,
    },
}

impl Placement {
    /// The stored kind the node row carries.
    pub(crate) fn stored_kind(&self) -> &str {
        match self {
            Self::Declared(kind) => kind,
            Self::Discovered { label, .. } => label,
        }
    }

    /// The bare label of a discovered placement.
    fn local(&self) -> Option<&str> {
        match self {
            Self::Discovered { label, .. } => label.strip_prefix(DISCOVERED_PREFIX),
            Self::Declared(_) => None,
        }
    }
}

/// Place `kind` (EN-F5): declared kinds pass in their canonical spelling; an undeclared bare
/// or `x:`-prefixed label is checked (identifier regex, governed shadow, policy) and returned
/// as `x:<Label>`. An undeclared name qualified with another namespace is `UnknownKind` as
/// before: extractors emit labels, not foreign qualified names.
///
/// # Errors
/// `Ontology(invalid_identifier)` for a label failing `^[A-Z][A-Za-z0-9]{0,63}$`;
/// `KindNotAllowed` under policy `deny` or when the label's bare name is a governed kind's;
/// `UnknownKind` for an undeclared foreign qualified name; backend errors.
pub(crate) fn place_kind(
    conn: &Connection,
    governed: &GovernedSet,
    rules: &DiscoveredRules,
    kind: &str,
) -> Result<Placement> {
    let canonical = ontology::normalise_stored(kind);
    if ontology::kind_exists(conn, &canonical)? {
        return Ok(Placement::Declared(canonical));
    }
    // a store still at schema 0003 (fixtures, benches) has no discovered-kind tables: an
    // undeclared kind is `UnknownKind` there, as before 0004
    if TypeTable::of(conn)? == TypeTable::Stored {
        return Err(StoreError::UnknownKind(canonical));
    }
    // `x` is not a parseable `Ns` (reserved, engine-spec §3.1), so the prefix is split here
    let local = match canonical.strip_prefix(DISCOVERED_PREFIX) {
        Some(local) => local,
        None => match split_stored(&canonical) {
            (None, bare) => bare,
            (Some(_), _) => return Err(StoreError::UnknownKind(canonical)),
        },
    };
    if !valid_bare_label(local) {
        return Err(OntologyError::InvalidIdentifier {
            path: "kind".to_owned(),
        }
        .into());
    }
    let shadows_governed = governed
        .kinds()
        .any(|stored| split_stored(stored).1 == local);
    if shadows_governed || rules.policy == DiscoveredPolicyName::Deny {
        // the fixed class alone: the label is caller data (core S3, RE3-8)
        return Err(StoreError::KindNotAllowed);
    }
    let label = format!("{DISCOVERED_NS}:{local}");
    // OWN-84 (1) over (2) (wave-3 review RW3-2): the thresholds clear a queue item at the
    // `agent` tier only; a label the `discovered` block puts at `human` keeps its row however
    // its counts read (fail closed: a count is no person)
    let auto_accept = match governed.tier(&label) {
        Acceptance::Agent => governed.auto_accept().cloned(),
        Acceptance::Human => None,
    };
    Ok(Placement::Discovered {
        label,
        policy: rules.policy,
        auto_accept,
    })
}

/// Whether a discovered label may take a node row in `ws`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Admission {
    /// Counted already, or a new kind under the cap.
    Admitted,
    /// A new kind beyond the cap: `n_rejected` moved, nothing else may be written.
    CapReached,
}

/// Admit `placement` in `ws` under `rules.cap` (EN-F5 cap): an `x:` kind already counted in
/// the workspace is admitted; a new one is admitted while fewer than `cap` are counted,
/// otherwise `discovered_overflow.n_rejected` is incremented and nothing else is written.
/// Declared placements are always admitted.
pub(crate) fn admit_discovered(
    conn: &Connection,
    ws: &str,
    rules: &DiscoveredRules,
    placement: &Placement,
) -> Result<Admission> {
    let Some(local) = placement.local() else {
        return Ok(Admission::Admitted);
    };
    let counted: Option<i64> = conn
        .prepare_cached(
            "SELECT 1 FROM kind_stats WHERE workspace_id = ?1 AND ns = ?2 AND kind = ?3",
        )
        .sql()?
        .query_row(params![ws, DISCOVERED_NS, local], |r| r.get(0))
        .optional()
        .sql()?;
    if counted.is_some() {
        return Ok(Admission::Admitted);
    }
    let n: i64 = conn
        .prepare_cached("SELECT count(*) FROM kind_stats WHERE workspace_id = ?1 AND ns = ?2")
        .sql()?
        .query_row(params![ws, DISCOVERED_NS], |r| r.get(0))
        .sql()?;
    if n < i64::from(rules.cap) {
        return Ok(Admission::Admitted);
    }
    conn.execute(
        "INSERT INTO discovered_overflow(workspace_id, n_rejected, last_rejected_at) VALUES (?1, 1, unixepoch())
         ON CONFLICT(workspace_id) DO UPDATE SET n_rejected = n_rejected + 1, last_rejected_at = unixepoch()",
        [ws],
    )
    .sql()?;
    Ok(Admission::CapReached)
}

/// Record the node row `rid` of a discovered placement in `ws` (after the insert): the `x:`
/// `schema_type` row (shared by every workspace; the cap is per workspace through
/// `kind_stats`), the `kind_stats` upsert and, under `review`, the `node_candidate` row.
/// A declared placement only refreshes the row a promotion may have left under its name
/// (the module's counting semantics; a kind without a row is untouched).
pub(crate) fn record_discovered(
    conn: &Connection,
    ws: &str,
    rid: i64,
    placement: &Placement,
    now_iso: &str,
) -> Result<()> {
    let (
        Some(local),
        Placement::Discovered {
            label,
            policy,
            auto_accept,
        },
    ) = (placement.local(), placement)
    else {
        return touch_stats(conn, ws, placement.stored_kind());
    };
    conn.execute(
        "INSERT OR IGNORE INTO schema_type(ns, kind, name, description, builtin, created_at) VALUES (?1, 'label', ?2, '', 0, ?3)",
        params![DISCOVERED_NS, local, now_iso],
    )
    .sql()?;
    bump_stats(conn, ws, DISCOVERED_NS, local, label)?;
    if *policy == DiscoveredPolicyName::Review {
        // OWN-84 (2): the obvious case needs no queue item: the label's row (this node
        // counted) passes all three thresholds, so the node is placed as a released
        // candidate would be (governance `none`, no `node_candidate` row)
        if let Some(when) = auto_accept
            && stats_pass(conn, ws, label, when)?
        {
            return Ok(());
        }
        conn.execute(
            "INSERT OR REPLACE INTO node_candidate(rid, workspace_id, reason, group_key, since) VALUES (?1, ?2, ?3, ?4, unixepoch())",
            params![rid, ws, REASON_DISCOVERED, label],
        )
        .sql()?;
    }
    Ok(())
}

/// OWN-84 (2): whether the `kind_stats` row of the stored kind `stored` in `ws` passes `when`
/// on all three counts (`n_nodes`, `n_sources`, `mean_score`). No row, or a `NULL`
/// `mean_score` (the E1.8 bridge has not run: a label with no reconciled edges), never passes.
pub(crate) fn stats_pass(
    conn: &Connection,
    ws: &str,
    stored: &str,
    when: &kaban_core::ontology::meta::PromoteWhen,
) -> Result<bool> {
    let (where_sql, binds) = stats_where(stored);
    let sql = format!(
        "SELECT n_nodes, n_sources, mean_score FROM kind_stats WHERE workspace_id = ?1 AND {where_sql}"
    );
    let read = |r: &rusqlite::Row<'_>| Ok((r.get(0)?, r.get(1)?, r.get(2)?));
    // the `x:` and bare branches bind one name, the qualified branch two (`?3` is the namespace)
    let row: Option<(i64, i64, Option<f64>)> = match binds.as_slice() {
        [kind, ns] => conn.query_row(&sql, params![ws, kind, ns], read),
        [kind] => conn.query_row(&sql, params![ws, kind], read),
        _ => return Ok(false),
    }
    .optional()
    .sql()?;
    Ok(match row {
        Some((n_nodes, n_sources, Some(mean))) => {
            n_nodes >= i64::from(when.min_count)
                && n_sources >= i64::from(when.min_sources)
                && mean >= when.min_mean_score
        }
        _ => false,
    })
}

/// The `WHERE` fragment (after `workspace_id = ?1`) and the binds selecting the `kind_stats`
/// row of the stored kind `stored`: `x:<Label>` by the reserved namespace, `ns:Name` by its
/// namespace, a bare name by any bare namespace (one row at most: a bare user name never
/// shadows a shipped one, `ontology::extend`).
fn stats_where(stored: &str) -> (&'static str, Vec<String>) {
    if let Some(local) = stored.strip_prefix(DISCOVERED_PREFIX) {
        return ("ns = 'x' AND kind = ?2", vec![local.to_owned()]);
    }
    match split_stored(stored) {
        (Some(ns), bare) => (
            "ns = ?3 AND kind = ?2",
            vec![bare.to_owned(), ns.to_string()],
        ),
        (None, bare) => (
            "ns IN ('core', 'adr', 'user') AND kind = ?2",
            vec![bare.to_owned()],
        ),
    }
}

/// Refresh the `kind_stats` row of the stored kind `stored` in `ws` from the workspace's
/// active nodes (`n_nodes`, `n_sources`) and, when `seen`, mark the kind seen now. A kind
/// without a row is untouched (`UPDATE` matches nothing), so every write path may call it;
/// a store still at schema 0003 (fixtures, benches) has no `kind_stats` table and nothing
/// to maintain.
fn refresh_stats(conn: &Connection, ws: &str, stored: &str, seen: bool) -> Result<()> {
    if TypeTable::of(conn)? == TypeTable::Stored {
        return Ok(());
    }
    let (where_sql, binds) = stats_where(stored);
    // `?3` is the namespace bind of a qualified name; a bare or `x:` name binds a placeholder
    let ns_bind = binds.get(1).cloned().unwrap_or_default();
    conn.execute(
        &format!(
            "UPDATE kind_stats SET
               n_nodes = (SELECT count(*) FROM node n WHERE n.workspace_id = ?1 AND n.kind = ?4 AND n.lifecycle = 'active'),
               n_sources = (SELECT count(DISTINCT coalesce(n.session_id, n.agent_id, n.repo, '')) FROM node n WHERE n.workspace_id = ?1 AND n.kind = ?4 AND n.lifecycle = 'active'),
               last_seen = CASE WHEN ?5 THEN unixepoch() ELSE last_seen END
             WHERE workspace_id = ?1 AND {where_sql}"
        ),
        params![ws, binds[0], ns_bind, stored, seen],
    )
    .sql()?;
    Ok(())
}

/// After an insert or a content update of a node of `stored` in `ws`: counts and `last_seen`
/// follow (E1.9a RE3-7, the `apply_batch` update branch and the promoted declared kinds).
pub(crate) fn touch_stats(conn: &Connection, ws: &str, stored: &str) -> Result<()> {
    refresh_stats(conn, ws, stored, true)
}

/// After a retirement of a node of `stored` in `ws` (`forget`, a batch tombstone, an
/// `on_link` effect, `prune`): the counts follow, `last_seen` stays (E1.9a RE3-7).
pub(crate) fn recount_stats(conn: &Connection, ws: &str, stored: &str) -> Result<()> {
    refresh_stats(conn, ws, stored, false)
}

/// Upsert the `kind_stats` row of `(ws, ns, kind)` after one more node of stored name
/// `stored`: `n_nodes` counts the workspace's active rows of the kind, `n_sources` the
/// distinct writer identities among them (session, else agent, else repo).
fn bump_stats(conn: &Connection, ws: &str, ns: &str, kind: &str, stored: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO kind_stats(workspace_id, ns, kind, n_nodes, n_sources, mean_score, first_seen, last_seen)
         SELECT ?1, ?2, ?3, count(*), count(DISTINCT coalesce(session_id, agent_id, repo, '')), NULL, unixepoch(), unixepoch()
           FROM node WHERE workspace_id = ?1 AND kind = ?4 AND lifecycle = 'active'
         ON CONFLICT(workspace_id, ns, kind) DO UPDATE SET n_nodes = excluded.n_nodes, n_sources = excluded.n_sources, last_seen = excluded.last_seen",
        params![ws, ns, kind, stored],
    )
    .sql()?;
    Ok(())
}

/// The `kind_stats` rows of `ws`, ordered by `(ns, kind)`.
pub(crate) fn kind_stats(conn: &Connection, ws: &str) -> Result<Vec<KindStat>> {
    let mut st = conn
        .prepare_cached(
            "SELECT workspace_id, ns, kind, n_nodes, n_sources, mean_score, first_seen, last_seen
             FROM kind_stats WHERE workspace_id = ?1 ORDER BY ns, kind",
        )
        .sql()?;
    let rows = st
        .query_map([ws], |r| {
            Ok(KindStat {
                workspace_id: r.get(0)?,
                ns: r.get(1)?,
                kind: r.get(2)?,
                n_nodes: r.get::<_, i64>(3)?.try_into().unwrap_or(0),
                n_sources: r.get::<_, i64>(4)?.try_into().unwrap_or(0),
                mean_score: r.get(5)?,
                first_seen: r.get(6)?,
                last_seen: r.get(7)?,
            })
        })
        .sql()?
        .collect::<rusqlite::Result<Vec<_>>>()
        .sql()?;
    Ok(rows)
}

/// The exclusion predicate of EN-F49 for the node alias `alias`, or nothing when candidates
/// are included or the store is still at schema 0003 (no `node_candidate` table). The probe
/// is on `node_candidate`'s `INTEGER PRIMARY KEY`, so the planner uses the rowid lookup,
/// never a scan (asserted by the `EXPLAIN QUERY PLAN` test).
pub(crate) fn exclusion_sql(
    conn: &Connection,
    alias: &str,
    filters: ReadFilters,
) -> Result<String> {
    if filters.include_candidates || TypeTable::of(conn)? == TypeTable::Stored {
        Ok(String::new())
    } else {
        Ok(predicate(alias))
    }
}

/// The predicate text itself.
fn predicate(alias: &str) -> String {
    format!(" AND NOT EXISTS (SELECT 1 FROM node_candidate c WHERE c.rid = {alias}.rid)")
}

/// The bare label of a discovered stored name (`x:Vendor`), or the fixed class
/// `unknown_kind` with the path payload `promote.from` (never the source itself: it is
/// caller data, core S3; E1.9a RE3-8).
fn discovered_parts(from: &str) -> Result<&str> {
    from.strip_prefix(DISCOVERED_PREFIX)
        .filter(|bare| valid_bare_label(bare))
        .ok_or_else(|| {
            OntologyError::UnknownKind {
                path: "promote.from".to_owned(),
            }
            .into()
        })
}

/// Drop the `x:` `schema_type` row of `local` when no workspace counts it any more (the
/// row is shared; the cap is per workspace).
fn drop_unreferenced_type(conn: &Connection, local: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM schema_type WHERE ns = ?1 AND kind = 'label' AND name = ?2 AND builtin = 0
           AND NOT EXISTS (SELECT 1 FROM kind_stats k WHERE k.ns = ?1 AND k.kind = ?2)",
        params![DISCOVERED_NS, local],
    )
    .sql()?;
    Ok(())
}

/// `promote_kind` inside the caller's transaction (the actor check and the target's
/// compatibility with the loaded ontology are the caller's, RE3-4): rename every non-purged
/// node of the group with its dedup identity recomputed under `to` (RE3-3), delete the
/// group's `discovered_kind` rows, move the `kind_stats` row under `to` and recount it from
/// the nodes (RE3-7), drop the `x:` type row when unreferenced, append the event.
///
/// # Errors
/// `Ontology(unknown_kind)` when `to` is not declared or `from` is not a counted `x:` kind of
/// `ws`; `PromoteConflict` when a renamed row would take the identity of an existing active
/// row of `to` (nothing is renamed: the caller's transaction rolls back); backend errors.
pub(crate) fn promote(
    conn: &Connection,
    ws: &str,
    from: &str,
    to: &kaban_core::Qualified,
    now_iso: &str,
) -> Result<PromoteReport> {
    let to_stored = &kaban_core::stored_name(to);
    let (to_ns, to_name) = (to.ns.as_str(), to.name.as_str());
    let local = discovered_parts(from)?;
    if !ontology::kind_exists(conn, to_stored)? {
        return Err(OntologyError::UnknownKind {
            path: "promote.to".to_owned(),
        }
        .into());
    }
    let counted: Option<i64> = conn
        .query_row(
            "SELECT n_nodes FROM kind_stats WHERE workspace_id = ?1 AND ns = ?2 AND kind = ?3",
            params![ws, DISCOVERED_NS, local],
            |r| r.get(0),
        )
        .optional()
        .sql()?;
    if counted.is_none() {
        return Err(OntologyError::UnknownKind {
            path: "promote.from".to_owned(),
        }
        .into());
    }
    // every row's identity under the target kind first: a collision refuses the whole group
    // before any rename (the statement order inside the transaction is irrelevant to the
    // outcome, the early return is what keeps the common path short)
    let group: Vec<(i64, String)> = {
        let mut st = conn
            .prepare_cached(
                "SELECT rid, text FROM node WHERE workspace_id = ?1 AND kind = ?2 AND lifecycle <> 'purged' ORDER BY rid",
            )
            .sql()?;
        let rows = st
            .query_map(params![ws, from], |r| {
                Ok((r.get(0)?, crate::body_codec::read_column(r, 1)?))
            })
            .sql()?;
        rows.collect::<rusqlite::Result<_>>().sql()?
    };
    let mut collides = conn
        .prepare_cached(
            "SELECT 1 FROM node WHERE content_hash = ?1 AND lifecycle = 'active' AND rid <> ?2",
        )
        .sql()?;
    let mut rename = conn
        .prepare_cached(
            "UPDATE node SET kind = ?1, content_hash = ?2, updated_at = ?3, rev = rev + 1 WHERE rid = ?4",
        )
        .sql()?;
    for (rid, text) in &group {
        let hash = crate::sqlite::content_hash(ws, to_stored, text);
        let taken: Option<i64> = collides
            .query_row(params![hash, rid], |r| r.get(0))
            .optional()
            .sql()?;
        if taken.is_some() {
            return Err(StoreError::PromoteConflict);
        }
        rename
            .execute(params![to_stored, hash, now_iso, rid])
            .sql()?;
    }
    let released = conn
        .execute(
            "DELETE FROM node_candidate WHERE workspace_id = ?1 AND reason = ?2 AND group_key = ?3",
            params![ws, REASON_DISCOVERED, from],
        )
        .sql()?;
    // the row moves under the declared name (its `first_seen`/`last_seen` merged with an
    // existing row's) and is recounted from the nodes: the distinct union of the sources
    conn.execute(
        "INSERT INTO kind_stats(workspace_id, ns, kind, n_nodes, n_sources, mean_score, first_seen, last_seen)
         SELECT workspace_id, ?1, ?2, 0, 0, mean_score, first_seen, last_seen
           FROM kind_stats WHERE workspace_id = ?3 AND ns = ?4 AND kind = ?5
         ON CONFLICT(workspace_id, ns, kind) DO UPDATE SET
           first_seen = min(first_seen, excluded.first_seen), last_seen = max(last_seen, excluded.last_seen)",
        params![to_ns, to_name, ws, DISCOVERED_NS, local],
    )
    .sql()?;
    conn.execute(
        "DELETE FROM kind_stats WHERE workspace_id = ?1 AND ns = ?2 AND kind = ?3",
        params![ws, DISCOVERED_NS, local],
    )
    .sql()?;
    recount_stats(conn, ws, to_stored)?;
    drop_unreferenced_type(conn, local)?;
    Ok(PromoteReport {
        from: from.to_owned(),
        to: to_stored.to_owned(),
        nodes: u64::try_from(group.len()).unwrap_or(0),
        candidates_released: u64::try_from(released).unwrap_or(0),
        event_seq: 0,
    })
}

/// `prune_kinds` inside the caller's transaction: list the `x:` kinds of `ws` with `n_nodes
/// < below.min_count` and `last_seen` at least `older_than` ago; unless `dry_run`, tombstone
/// the nodes of each that hold a `discovered_kind` candidate row (RE3-5: a released or
/// `allow`-placed node of the kind survives), delete the candidate rows, recount the
/// `kind_stats` row and delete it only when no node of the kind is active any more, and drop
/// the type row when no workspace counts the kind.
pub(crate) fn prune(
    conn: &Connection,
    ws: &str,
    below: &PromoteWhen,
    older_than: Duration,
    dry_run: bool,
    now_iso: &str,
) -> Result<PruneReport> {
    let age = i64::try_from(older_than.as_secs()).unwrap_or(i64::MAX);
    let locals: Vec<String> = {
        let mut st = conn
            .prepare_cached(
                "SELECT kind FROM kind_stats WHERE workspace_id = ?1 AND ns = ?2 AND n_nodes < ?3
                   AND last_seen <= unixepoch() - ?4 ORDER BY kind",
            )
            .sql()?;
        let rows = st
            .query_map(
                params![ws, DISCOVERED_NS, i64::from(below.min_count), age],
                |r| r.get(0),
            )
            .sql()?;
        rows.collect::<rusqlite::Result<_>>().sql()?
    };
    let kinds: Vec<String> = locals
        .iter()
        .map(|l| format!("{DISCOVERED_NS}:{l}"))
        .collect();
    let mut tombstoned = 0u64;
    if !dry_run {
        for (local, stored) in locals.iter().zip(&kinds) {
            // the authorised set is the kind's candidate rows (EN-F5 "their candidate nodes")
            let rids=conn.prepare_cached("SELECT rid FROM node_candidate WHERE workspace_id=?1 AND reason=?2 AND group_key=?3").sql()?.query_map(params![ws,REASON_DISCOVERED,stored],|r|r.get::<_,i64>(0)).sql()?.collect::<rusqlite::Result<Vec<_>>>().sql()?;
            for rid in rids {
                let n=conn.execute("UPDATE node SET lifecycle='tombstoned',valid_to=?1,updated_at=?1,rev=rev+1 WHERE rid=?2 AND workspace_id=?3 AND kind=?4 AND lifecycle IN ('active','archived')",params![now_iso,rid,ws,stored]).sql()?;
                tombstoned += u64::try_from(n).unwrap_or(0);
                conn.execute("DELETE FROM node_candidate WHERE rid=?1 AND workspace_id=?2 AND reason=?3 AND group_key=?4",params![rid,ws,REASON_DISCOVERED,stored]).sql()?;
            }
            // the counters follow the survivors; a kind with none left loses its row (the
            // cap slot is released) and, when no workspace counts it, its type row
            recount_stats(conn, ws, stored)?;
            conn.execute(
                "DELETE FROM kind_stats WHERE workspace_id = ?1 AND ns = ?2 AND kind = ?3 AND n_nodes = 0",
                params![ws, DISCOVERED_NS, local],
            )
            .sql()?;
            drop_unreferenced_type(conn, local)?;
        }
    }
    let n_rejected: Option<i64> = conn
        .query_row(
            "SELECT n_rejected FROM discovered_overflow WHERE workspace_id = ?1",
            [ws],
            |r| r.get(0),
        )
        .optional()
        .sql()?;
    Ok(PruneReport {
        dry_run,
        kinds,
        nodes_tombstoned: tombstoned,
        n_rejected: n_rejected.and_then(|n| u64::try_from(n).ok()).unwrap_or(0),
    })
}
