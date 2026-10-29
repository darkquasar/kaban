//! `kaban review` (engine-spec EN-F14a; engine-plan E2.5): every pending item of the bound
//! workspace with its one sentence and its first next action, so a person can work through
//! them in one sitting. Pending items only: a settled status is terminal and never listed.
//!
//! ```text
//!   ReviewQuery { ontology, kind, status, limit }
//!     |
//!     v
//!   MemoryStore::review_rows(ws)        -- candidate / unresolved groups, discovered rows
//!   SyncStore::conflicts(ws)            -- equal-rev conflicts
//!   MemoryStore::proposed_nodes_in(ws)  -- proposed records of every kind (EN-F4, EN-F13)
//!   IngestStore::list_nodes(CodeRef)    -- stale anchors against the resolved snapshot
//!     |  each through KabanApi::status_of (explain.rs), one item per (subject, group)
//!     v
//!   Vec<ReviewItem { subject, group_key, status, sentence, first_action }>   -- filtered, cut at limit
//! ```

use crate::api::KabanApi;
use crate::error::ApiError;
use crate::explain::{
    Addressable, GroupPick, NextAction, PendingStatus, PendingStatusKind, Status,
};
use kaban_core::NodeId;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// The default and the ceiling of `limit`.
const DEFAULT_LIMIT: usize = 50;
const MAX_LIMIT: usize = 500;
/// One page of the record and anchor scans.
const PAGE: usize = 500;

/// `kaban review` input.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ReviewQuery {
    /// Only items whose kind belongs to this ontology namespace (`infra`, `x`, ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ontology: Option<String>,
    /// Only items of this stored kind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Only items of this pending status.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<PendingStatusKind>,
    /// At most this many items (default 50, max 500).
    #[serde(default)]
    pub limit: usize,
}

/// One pending item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ReviewItem {
    /// What is pending.
    pub subject: Addressable,
    /// Why.
    pub status: PendingStatus,
    /// The one sentence (the `explain` sentence).
    pub sentence: String,
    /// The first next action of `explain`.
    pub first_action: NextAction,
    /// The item's stored kind (the review filters apply to it).
    pub kind: String,
    /// The conflict group the item is about (its canonical key, or the `x:` label of a
    /// `discovered_kind` item), when the item is one group's: a node with two outstanding
    /// groups is two items (the wave-2 review's RW2-4). Absent for a conflict, a proposed
    /// record or a stale anchor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_key: Option<String>,
}

/// `kaban review` output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ReviewResponse {
    /// The pending items, review order: review rows, conflicts, proposed records, stale anchors.
    pub items: Vec<ReviewItem>,
    /// `true` when more items existed than `limit`.
    pub partial: bool,
}

/// Whether `kind` belongs to `ns` (`x:Org` to `x`, `infra:Vendor` to `infra`, a bare name to
/// the shipped namespaces `core`, `adr` and `user`).
fn in_namespace(kind: &str, ns: &str) -> bool {
    match kind.split_once(':') {
        Some((k, _)) => k == ns,
        None => matches!(ns, "core" | "adr" | "user"),
    }
}

/// Every pending item of the bound workspace (EN-F14a), in review order.
///
/// # Errors
/// Store errors.
pub fn review(api: &KabanApi, q: &ReviewQuery) -> Result<ReviewResponse, ApiError> {
    api.review(q)
}

impl KabanApi {
    /// [`review`]: the pending items of the bound workspace.
    ///
    /// # Errors
    /// Store errors.
    pub fn review(&self, q: &ReviewQuery) -> Result<ReviewResponse, ApiError> {
        let ws = self.config.workspace.as_str().to_owned();
        let limit = if q.limit == 0 {
            DEFAULT_LIMIT
        } else {
            q.limit.min(MAX_LIMIT)
        };
        // one item per (node, group): a node's groups are each listed; a node-level source
        // (conflict, proposed, stale) adds nothing to a node a group already lists
        let mut subjects: Vec<(NodeId, Option<String>, String)> = Vec::new();
        let mut seen_nodes: BTreeSet<String> = BTreeSet::new();
        let mut seen: BTreeSet<(String, Option<String>)> = BTreeSet::new();
        let mut push = |id: NodeId, group: Option<String>, kind: String| {
            let node_seen = !seen_nodes.insert(id.to_string());
            if group.is_none() && node_seen {
                return;
            }
            if seen.insert((id.to_string(), group.clone())) {
                subjects.push((id, group, kind));
            }
        };
        for row in self.store.review_rows(&ws, None)? {
            push(row.node, Some(row.group_key), row.kind);
        }
        for c in self.store.conflicts(&ws)? {
            if let Ok(id) = NodeId::parse(&c.node_id)
                && let Some(node) = self.store.get(&id, false)?
            {
                push(id, None, node.kind);
            }
        }
        // the proposed records of every kind (EN-F4's governed kinds and the records the
        // reconciler materialised, EN-F13; RW2-5: one read seam, no kind list), then the
        // `CodeRef` anchors (stale against the resolved snapshot); each page in id order (N5)
        let mut offset = 0usize;
        loop {
            let page = self.store.proposed_nodes_in(&ws, offset, PAGE)?;
            let n = page.len();
            for node in page {
                if let Ok(id) = NodeId::parse(&node.id) {
                    push(id, None, node.kind);
                }
            }
            if n < PAGE {
                break;
            }
            offset += n;
        }
        let mut offset = 0usize;
        loop {
            let page = self.store.list_nodes("CodeRef", &ws, offset, PAGE)?;
            let n = page.len();
            for node in page {
                if let Ok(id) = NodeId::parse(&node.id) {
                    push(id, None, node.kind);
                }
            }
            if n < PAGE {
                break;
            }
            offset += n;
        }
        // the discovered kinds themselves, after the nodes (EN-F5: each `x:` kind counted in
        // the workspace is one item awaiting promotion or pruning)
        let mut addresses: Vec<(Addressable, Option<String>, String)> = subjects
            .into_iter()
            .map(|(id, group, kind)| (Addressable::Node(id), group, kind))
            .collect();
        addresses.extend(
            self.store
                .kind_stats(&self.config.workspace)?
                .into_iter()
                .filter(|k| k.ns == "x" && k.n_nodes > 0)
                .map(|k| {
                    let label = k.stored();
                    (Addressable::DiscoveredKind(label.clone()), None, label)
                }),
        );
        let mut items = Vec::new();
        let mut partial = false;
        for (address, group, kind) in addresses {
            if let Some(ns) = q.ontology.as_deref()
                && !in_namespace(&kind, ns)
            {
                continue;
            }
            if let Some(k) = q.kind.as_deref()
                && k != kind
            {
                continue;
            }
            let status = match (&address, group.as_deref()) {
                (Addressable::Node(id), Some(key)) => {
                    self.node_status_in(id, GroupPick::Group(key))
                }
                _ => self.status_of(&address),
            };
            let status = match status {
                Ok(s) => s,
                Err(ApiError::NotFound(_)) => continue,
                Err(e) => return Err(e),
            };
            let Status::Pending(pending) = status.status else {
                continue;
            };
            if let Some(want) = q.status
                && pending.kind() != want
            {
                continue;
            }
            let Some(first_action) = status.next_actions.into_iter().next() else {
                continue;
            };
            if items.len() == limit {
                partial = true;
                break;
            }
            items.push(ReviewItem {
                subject: status.subject,
                status: pending,
                sentence: status.sentence,
                first_action,
                kind,
                group_key: group,
            });
        }
        Ok(ReviewResponse { items, partial })
    }
}
