//! Graph assembly and the per-document write batch (docs-ingest-plan.md DI1.5; docs-ingest-spec
//! D2, D3, D5): ids and reading order for the nodes of one document, wikilink resolution to
//! Document ids, `TAGGED` edges, and the diff against the stored document that tombstones
//! what disappeared.
//!
//! ```text
//!   prepare_document ==> assemble (ids, NEXT order)  ==> PreparedGraph
//!                                                          |
//!   store.get(document_id) ==> ExistingDocument ----------+--> build_batch ==> WriteBatch
//!   store.get(target ids)  ==> resolve_links -------------+       (one apply_batch, N4)
//! ```
//!
//! The batch names every node and edge of the document on every run: the store writes no row
//! and no outbox row for a node whose content is unchanged (WD-7) and its MD-23 edge diff drops
//! the outgoing edges the batch no longer names, so `NEXT` is re-linked around a deleted chunk
//! without any store-side knowledge of documents. Ids the stored document had and the new graph
//! lacks are tombstoned (overwrite mode; `history: true` is DI3.4).
//!
//! Engine boundary (engine-plan §10, EN-F25): identifier matching (`find_mentions`, the
//! gazetteer, `MENTIONS`) is not here. The store-side owned edge diff (spec D5 [A-3],
//! `EdgeDiff::Owned`) is core EN-E4 and has not landed; every edge written here already carries
//! `props.owner = "kaban-ingest"`; DI1.8 landed `EdgeDiff::Owned` and every batch here uses it.

use crate::chunk::{ChunkDraft, estimate_tokens, slices_for};
use crate::extract::{AnchorKind, LinkDraft, SectionDraft};
use crate::gate::{GatedChunk, gate_heading_path};
use crate::hash::gate_path;
use crate::ids::{chunk_id, document_id, section_id, tag_id};
use crate::ingestor::IngestConfig;
use crate::model::{
    ChunkKind, ChunkNode, ChunkProps, DocStatus, DocumentNode, DocumentProps, Format,
    PreparedGraph, SectionNode, SectionProps, SourceId, TOKEN_ESTIMATOR,
};
use kaban_core::{
    AuditEntry, EdgeDiff, INGEST_OWNER, NewNode, Node, NodeId, Op, Owner, WorkspaceId, WriteBatch,
};
use kaban_policy::{AuditSigner, Policy};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

/// `props.owner` of every node and edge this pipeline writes (spec D5 [A-3]): the core's
/// [`INGEST_OWNER`], the wire form of [`Owner::KabanIngest`] that `EdgeDiff::Owned` compares
/// against and the only owner the store reactivates for (DI1.8; typed in E2.2).
pub const EDGE_OWNER: &str = INGEST_OWNER;

/// Every batch this pipeline writes: the owned edge diff (spec D5 [A-3], engine-spec EN-E4), so
/// a re-ingest deletes only the edges it created and a human `link` or a `remember` evidence
/// edge from an unchanged chunk survives.
pub fn ingest_batch(workspace: &WorkspaceId, ops: Vec<Op>) -> WriteBatch {
    let mut batch = WriteBatch::new(workspace.clone(), ops);
    batch.edge_diff = EdgeDiff::Owned {
        owner: Owner::KabanIngest,
    };
    batch
}

/// The kinds this pipeline owns (the store's reconciliation transition is restricted to them).
const INGEST_KINDS: [&str; 3] = ["Document", "Section", "Chunk"];
/// Props key of the owner mark on nodes and edges.
const OWNER: &str = "owner";
/// Props key `forget --document` sets on the Document with its tombstone (DI1.8, RD2-5): a
/// directory or workspace walk leaves the document alone until an ingest names its path or
/// runs with `--include-forgotten`.
pub const FORGOTTEN: &str = "forgotten";
/// Props key on the stored Document node listing its current Section ids (read back as
/// [`ExistingDocument::section_ids`]).
const SECTION_IDS: &str = "section_ids";
/// Props key on the stored Document node listing its current Chunk ids.
const CHUNK_IDS: &str = "chunk_ids";
/// Props key on the stored Document node listing the links that bound to no Document
/// (URLs, targets the gate refused, targets escaping the root).
const UNRESOLVED_LINKS: &str = "unresolved_links";

