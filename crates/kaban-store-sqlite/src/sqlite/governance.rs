//! Governance transitions and the `governing` lookup: the bodies of
//! `GovernanceStore::transition` (the strict core-spec §4 move with its event, the EN-F5a
//! candidate release and the EN-F12c human record claim) and `GovernanceStore::governing` (the
//! `why` lookup of core-spec F2 over a query or path target). Split out of `sqlite.rs` (OWN-79;
//! v4 CG5.3); the trait impl stays in the parent and forwards here.

use crate::audit_table;
use crate::candidates;
use crate::events;
use crate::reconcile::{self, persist};
use crate::rows::{NODE_COLS, row_node};
use crate::sql_error::SqlResultExt;
use kaban_core::Result;
use kaban_core::SignerKind;
use kaban_core::text_index::{MatchMode, Searched, SeedFilter, TextQuery};
use kaban_core::{
    Chain, ChainDirection, Governance, GoverningQuery, Lifecycle, NewEvent, Node, NodeId,
    StoreError, WorkspaceFilter, WriteCtx, now_iso,
};
use rusqlite::{OptionalExtension, params};
use std::collections::HashSet;

use super::{SqliteStore, WS_COL};

/// How many hits one page of the attached index's query-target search asks for.
const QUERY_TARGET_PAGE: usize = 256;

