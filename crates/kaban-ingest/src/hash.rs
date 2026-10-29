//! Identities and the skip decision (docs-ingest-plan.md DI0.2; docs-ingest-spec D5, D6;
//! core-spec S1; PR-2): the path gate that runs before anything is hashed, the stable
//! [`SourceId`], keyed identities for material that failed gating and the
//! `(identity, pipeline, extractor, config)` skip key.
//!
//! Preimage encoding (D-4, frozen in `docs/ids.md`): the ASCII domain tag
//! `kaban-ingest/v1/<purpose>` followed, for every field, by its length as a `u64` little-endian
//! and its bytes. Length prefixes make `("a", "bc.md")` and `("ab", "c.md")` distinct preimages.

use crate::graph::ingest_batch;
use crate::ids::{blocked_document_id, document_id};
use crate::model::{DocStatus, IngestError, PreparedGraph, SourceId};
use crate::walk::{Skip, WalkConfig};
use kaban_core::{AuditEntry, AuditOp, MemoryStore, NewNode, NodeId, Op, WorkspaceId, WriteBatch};
use kaban_policy::{AuditSigner, Policy};
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;

/// Domain-tagged, length-prefixed preimage (D-4).
pub(crate) fn preimage(purpose: &str, fields: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::with_capacity(32 + fields.iter().map(|f| f.len() + 8).sum::<usize>());
    out.extend_from_slice(b"kaban-ingest/v1/");
    out.extend_from_slice(purpose.as_bytes());
    for f in fields {
        out.extend_from_slice(&(f.len() as u64).to_le_bytes());
        out.extend_from_slice(f);
    }
    out
}

/// Keyed identity for material that failed gating: `HMAC(audit_key, kind || data)` as lowercase
/// hex, so a blocked file can be recognised on a re-run without persisting an unkeyed hash of a
/// secret-bearing name or body (core-spec S1: a stub cannot confirm a guess). `kind` is `"path"`
/// or `"bytes"`.
pub fn keyed_id(signer: &dyn AuditSigner, kind: &'static str, data: &[u8]) -> String {
    signer.hmac_hex(&preimage("keyed", &[kind.as_bytes(), data]))
}

/// The keyed id of an ungated relative path inside a workspace: `keyed_id("path", workspace ||
/// raw_path)` with the D-4 field encoding. The walker reports every skip under this id and
/// `PathRejected` documents are stored under `blocked_document_id` of it; the path itself is
/// never hashed unkeyed, logged or stored.
pub fn keyed_path_id(signer: &dyn AuditSigner, workspace: &WorkspaceId, raw_path: &str) -> String {
    keyed_id(
        signer,
        "path",
        &preimage(
            "path",
            &[workspace.as_str().as_bytes(), raw_path.as_bytes()],
        ),
    )
}

/// A relative path that passed the denylist gate unchanged. The only input [`SourceId::new`]
/// accepts (`type-newtype-validated`): a path cannot be masked and stay an identity, so any
/// hit, redact or reject, means `PathRejected` and no `GatedPath` exists.
///
/// Only [`gate_path`] constructs one (review RD-6): the field is private, so a caller cannot
/// hand-build a `GatedPath` around an unscanned path and feed it to `SourceId::new`.
///
/// ```compile_fail
/// // an ungated path cannot be smuggled into an identity (RD-6)
/// let forged = kaban_ingest::GatedPath { relative_path: "notes-AKIA.md".to_owned() };
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GatedPath {
    /// Identical to the raw relative path (no hit occurred).
    relative_path: String,
}

impl GatedPath {
    /// The gated relative path (`/`-separated, workspace-relative).
    pub fn relative_path(&self) -> &str {
        &self.relative_path
    }
}