/// The stored current document, as the diff needs it (docs-ingest-plan.md DI1.5): the ids of
/// its sections and chunks, read from the props `build_batch` wrote.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ExistingDocument {
    /// `document_id(source)`.
    pub document_id: NodeId,
    /// The stored sanitized `content_hash` (`hash::content_hash` over the prepared graph;
    /// `None` for blocked, failed or missing documents).
    pub content_hash: Option<String>,
    /// Section ids the stored document has.
    pub section_ids: Vec<NodeId>,
    /// Chunk ids the stored document has.
    pub chunk_ids: Vec<NodeId>,
    /// `0` in overwrite mode.
    pub version: u32,
    /// Whether the stored Document row is `active` (a tombstoned one is absent for the skip
    /// decision and its re-appearing ids are reactivated, DI1.8).
    pub active: bool,
    /// `forget --document` marked it (see [`FORGOTTEN`]).
    pub forgotten: bool,
}

impl ExistingDocument {
    /// The section and chunk ids the stored document lists, in that order.
    pub fn child_ids(&self) -> impl Iterator<Item = &NodeId> {
        self.section_ids.iter().chain(self.chunk_ids.iter())
    }

    /// The ids of `graph` whose stored rows may be tombstoned and need `Op::Reactivate`
    /// (DI1.8, RD2-5): every id when the stored document is not an active `ok`/`partial`
    /// one (forgotten, failed, blocked: its children were tombstoned), otherwise only the ids
    /// the stored lists do not carry (a chunk edited away earlier and now restored).
    pub fn reactivation_candidates<'g>(
        previous: Option<&Self>,
        graph: &'g PreparedGraph,
    ) -> Vec<&'g NodeId> {
        let all = || {
            std::iter::once(&graph.document.id)
                .chain(
                    graph
                        .sections
                        .iter()
                        .flat_map(|s| std::iter::once(&s.id).chain(s.chunks.iter().map(|c| &c.id))),
                )
                .collect::<Vec<_>>()
        };
        match previous {
            None => Vec::new(),
            Some(p) if p.active && p.content_hash.is_some() => {
                let listed: BTreeSet<&NodeId> = p.child_ids().collect();
                all()
                    .into_iter()
                    .filter(|id| **id != graph.document.id && !listed.contains(id))
                    .collect()
            }
            Some(_) => all(),
        }
    }

    /// Read the diff inputs back from a stored Document node. A node that is not a `Document`
    /// (or whose props carry no id lists: blocked, failed, missing) yields empty lists, so the
    /// first successful ingest after such a state tombstones nothing.
    pub fn from_node(node: &Node) -> Self {
        let ids = |key: &str| -> Vec<NodeId> {
            node.props
                .get(key)
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .filter_map(|s| NodeId::parse(s).ok())
                        .collect()
                })
                .unwrap_or_default()
        };
        Self {
            document_id: NodeId::parse(&node.id).unwrap_or_default(),
            content_hash: node
                .props
                .get("content_hash")
                .and_then(Value::as_str)
                .map(str::to_owned),
            section_ids: ids(SECTION_IDS),
            chunk_ids: ids(CHUNK_IDS),
            version: node
                .props
                .get("version")
                .and_then(Value::as_u64)
                .and_then(|v| u32::try_from(v).ok())
                .unwrap_or(0),
            active: node.lifecycle == kaban_core::Lifecycle::Active,
            forgotten: node.props.get(FORGOTTEN).and_then(Value::as_bool) == Some(true),
        }
    }
}

/// What one document contributes to [`assemble`]: sanitized document props, the chunker's
/// sections with their gated chunks in the same order, the gated links and tags, and the audit
/// rows collected so far.
#[derive(Debug)]
pub struct AssembleInput<'a> {
    /// The source identity.
    pub source: &'a SourceId,
    /// Sanitized props (title, frontmatter, counts filled; ids are not props).
    pub props: DocumentProps,
    /// The chunker's output: section drafts (heading paths still ungated) with their chunk
    /// drafts; `chunks` holds the gated version of the same chunks, flattened in order.
    pub sections: &'a [(SectionDraft, Vec<ChunkDraft>)],
    /// Gated chunks, flattened in document order (one per draft in `sections`).
    pub chunks: Vec<GatedChunk>,
    /// Audit rows collected before assembly.
    pub audit: Vec<AuditEntry>,
    /// [`crate::PIPELINE_VERSION`] (an id input).
    pub pipeline_version: u32,
    /// Delivery slice size for `ChunkProps::slices` (spec D4 [A-2]).
    pub slice_bytes: u32,
}

