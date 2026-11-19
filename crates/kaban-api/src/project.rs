//! Outgoing projections: every string that leaves the service passes through one read-time
//! gate (core-spec S4; review R1-4): node cards (text, summary, tags, props, kind,
//! workspace, timestamp, `why`), edge cards (ids, relationship), schema definitions (names,
//! descriptions, endpoint kinds, property schemas) and stats keys. A redact-rule hit masks the
//! string; a reject-rule hit hides the whole item (card, edge, definition or stats entry).
//! Read-path verdicts are not recorded: the masked item is returned, a hidden one is dropped.

use crate::api::KabanApi;
use crate::control::ControlPath;
use crate::enrich::{Enrichment, pointer_of};
use crate::error::ApiError;
use crate::types::{Card, EdgeCard, Provenance};
use kaban_core::{CodeRef, Edge, Node, TypeDef};
use serde_json::Value;

/// Tool name recorded for read-path verdicts.
const READ: &str = "read";

impl KabanApi {
    /// Whether outgoing strings are re-scanned (`defaults.scan_on_read`).
    pub(crate) fn scan_on_read(&self) -> bool {
        self.pol().file.defaults.scan_on_read
    }

    /// Outgoing projection of a node: every string re-scanned (the policy may have grown since
    /// the node was stored). `None` when the node must be hidden (a reject-rule hit).
    pub(crate) fn project(&self, n: &Node, full: bool, why: Option<&[String]>) -> Option<Card> {
        self.project_card(n, full, why).map(|(card, _)| card)
    }

    /// Whether the read gate hides a node, without its projection: a reject rule hits one of
    /// its persisted strings. The eligibility predicate of a store's recall (spec 05 SS-11)
    /// runs under the store's own lock, so it reads nothing from the store. Nothing is hidden
    /// without `scan_on_read`.
    pub(crate) fn hidden_read(&self, n: &Node) -> bool {
        self.scan_on_read() && self.scan_node(n, None, &mut Vec::new()).is_none()
    }

    /// [`Self::project`] plus the `CodeRef` pointer the node is (core-spec F19, M3-27; the
    /// linked pointer nodes of a response, R3-10): parsed from the **projected** props, so it
    /// is masked exactly like the card and absent when the card is hidden; `None` for every
    /// kind but `CodeRef`.
    pub(crate) fn project_card(
        &self,
        n: &Node,
        full: bool,
        why: Option<&[String]>,
    ) -> Option<(Card, Option<CodeRef>)> {
        let (projected, red) = if self.scan_on_read() {
            let mut red = Vec::new();
            let projected = self.scan_node(n, why, &mut red);
            (projected?, red)
        } else {
            (Projected::raw(n, why), Vec::new())
        };
        let pointer = pointer_of(&projected.kind, &projected.props);
        Some((projected.card(n, full, red), pointer))
    }

    /// Outgoing projection of an edge in `tool`'s `edges[]`; `None` when a string hits a reject
    /// rule. The ids are control (validated by shape, review RC-1); the relationship is content.
    pub(crate) fn project_edge(&self, tool: &'static str, e: &Edge) -> Option<EdgeCard> {
        if !self.scan_on_read() {
            return Some(EdgeCard {
                id: e.id.clone(),
                from: e.src.clone(),
                rel: e.rel.clone(),
                to: e.dst.clone(),
                confidence: e.confidence,
            });
        }
        let mut red = Vec::new();
        let at = |red: &mut Vec<String>, path, field, s: &str| {
            self.scan_control(READ, ControlPath::response(tool, path), field, s, red)
        };
        let gated = (|| -> Result<EdgeCard, ApiError> {
            Ok(EdgeCard {
                id: at(&mut red, "edges[].id", "edge.id", &e.id)?,
                from: at(&mut red, "edges[].from", "edge.from", &e.src)?,
                rel: self.scan_str(READ, "edge.rel", &e.rel, &mut red)?,
                to: at(&mut red, "edges[].to", "edge.to", &e.dst)?,
                confidence: e.confidence,
            })
        })();
        gated.ok()
    }

    /// Outgoing projection of a schema definition; `None` when a string hits a reject rule.
    pub(crate) fn project_typedef(&self, def: TypeDef) -> Option<TypeDef> {
        if !self.scan_on_read() {
            return Some(def);
        }
        let value = serde_json::to_value(&def).ok()?;
        let clean = self.project_value("schema", &value)?;
        serde_json::from_value(clean).ok()
    }

