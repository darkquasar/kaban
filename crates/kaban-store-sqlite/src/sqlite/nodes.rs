//! Node identity and persistence: the `remember` write workflow (`remember_with_impl`,
//! `attach_links`, `attach_refs`), placement and insertion (`place`, `insert_node`,
//! `maintain_anchors`, `active_by_hash`), the batch upsert (`upsert_node`, `StoredNode`) and
//! the three content-hash encodings (`content_hash`, `batch_content_hash`,
//! `redacted_content_hash`) plus `ignored_fields`. Split out of `sqlite.rs` (OWN-79).

use crate::anchors;
use crate::audit_table;
use crate::candidates::{self, Placement};
use crate::changelog;
use crate::reconcile;
use crate::rows::{NODE_COLS, row_node};
use crate::sql_error::SqlResultExt;
use kaban_core::Result;
use kaban_core::{
    BatchReport, Governance, InvalidTransition, Lifecycle, LinkReq, Method, NewNode, Node, NodeId,
    OntologyError, OutcomeKind, RedactedStrings, RememberOutcome, StoreError, WorkspaceId,
    WriteCtx, new_id, now_iso,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::json;

use super::{SqliteStore, Tenant};

/// Dedup identity of a note (F1): sha256 over `workspace \u{1} kind \u{1} trimmed text`. The
/// migrator recomputes it for rows that predate bound workspaces (MD-8).
pub(crate) fn content_hash(workspace_id: &str, kind: &str, text: &str) -> String {
    let canonical = format!("{workspace_id}\u{1}{kind}\u{1}{}", text.trim());
    kaban_policy::sha256_hex(canonical.as_bytes())
}

impl SqliteStore {
    /// The body of [`kaban_core::MemoryStore::remember_with`].
    #[allow(clippy::needless_pass_by_value)] // the trait hands the node over by value; the body keeps that shape
    pub(super) fn remember_with_impl(
        &self,
        n: NewNode,
        links: &[LinkReq],
        refs: &[NewNode],
        ctx: &WriteCtx,
    ) -> Result<RememberOutcome> {
        let policy_version = ctx.policy_version.as_str();
        let mut conn = self.write();
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .sql()?;
        audit_table::append(&tx, self.signer.as_ref(), &ctx.audit)?;
        let workspace_id = n
            .workspace_id
            .as_deref()
            .unwrap_or(WorkspaceId::DEFAULT)
            .to_owned();
        let tenant = Tenant::of(ctx, &workspace_id);
        // EN-F5: the one-record batch; a refused label is the error, a new kind beyond the
        // cap is `discovered_cap_reached` with its `n_rejected` increment (and the audit rows)
        // committed and nothing else written
        let Some(placement) = Self::place(&tx, &tenant, &n.kind)? else {
            tx.commit().sql()?;
            return Err(StoreError::Ontology(OntologyError::DiscoveredCapReached));
        };
        let hash = content_hash(&workspace_id, placement.stored_kind(), &n.text);
        // dedup is on active rows only (OD-16): a tombstone is never returned as `existing`
        if let Some(existing) = Self::active_by_hash(&tx, &hash)? {
            // m2 N5: a retry still attaches its links (and refs); metadata differences are
            // reported, not applied
            let links_created =
                Self::attach_links(&tx, &self.reconcile, &tenant, &existing.id, links)?;
            Self::attach_refs(
                &tx,
                &self.reconcile,
                &tenant,
                &existing.id,
                refs,
                policy_version,
            )?;
            let fields_ignored = ignored_fields(&n, &existing);
            changelog::record_terminal(&tx, ctx)?;
            tx.commit().sql()?;
            return Ok(RememberOutcome::Existing {
                node: existing,
                links_created,
                fields_ignored,
            });
        }
        let id = Self::insert_node(&tx, &tenant, &placement, &n, &hash, policy_version)?;
        // a new node has no edges yet, so every distinct link is an insert; the API reports
        // the distinct count (review R1-8)
        Self::attach_links(&tx, &self.reconcile, &tenant, &id, links)?;
        Self::attach_refs(&tx, &self.reconcile, &tenant, &id, refs, policy_version)?;
        let node = tx
            .query_row(
                &format!("SELECT {NODE_COLS} FROM node WHERE id=?"),
                [&id],
                row_node,
            )
            .sql()?;
        changelog::record_terminal(&tx, ctx)?;
        tx.commit().sql()?;
        Ok(RememberOutcome::Created(node))
    }
    /// Create the inline links of a `remember` around node `id`; returns how many edges were
    /// actually inserted (a link that already existed, or a duplicate inside `links`, is
    /// refreshed and not counted; review R1-8).
    fn attach_links(
        conn: &Connection,
        setup: &reconcile::Setup,
        tenant: &Tenant<'_>,
        id: &str,
        links: &[LinkReq],
    ) -> Result<usize> {
        let mut created = 0;
        for l in links {
            let (from, to) = match (&l.from, &l.to) {
                (None, Some(t)) => (id, t.as_str()),
                (Some(f), None) => (f.as_str(), id),
                _ => return Err(StoreError::BadLink),
            };
            let (_, inserted) = Self::link_in(
                conn,
                setup,
                tenant,
                from,
                to,
                &l.rel,
                l.confidence,
                None,
                Method::Remember,
            )?;
            created += usize::from(inserted);
        }
        Ok(created)
    }
    /// The active node with this content hash, if any (dedup lookup of `remember`, M3-2).
    fn active_by_hash(conn: &Connection, hash: &str) -> Result<Option<Node>> {
        conn.query_row(
            &format!("SELECT {NODE_COLS} FROM node WHERE content_hash=? AND lifecycle='active'"),
            [hash],
            row_node,
        )
        .optional()
        .sql()
    }
    /// Insert a fresh node row from a gated [`NewNode`] in the tenant's workspace under its
    /// [`Placement`]; returns its id. Governed kinds (the tenant's set, EN-F4) and
    /// `allow`-placed labels start `proposed`, every other kind has no governance (core-spec
    /// §4, EN-F5); a discovered placement also writes its `x:` type, stats and candidate rows.
    fn insert_node(
        conn: &Connection,
        tenant: &Tenant<'_>,
        placement: &Placement,
        n: &NewNode,
        hash: &str,
        policy_version: &str,
    ) -> Result<String> {
        let id = new_id();
        let now = now_iso();
        let tags = serde_json::to_string(&n.tags)?;
        let props = serde_json::to_string(n.props.as_ref().unwrap_or(&json!({})))?;
        let path_scope = serde_json::to_string(&n.path_scope)?;
        let governance = placement.initial_governance(tenant.governed);
        let bodies = crate::body_codec::Writer::for_connection(conn)?;
        conn.execute(
            "INSERT INTO node(id,kind,text,summary,tags,confidence,importance,lifecycle,governance,store_scope,path_scope,dup_key,repo,created_at,updated_at,valid_from,session_id,agent_id,workspace_id,props,content_hash,policy_version)
             VALUES (?,?,?,?,?,?,?,'active',?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
            params![id, placement.stored_kind(), bodies.stored(&n.text), n.summary, tags, n.confidence, n.importance, governance.as_str(), n.store_scope.as_str(), path_scope, n.dup_key, n.repo, now, now, n.valid_from, n.session_id, n.agent_id, tenant.ws, props, hash, policy_version],
        ).sql()?;
        let rid = conn.last_insert_rowid();
        candidates::record_discovered(conn, tenant.ws, rid, placement, &now)?;
        Self::maintain_anchors(conn, tenant, rid, placement.stored_kind(), n)?;
        Ok(id)
    }
    /// EN-F10: the anchor rows of a node just written from `n` (a store below 0004 has no
    /// `node_anchor` table and nothing to maintain).
    fn maintain_anchors(
        conn: &Connection,
        tenant: &Tenant<'_>,
        rid: i64,
        stored_kind: &str,
        n: &NewNode,
    ) -> Result<()> {
        if !reconcile::engine_tables(conn)? {
            return Ok(());
        }
        let empty = json!({});
        anchors::maintain(
            conn,
            &anchors::NodeFacts {
                rid,
                workspace_id: tenant.ws,
                kind: stored_kind,
                repo: n.repo.as_deref(),
                props: n.props.as_ref().unwrap_or(&empty),
                path_scope: &n.path_scope,
            },
            tenant.governed,
        )?;
        Ok(())
    }
    /// Place `kind` for the tenant (EN-F5) and admit it under the workspace cap: the
    /// placement, or `None` when the cap refused a new discovered kind (`n_rejected` moved;
    /// the caller decides between a skipped record and `discovered_cap_reached`).
    fn place(conn: &Connection, tenant: &Tenant<'_>, kind: &str) -> Result<Option<Placement>> {
        let placement = candidates::place_kind(conn, tenant.governed, tenant.discovered, kind)?;
        match candidates::admit_discovered(conn, tenant.ws, tenant.discovered, &placement)? {
            candidates::Admission::Admitted => Ok(Some(placement)),
            candidates::Admission::CapReached => Ok(None),
        }
    }
    /// Create the `CodeRef` nodes of a `remember` (deduplicated against active nodes by content
    /// hash, in `workspace_id`) and link the note `ABOUT` each of them (M3-2).
    fn attach_refs(
        conn: &Connection,
        setup: &reconcile::Setup,
        tenant: &Tenant<'_>,
        id: &str,
        refs: &[NewNode],
        policy_version: &str,
    ) -> Result<()> {
        for r in refs {
            let placement = Self::place(conn, tenant, &r.kind)?
                .ok_or(StoreError::Ontology(OntologyError::DiscoveredCapReached))?;
            let hash = content_hash(tenant.ws, placement.stored_kind(), &r.text);
            let ref_id = match Self::active_by_hash(conn, &hash)? {
                Some(existing) => existing.id,
                None => Self::insert_node(conn, tenant, &placement, r, &hash, policy_version)?,
            };
            Self::link_in(
                conn,
                setup,
                tenant,
                id,
                &ref_id,
                "ABOUT",
                None,
                None,
                Method::Remember,
            )?;
        }
        Ok(())
    }
    /// `Op::UpsertNode`: insert a node under its explicit id, or update the stored row's
    /// content in place (lifecycle untouched, WD-1). Returns the row id.
    pub(super) fn upsert_node(
        conn: &Connection,
        tenant: &Tenant<'_>,
        id: &NodeId,
        n: &NewNode,
        policy_version: &str,
        report: &mut BatchReport,
    ) -> Result<Option<i64>> {
        let ws = tenant.ws;
        // EN-F5: an undeclared label is placed under the tenant's discovered rules; a refused
        // label or a new kind beyond the cap skips the record (the batch goes on, the
        // `n_rejected` increment commits with it)
        let placement = match Self::place(conn, tenant, &n.kind) {
            Ok(Some(placement)) => placement,
            Ok(None) => {
                report.record(id, &n.kind, OutcomeKind::CapReached);
                return Ok(None);
            }
            Err(StoreError::KindNotAllowed) => {
                report.record(id, &n.kind, OutcomeKind::KindNotAllowed);
                return Ok(None);
            }
            Err(e) => return Err(e),
        };
        let stored_kind = placement.stored_kind();
        let now = now_iso();
        let tags = serde_json::to_string(&n.tags)?;
        let props = serde_json::to_string(n.props.as_ref().unwrap_or(&json!({})))?;
        let path_scope = serde_json::to_string(&n.path_scope)?;
        let hash = batch_content_hash(ws, id.as_str(), n, &props);
        let stored: Option<StoredNode> = conn
            .query_row(
                "SELECT rid, content_hash, kind, lifecycle, workspace_id, dup_key, repo, store_scope, governance FROM node WHERE id=?",
                [id.as_str()],
                |r| {
                    Ok(StoredNode {
                        rid: r.get(0)?,
                        hash: r.get(1)?,
                        kind: r.get(2)?,
                        lifecycle: r.get(3)?,
                        workspace_id: r.get(4)?,
                        dup_key: r.get(5)?,
                        repo: r.get(6)?,
                        store_scope: r.get(7)?,
                        governance: r.get(8)?,
                    })
                },
            )
            .optional()
            .sql()?;
        if let Some(row) = &stored {
            // a purged id never reappears (F5, WD-1) and another tenant's row is invisible
            // (F17, WD-5): both are `NotFound`, no existence leak
            if row.lifecycle == Lifecycle::Purged.as_str() || row.workspace_id != ws {
                return Err(StoreError::NotFound(id.to_string()));
            }
            if row.kind != stored_kind {
                return Err(StoreError::KindMismatch {
                    id: id.to_string(),
                    stored: row.kind.clone(),
                    requested: stored_kind.to_owned(),
                });
            }
        }
        // the EN-E3 metadata (`dup_key`, `repo`, `store_scope`) is persisted by the UPDATE
        // but outside the content identity (the hash is pinned by the DI1/E1 goldens), so it
        // is compared on its own (E2 interim review RE4-14): a metadata-only change is an
        // update, and a repo change re-derives the anchors
        let same_meta = stored.as_ref().is_some_and(|row| {
            row.dup_key == n.dup_key
                && row.repo == n.repo
                && row.store_scope == n.store_scope.as_str()
        });
        let same_hash = stored.as_ref().is_some_and(|row| row.hash == hash);
        let same_repo = stored.as_ref().is_some_and(|row| row.repo == n.repo);
        let rid = match stored {
            // same content identity and metadata: the row is not touched, so no `changes`
            // row either
            Some(StoredNode { rid, .. }) if same_hash && same_meta => {
                report.record(id, stored_kind, OutcomeKind::Unchanged);
                rid
            }
            None => {
                let governance = placement.initial_governance(tenant.governed);
                let bodies = crate::body_codec::Writer::for_connection(conn)?;
                // EN-E3: `store_scope`, `path_scope`, `dup_key` and `repo` are persisted on
                // insert and update (before E2.2 the batch path dropped them)
                conn.execute(
                    "INSERT INTO node(id,kind,text,summary,tags,confidence,importance,lifecycle,governance,store_scope,path_scope,dup_key,repo,created_at,updated_at,valid_from,session_id,agent_id,workspace_id,props,content_hash,policy_version)
                     VALUES (?,?,?,?,?,?,?,'active',?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
                    params![id.as_str(), stored_kind, bodies.stored(&n.text), n.summary, tags, n.confidence, n.importance, governance.as_str(), n.store_scope.as_str(), path_scope, n.dup_key, n.repo, now, now, n.valid_from, n.session_id, n.agent_id, ws, props, hash, policy_version],
                ).sql()?;
                let rid = conn.last_insert_rowid();
                candidates::record_discovered(conn, ws, rid, &placement, &now)?;
                report.record(id, stored_kind, OutcomeKind::Inserted);
                rid
            }
            Some(StoredNode {
                rid, governance, ..
            }) => {
                // EN-F35: unchanged retries above remain no-ops. Any persisted terminal
                // governance protects content even if the current ontology drops the mixin.
                let governance = Governance::parse(&governance)
                    .ok_or_else(|| crate::rows::bad_enum(8, &governance))
                    .sql()?;
                if governance != Governance::None && governance != Governance::Proposed {
                    return Err(InvalidTransition::Governance {
                        from: governance,
                        to: Governance::Proposed,
                    }
                    .into());
                }
                let bodies = crate::body_codec::Writer::for_connection(conn)?;
                conn.execute(
                    "UPDATE node SET text=?1, summary=?2, tags=?3, confidence=?4, importance=?5, valid_from=?6,
                         session_id=?7, agent_id=?8, props=?9, content_hash=?10, policy_version=?11,
                         updated_at=?12, store_scope=?14, path_scope=?15, dup_key=?16, repo=?17, rev=rev+1
                     WHERE rid=?13",
                    params![bodies.stored(&n.text), n.summary, tags, n.confidence, n.importance, n.valid_from, n.session_id, n.agent_id, props, hash, policy_version, now, rid, n.store_scope.as_str(), path_scope, n.dup_key, n.repo],
                ).sql()?;
                // the counters follow a content or source change (EN-F6; E1.9a RE3-7)
                super::artifacts::invalidate_stale(conn, rid)?;
                candidates::touch_stats(conn, ws, stored_kind)?;
                report.record(id, stored_kind, OutcomeKind::Updated);
                rid
            }
        };
        if !(same_hash && same_repo) {
            Self::maintain_anchors(conn, tenant, rid, stored_kind, n)?;
        }
        Ok(Some(rid))
    }
}

/// The stored row an `Op::UpsertNode` is compared against: the content identity and the
/// EN-E3 metadata the identity leaves out (RE4-14).
struct StoredNode {
    rid: i64,
    hash: String,
    kind: String,
    lifecycle: String,
    workspace_id: String,
    dup_key: Option<String>,
    repo: Option<String>,
    store_scope: String,
    governance: String,
}

/// Content identity of a node written by `apply_batch` (WD-7, D-16): sha256 over a
/// length-prefixed, domain-tagged (`kaban-batch/v1`) encoding of workspace, kind, id, trimmed
/// text, summary, sorted tags and the canonical props JSON (`serde_json` writes object keys
/// sorted). Every persisted content field takes part, so a props-only change is an update; the
/// id is mixed in so two batch nodes with the same text never collide on `node_hash_active`;
/// the domain tag keeps batch hashes disjoint from `remember`'s `(workspace, kind, text)`
/// hashes.
fn batch_content_hash(workspace_id: &str, id: &str, n: &NewNode, props_json: &str) -> String {
    let mut tags: Vec<&str> = n.tags.iter().map(String::as_str).collect();
    tags.sort_unstable();
    let mut buf = Vec::new();
    let mut put = |field: &str| {
        buf.extend_from_slice(&(field.len() as u64).to_le_bytes());
        buf.extend_from_slice(field.as_bytes());
    };
    for field in [
        "kaban-batch/v1",
        workspace_id,
        &n.kind,
        id,
        n.text.trim(),
        n.summary.as_deref().unwrap_or(""),
        if n.summary.is_some() { "1" } else { "0" },
    ] {
        put(field);
    }
    put(&tags.len().to_string());
    for t in tags {
        put(t);
    }
    put(props_json);
    // EN-E3 (E2.2): the scope globs take part, appended so a node without a scope keeps the
    // hash it had before (the DI1/E1 goldens); a scope-only change is an update
    for g in &n.path_scope {
        put(g);
    }
    kaban_policy::sha256_hex(&buf)
}

/// Content identity of a node after an in-place redact (R2-2): sha256 over a length-prefixed,
/// domain-tagged (`kaban-redacted/v1`) encoding of the node id and every masked string
/// (`Option` fields carry a presence byte so `None` and `""` differ; lists carry their length).
/// The id makes it unique per node (no `node_hash_active` collision between two notes masked
/// alike) and no raw secret takes part (no guess oracle); the tag keeps it disjoint from the
/// `remember` and batch formulas, so a later `remember` of the masked text is a new node.
pub(super) fn redacted_content_hash(id: &str, s: &RedactedStrings) -> String {
    let mut buf = Vec::new();
    let mut put = |field: &str| {
        buf.extend_from_slice(&(field.len() as u64).to_le_bytes());
        buf.extend_from_slice(field.as_bytes());
    };
    let mut put_opt = |field: Option<&str>| {
        put(if field.is_some() { "1" } else { "0" });
        put(field.unwrap_or(""));
    };
    put_opt(Some("kaban-redacted/v1"));
    put_opt(Some(id));
    put_opt(Some(&s.text));
    put_opt(s.summary.as_deref());
    put_opt(Some(&s.tags.len().to_string()));
    for t in &s.tags {
        put_opt(Some(t));
    }
    put_opt(Some(&s.props.to_string()));
    put_opt(Some(&s.path_scope.len().to_string()));
    for g in &s.path_scope {
        put_opt(Some(g));
    }
    for f in [
        &s.session_id,
        &s.agent_id,
        &s.repo,
        &s.commit_sha,
        &s.accepted_commit,
        &s.dup_key,
    ] {
        put_opt(f.as_deref());
    }
    kaban_policy::sha256_hex(&buf)
}

/// Request fields of a deduplicated `remember` whose values differ from the stored node (the
/// stored values win; the caller is told what was not applied).
fn ignored_fields(req: &NewNode, stored: &Node) -> Vec<String> {
    let mut out = Vec::new();
    if req.summary.is_some() && req.summary != stored.summary {
        out.push("summary".into());
    }
    if !req.tags.is_empty() && req.tags != stored.tags {
        out.push("tags".into());
    }
    if req.confidence.is_some() && req.confidence != stored.confidence {
        out.push("confidence".into());
    }
    if req.importance.is_some() && req.importance != stored.importance {
        out.push("importance".into());
    }
    if req.session_id.is_some() && req.session_id != stored.session_id {
        out.push("session_id".into());
    }
    if req.agent_id.is_some() && req.agent_id != stored.agent_id {
        out.push("agent_id".into());
    }
    if let Some(props) = &req.props
        && *props != json!({})
        && *props != stored.props
    {
        out.push("props".into());
    }
    out
}