/// The assembled graph and the audit rows (heading redactions appended). Links and tags are
/// gated after assembly ([`crate::gate::gate_links_and_tags`] needs the heading verdicts,
/// RD2-3) and set on the graph by the caller.
#[derive(Debug)]
pub struct Assembled {
    /// Nodes with ids assigned, chunks in reading order; `links` and `tags` empty.
    pub graph: PreparedGraph,
    /// Audit rows for the same transaction.
    pub audit: Vec<AuditEntry>,
    /// Source ranges of the sections whose heading path carried a hit (RD2-3).
    pub redacted_sections: Vec<std::ops::Range<u64>>,
}

/// Assign `section_id`/`chunk_id` (DI1.1) over the sanitized heading paths and chunk texts and
/// fix the reading order (`order` on sections and chunks, which `build_batch` turns into
/// `NEXT`). Heading texts are persisted and are id inputs, so they are gated here first (S1).
pub fn assemble(
    policy: &Policy,
    signer: &dyn AuditSigner,
    workspace: &WorkspaceId,
    input: AssembleInput<'_>,
) -> Assembled {
    let AssembleInput {
        source,
        mut props,
        sections,
        chunks,
        mut audit,
        pipeline_version,
        slice_bytes,
    } = input;
    let mut redacted_sections = Vec::new();
    let lang = props.lang.clone();
    let mut section_ordinals: BTreeMap<Vec<String>, u32> = BTreeMap::new();
    let mut chunk_ordinals: BTreeMap<(Vec<String>, String), u32> = BTreeMap::new();
    let mut gated_chunks = chunks.into_iter();
    let mut section_nodes = Vec::with_capacity(sections.len());
    let mut order = 0u32;
    for (i, (draft, drafts)) in sections.iter().enumerate() {
        let (heading_path, ids) =
            gate_heading_path(policy, signer, workspace, &draft.heading_path, &mut audit);
        if !ids.is_empty() {
            redacted_sections.push(draft.byte_start..draft.byte_end);
        }
        for r in ids {
            *props.rule_ids.entry(r).or_default() += 1;
        }
        let ordinal = section_ordinals.entry(heading_path.clone()).or_default();
        let id = section_id(source, &heading_path, *ordinal, pipeline_version);
        *ordinal += 1;
        let mut chunk_nodes = Vec::with_capacity(drafts.len());
        for g in gated_chunks.by_ref().take(drafts.len()) {
            let text = g.draft.text;
            let text_hash = blake3::hash(text.as_bytes()).to_hex().to_string();
            let among = chunk_ordinals
                .entry((heading_path.clone(), text_hash.clone()))
                .or_default();
            let cid = chunk_id(source, &heading_path, &text_hash, *among, pipeline_version);
            *among += 1;
            let bytes = u32::try_from(text.len()).unwrap_or(u32::MAX);
            chunk_nodes.push(ChunkNode {
                id: cid,
                section_path: heading_path.clone(),
                props: ChunkProps {
                    token_estimate: estimate_tokens(&text),
                    token_estimator: TOKEN_ESTIMATOR,
                    text_hash,
                    order,
                    byte_start: g.draft.byte_start,
                    byte_end: g.draft.byte_end,
                    page: g.draft.page,
                    context_prefix: g.draft.context_prefix,
                    lang: if g.draft.kind == ChunkKind::Stub {
                        None
                    } else {
                        lang.clone()
                    },
                    kind: g.draft.kind,
                    oversize: g.draft.oversize,
                    table_unknown: g.draft.table_unknown,
                    stub_hmac: g.stub_hmac,
                    slices: slices_for(&text, slice_bytes),
                    bytes,
                    text,
                },
            });
            order += 1;
        }
        section_nodes.push(SectionNode {
            id,
            props: SectionProps {
                heading_path,
                level: draft.level,
                order: u32::try_from(i).unwrap_or(u32::MAX),
                byte_start: draft.byte_start,
                byte_end: draft.byte_end,
                page_start: draft.page,
                page_end: draft.page,
                kind: draft.kind,
            },
            chunks: chunk_nodes,
        });
    }
    Assembled {
        graph: PreparedGraph {
            document: DocumentNode {
                id: document_id(source),
                source: source.clone(),
                props,
            },
            sections: section_nodes,
            links: Vec::new(),
            tags: Vec::new(),
        },
        audit,
        redacted_sections,
    }
}