/// Step 1 of every document: scan the relative path BEFORE it is hashed, logged, stored or used
/// for identity (core-spec S1; PR-2). Any hit means `Skip::PathRejected` with the rule ids.
///
/// # Errors
/// `Skip::PathRejected { rule_ids }` on any denylist hit (or an over-long path).
pub fn gate_path(policy: &Policy, raw: &str) -> Result<GatedPath, Skip> {
    let verdict = policy.scan(raw);
    if verdict.rejected || !verdict.hits.is_empty() {
        let mut rule_ids: Vec<String> = verdict.hits.into_iter().map(|h| h.rule).collect();
        rule_ids.extend(verdict.reject_rules);
        rule_ids.sort();
        rule_ids.dedup();
        // rule ids only: the path is the very thing being judged (`obs-no-sensitive-data`)
        tracing::debug!(rules = ?rule_ids, "path rejected by the denylist");
        return Err(Skip::PathRejected { rule_ids });
    }
    Ok(GatedPath {
        relative_path: raw.to_owned(),
    })
}

impl SourceId {
    /// The stable identity of `(workspace, relative_path)`: `blake3` hex over the D-4 preimage
    /// `kaban-ingest/v1/source`. Only constructible from a gated path.
    pub fn new(workspace: &WorkspaceId, path: &GatedPath) -> Self {
        let digest = blake3::hash(&preimage(
            "source",
            &[workspace.as_str().as_bytes(), path.relative_path.as_bytes()],
        ));
        Self::from_hex(digest.to_hex().to_string())
    }
}

/// `blake3` hex of the canonical sanitized representation of a document that passed gating
/// (RD1-1; core-spec S1): the only input is a [`PreparedGraph`], which exists only after the
/// gate returned `Proceed` and holds nothing but gated strings, so no digest of a raw byte or
/// of a masked secret is ever persisted (a stored hash cannot confirm a guess about the
/// original). Preimage (D-4 encoding, domain `kaban-ingest/v1/content`, frozen in
/// `docs/ids.md`): the title (or empty), the frontmatter as compact JSON (keys in map order),
/// then per section its level, kind, heading path length and headings, chunk count, and per
/// chunk its kind as JSON, text and `stub_hmac` (or empty); then (RD2-4) the link count and
/// per link, in document order, its raw target, anchor kind as JSON, alias (or empty) and
/// embed flag, then the tag count and every tag. A stubbed secret contributes its keyed hash
/// only, so an edit inside it still changes the document's identity. Positions are not here:
/// they are the [`position_fingerprint`].
pub fn content_hash(graph: &PreparedGraph) -> String {
    let props = &graph.document.props;
    let mut owned: Vec<Vec<u8>> = Vec::new();
    owned.push(props.title.clone().unwrap_or_default().into_bytes());
    owned.push(props.frontmatter.to_string().into_bytes());
    for section in &graph.sections {
        let sp = &section.props;
        owned.push(vec![sp.level]);
        owned.push(serde_json::to_vec(&sp.kind).unwrap_or_default());
        owned.push((sp.heading_path.len() as u64).to_le_bytes().to_vec());
        for h in &sp.heading_path {
            owned.push(h.clone().into_bytes());
        }
        owned.push((section.chunks.len() as u64).to_le_bytes().to_vec());
        for chunk in &section.chunks {
            let cp = &chunk.props;
            owned.push(serde_json::to_vec(&cp.kind).unwrap_or_default());
            owned.push(cp.text.clone().into_bytes());
            owned.push(cp.stub_hmac.clone().unwrap_or_default().into_bytes());
        }
    }
    owned.push((graph.links.len() as u64).to_le_bytes().to_vec());
    for link in &graph.links {
        owned.push(link.raw_target.clone().into_bytes());
        owned.push(serde_json::to_vec(&link.anchor_kind).unwrap_or_default());
        owned.push(link.alias.clone().unwrap_or_default().into_bytes());
        owned.push(vec![u8::from(link.embed)]);
    }
    owned.push((graph.tags.len() as u64).to_le_bytes().to_vec());
    for tag in &graph.tags {
        owned.push(tag.tag.clone().into_bytes());
    }
    let fields: Vec<&[u8]> = owned.iter().map(Vec::as_slice).collect();
    blake3::hash(&preimage("content", &fields))
        .to_hex()
        .to_string()
}

