//! `KabanApi::remember` and its helpers: the gated memory write with typed props, inline links
//! and `CodeRef` refs.

use super::{KabanApi, MAX_PATH_SCOPE, MAX_PROPS_BYTES, check_unit};
use crate::coderef::{self, coderef_text, dup_key};
use crate::control::ControlPath;
use crate::error::ApiError;
use crate::ontology::validate_kind_props;
use crate::types::{CodeRefWire, RememberRequest, RememberResponse, RememberStatus};
use kaban_codegraph::{CodeGraphReader, HeadInput, Resolved};
use kaban_core::ontology::meta::{Dedup, Stamp};
use kaban_core::ontology::resolve::normalise_stored;
use kaban_core::{
    CodeRef, LinkReq, NewNode, NodeId, RememberOutcome, StoreScope, WorkspaceId, WriteCtx,
};
use kaban_core::{StoreError, UnstampedReason};
use std::sync::Arc;

const MAX_TAGS: usize = 32;
const MAX_LINKS: usize = 32;
const MAX_REFS: usize = 32;

/// The `id` of every `props.evidence[]` entry (gated props; non-object entries are skipped).
fn evidence_ids(props: Option<&serde_json::Value>) -> Vec<String> {
    props
        .and_then(|p| p.get("evidence"))
        .and_then(serde_json::Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|e| e.get("id").and_then(serde_json::Value::as_str))
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

impl KabanApi {
    /// Store a memory node (with inline links and `CodeRef` refs) after the denylist gate
    /// (core-spec F1): `props` are validated against the kind's `props_schema` (engine-spec
    /// EN-F3; the shipped Decision, Convention and Failure schemas are the typed structs'); a
    /// kind with `dedup: title+scope` (the shipped Decision) reports active near-duplicates of
    /// its kind (same normalized title and `path_scope`) and is still created.
    ///
    /// # Errors
    /// `Invalid` for bound violations, typed-props violations (`props.<field>`), a ref's
    /// `symbol` grammar (`refs[].symbol: symbol_invalid`), an unresolvable derivation
    /// (`refs[].name_path: ambiguous_symbol`), a `snapshot_commit` that canonicalises to no one
    /// commit (`refs[].snapshot_commit`), an `observed_at` off the RFC 3339 shape
    /// (`refs[].observed_at`) and `scope: personal` (M6), `Rejected` for denylist hits, store
    /// errors otherwise; nothing is stored on error.
    #[allow(clippy::needless_pass_by_value)] // core-plan §6 interface: requests are taken by value
    pub fn remember(&self, p: RememberRequest) -> Result<RememberResponse, ApiError> {
        let mut response = self.remember_unsettled(p)?;
        response.receipt = self.await_searchable(response.receipt);
        Ok(response)
    }

    /// [`Self::remember`] before its write barrier (SS-10): the body of the public call, for a
    /// public call that embeds a `remember` (a hook) and runs the one barrier itself.
    #[allow(clippy::needless_pass_by_value)] // the public call's body: the request is taken by value
    pub(crate) fn remember_unsettled(
        &self,
        p: RememberRequest,
    ) -> Result<RememberResponse, ApiError> {
        if p.tags.len() > MAX_TAGS || p.links.len() > MAX_LINKS {
            return Err(ApiError::Invalid("too many tags or links".into()));
        }
        if p.refs.len() > MAX_REFS || p.path_scope.len() > MAX_PATH_SCOPE {
            return Err(ApiError::Invalid(
                "too many refs or path_scope entries".into(),
            ));
        }
        if p.scope == Some(StoreScope::Personal) {
            return Err(ApiError::Invalid("scope personal lands in M6".into()));
        }
        if let Some(props) = &p.props
            && props.to_string().len() > MAX_PROPS_BYTES
        {
            return Err(ApiError::Invalid("props too large".into()));
        }
        check_unit("confidence", p.confidence)?;
        check_unit("importance", p.importance)?;
        if p.workspace_id.as_deref() == Some(WorkspaceId::ALL) {
            return Err(ApiError::Invalid(
                "workspace_id \"*\" selects every workspace on reads and cannot be written".into(),
            ));
        }
        let mut red = Vec::new();
        let t = "remember";
        // The workspace is gated first and the write context is built from the sanitized value,
        // so a redact-class workspace never reaches an audit row or the export raw (review R1-1).
        let mut ctx = self.write_ctx(None);
        let workspace_id = match &p.workspace_id {
            Some(s) => self.gate(&mut ctx, t, "workspace_id", s, &mut red)?,
            None => self.config.workspace.as_str().to_owned(),
        };
        ctx.workspace_id = WorkspaceId::new(workspace_id.as_str());
        for e in &mut ctx.audit {
            e.workspace_id = ctx.workspace_id.clone();
        }
        // joined view first, so a phrase split across fields is still caught
        let joined = [
            Some(p.kind.as_str()),
            Some(p.text.as_str()),
            p.summary.as_deref(),
            p.workspace_id.as_deref(),
            p.session_id.as_deref(),
            p.agent_id.as_deref(),
        ]
        .into_iter()
        .flatten()
        .chain(p.tags.iter().map(String::as_str))
        .chain(p.path_scope.iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join(" ");
        self.gate_joined(&mut ctx, t, "all", &joined)?;
        // the gated kind in its canonical stored spelling (E1.9a RE3-2: `adr:Decision` is
        // `Decision`, `user:Widget` is `Widget`), the one name every registry lookup below and
        // the store's placement judge; a foreign qualified name is itself
        let kind = normalise_stored(&self.gate(&mut ctx, t, "kind", &p.kind, &mut red)?);
        let text = self.gate(&mut ctx, t, "text", &p.text, &mut red)?;
        let summary = match &p.summary {
            Some(s) => Some(self.gate(&mut ctx, t, "summary", s, &mut red)?),
            None => None,
        };
        let mut tags = Vec::new();
        for s in &p.tags {
            tags.push(self.gate(&mut ctx, t, "tag", s, &mut red)?);
        }
        // stored as its canonical set (R3-6), the denominator of its F14 currency
        let path_scope = crate::currency::canonical_path_scope(&self.gate_all(
            &mut ctx,
            t,
            "path_scope",
            &p.path_scope,
            &mut red,
        )?);
        let session_id = match &p.session_id {
            Some(s) => Some(self.gate(&mut ctx, t, "session_id", s, &mut red)?),
            None => None,
        };
        let agent_id = match &p.agent_id {
            Some(s) => Some(self.gate(&mut ctx, t, "agent_id", s, &mut red)?),
            None => None,
        };
        let props = match &p.props {
            Some(v) => Some(self.gate_json(&mut ctx, t, "props", v, &mut red)?),
            None => None,
        };
        // kind props against the kind's `props_schema` in the registry (engine-spec EN-F3; keys
        // were gated above, so the error names a gated key at most)
        validate_kind_props(
            &self.registry,
            &kind,
            props
                .as_ref()
                .unwrap_or(&serde_json::Value::Object(serde_json::Map::new())),
        )?;
        // refs: every wire field is gated (`info` included: scanned, then dropped), the symbol
        // grammar, the commit canonicalisation and the derivation run on the gated values
        // (G1-F9, G1-S4, SO-2), then the stored form is a CodeRef node whose text is its
        // canonical fields (M3-2). The head and the snapshot are fetched once per call and only
        // when a ref needs them.
        let (head, resolved) = if p.refs.iter().any(needs_snapshot) {
            self.snapshot_for_refs()
        } else {
            (None, None)
        };
        let mut refs: Vec<NewNode> = Vec::new();
        for w in &p.refs {
            let gated = self.gate_coderef(&mut ctx, t, w, &mut red)?;
            let gated = self.verify_coderef(
                &mut ctx,
                t,
                &mut red,
                gated,
                head.as_deref(),
                resolved.as_ref(),
            )?;
            let stored = CodeRef::try_from(gated)?;
            refs.push(NewNode {
                kind: "CodeRef".into(),
                text: coderef_text(&stored),
                summary: Some(match &stored.name_path {
                    Some(n) => format!("{}:{n}", stored.relative_path),
                    None => stored.relative_path.clone(),
                }),
                repo: Some(stored.repo.clone()),
                props: serde_json::to_value(&stored).ok(),
                ..Default::default()
            });
        }
        // the same (rel, from, to) named twice in one request is one edge (review R1-8); the
        // ids are control (validated by shape, scanned only when malformed)
        let mut links: Vec<LinkReq> = Vec::new();
        for l in &p.links {
            check_unit("link.confidence", l.confidence)?;
            let req = LinkReq {
                rel: self.gate(&mut ctx, t, "link.rel", &l.rel, &mut red)?,
                from: match &l.from {
                    Some(s) => Some(Self::node_id(&self.gate_control(
                        &mut ctx,
                        t,
                        ControlPath::request(t, "links[].from"),
                        "link.from",
                        s,
                        &mut red,
                    )?)?),
                    None => None,
                },
                to: match &l.to {
                    Some(s) => Some(Self::node_id(&self.gate_control(
                        &mut ctx,
                        t,
                        ControlPath::request(t, "links[].to"),
                        "link.to",
                        s,
                        &mut red,
                    )?)?),
                    None => None,
                },
                confidence: l.confidence,
            };
            if !links
                .iter()
                .any(|x| x.rel == req.rel && x.from == req.from && x.to == req.to)
            {
                links.push(req);
            }
        }
        // `props.evidence[]` entries that name an existing CodeRef of this workspace become
        // EVIDENCE_FOR edges (CodeRef -> note); other ids, including another workspace's
        // CodeRef (R3-3: no cross-workspace edge, no existence leak), are ignored (M3-2)
        for ev in evidence_ids(props.as_ref()) {
            if let Ok(id) = NodeId::parse(&ev)
                && let Some(n) = self.store.get(&id, false)?
                && n.kind == "CodeRef"
                && n.workspace_id == workspace_id
                && !links
                    .iter()
                    .any(|x| x.rel == "EVIDENCE_FOR" && x.from == Some(id.clone()))
            {
                links.push(LinkReq {
                    rel: "EVIDENCE_FOR".into(),
                    from: Some(id),
                    to: None,
                    confidence: None,
                });
            }
        }
        // the near-duplicate key of a kind whose mixin says `dedup: title+scope` (EN-F4; the
        // shipped Decision)
        let dedups = self
            .registry
            .with_kind(&kind, |k| {
                k.governed
                    .as_ref()
                    .is_some_and(|g| g.dedup == Dedup::TitleScope)
            })
            .unwrap_or(false);
        let n = NewNode {
            dup_key: dedups.then(|| dup_key(summary.as_deref(), &text, &path_scope)),
            kind,
            text,
            summary,
            tags,
            confidence: p.confidence,
            importance: p.importance,
            valid_from: None,
            session_id,
            agent_id,
            workspace_id: Some(workspace_id),
            props,
            path_scope,
            repo: self.config.repo.clone(),
            store_scope: StoreScope::Project,
        };
        let outcome = self.store.remember_with(n, &links, &refs, &ctx)?;
        self.export_audit(&ctx.audit);
        match outcome {
            RememberOutcome::Created(node) => {
                // OWN-65 (engine-spec §3.1 "Sources that are not repositories"): a record of a
                // `stamp: commit` kind written where no stamp can come records why; never an
                // error: the record committed, so a failure here is logged and the receipt
                // stands at the record's own position (SS-10); when the row is written, the
                // call's terminal position is its transaction's
                let mut seq = ctx.terminal_seq();
                if let Some(reason) = self.unstamped_reason(&node.kind) {
                    match NodeId::parse(&node.id)
                        .map_err(|e| ApiError::from(StoreError::from(e)))
                        .and_then(|id| {
                            Ok(self
                                .store
                                .record_unstamped(&id, &node.workspace_id, reason)?)
                        }) {
                        Ok(terminal) => seq = Some(terminal),
                        Err(e) => {
                            tracing::warn!(error = %e, "the unstamped event failed after the record committed");
                        }
                    }
                }
                // candidates are records of the same kind (the key is title and scope only)
                let candidate_duplicates = match &node.dup_key {
                    Some(k) => self
                        .store
                        .find_dup_key(&node.workspace_id, k)?
                        .iter()
                        .filter(|d| d.id != node.id && d.kind == node.kind)
                        .filter_map(|d| self.project(d, false, None))
                        .collect(),
                    None => Vec::new(),
                };
                let receipt = self.committed_receipt(seq);
                Ok(RememberResponse {
                    status: RememberStatus::Created,
                    id: node.id,
                    kind: node.kind,
                    redactions: red,
                    links: Some(links.len()),
                    links_created: links.len(),
                    links_ignored: false,
                    fields_ignored: Vec::new(),
                    candidate_duplicates,
                    receipt,
                })
            }
            RememberOutcome::Existing {
                node,
                links_created,
                fields_ignored,
            } => Ok(RememberResponse {
                status: RememberStatus::Existing,
                id: node.id,
                kind: node.kind,
                redactions: red,
                links: None,
                links_created,
                links_ignored: false,
                fields_ignored,
                candidate_duplicates: Vec::new(),
                // a no-op write holds at the head like any other (SS-10)
                receipt: self.committed_receipt(ctx.terminal_seq()),
            }),
        }
    }

    /// OWN-65: why a record of `kind` written now gets no commit sha: the process runs outside
    /// any repository (`repo_root` unset). Repository history is not queried on this write
    /// path: doctor reports its current availability on demand. An unknown kind keeps the
    /// spec default `commit`.
    fn unstamped_reason(&self, kind: &str) -> Option<UnstampedReason> {
        if self.registry.index_policy(kind).unwrap_or(Stamp::Commit) != Stamp::Commit {
            return None;
        }
        match self.config.repo_root.as_deref() {
            None => Some(UnstampedReason::NoRepository),
            Some(_) => None,
        }
    }

    /// Gate every string field of a wire ref (field names `refs.<name>`); `info` is scanned
    /// here and dropped by the conversion to the stored form. `symbol` and `observed_at` are
    /// content (G1-F9: scanned before any grammar sees them); `snapshot_commit` is the one
    /// control field of a ref (`refs[].snapshot_commit`, SO-2): a value its shape accepts is
    /// validated, any other value is content and scanned.
    fn gate_coderef(
        &self,
        ctx: &mut WriteCtx,
        t: &str,
        w: &CodeRefWire,
        red: &mut Vec<String>,
    ) -> Result<CodeRefWire, ApiError> {
        Ok(CodeRefWire {
            repo: self.gate(ctx, t, "refs.repo", &w.repo, red)?,
            commit: self.gate(ctx, t, "refs.commit", &w.commit, red)?,
            tool: self.gate(ctx, t, "refs.tool", &w.tool, red)?,
            relative_path: self.gate(ctx, t, "refs.relative_path", &w.relative_path, red)?,
            name_path: self.gate_opt(ctx, t, "refs.name_path", w.name_path.as_deref(), red)?,
            kind: self.gate_opt(ctx, t, "refs.kind", w.kind.as_deref(), red)?,
            body_location: w.body_location,
            location: w.location,
            reference_line: w.reference_line,
            symbol: self.gate_opt(ctx, t, "refs.symbol", w.symbol.as_deref(), red)?,
            snapshot_commit: match &w.snapshot_commit {
                Some(s) => Some(self.gate_control(
                    ctx,
                    t,
                    ControlPath::request("remember", "refs[].snapshot_commit"),
                    "refs.snapshot_commit",
                    s,
                    red,
                )?),
                None => None,
            },
            observed_at: self.gate_opt(
                ctx,
                t,
                "refs.observed_at",
                w.observed_at.as_deref(),
                red,
            )?,
            info: self.gate_opt(ctx, t, "refs.info", w.info.as_deref(), red)?,
        })
    }

    /// The typed steps after the gate, on one gated ref (G1-F9, SO-2, §4.4): the `symbol`
    /// grammar (stored normalised), the 40-hex canonicalisation of `snapshot_commit`, the
    /// derivation of `symbol` from `relative_path` and `name_path` through the resolved snapshot
    /// (only for this process's repository, G1-S7; an explicit `symbol` is never looked up;
    /// `snapshot_commit` is set to the resolved commit when derived and absent), and the
    /// `observed_at` default. Without a snapshot the derivation is skipped and nothing fails.
    ///
    /// The derived key comes from a snapshot admitted under the policy of its index time, so it
    /// is gated under the live policy exactly as an explicit `refs.symbol` is (G1-S1, RU3-1): a
    /// hit is the gate's own error, and a redaction that would change a structural key is a
    /// refusal, never a rewrite. The derived key also passes the key-form check an explicit
    /// `symbol` passes (`check_derived_symbol`, RU3-1): a versioned or document-local key is
    /// refused `symbol_invalid` without a rewrite (E2.5d).
    fn verify_coderef(
        &self,
        ctx: &mut WriteCtx,
        t: &str,
        red: &mut Vec<String>,
        mut w: CodeRefWire,
        head: Option<&HeadInput>,
        resolved: Option<&Resolved>,
    ) -> Result<CodeRefWire, ApiError> {
        if let Some(s) = &w.symbol {
            w.symbol = Some(coderef::check_symbol(s)?);
        }
        if let Some(c) = &w.snapshot_commit {
            w.snapshot_commit = Some(coderef::canonical_commit(c, resolved, head)?);
        }
        if w.symbol.is_none()
            && let Some(name_path) = &w.name_path
            && let Some(r) = resolved
            && w.repo == self.config.repo.as_deref().unwrap_or("")
        {
            let path = w.relative_path.replace('\\', "/");
            match r.snapshot.nodes_in_file(&path) {
                Ok(rows) => {
                    let key = coderef::derive_symbol(&*r.snapshot, &rows, name_path)?;
                    if self.gate(ctx, t, "refs.symbol", &key, red)? != key {
                        return Err(coderef::symbol_invalid());
                    }
                    // the key-form rule of an explicit `symbol` (RU3-6), on the derived key too (RW1-3)
                    coderef::check_derived_symbol(&key)?;
                    w.symbol = Some(key);
                    if w.snapshot_commit.is_none() {
                        w.snapshot_commit = Some(r.r#ref.commit.clone());
                    }
                }
                // a snapshot that cannot be read verifies nothing: the ref is stored as given
                Err(e) => tracing::debug!(error = ?e, "symbol derivation skipped"),
            }
        }
        match &w.observed_at {
            Some(s) => coderef::check_observed_at(s)?,
            None => w.observed_at = coderef::now_rfc3339(),
        }
        Ok(w)
    }

    /// The head of `repo_root` and the snapshot the provider resolves for it (cg-join-v1 §4,
    /// SD-5: `ApiConfig.repo` is the join identity, `""` when unset), fetched once per
    /// `remember` through the code side's one head-fetch path (`HeadSource::head_input`, the
    /// shared `HeadCache`; RU3-5). `None` for either when there is no provider, no repository,
    /// no readable head or no snapshot; the ref steps then skip what needs them.
    fn snapshot_for_refs(&self) -> (Option<Arc<HeadInput>>, Option<Resolved>) {
        let Some(provider) = self.snapshot_provider.as_ref() else {
            return (None, None);
        };
        let Some(root) = self.config.repo_root.as_deref() else {
            return (None, None);
        };
        let head = match self.head.head_input(&self.git, root) {
            Ok(head) => head,
            Err(e) => {
                tracing::debug!(
                    class = e.class(),
                    "head not fetched; refs stored unverified"
                );
                return (None, None);
            }
        };
        let resolved = match provider.resolve(self.config.repo.as_deref().unwrap_or(""), &head) {
            Ok(r) => Some(r),
            Err(reason) => {
                tracing::debug!(?reason, "no snapshot for the refs of this remember");
                None
            }
        };
        (Some(head), resolved)
    }
}

/// Whether a ref needs the head and the snapshot: a `name_path` without `symbol` (derivation)
/// or a `snapshot_commit` shorter than 40 hex (canonicalisation).
fn needs_snapshot(w: &CodeRefWire) -> bool {
    (w.name_path.is_some() && w.symbol.is_none())
        || w.snapshot_commit.as_deref().is_some_and(|c| c.len() < 40)
}