/// Where a link ended up (spec D3 `LINKS_TO.resolved`; `type-enum-states`). `resolved` means
/// "bound to a Document id": whether the file exists is the target node's `status`, so the
/// edge props never depend on commit order (N5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkTarget {
    /// The target normalized to a gated path and is bound to `document_id(source)`
    /// (`resolved: true`).
    Document {
        /// `document_id(source)`.
        id: NodeId,
        /// `SourceId::new(workspace, gated normalized target)`.
        source: SourceId,
        /// The gated, normalized, workspace-relative target path.
        path: String,
        /// Whether a Document row exists under `id` when this batch is applied (stored
        /// already, or committed earlier in the same run). When false the batch writes a
        /// `Document{status: missing}` placeholder, which the file upgrades in place when it
        /// appears.
        stored: bool,
    },
    /// A URL, a target the path gate refused, or one escaping the root: no edge, no
    /// placeholder; the link is recorded on the Document (`unresolved_links`) with the masked
    /// raw target (`resolved: false`).
    Unresolvable,
}

/// One link with its resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedLink {
    /// The gated link as extracted.
    pub link: LinkDraft,
    /// Where it points.
    pub target: LinkTarget,
}

/// Normalize a wikilink target into a workspace-relative path (D1, D3): the `#heading` or
/// `^block` anchor is dropped, a target with no extension gets `.md`, a target with a `/` is
/// vault-root relative (Obsidian's folder form) and a bare name is a sibling of the linking
/// document; `.` and `..` components are folded and a path escaping the root is `None`.
fn normalize_target(raw: &str, anchor: AnchorKind, doc_path: &str) -> Option<String> {
    let name = match anchor {
        AnchorKind::Heading => raw.split_once('#').map_or(raw, |(n, _)| n),
        AnchorKind::Block => raw.split_once('^').map_or(raw, |(n, _)| n),
        AnchorKind::Note => raw,
        AnchorKind::Url => return None,
    }
    .trim();
    if name.is_empty() {
        return None;
    }
    let mut components: Vec<&str> = Vec::new();
    if let Some((dir, _)) = doc_path.rsplit_once('/')
        && !name.contains('/')
    {
        components.extend(dir.split('/'));
    }
    for part in name.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                components.pop()?;
            }
            p => components.push(p),
        }
    }
    let last = components.last()?;
    let mut path = components.join("/");
    if !last.contains('.') {
        path.push_str(".md");
    }
    Some(path)
}

/// Resolve the document's links (D3): the normalized target is gated (`gate_path`: a refused
/// target yields no identity, PR-2) and bound to `document_id(SourceId::new(workspace,
/// target))`; `stored(id, path)` says whether a Document row will exist under that id when the
/// batch is applied (otherwise the batch writes the placeholder).
pub fn resolve_links(
    policy: &Policy,
    workspace: &WorkspaceId,
    doc_path: &str,
    links: Vec<LinkDraft>,
    mut stored: impl FnMut(&NodeId, &str) -> bool,
) -> Vec<ResolvedLink> {
    links
        .into_iter()
        .map(|link| {
            let target = normalize_target(&link.raw_target, link.anchor_kind, doc_path)
                .and_then(|path| gate_path(policy, &path).ok().map(|g| (path, g)))
                .map_or(LinkTarget::Unresolvable, |(path, gated)| {
                    let source = SourceId::new(workspace, &gated);
                    let id = document_id(&source);
                    let stored = stored(&id, &path);
                    LinkTarget::Document {
                        id,
                        source,
                        path,
                        stored,
                    }
                });
            ResolvedLink { link, target }
        })
        .collect()
}

/// Case-fold a tag for identity (Obsidian tags are case-insensitive; nested `a/b` kept).
fn normalize_tag(tag: &str) -> String {
    tag.trim_matches('/').to_lowercase()
}