/// Keyed positional fingerprint of a prepared graph (RD2-4; `docs/ids.md`): `HMAC(audit_key,
/// preimage("positions", ..))` over, in order, every section's `byte_start`/`byte_end`, every
/// chunk's `byte_start`/`byte_end`, every link's and every tag's offset. Offsets derive from
/// the raw layout, so the value is keyed as RD1-1 requires of anything raw-derived; it is not
/// an identity (ids never see it), only the freshness decision does, beside `content_hash`.
pub fn position_fingerprint(signer: &dyn AuditSigner, graph: &PreparedGraph) -> String {
    let mut owned: Vec<[u8; 8]> = Vec::new();
    for section in &graph.sections {
        owned.push(section.props.byte_start.to_le_bytes());
        owned.push(section.props.byte_end.to_le_bytes());
        for chunk in &section.chunks {
            owned.push(chunk.props.byte_start.to_le_bytes());
            owned.push(chunk.props.byte_end.to_le_bytes());
        }
    }
    for link in &graph.links {
        owned.push(link.byte_offset.to_le_bytes());
    }
    for tag in &graph.tags {
        owned.push(tag.byte_offset.to_le_bytes());
    }
    let fields: Vec<&[u8]> = owned.iter().map(<[u8; 8]>::as_slice).collect();
    signer.hmac_hex(&preimage("positions", &fields))
}

/// Which stored identity a skip check compares against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Identity {
    /// The Document's `content_hash` prop (a document that passed gating).
    ContentHash(String),
    /// The Document's `blocked_hmac` prop (material that failed gating; a keyed id).
    Keyed(String),
}

/// What must be unchanged for a document to be skipped (docs-ingest-spec D5): the identity,
/// `pipeline_version`, `extractor_version` and `config_hash`, compared with the props of the
/// same names on the stored Document (`content_hash` or `blocked_hmac` for the identity).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkipKey {
    /// Content hash or keyed id.
    pub identity: Identity,
    /// [`crate::PIPELINE_VERSION`] at write time.
    pub pipeline_version: u32,
    /// The extractor's `crate@version` (empty for blocked documents).
    pub extractor_version: String,
    /// [`config_hash`] of the walk and policy configuration (D-12).
    pub config_hash: String,
    /// [`position_fingerprint`] of the prepared graph (RD2-4); `None` for blocked documents.
    /// Compared only when set: a stored Document without the prop (written before DI1.8b) is
    /// then fresh once and gains it.
    pub position_fingerprint: Option<String>,
}

/// Configuration digest that invalidates skips (D-12): `blake3` over canonical JSON of the
/// file-size cap, `include_code`, the deny globs and the policy's `effective_hash`, so a policy
/// change re-gates every document (core-spec S4). The chunk configuration joins in DI1.
pub fn config_hash(cfg: &WalkConfig, policy: &Policy) -> String {
    // field order is alphabetical, so the serialization is canonical whatever serde_json's
    // map implementation preserves
    #[derive(Serialize)]
    struct Canonical<'a> {
        deny_globs: &'a [String],
        include_code: bool,
        max_file_bytes: u64,
        policy_hash: &'a str,
    }
    let json = serde_json::to_vec(&Canonical {
        deny_globs: &cfg.deny_globs,
        include_code: cfg.include_code,
        max_file_bytes: cfg.max_file_bytes,
        policy_hash: policy.effective_hash(),
    })
    // Invariant: a struct of scalars and strings always serializes.
    .unwrap_or_default();
    blake3::hash(&preimage("config", &[&json]))
        .to_hex()
        .to_string()
}

/// Whether the Document stored for `source` carries exactly `key` (D5 skip): false when no
/// node exists or any component differs.
///
/// # Errors
/// Store errors from the lookup.
pub fn should_skip(
    store: &dyn MemoryStore,
    source: &SourceId,
    key: &SkipKey,
) -> Result<bool, IngestError> {
    skip_by_node(store, &document_id(source), key)
}