impl SqliteStore {
    /// The body of [`kaban_core::GovernanceStore::transition`].
    // the trait passes `ev` by value and the body is the one lifted byte for byte from the
    // trait impl (CG5.3 item 1), so the signature follows the trait
    #[allow(clippy::needless_pass_by_value)]
    pub(super) fn transition_impl(
        &self,
        id: &NodeId,
        to: Governance,
        ev: NewEvent,
        ctx: &WriteCtx,
    ) -> Result<(Node, u64)> {
        let id = id.as_str();
        let mut conn = self.write();
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .sql()?;
        let st = Self::state_of(&tx, id)?;
        audit_table::append(&tx, self.signer.as_ref(), &ctx.audit)?;
        let now = now_iso();
        // EN-F5a: human acceptance of a discovered-label candidate (an `x:` kind, never
        // governed) is the status change that releases its `node_candidate` row, in this
        // transaction; the governance column stays `none`
        let candidate_release = to == Governance::Accepted
            && st.governance == Governance::None
            && candidates::is_candidate(&tx, st.rid)?;
        // EN-F13 (E2.5b): a `proposed` record the reconciler materialised for an ungoverned
        // declared kind (`props.proposed_by = kaban-reconcile`) is promoted by a human the
        // same way, to `accepted`, with its event and the EN-F12c claim below; a record of a
        // kind the loaded ontology governs takes the §4 table instead
        let proposed_release = to == Governance::Accepted
            && st.governance == Governance::Proposed
            && !ctx.governed.contains(&st.kind)
            && reconcile_proposed(&tx, st.rid)?;
        let seq = if candidate_release || proposed_release {
            // OWN-84 (1) (E2.5g): the F4 tier of the kind (`GovernedSet::tier`: its own
            // `acceptance`, else the `discovered` block's, else `agent`); the store is the
            // boundary, so the refusal is enforced here as well as in the service
            if ctx.governed.tier(&st.kind) == kaban_core::ontology::meta::Acceptance::Human
                && ctx.actor != kaban_core::Actor::HumanCli
            {
                return Err(StoreError::HumanOnly("accept_candidate"));
            }
            if st.lifecycle != Lifecycle::Active {
                return Err(kaban_core::InvalidTransition::NotActive(st.lifecycle).into());
            }
            if proposed_release {
                tx.execute(
                    "UPDATE node SET governance='accepted', updated_at=?1, rev=rev+1 WHERE rid=?2",
                    params![now, st.rid],
                )
                .sql()?;
            } else {
                tx.execute(
                    "UPDATE node SET updated_at=?1, rev=rev+1 WHERE rid=?2",
                    params![now, st.rid],
                )
                .sql()?;
            }
            candidates::release_candidate(&tx, st.rid)?;
            events::append(&tx, &st.id, &st.workspace_id, to.as_str(), &ev)?
        } else {
            let seq = Self::govern(&tx, &ctx.governed, &st, to, &ev, ctx.repo.as_deref(), &now)?;
            // EN-F10 (E2.7): acceptance is the write that gives a record its `path_scope` (and
            // its `repo`), and supersession archives it; the anchor rows follow the row in
            // this transaction, as every other node write's do (E2.2), or the `node_anchor`
            // prefilter of `governing` never sees a record scoped at `kaban accept`; a store
            // held at an older schema (the paired bench's 0003 copy) has no `node_anchor`
            if reconcile::engine_tables(&tx)? {
                crate::anchors::refresh(&tx, st.rid, &ctx.governed)?;
            }
            seq
        };
        let seq = u64::try_from(seq).unwrap_or(0);
        let node = tx
            .query_row(
                &format!("SELECT {NODE_COLS} FROM node WHERE rid=?"),
                [st.rid],
                row_node,
            )
            .sql()?;
        // EN-F12c: the acceptance of a governed record is a `preferred` human claim about its
        // kind, written and decided in this transaction (a discovered-label release is not: its
        // `x:` kind is no kind the reconciler's ontology can validate a record claim against).
        // OWN-84 (1) (E2.5g, default (c)): a human's acceptance only, so protection stays
        // human-only and `kaban forget <id>` is the human's revert of an agent's acceptance.
        // The person is the event's signer kind, not the process actor (wave-3 review
        // residual): a confirmed handshake (`HumanViaChat`) runs in the agent's context and
        // is a human's acceptance all the same; `Agent` and `System` acceptances write none
        let human_signer = matches!(
            ev.signer_kind,
            SignerKind::HumanCli | SignerKind::HumanPr | SignerKind::HumanViaChat
        );
        if to == Governance::Accepted
            && !candidate_release
            && human_signer
            && reconcile::engine_tables(&tx)?
        {
            reconcile::human_record(
                &tx,
                &self.reconcile,
                &st.workspace_id,
                &reconcile::HumanRecord {
                    rid: st.rid,
                    kind: &node.kind,
                    props: &node.props,
                    actor: &ctx.actor,
                    event_id: seq,
                },
                persist::now_unix(),
            )?;
        }
        tx.commit().sql()?;
        Ok((node, seq))
    }
    /// The body of [`kaban_core::GovernanceStore::governing`].
    pub(super) fn governing_impl(&self, q: &GoverningQuery) -> Result<Vec<Node>> {
        if q.kinds.is_empty() || q.governance.is_empty() || q.limit == 0 {
            return Ok(Vec::new());
        }
        // the attached index's candidates first, with no connection held (spec 05 OWN-M,
        // constraint 1: the adapter may read the store back through its `Rows`)
        let indexed = match &q.query {
            Some(query) => self.indexed_candidates(query, q)?,
            None => None,
        };
        let conn = self.lock();
        let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        // EN-F49: a candidate record never governs a path unless asked
        let mut where_sql = format!(
            "n.lifecycle = 'active' AND n.kind IN ({}) AND n.governance IN ({}){}",
            vec!["?"; q.kinds.len()].join(","),
            vec!["?"; q.governance.len()].join(","),
            candidates::exclusion_sql(&conn, "n", q.filters)?
        );
        for k in &q.kinds {
            params.push(Box::new(k.clone()));
        }
        for g in &q.governance {
            params.push(Box::new(g.as_str()));
        }
        if let Some(ws) = q.workspace.as_option() {
            where_sql.push_str(" AND ");
            where_sql.push_str(WS_COL);
            where_sql.push_str(" = ?");
            params.push(Box::new(ws.to_owned()));
        }
        let from = if q.query.is_some() {
            // a query target: the text index decides, like a recall seed (AND terms)
            let Some(from) = Self::query_target(indexed, &mut params)? else {
                return Ok(Vec::new());
            };
            from
        } else {
            // a path target: an entry equal to the path or one of its ancestors, or a glob
            // entry that may match it (SQLite GLOB is a superset of the exact globset match
            // except for `{}` alternation and `[!]` classes, which pass through to the caller)
            if q.path_candidates.is_empty() {
                return Ok(Vec::new());
            }
            let target = q.path_candidates[0].clone();
            // EN-F10 / EN-F21 (E2.7): on a store past 0004 the `node_anchor` path rows
            // prefilter the candidates before the `json_each` scan below, which stays the
            // decider verbatim; the key set is `prefilter_keys(target)`. A record with no
            // `path` row at all (a kind the writer's governed set did not anchor, such as the
            // `Fact` and the `x:` candidate of `tests/discovered.rs`, or an unscoped record)
            // is never filtered: the prefilter only ever removes a record it has rows for;
            // `[why] anchor_prefilter = false` (E2.7b) runs the scan alone
            if q.anchor_prefilter && reconcile::engine_tables(&conn)? {
                where_sql.push_str(ANCHOR_PREFILTER);
                params.push(Box::new(serde_json::to_string(&prefilter_keys(&target))?));
                if let Some(ws) = q.workspace.as_option() {
                    where_sql.push_str(" AND a.workspace_id = ?");
                    params.push(Box::new(ws.to_owned()));
                }
                where_sql.push_str(ANCHOR_PREFILTER_FALLTHROUGH);
            }
            let _ = std::fmt::Write::write_fmt(
                &mut where_sql,
                format_args!(
                    " AND EXISTS (SELECT 1 FROM json_each(n.path_scope) ps WHERE ps.value IN ({}) \
                 OR (instr(ps.value, '*') > 0 OR instr(ps.value, '?') > 0 OR instr(ps.value, '[') > 0 \
                 OR instr(ps.value, '{{') > 0) AND (? GLOB ps.value OR instr(ps.value, '{{') > 0 \
                 OR instr(ps.value, '[') > 0))",
                    vec!["?"; q.path_candidates.len()].join(",")
                ),
            );
            for c in &q.path_candidates {
                params.push(Box::new(c.clone()));
            }
            params.push(Box::new(target));
            "node n WHERE ".to_owned()
        };
        let sql = format!(
            "SELECT {} FROM {from}{where_sql} ORDER BY CASE n.governance WHEN 'accepted' THEN 0 ELSE 1 END, COALESCE(n.importance, 0) DESC, n.created_at DESC, n.id LIMIT ? OFFSET ?",
            NODE_COLS
                .split(',')
                .map(|c| format!("n.{c}"))
                .collect::<Vec<_>>()
                .join(",")
        );
        params.push(Box::new(i64::try_from(q.limit).unwrap_or(i64::MAX)));
        params.push(Box::new(i64::try_from(q.offset).unwrap_or(i64::MAX)));
        let mut st = conn.prepare_cached(&sql).sql()?;
        let refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(AsRef::as_ref).collect();
        let rows = st.query_map(refs.as_slice(), row_node).sql()?;
        rows.collect::<std::result::Result<Vec<_>, _>>().sql()
    }
    /// The attached index's candidates for a query target (T6 review t6-3), read with no
    /// connection held: its lexical hits `(rid, revision)`, as they seed a recall, every page
    /// of one pinned generation, strict (no widening to OR); `None` when nothing is attached
    /// (the in-database index decides under the statement's lock).
    fn indexed_candidates(
        &self,
        query: &str,
        q: &GoverningQuery,
    ) -> Result<Option<Vec<(i64, i64)>>> {
        let Some(index) = self.seed_index() else {
            return Ok(None);
        };
        let view = index.pin()?;
        let filter = SeedFilter {
            kinds: &q.kinds,
            tags: &[],
            workspace_id: q.workspace.as_option(),
            code_ref: None,
            read: q.filters,
        };
        let mut rows: Vec<(i64, i64)> = Vec::new();
        loop {
            let page = view.search(&TextQuery {
                text: query,
                mode: MatchMode::Lexical,
                filter,
                limit: QUERY_TARGET_PAGE,
                sufficient: 0,
                skip: rows.len(),
            })?;
            let Searched::Hits { hits, exhausted } = page else {
                break;
            };
            if hits.is_empty() {
                break;
            }
            rows.extend(hits.iter().map(|hit| (hit.rid, hit.revision)));
            if exhausted {
                break;
            }
        }
        Ok(Some(rows))
    }
    /// The candidates of a query target (T6 review t6-3): the `FROM ... WHERE` opening of
    /// [`Self::governing_impl`]'s statement with its first bind pushed to the front of
    /// `params`, or `None` when the query can select nothing. With a text index attached, its
    /// hits ([`Self::indexed_candidates`], `indexed`) are the candidates, a hit whose row moved
    /// on since it was indexed dropped; the page, the order and the governance, workspace and
    /// candidate eligibility stay the statement's. A store with nothing attached has no index
    /// to decide with: refused, never an empty page.
    fn query_target(
        indexed: Option<Vec<(i64, i64)>>,
        params: &mut Vec<Box<dyn rusqlite::ToSql>>,
    ) -> Result<Option<String>> {
        if let Some(rows) = indexed {
            if rows.is_empty() {
                return Ok(None);
            }
            params.insert(0, Box::new(serde_json::to_string(&rows)?));
            return Ok(Some(
                "node n WHERE EXISTS (SELECT 1 FROM json_each(?) h \
                 WHERE json_extract(h.value, '$[0]') = n.rid AND json_extract(h.value, '$[1]') = n.rev) AND "
                    .to_owned(),
            ));
        }
        // a store with nothing attached has no index to decide with: refused, never an
        // empty page
        Err(StoreError::TextIndexUnavailable)
    }
    /// The body of [`kaban_core::GovernanceStore::supersession_chain`] (v4 CG5.3, default 1 of
    /// the brief): breadth-first over `SUPERSEDES` edges in `created_at, id` order with a
    /// visited set; `limit` bounds `ids.len()`.
    pub(super) fn supersession_chain_impl(
        &self,
        id: &NodeId,
        direction: ChainDirection,
        limit: usize,
        ws: &WorkspaceFilter,
    ) -> Result<Chain> {
        let conn = self.lock();
        // the seed, checked like a traversal start (S6; `traverse.rs`): unknown, purged,
        // tombstoned or outside the filter is `NotFound`; archived is allowed (a superseded
        // record is the usual seed of its own history)
        let seed_row: Option<(i64, String, String)> = conn
            .query_row(
                "SELECT rid, workspace_id, lifecycle FROM node WHERE id=?1 AND lifecycle <> 'purged'",
                [id.as_str()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()
            .sql()?;
        let Some((rid, seed_ws, seed_lifecycle)) = seed_row else {
            return Err(StoreError::NotFound(id.to_string()));
        };
        if !in_filter(ws, &seed_ws) || seed_lifecycle == Lifecycle::Tombstoned.as_str() {
            return Err(StoreError::NotFound(id.to_string()));
        }
        // `A SUPERSEDES B` is `edge(src = A, dst = B)`: predecessors follow `src = current`
        // to `dst`, successors the reverse; the far endpoint's workspace and lifecycle come
        // back with the row and are judged here, so a cut can be counted (RS-9)
        let sql = match direction {
            ChainDirection::Predecessors => {
                "SELECT n.rid, n.id, n.workspace_id, n.lifecycle FROM edge e JOIN node n ON n.rid = e.dst \
                 WHERE e.src = ?1 AND e.rel = 'SUPERSEDES' ORDER BY e.created_at, e.id"
            }
            ChainDirection::Successors => {
                "SELECT n.rid, n.id, n.workspace_id, n.lifecycle FROM edge e JOIN node n ON n.rid = e.src \
                 WHERE e.dst = ?1 AND e.rel = 'SUPERSEDES' ORDER BY e.created_at, e.id"
            }
        };
        let mut st = conn.prepare_cached(sql).sql()?;
        let mut seen: HashSet<i64> = HashSet::from([rid]);
        let mut frontier = vec![rid];
        let mut chain = Chain::default();
        'walk: while !frontier.is_empty() {
            let mut next = Vec::new();
            for current in &frontier {
                let hops = st
                    .query_map([current], |r| {
                        Ok((
                            r.get::<_, i64>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, String>(2)?,
                            r.get::<_, String>(3)?,
                        ))
                    })
                    .sql()?
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .sql()?;
                for (hop_rid, hop_id, hop_ws, hop_lifecycle) in hops {
                    if !seen.insert(hop_rid) {
                        continue;
                    }
                    if chain.ids.len() >= limit {
                        chain.cut = true;
                        break 'walk;
                    }
                    // RS-9, S6: a hop outside the filter ends its branch, counted, never
                    // returned or expanded (the far endpoint's workspace against the filter,
                    // as `traverse.rs` does before a node joins the frontier)
                    if !in_filter(ws, &hop_ws) {
                        chain.cut_by_workspace += 1;
                        chain.cut = true;
                        continue;
                    }
                    // a tombstoned hop ends its branch (F5: tombstoned rows never resurface)
                    if hop_lifecycle == Lifecycle::Tombstoned.as_str() {
                        chain.cut = true;
                        continue;
                    }
                    chain.ids.push(NodeId::parse(&hop_id)?);
                    next.push(hop_rid);
                }
            }
            frontier = next;
        }
        Ok(chain)
    }
}

/// The EN-F10 path prefilter of `governing_impl` (engine-spec EN-F21; E2.7 default 4): the
/// record's `path` anchor key must be one of the target's byte prefixes ([`prefilter_keys`]),
/// bound as one JSON array, or the record has no `path` row at all. Opened for the optional
/// workspace clause and closed by [`ANCHOR_PREFILTER_FALLTHROUGH`]; planned through `node_anchor_key(anchor_kind, key, workspace_id)` (0004:111).
/// `tests/north_star.rs` `governing_prefilter_uses_index` mirrors this text after `n.rid IN`;
/// a change here changes both.
///
/// Why prefixes and not the spec's `key IN (<path and ancestors>) OR ? GLOB key`: the stored
/// keys are literal prefixes (`anchors::literal_prefix`), `src/api/` for
/// `src/api/**` and `""` for `**/*.rs`; an ancestor candidate carries no trailing `/` and
/// `? GLOB <literal>` is equality, so that form loses both rows (the staged red of
/// `prefilter_keeps_prefix_universal_and_exact_rows`). A glob's literal prefix is a prefix of
/// every path it matches, so the prefix set is complete; it never over-matches past what the
/// `json_each` GLOB check that follows decides.
const ANCHOR_PREFILTER: &str = " AND (n.rid IN (SELECT a.rid FROM node_anchor a \
     WHERE a.anchor_kind = 'path' AND a.key IN (SELECT value FROM json_each(?))";

/// The close of [`ANCHOR_PREFILTER`]: a record without any `path` anchor row falls through to
/// the `json_each` decider (its kind was not anchored by the writer, or it has no scope).
const ANCHOR_PREFILTER_FALLTHROUGH: &str = ") OR NOT EXISTS (SELECT 1 FROM node_anchor a2 \
     WHERE a2.rid = n.rid AND a2.anchor_kind = 'path'))";

/// Every prefix of `target` on a character boundary, the empty universal key first and the
/// target itself last (a stored key is valid UTF-8, so no other byte prefix can equal one).
fn prefilter_keys(target: &str) -> Vec<&str> {
    target
        .char_indices()
        .map(|(i, _)| &target[..i])
        .chain(std::iter::once(target))
        .collect()
}

/// Whether node `rid` is a record the reconciler proposed (EN-F13: `props.proposed_by` is
/// `kaban-reconcile`), the one `proposed` record a human promotes without the EN-F4 handshake.
fn reconcile_proposed(conn: &rusqlite::Connection, rid: i64) -> Result<bool> {
    let by: Option<String> = conn
        .query_row(
            "SELECT json_extract(props, '$.proposed_by') FROM node WHERE rid = ?1",
            [rid],
            |r| r.get(0),
        )
        .optional()
        .sql()?
        .flatten();
    Ok(by.as_deref() == Some(crate::reconcile::materialise::PROPOSED_BY))
}

/// Whether a node in workspace `node_ws` is inside the read filter (`WorkspaceFilter::All` is
/// every workspace; the Rust twin of the `(?1 IS NULL OR n.workspace_id = ?1)` clause of
/// `traverse.rs`).
fn in_filter(ws: &WorkspaceFilter, node_ws: &str) -> bool {
    ws.as_option().is_none_or(|w| w == node_ws)
}