/// Props of a `LINKS_TO` edge (spec D3), serialized onto the edge.
#[derive(Debug, Serialize)]
struct LinkEdgeProps<'a> {
    raw_target: &'a str,
    anchor_kind: AnchorKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    alias: Option<&'a str>,
    resolved: bool,
    embed: bool,
    owner: &'static str,
}

impl<'a> LinkEdgeProps<'a> {
    fn of(link: &'a LinkDraft, resolved: bool) -> Value {
        // Invariant: a struct of scalars and strings always serializes.
        serde_json::to_value(Self {
            raw_target: &link.raw_target,
            anchor_kind: link.anchor_kind,
            alias: link.alias.as_deref(),
            resolved,
            embed: link.embed,
            owner: EDGE_OWNER,
        })
        .unwrap_or_default()
    }
}

fn owned_edge(src: &NodeId, dst: &NodeId, rel: &str) -> Op {
    Op::UpsertEdge {
        src: src.clone(),
        dst: dst.clone(),
        rel: rel.to_owned(),
        props: Some(json!({ "owner": EDGE_OWNER })),
    }
}

/// A node op of this pipeline. Document, Section and Chunk nodes carry `props.owner =
/// INGEST_OWNER` (DI1.8): the mark the store's reconciliation transition requires, so only the
/// pipeline's own tombstones ever come back; shared Tag nodes are not owned by anyone.
fn node(kind: &str, text: String, summary: Option<String>, props: Value) -> NewNode {
    let props = match props {
        Value::Object(mut m) if INGEST_KINDS.contains(&kind) => {
            m.insert(OWNER.to_owned(), Value::String(EDGE_OWNER.to_owned()));
            Value::Object(m)
        }
        other => other,
    };
    NewNode {
        kind: kind.to_owned(),
        text,
        summary,
        props: Some(props),
        ..NewNode::default()
    }
}

/// Props of a `Document{status: missing}` placeholder: deterministic, so every document
/// linking to the same missing target upserts the same bytes (N5).
fn placeholder_props(source: &SourceId, path: &str, pipeline_version: u32) -> DocumentProps {
    DocumentProps {
        source_id: source.as_str().to_owned(),
        path: path.to_owned(),
        content_hash: None,
        blocked_hmac: None,
        position_fingerprint: None,
        version: 0,
        current: true,
        format: Format::Markdown,
        size: 0,
        mtime: 0,
        title: None,
        lang: None,
        frontmatter: Value::Object(serde_json::Map::new()),
        pipeline_version,
        extractor: String::new(),
        extractor_version: String::new(),
        config_hash: String::new(),
        status: DocStatus::Missing,
        error: None,
        needs_ocr: false,
        page_count: None,
        blocked_chunks: 0,
        table_unknown_chunks: 0,
        rule_ids: BTreeMap::new(),
    }
}

/// Serialize props to a JSON object (a props struct of scalars, strings and maps always
/// serializes; the fallback is an empty object).
fn props_json<T: Serialize>(props: &T) -> serde_json::Map<String, Value> {
    match serde_json::to_value(props) {
        Ok(Value::Object(m)) => m,
        _ => serde_json::Map::new(),
    }
}

/// `Tombstone` ops for every section and chunk id the stored document had and `keep` lacks.
fn tombstones(previous: Option<&ExistingDocument>, keep: &BTreeSet<&NodeId>) -> Vec<Op> {
    previous
        .into_iter()
        .flat_map(|p| p.section_ids.iter().chain(p.chunk_ids.iter()))
        .filter(|id| !keep.contains(id))
        .map(|id| Op::Tombstone {
            id: id.clone(),
            reason: "absent from the re-ingested document".to_owned(),
        })
        .collect()
}

/// The batch for a document that stores no children (blocked or failed, D6/D8): the Document
/// node and tombstones for every section and chunk the stored document had, since its text is
/// no longer provided by the source.
pub fn document_only_batch(
    document_id: &NodeId,
    props: &DocumentProps,
    previous: Option<&ExistingDocument>,
    workspace: &WorkspaceId,
) -> WriteBatch {
    let mut json = props_json(props);
    if let Some(p) = previous {
        carry_inventory(&mut json, p);
    }
    let mut ops = vec![Op::UpsertNode {
        id: document_id.clone(),
        node: node("Document", props.path.clone(), None, Value::Object(json)),
    }];
    ops.extend(tombstones(previous, &BTreeSet::new()));
    ingest_batch(workspace, ops)
}