/// [`should_skip`] for a file whose path was rejected: no `SourceId` exists, the node is
/// `blocked_document_id(keyed_path_id)`.
///
/// # Errors
/// Store errors from the lookup.
pub fn should_skip_blocked(
    store: &dyn MemoryStore,
    keyed_path_id: &str,
    key: &SkipKey,
) -> Result<bool, IngestError> {
    skip_by_node(store, &blocked_document_id(keyed_path_id), key)
}

fn skip_by_node(store: &dyn MemoryStore, id: &NodeId, key: &SkipKey) -> Result<bool, IngestError> {
    // a tombstoned or archived Document is absent for the skip decision (DI1.8, RD2-5): the
    // batch then carries the reactivations that bring its ids back
    let Some(node) = store.get(id, true)? else {
        return Ok(false);
    };
    if node.lifecycle != kaban_core::Lifecycle::Active {
        return Ok(false);
    }
    let p = &node.props;
    let identity_matches = match &key.identity {
        Identity::ContentHash(h) => p.get("content_hash").and_then(Value::as_str) == Some(h),
        Identity::Keyed(k) => p.get("blocked_hmac").and_then(Value::as_str) == Some(k),
    };
    let positions_match = key
        .position_fingerprint
        .as_deref()
        .is_none_or(|f| p.get("position_fingerprint").and_then(Value::as_str) == Some(f));
    Ok(identity_matches
        && positions_match
        && p.get("pipeline_version").and_then(Value::as_u64)
            == Some(u64::from(key.pipeline_version))
        && p.get("extractor_version").and_then(Value::as_str)
            == Some(key.extractor_version.as_str())
        && p.get("config_hash").and_then(Value::as_str) == Some(key.config_hash.as_str()))
}

/// The `Document{status: blocked}` write for a path the denylist rejected: one node op under
/// `blocked_document_id` whose props carry the keyed id, the skip key and the rule counts but
/// no path, plus the audit row (`reject`, tool `ingest`, field `path`, `input_hmac` = the keyed
/// id, rule ids) to append in the same transaction (core-spec S3; D-17).
#[derive(Debug, Clone)]
pub struct BlockedDocument {
    /// `blocked_document_id(keyed_path_id)`.
    pub id: NodeId,
    /// The single-op batch for `apply_batch`.
    pub batch: WriteBatch,
    /// The audit row for `WriteCtx::audit`.
    pub audit: AuditEntry,
}

/// Build the blocked Document write for a `PathRejected` candidate (see [`BlockedDocument`]).
pub fn blocked_path_document(
    workspace: &WorkspaceId,
    keyed_path_id: &str,
    rule_ids: &[String],
    key: &SkipKey,
    policy_version: &str,
) -> BlockedDocument {
    let id = blocked_document_id(keyed_path_id);
    let mut counts: BTreeMap<&str, u32> = BTreeMap::new();
    for r in rule_ids {
        *counts.entry(r.as_str()).or_default() += 1;
    }
    let props = serde_json::json!({
        "status": DocStatus::Blocked,
        "blocked_hmac": keyed_path_id,
        "pipeline_version": key.pipeline_version,
        "extractor_version": key.extractor_version,
        "config_hash": key.config_hash,
        "rule_ids": counts,
        "owner": kaban_core::INGEST_OWNER,
    });
    let node = NewNode {
        kind: "Document".into(),
        text: String::new(),
        props: Some(props),
        ..NewNode::default()
    };
    let batch = ingest_batch(
        workspace,
        vec![Op::UpsertNode {
            id: id.clone(),
            node,
        }],
    );
    let audit = AuditEntry {
        op: AuditOp::Reject,
        tool: "ingest".into(),
        field: "path".into(),
        workspace_id: workspace.clone(),
        policy_version: policy_version.to_owned(),
        input_hmac: keyed_path_id.to_owned(),
        rules: rule_ids.to_vec(),
        node_id: Some(id.as_str().to_owned()),
    };
    BlockedDocument { id, batch, audit }
}