    /// Outgoing projection of a stats map (`by_kind`, `by_rel`): every key gated, an entry whose
    /// key hits a reject rule is dropped.
    pub(crate) fn project_stat_keys(&self, field: &str, map: &Value) -> Value {
        let Some(obj) = map.as_object() else {
            return map.clone();
        };
        if !self.scan_on_read() {
            return map.clone();
        }
        let mut out = serde_json::Map::new();
        for (k, v) in obj {
            if let Some(Value::String(k2)) = self.project_value(field, &Value::String(k.clone())) {
                out.insert(k2, v.clone());
            }
        }
        Value::Object(out)
    }

    /// Outgoing projection of the stats `scope` object (names the counted workspace).
    pub(crate) fn project_stats_scope(&self, scope: &Value) -> Option<Value> {
        if !self.scan_on_read() {
            return Some(scope.clone());
        }
        self.project_value("scope", scope)
    }

    /// Outgoing projection of an enricher's output (core-spec F19: "outputs pass the denylist
    /// like any read"); `None` when a string hits a reject rule.
    pub(crate) fn project_enrichment(&self, e: &Enrichment) -> Option<Enrichment> {
        if !self.scan_on_read() {
            return Some(e.clone());
        }
        let value = serde_json::to_value(e).ok()?;
        let clean = self.project_value("enrichment", &value)?;
        serde_json::from_value(clean).ok()
    }

    /// Gate a JSON value on the read path as `field`; `None` on a reject hit.
    fn project_value(&self, field: &str, value: &Value) -> Option<Value> {
        self.scan_json(READ, field, value, &mut Vec::new()).ok()
    }
}

/// The gated strings of a node.
struct Projected {
    kind: String,
    created_at: String,
    workspace_id: String,
    text: String,
    summary: Option<String>,
    tags: Vec<String>,
    props: Value,
    why: Option<Vec<String>>,
    provenance: Provenance,
}

impl KabanApi {
    /// Re-scan every string of the node: the shared inventory of persisted strings
    /// (`inventory.rs`, R2-2: a reject hit in any persisted column hides the card, even one the
    /// card does not show), the timestamp and the `why` entries. `None` when any field hits a
    /// reject rule. `why` entries carry relationship names: a redacted entry is masked, a
    /// rejected one dropped.
    fn scan_node(
        &self,
        n: &Node,
        why: Option<&[String]>,
        red: &mut Vec<String>,
    ) -> Option<Projected> {
        let t = READ;
        let masked = self.scan_node_strings(t, n, red).ok()?;
        let created_at = self.scan_str(t, "created_at", &n.created_at, red).ok()?;
        let why = why.map(|entries| {
            entries
                .iter()
                .filter_map(|w| self.scan_str(t, "why", w, red).ok())
                .collect()
        });
        Some(Projected {
            kind: masked.kind,
            created_at,
            workspace_id: masked.workspace_id,
            text: masked.strings.text,
            summary: masked.strings.summary,
            tags: masked.strings.tags,
            props: masked.strings.props,
            why,
            provenance: Provenance {
                agent_id: masked.strings.agent_id,
                session_id: masked.strings.session_id,
                repo: masked.strings.repo,
                commit_sha: masked.strings.commit_sha,
                store: n.store_scope,
            },
        })
    }
}

impl Projected {
    /// The node's strings as stored (no re-scan).
    fn raw(n: &Node, why: Option<&[String]>) -> Self {
        Self {
            kind: n.kind.clone(),
            created_at: n.created_at.clone(),
            workspace_id: n.workspace_id.clone(),
            text: n.text.clone(),
            summary: n.summary.clone(),
            tags: n.tags.clone(),
            props: n.props.clone(),
            why: why.map(<[String]>::to_vec),
            provenance: Provenance {
                agent_id: n.agent_id.clone(),
                session_id: n.session_id.clone(),
                repo: n.repo.clone(),
                commit_sha: n.commit_sha.clone(),
                store: n.store_scope,
            },
        }
    }

    /// Assemble the card (text only when requested or when there is no summary; props only
    /// with full text).
    fn card(self, n: &Node, full: bool, redactions: Vec<String>) -> Card {
        let show_text = full || self.summary.is_none();
        let show_props = full && self.props.as_object().is_some_and(|o| !o.is_empty());
        // lifecycle and governance are closed vocabularies, never user content: no re-scan
        Card {
            id: n.id.clone(),
            kind: self.kind,
            lifecycle: n.lifecycle,
            created_at: self.created_at,
            text: show_text.then_some(self.text),
            summary: self.summary,
            tags: self.tags,
            confidence: n.confidence,
            workspace_id: self.workspace_id,
            props: show_props.then_some(self.props),
            redactions,
            provenance: self.provenance,
            score: None,
            why: self.why,
        }
    }
}