/// Keep the stored child inventory on a Document that stores no children (DI1.8, RD2-7), so
/// a later successful ingest and `forget --document` still find what to tombstone.
fn carry_inventory(props: &mut serde_json::Map<String, Value>, previous: &ExistingDocument) {
    let list =
        |ids: &[NodeId]| Value::Array(ids.iter().map(|i| Value::String(i.to_string())).collect());
    if !previous.section_ids.is_empty() {
        props.insert(SECTION_IDS.to_owned(), list(&previous.section_ids));
    }
    if !previous.chunk_ids.is_empty() {
        props.insert(CHUNK_IDS.to_owned(), list(&previous.chunk_ids));
    }
}

/// Reconcile a failure batch built without store access (a timeout, a panic, a read error)
/// against the stored child inventory at the commit boundary (DI1.8, RD2-7): the Document op
/// keeps the previous child lists and every listed child is tombstoned as a
/// `document_only_batch` promise.
pub fn reconcile_failed_batch(mut batch: WriteBatch, previous: &ExistingDocument) -> WriteBatch {
    for op in &mut batch.ops {
        if let Op::UpsertNode { node, .. } = op
            && node.kind == "Document"
            && let Some(Value::Object(props)) = node.props.as_mut()
        {
            carry_inventory(props, previous);
        }
    }
    let named: BTreeSet<&NodeId> = batch
        .ops
        .iter()
        .filter_map(|op| match op {
            Op::Tombstone { id, .. } => Some(id),
            _ => None,
        })
        .collect();
    let extra: Vec<Op> = tombstones(Some(previous), &named);
    batch.ops.extend(extra);
    batch
}

/// The batch of `kaban forget --document <path>` (docs-ingest-spec D10; plan §12 rollback):
/// tombstone the Document and every section and chunk it lists.
pub fn forget_document_batch(
    previous: &ExistingDocument,
    stored: &Node,
    workspace: &WorkspaceId,
) -> WriteBatch {
    // the persisted `forgotten` mark (DI1.8): the stored row's own content plus the flag, so
    // the upsert rewrites the props only; the tombstone follows in the same transaction
    let mut props = match &stored.props {
        Value::Object(m) => m.clone(),
        _ => serde_json::Map::new(),
    };
    props.insert(FORGOTTEN.to_owned(), Value::Bool(true));
    let mut ops = vec![
        Op::UpsertNode {
            id: previous.document_id.clone(),
            node: NewNode {
                kind: stored.kind.clone(),
                text: stored.text.clone(),
                summary: stored.summary.clone(),
                tags: stored.tags.clone(),
                props: Some(Value::Object(props)),
                ..NewNode::default()
            },
        },
        Op::Tombstone {
            id: previous.document_id.clone(),
            reason: "forget --document".to_owned(),
        },
    ];
    ops.extend(
        previous
            .section_ids
            .iter()
            .chain(previous.chunk_ids.iter())
            .map(|id| Op::Tombstone {
                id: id.clone(),
                reason: "forget --document".to_owned(),
            }),
    );
    ingest_batch(workspace, ops)
}

/// What `forget --document` did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct ForgottenDocument {
    /// The tombstoned Document id.
    pub document_id: String,
    /// Nodes moved to `tombstoned` by the batch (the Document, its sections and chunks that
    /// were still active).
    pub tombstoned: u32,
}

/// `kaban forget --document <path>` (docs-ingest-spec D10): gate the path (PR-2), find the
/// stored Document and tombstone it with its sections and chunks in one transaction. The
/// placeholder of a link target and a blocked document are documents too and are forgotten
/// the same way; the next `ingest` of the file re-creates the ids (same identities).
///
/// # Errors
/// `Invalid` when the path hits the denylist, `NotFound` when no document is stored under
/// the path, store errors from the batch.
pub fn forget_document(
    store: &dyn kaban_core::GraphStore,
    policy: &Policy,
    ctx: &kaban_core::WriteCtx,
    path: &str,
) -> Result<ForgottenDocument, kaban_api::ApiError> {
    let gated = gate_path(policy, path)
        .map_err(|_| kaban_api::ApiError::Invalid("path rejected by the denylist".to_owned()))?;
    let id = document_id(&SourceId::new(&ctx.workspace_id, &gated));
    let node = store
        .get(&id, true)?
        .ok_or_else(|| kaban_api::ApiError::NotFound(id.to_string()))?;
    let previous = ExistingDocument::from_node(&node);
    let mut removal_ctx = ctx.clone();
    removal_ctx.removal_intent = kaban_core::RemovalIntent::Explicit;
    let report = store.apply_batch(
        forget_document_batch(&previous, &node, &ctx.workspace_id),
        &removal_ctx,
    )?;
    Ok(ForgottenDocument {
        document_id: id.to_string(),
        tombstoned: report.tombstoned,
    })
}

/// Build the one-transaction batch of a prepared document (docs-ingest-plan.md DI1.5): node
/// upserts for the Document (text = path, props = [`DocumentProps`] plus `section_ids`,
/// `chunk_ids` and `unresolved_links`), Sections (text = last heading) and Chunks (text = chunk
/// text, summary = `context_prefix` so the FTS weights apply, props = [`ChunkProps`] without the
/// duplicated `text` plus `path`); `PART_OF` Chunk->Section->Document, `NEXT` between sibling
/// chunks and sibling sections (same parent heading path), `LINKS_TO` Document->Document with
/// a `missing` placeholder for an unknown target, `TAGGED` Document->Tag with the Tag node;
/// then tombstones for the stored ids that disappeared. Every edge carries `props.owner`.
pub fn build_batch(
    graph: &PreparedGraph,
    links: &[ResolvedLink],
    previous: Option<&ExistingDocument>,
    reactivate: &[NodeId],
    cfg: &IngestConfig,
    workspace: &WorkspaceId,
) -> WriteBatch {
    // `history: true` (DI3.4) will snapshot instead of tombstoning; overwrite mode here
    debug_assert!(!cfg.history, "document history is DI3.4");
    let doc = &graph.document;
    let pipeline_version = doc.props.pipeline_version;
    let mut nodes: Vec<Op> = Vec::new();
    let mut edges: Vec<Op> = Vec::new();
    let mut keep: BTreeSet<&NodeId> = BTreeSet::new();
    let mut section_ids = Vec::with_capacity(graph.sections.len());
    let mut chunk_ids = Vec::new();

    // sections and chunks, with PART_OF and NEXT
    let mut previous_by_parent: BTreeMap<&[String], &NodeId> = BTreeMap::new();
    for section in &graph.sections {
        keep.insert(&section.id);
        section_ids.push(Value::String(section.id.to_string()));
        nodes.push(Op::UpsertNode {
            id: section.id.clone(),
            node: node(
                "Section",
                section
                    .props
                    .heading_path
                    .last()
                    .cloned()
                    .unwrap_or_default(),
                None,
                Value::Object(props_json(&section.props)),
            ),
        });
        edges.push(owned_edge(&section.id, &doc.id, "PART_OF"));
        let parent = section
            .props
            .heading_path
            .split_last()
            .map_or(&[][..], |(_, parent)| parent);
        if let Some(prev) = previous_by_parent.insert(parent, &section.id) {
            edges.push(owned_edge(prev, &section.id, "NEXT"));
        }
        let mut previous_chunk: Option<&NodeId> = None;
        for chunk in &section.chunks {
            keep.insert(&chunk.id);
            chunk_ids.push(Value::String(chunk.id.to_string()));
            let mut props = props_json(&chunk.props);
            props.remove("text"); // the node's `text` column is the one copy
            props.insert("path".to_owned(), Value::String(doc.props.path.clone()));
            nodes.push(Op::UpsertNode {
                id: chunk.id.clone(),
                node: node(
                    "Chunk",
                    chunk.props.text.clone(),
                    Some(chunk.props.context_prefix.clone()),
                    Value::Object(props),
                ),
            });
            edges.push(owned_edge(&chunk.id, &section.id, "PART_OF"));
            if let Some(prev) = previous_chunk {
                edges.push(owned_edge(prev, &chunk.id, "NEXT"));
            }
            previous_chunk = Some(&chunk.id);
        }
    }

    // links: one materialized edge per (src, dst, rel); the first occurrence's props win
    let mut linked: BTreeSet<&NodeId> = BTreeSet::new();
    let mut placeholders: BTreeSet<&NodeId> = BTreeSet::new();
    let mut unresolved: Vec<Value> = Vec::new();
    for r in links {
        match &r.target {
            // a self-link needs no edge (`SelfLink` would fail the batch)
            LinkTarget::Document { id, .. } if id == &doc.id => {}
            LinkTarget::Document {
                id,
                source,
                path,
                stored,
            } => {
                if !stored && placeholders.insert(id) {
                    let props = placeholder_props(source, path, pipeline_version);
                    nodes.push(Op::UpsertNode {
                        id: id.clone(),
                        node: node(
                            "Document",
                            path.clone(),
                            None,
                            Value::Object(props_json(&props)),
                        ),
                    });
                }
                if linked.insert(id) {
                    edges.push(Op::UpsertEdge {
                        src: doc.id.clone(),
                        dst: id.clone(),
                        rel: "LINKS_TO".to_owned(),
                        props: Some(LinkEdgeProps::of(&r.link, true)),
                    });
                }
            }
            LinkTarget::Unresolvable => unresolved.push(LinkEdgeProps::of(&r.link, false)),
        }
    }

    // tags: shared Tag nodes, case-folded identity
    let mut tagged: BTreeSet<String> = BTreeSet::new();
    for t in &graph.tags {
        let name = normalize_tag(&t.tag);
        if name.is_empty() || !tagged.insert(name.clone()) {
            continue;
        }
        let id = tag_id(workspace, &name);
        nodes.push(Op::UpsertNode {
            id: id.clone(),
            node: node("Tag", name.clone(), None, json!({ "tag": name })),
        });
        edges.push(owned_edge(&doc.id, &id, "TAGGED"));
    }

    // the Document itself, last among the nodes so its props list what exists above
    let mut props = props_json(&doc.props);
    props.insert(SECTION_IDS.to_owned(), Value::Array(section_ids));
    props.insert(CHUNK_IDS.to_owned(), Value::Array(chunk_ids));
    if !unresolved.is_empty() {
        props.insert(UNRESOLVED_LINKS.to_owned(), Value::Array(unresolved));
    }
    // reactivations first (DI1.8, RD2-5): the tombstoned rows whose ids re-appear come back
    // before their content upsert, which then reports `unchanged` when nothing else moved
    let mut ops: Vec<Op> = reactivate
        .iter()
        .map(|id| Op::Reactivate { id: id.clone() })
        .collect();
    ops.push(Op::UpsertNode {
        id: doc.id.clone(),
        node: node(
            "Document",
            doc.props.path.clone(),
            doc.props.title.clone(),
            Value::Object(props),
        ),
    });
    ops.append(&mut nodes);
    ops.append(&mut edges);
    ops.extend(tombstones(previous, &keep));
    ingest_batch(workspace, ops)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn wikilink_targets_normalize_relative_to_the_document_or_the_root() {
        // Arrange / Act / Assert: sibling, folder form, anchors, extension, escape
        assert_eq!(
            normalize_target("Other Note", AnchorKind::Note, "notes/a.md").as_deref(),
            Some("notes/Other Note.md")
        );
        assert_eq!(
            normalize_target("Other#Sec", AnchorKind::Heading, "a.md").as_deref(),
            Some("Other.md")
        );
        assert_eq!(
            normalize_target("Other^blk", AnchorKind::Block, "a.md").as_deref(),
            Some("Other.md")
        );
        assert_eq!(
            normalize_target("folder/Note", AnchorKind::Note, "notes/a.md").as_deref(),
            Some("folder/Note.md")
        );
        assert_eq!(
            normalize_target("diagram.png", AnchorKind::Note, "a.md").as_deref(),
            Some("diagram.png")
        );
        assert_eq!(
            normalize_target("../../x", AnchorKind::Note, "notes/a.md"),
            None
        );
        assert_eq!(
            normalize_target("https://e.com", AnchorKind::Url, "a.md"),
            None
        );
    }

    #[test]
    fn tags_fold_case_and_keep_nesting() {
        assert_eq!(normalize_tag("Project/Sub-Topic"), "project/sub-topic");
        assert_eq!(normalize_tag("/alpha/"), "alpha");
    }
}
