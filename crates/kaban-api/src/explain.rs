//! `kaban explain <key>` (engine-spec EN-F14; engine-plan E2.4): the explanation of the
//! conflict group a claim belongs to, and `kaban explain <address>` / the `explain` tool
//! (engine-spec EN-F14a; engine-plan E2.5): the one-stop status of any addressable thing, the
//! sentence a person needs, the EN-F14 explanation where one exists, the lineages with their
//! offsets, the policy that applies, the status history and the exact next actions.
//!
//! ```text
//!   ExplainRequest { key }                      ExplainStatusRequest { address }
//!     | 64 hex digits or `Invalid`                | gated as content, then parse_address
//!     v                                           v
//!   claim_group_in(ws, key) -> explain_group    Addressable -> resolve (one status source each)
//!     v                                           | Assertion => claim_group_in + explain_group
//!   ExplainResponse { key, explanation }         | Node => review_rows, conflicts, governance,
//!                                                |         CodeRef anchor against the snapshot
//!                                                | DiscoveredKind => kind_stats
//!                                                | SpanRef => chunk_covering(ws, path, offset),
//!                                                |           the pending span at the offset
//!                                                v
//!                                              StatusExplanation (sentence + next_actions)
//! ```
//!
//! Every sentence comes from the closed template table ([`templates`]); every `next_actions[]`
//! command embeds ids, keys and fixed literals only, never free text from the index, and is
//! rendered by one function ([`command`]) that quotes every argument for a POSIX shell
//! (RW4-1: a group key carries the workspace name, which may hold an apostrophe or a
//! metacharacter).

use crate::api::KabanApi;
use crate::error::ApiError;
use crate::snapshot::{self, SnapshotOutcome};
use crate::types::EventView;
use kaban_codegraph::CodeGraphReader;
use kaban_core::ontology::meta::Acceptance;
use kaban_core::{
    AssertionKey, AssertionStatus, CandidateReason, Explanation, Governance, Lifecycle, Node,
    NodeId, UnresolvedReason,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::borrow::Cow;

/// `explain` input: the claim key as `doctor`'s `unresolved` line and the edge props list it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExplainRequest {
    /// The claim's stable key (64 hex digits, EN-F12a).
    pub key: String,
}

/// `explain` output: the explanation of the claim's group.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExplainResponse {
    /// The claim's key, as asked.
    pub key: String,
    /// The group's explanation (EN-F14).
    pub explanation: Explanation,
}

/// The `explain` tool input (EN-F14a): one address, content first (gated), then grammar.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ExplainStatusRequest {
    /// What to explain: a node id (`UUIDv7`), `e:<edge id>`, a claim key (64 hex digits), a
    /// discovered kind (`x:Org`), or `<path>:<offset>` (a byte offset inside an ingested
    /// document).
    pub address: String,
}

/// Anything the index can explain the status of (EN-F14a).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Addressable {
    /// A node of any kind, by id.
    Node(#[schemars(with = "String")] NodeId),
    /// An edge, by id (`e:<id>`).
    Edge(String),
    /// A claim, by its stable key (64 hex digits).
    Assertion(#[schemars(with = "String")] AssertionKey),
    /// A discovered kind (`x:Org`).
    DiscoveredKind(String),
    /// A byte offset inside an ingested document (`<path>:<offset>`).
    SpanRef {
        /// The document's workspace-relative path.
        path: String,
        /// The byte offset inside the document.
        offset: u64,
    },
}

/// Why an item is pending (EN-F14a; engine-plan E2.5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum PendingStatus {
    /// A reconciliation left the item for review (EN-F12a `Candidate`).
    Candidate(#[schemars(with = "String")] CandidateReason),
    /// A conflict needs an explicit action (EN-F12b/c `Unresolved`).
    Unresolved(#[schemars(with = "String")] UnresolvedReason),
    /// The record's file and database disagree at the same rev (core-spec F13).
    Conflict,
    /// A `CodeRef` whose symbol the resolved snapshot no longer holds (EN-F17 `snapshot_mismatch`).
    Stale,
    /// A record awaiting acceptance (governance `proposed`).
    Proposed,
    /// A discovered `x:` kind awaiting promotion or pruning (EN-F5).
    Discovered {
        /// Active records of the kind in the workspace.
        count: u32,
    },
}

/// A status nothing is pending about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SettledStatus {
    /// Active, nothing pending.
    Active,
    /// Accepted (governance).
    Accepted,
    /// A claim that lost its vote and sits as dissent.
    Dissent,
    /// Rejected, invalidated or retired.
    Rejected,
    /// Replaced by a newer record.
    Superseded,
    /// An applied merge (EN-F51; E2.10 boundary review BR-6): the node reads as its canonical;
    /// the one settled status whose `next_actions` carries an action, the reversal.
    Merged,
}

/// The status of an addressable thing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Status {
    /// Needs a human action; `next_actions` is never empty.
    Pending(PendingStatus),
    /// Terminal; `next_actions` is empty, except for `merged`, which carries the reversal
    /// (`kaban unmerge <id>`).
    Settled(SettledStatus),
}

/// The pending kinds `kaban review --status` accepts (a clap `value_enum` on the CLI).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PendingStatusKind {
    /// [`PendingStatus::Unresolved`].
    Unresolved,
    /// [`PendingStatus::Candidate`].
    Candidate,
    /// [`PendingStatus::Conflict`].
    Conflict,
    /// [`PendingStatus::Stale`].
    Stale,
    /// [`PendingStatus::Proposed`].
    Proposed,
    /// [`PendingStatus::Discovered`].
    Discovered,
}

impl PendingStatus {
    /// The kind of this pending status.
    #[must_use]
    pub fn kind(&self) -> PendingStatusKind {
        match self {
            Self::Candidate(_) => PendingStatusKind::Candidate,
            Self::Unresolved(_) => PendingStatusKind::Unresolved,
            Self::Conflict => PendingStatusKind::Conflict,
            Self::Stale => PendingStatusKind::Stale,
            Self::Proposed => PendingStatusKind::Proposed,
            Self::Discovered { .. } => PendingStatusKind::Discovered,
        }
    }
}

/// One evidence lineage behind a status, by id, with the spans it covers (offsets, never
/// surface text, EN-S9).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct LineageSource {
    /// The lineage id (EN-F12b).
    pub lineage_id: String,
    /// The source document, when the lineage names one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Option<String>")]
    pub document: Option<NodeId>,
    /// `(offset, len)` byte spans inside the chunk.
    pub spans: Vec<(u64, u64)>,
}

/// The policy that applies to the item (EN-F14a: the owner decision, OD-E-3 policy, cap).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PolicyView {
    /// `governed.acceptance` of the kind, when the loaded ontology governs it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Option<String>")]
    pub acceptance: Option<Acceptance>,
    /// The discovered-labels policy name (`review`, `allow`, `deny`), for an `x:` kind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discovered_policy: Option<String>,
    /// The discovered-kind cap of the workspace, for an `x:` kind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cap: Option<u32>,
}

/// One exact next action: what it does and the command that does it (parses with clap).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct NextAction {
    /// What the command does (a template sentence).
    pub label: String,
    /// The command; ids, keys and fixed literals only.
    pub command: String,
}

/// The one document of EN-F14a.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct StatusExplanation {
    /// What was explained.
    pub subject: Addressable,
    /// Its status.
    pub status: Status,
    /// The one sentence a human needs: the status word with its reason.
    pub sentence: String,
    /// The EN-F14 explanation of the deciding group, when the item has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Option<Value>")]
    pub explanation: Option<Explanation>,
    /// The lineages behind the status, with their spans.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lineages: Vec<LineageSource>,
    /// The policy that applies.
    #[serde(default)]
    pub policy: PolicyView,
    /// The status history (`events`), oldest first, every string gated.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub history: Vec<EventView>,
    /// The exact next actions; non-empty for every pending status, empty for a settled one.
    #[serde(default)]
    pub next_actions: Vec<NextAction>,
}

// ---- the address grammar (engine-plan E2.5 `parse_address`) ----

/// Whether `s` is a claim key: 64 lowercase hex digits (EN-F12a).
fn is_claim_key(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Whether `s` is a kind identifier: `^[A-Z][A-Za-z0-9]{0,63}$` (engine-spec §3.1).
fn is_label(s: &str) -> bool {
    let mut bytes = s.bytes();
    matches!(bytes.next(), Some(b) if b.is_ascii_uppercase())
        && s.len() <= 64
        && bytes.all(|b| b.is_ascii_alphanumeric())
}

/// The address grammar, tried in order with no lookup: 64 hex digits is an `Assertion`;
/// `x:Name` (the identifier grammar behind the `x:` prefix) is a `DiscoveredKind`;
/// `<path>:<digits>` where `<path>` contains `/` or `.` is a `SpanRef`; `e:<id>` is an
/// `Edge`; anything else must be a node id (`UUIDv7`). The two collisions resolve by order:
/// `x:1` is no identifier, so it is a path only if it has a `/` or `.`, else `Invalid`; a
/// 64-hex string that would also be a path string is an `Assertion`.
///
/// # Errors
/// `Invalid` when no branch accepts the string.
pub fn parse_address(s: &str) -> Result<Addressable, ApiError> {
    let s = s.trim();
    if is_claim_key(s) {
        return Ok(Addressable::Assertion(AssertionKey(s.to_owned())));
    }
    if let Some(local) = s.strip_prefix("x:")
        && is_label(local)
    {
        return Ok(Addressable::DiscoveredKind(s.to_owned()));
    }
    if let Some((path, digits)) = s.rsplit_once(':')
        && !path.is_empty()
        && !digits.is_empty()
        && digits.bytes().all(|b| b.is_ascii_digit())
        && (path.contains('/') || path.contains('.'))
    {
        let offset = digits
            .parse::<u64>()
            .map_err(|_| ApiError::Invalid("address: offset out of range".into()))?;
        return Ok(Addressable::SpanRef {
            path: path.to_owned(),
            offset,
        });
    }
    if let Some(id) = s.strip_prefix("e:")
        && NodeId::parse(id).is_ok()
    {
        return Ok(Addressable::Edge(id.to_owned()));
    }
    NodeId::parse(s).map(Addressable::Node).map_err(|_| {
        ApiError::Invalid(
            "address: not a node id, edge id, claim key, x:Kind or path:offset".into(),
        )
    })
}

// ---- the template table ----

/// The sentence templates, one per status variant, the status word first and its reason
/// adjacent (EN-F14a: "no status is ever shown without its reason"). Exposed for the
/// `no_probability_words` grep (OWN-81 (5)); `{...}` holes take ids, keys, kind names and
/// offsets only.
#[doc(hidden)]
#[must_use]
pub fn templates() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            "candidate",
            "candidate: {reason}, the vote on {subject} did not clear its gate",
        ),
        (
            "unresolved",
            "unresolved: {reason}, two claims disagree on {subject}",
        ),
        (
            "conflict",
            "conflict: the record's file and database disagree at the same rev",
        ),
        (
            "stale",
            "stale: the resolved snapshot no longer holds the symbol this CodeRef anchors",
        ),
        ("proposed", "proposed: the record awaits a human acceptance"),
        (
            "discovered",
            "discovered: {kind} is an undeclared kind with {count} record(s) awaiting promotion or pruning",
        ),
        ("active", "active: nothing pending"),
        ("accepted", "accepted: nothing pending"),
        // an applied merge (EN-F51; E2.10 boundary review BR-6): the pair as node ids, the
        // reversal as a runnable `kaban unmerge <id>`; rendered once the store exposes the
        // redirect of a node to the service (E2.11 report, residual)
        (
            "merged",
            "merged: {subject} reads as {canonical}; a human reverses it with kaban unmerge {subject}",
        ),
        (
            "dissent",
            "dissent: the claim lost its vote, nothing pending",
        ),
        ("rejected", "rejected: nothing pending"),
        ("superseded", "superseded: nothing pending"),
        // the next-action labels
        ("accept", "accept the record as a person"),
        (
            "deprecate",
            "deprecate the losing claim so the group is re-decided without it",
        ),
        ("forget", "retire the node"),
        (
            "unmerge",
            "reverse the merge as a person; both nodes read as themselves again",
        ),
        ("promote", "promote the discovered kind to a declared kind"),
        ("prune", "list the prunable discovered kinds"),
        (
            "resolve_db",
            "keep the database record and rewrite the file",
        ),
        ("resolve_file", "import the file over the database record"),
        (
            "resolve_winner",
            "name the winning value as a person; the group is decided and leaves review",
        ),
        (
            "resolve_symbol",
            "re-target the anchor to the key the symbol has now, as a person; the CodeRef leaves review",
        ),
    ]
}

fn template(name: &str) -> &'static str {
    templates()
        .into_iter()
        .find(|(n, _)| *n == name)
        .map_or("", |(_, t)| t)
}

/// A rendered template: every `{hole}` replaced in order.
fn render(name: &str, holes: &[(&str, &str)]) -> String {
    let mut out = template(name).to_owned();
    for (hole, value) in holes {
        out = out.replace(&format!("{{{hole}}}"), value);
    }
    out
}

fn action(label: &str, command: String) -> NextAction {
    NextAction {
        label: template(label).to_owned(),
        command,
    }
}

/// Whether `s` is a word a POSIX shell hands over unchanged: a node id, a claim key, a kind
/// key (`infra:Team`, `x:Widget`), an option name or a fixed literal of the command table.
fn is_bare_word(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b':' | b'_' | b'-' | b'.'))
}

/// One argument as a POSIX shell reads it back whole (RW4-1): a bare word as is, anything
/// else inside single quotes with every apostrophe written `'\''`, so a canonical group key
/// (it carries the workspace name), a glob such as `**` or a `<key>` placeholder is one word
/// to the shell whatever bytes it holds.
fn shell_arg(s: &str) -> Cow<'_, str> {
    if is_bare_word(s) {
        Cow::Borrowed(s)
    } else {
        Cow::Owned(format!("'{}'", s.replace('\'', "'\\''")))
    }
}

/// The one renderer of every `next_actions[].command`: `argv` joined by single spaces, each
/// argument through [`shell_arg`].
fn command(argv: &[&str]) -> String {
    argv.iter()
        .map(|a| shell_arg(a))
        .collect::<Vec<_>>()
        .join(" ")
}

/// The `span:<offset>:<len>` form of a group key's subject, when it is a span.
fn span_of(group_key: &str) -> Option<(u64, u64)> {
    let v: Value = serde_json::from_str(group_key).ok()?;
    let span = v.get(2)?.get("span")?;
    Some((span.get("offset")?.as_u64()?, span.get("len")?.as_u64()?))
}

/// The subject phrase of a group: `span <offset>:<len>` or `the group`.
fn subject_phrase(group_key: &str) -> String {
    span_of(group_key).map_or_else(
        || "the group".to_owned(),
        |(offset, len)| format!("span {offset}:{len}"),
    )
}

/// The lineages of an explanation with the group's span, leader first.
fn lineages_of(x: &Explanation) -> Vec<LineageSource> {
    let spans = span_of(&x.group_key).map(|s| vec![s]).unwrap_or_default();
    x.candidates
        .iter()
        .flat_map(|c| c.lineages.iter())
        .map(|l| LineageSource {
            lineage_id: l.lineage_id.clone(),
            document: None,
            spans: spans.clone(),
        })
        .collect()
}

/// The leading candidate's value when it is a plain identifier (a kind name such as
/// `infra:Team`): the `<value>` of `kaban resolve <id> --winner <value>`. A value with
/// spaces, quotes or structure is never embedded in a command (E2.5 negative: ids and keys
/// only).
fn winner_value(x: &Explanation) -> Option<String> {
    let v = x.candidates.first()?.value.as_str()?;
    (!v.is_empty()
        && v.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b':' | b'_' | b'-' | b'.')))
    .then(|| v.to_owned())
}

/// The runner-up's strongest supporter key (the claim a human deprecates to settle a
/// `candidate` or `unresolved` group), when the group has one.
fn runner_up_key(x: &Explanation) -> Option<&AssertionKey> {
    x.candidates
        .get(1)
        .and_then(|c| c.lineages.first())
        .map(|l| &l.best.assertion_key)
}

/// The candidate reason an explanation implies (the store keeps the disposition, not the
/// reason): below the margin gate, else below the kind's minimum confidence.
fn candidate_reason(x: &Explanation, row_reason: Option<&str>) -> CandidateReason {
    if let Some(r) = row_reason.and_then(CandidateReason::parse)
        && r != CandidateReason::BelowMargin
    {
        return r;
    }
    if x.margin < x.threshold {
        CandidateReason::BelowMargin
    } else {
        CandidateReason::BelowMinConfidence
    }
}

/// Which of a node's outstanding groups a status renders (a node may hold several: two
/// pending spans of one chunk, a discovered record with a pending vote).
#[derive(Debug, Clone, Copy)]
pub(crate) enum GroupPick<'a> {
    /// The node's first row in review order, the `discovered_kind` admission row first
    /// (`kaban explain <id>`).
    First,
    /// The named group (`kaban review`: one item per group).
    Group(&'a str),
    /// The group whose span covers this chunk-relative byte offset (`<path>:<offset>`), else
    /// the first row.
    SpanAt(u64),
}

impl GroupPick<'_> {
    /// The review row this pick selects among `rows` (the node's rows in review order).
    fn choose(self, rows: &[kaban_core::ReviewRow]) -> Option<&kaban_core::ReviewRow> {
        let first = || {
            rows.iter()
                .find(|r| r.reason == "discovered_kind")
                .or_else(|| rows.first())
        };
        match self {
            Self::First => first(),
            Self::Group(key) => rows.iter().find(|r| r.group_key == key),
            Self::SpanAt(offset) => rows
                .iter()
                .find(|r| {
                    span_of(&r.group_key).is_some_and(|(start, len)| {
                        start <= offset && offset < start.saturating_add(len)
                    })
                })
                .or_else(first),
        }
    }
}

impl KabanApi {
    /// The explanation of the conflict group the claim `key` belongs to (EN-F14).
    ///
    /// # Errors
    /// `Invalid` for a key that is not 64 hex digits, `NotFound` for an unknown key or a key
    /// of another workspace (core-spec S6: the two are the same output), store errors.
    #[allow(clippy::needless_pass_by_value)] // core-plan §6 interface: requests are taken by value
    pub fn explain(&self, p: ExplainRequest) -> Result<ExplainResponse, ApiError> {
        // the key is control: a claim key is 64 lowercase hex digits (EN-F12a) and names no
        // content; anything else is refused before the store is asked
        if p.key.len() != 64 || !p.key.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(ApiError::Invalid("key must be 64 hex digits".into()));
        }
        let group_key = self.store.claim_group_in(self.workspace_str(), &p.key)?;
        let explanation = self.store.explain_group(&group_key)?;
        Ok(ExplainResponse {
            key: p.key,
            explanation,
        })
    }

    /// The one-stop status explanation of `address` (EN-F14a; the `explain` tool and `kaban
    /// explain <address>`): the address is gated as content first, then parsed.
    ///
    /// # Errors
    /// `Rejected` when the address hits a reject rule, `Invalid` when no grammar branch
    /// accepts it, `NotFound` when it names nothing in the bound workspace, store errors.
    #[allow(clippy::needless_pass_by_value)] // core-plan §6 interface: requests are taken by value
    pub fn explain_status(&self, p: ExplainStatusRequest) -> Result<StatusExplanation, ApiError> {
        let t = "explain";
        let mut ctx = self.write_ctx(None);
        let mut red = Vec::new();
        let gated = self.gate(&mut ctx, t, "address", &p.address, &mut red)?;
        self.flush_audit(&mut ctx)?;
        let subject = parse_address(&gated)?;
        self.status_of(&subject)
    }

    /// The status document of one parsed address.
    pub(crate) fn status_of(&self, subject: &Addressable) -> Result<StatusExplanation, ApiError> {
        match subject {
            Addressable::Assertion(key) => self.assertion_status(key),
            Addressable::Node(id) => self.node_status(id),
            Addressable::DiscoveredKind(label) => self.discovered_status(label),
            Addressable::SpanRef { path, offset } => self.span_status(path, *offset),
            // the edge read seam is not granted in E2.5b (see the lane report): an edge
            // address names nothing yet
            Addressable::Edge(id) => Err(ApiError::NotFound(format!("e:{id}"))),
        }
    }

    fn workspace_str(&self) -> &str {
        self.config.workspace.as_str()
    }

    /// A claim: its group's verdict decides. The lookup is bound to the workspace (S6): a key
    /// of another workspace is `NotFound` like an unknown one, and the group key the store
    /// returns names this workspace, so nothing foreign reaches the rendering.
    fn assertion_status(&self, key: &AssertionKey) -> Result<StatusExplanation, ApiError> {
        let group_key = self.store.claim_group_in(self.workspace_str(), &key.0)?;
        let x = self.store.explain_group(&group_key)?;
        let subject = Addressable::Assertion(key.clone());
        let phrase = subject_phrase(&group_key);
        let (status, sentence, next_actions) = match x.verdict {
            AssertionStatus::Candidate => {
                let reason = candidate_reason(&x, None);
                (
                    Status::Pending(PendingStatus::Candidate(reason)),
                    render(
                        "candidate",
                        &[("reason", reason.as_str()), ("subject", &phrase)],
                    ),
                    Self::group_actions(&x, None),
                )
            }
            AssertionStatus::Unresolved => {
                let reason = x
                    .unresolved_reason
                    .unwrap_or(UnresolvedReason::PreferredConflict);
                (
                    Status::Pending(PendingStatus::Unresolved(reason)),
                    render(
                        "unresolved",
                        &[("reason", reason.as_str()), ("subject", &phrase)],
                    ),
                    Self::group_actions(&x, None),
                )
            }
            AssertionStatus::Rejected => (
                Status::Settled(SettledStatus::Rejected),
                render("rejected", &[]),
                vec![],
            ),
            AssertionStatus::Superseded => (
                Status::Settled(SettledStatus::Superseded),
                render("superseded", &[]),
                vec![],
            ),
            // a winner group: the asked claim is the winner or one of its supporters
            // (`active`), or a dissenting one (`dissent`)
            _ => {
                let supports_leader = x
                    .candidates
                    .first()
                    .is_some_and(|c| c.lineages.iter().any(|l| l.best.assertion_key == *key));
                if supports_leader {
                    (
                        Status::Settled(SettledStatus::Active),
                        render("active", &[]),
                        vec![],
                    )
                } else {
                    (
                        Status::Settled(SettledStatus::Dissent),
                        render("dissent", &[]),
                        vec![],
                    )
                }
            }
        };
        Ok(StatusExplanation {
            subject,
            status,
            sentence,
            lineages: lineages_of(&x),
            explanation: Some(x),
            policy: PolicyView::default(),
            history: vec![],
            next_actions,
        })
    }

    /// The actions that settle a candidate or unresolved group: deprecate the runner-up's
    /// claim (the group is re-decided without it), then retire the subject node.
    fn group_actions(x: &Explanation, node: Option<&NodeId>) -> Vec<NextAction> {
        let mut out = Vec::new();
        if let Some(key) = runner_up_key(x) {
            out.push(action(
                "deprecate",
                command(&["kaban", "explain", &key.0, "--deprecate", "reviewed"]),
            ));
        }
        if let Some(id) = node {
            out.push(action("forget", command(&["kaban", "forget", id.as_str()])));
        }
        if out.is_empty()
            && let Some(key) = x
                .candidates
                .first()
                .and_then(|c| c.lineages.first())
                .map(|l| &l.best.assertion_key)
        {
            out.push(action(
                "deprecate",
                command(&["kaban", "explain", &key.0, "--deprecate", "reviewed"]),
            ));
        }
        out
    }

    /// A node: review rows first (candidate, unresolved, discovered), then the sync
    /// conflict, then a stale `CodeRef` anchor, then governance.
    fn node_status(&self, id: &NodeId) -> Result<StatusExplanation, ApiError> {
        self.node_status_in(id, GroupPick::First)
    }

    /// [`Self::node_status`] rendering the outstanding group `pick` names (`review` lists a
    /// node with two outstanding groups twice, one item per group: the wave-2 review's
    /// RW2-4); a group the node no longer holds falls through to the node's own status.
    pub(crate) fn node_status_in(
        &self,
        id: &NodeId,
        pick: GroupPick<'_>,
    ) -> Result<StatusExplanation, ApiError> {
        let ws = self.workspace_str().to_owned();
        let node = self
            .store
            .get(id, true)?
            .filter(|n| n.workspace_id == ws)
            .ok_or_else(|| ApiError::NotFound(id.to_string()))?;
        let subject = Addressable::Node(id.clone());
        let history: Vec<EventView> = self
            .store
            .events(id)?
            .iter()
            .filter(|e| e.workspace_id == ws)
            .filter_map(|e| self.project_event(e))
            .collect();
        let mut policy = PolicyView {
            acceptance: self.governed().acceptance(&node.kind),
            ..PolicyView::default()
        };
        let rows = self.store.review_rows(&ws, Some(id))?;
        let chosen = pick.choose(&rows);
        if let Some(row) = chosen.filter(|r| r.reason == "discovered_kind") {
            let knobs = self.registry().discovered(self.engine());
            policy.discovered_policy = Some(policy_name(knobs.rules.policy).to_owned());
            policy.cap = Some(knobs.rules.cap);
            let reason = CandidateReason::DiscoveredKind;
            return Ok(StatusExplanation {
                subject,
                status: Status::Pending(PendingStatus::Candidate(reason)),
                sentence: render(
                    "candidate",
                    &[("reason", reason.as_str()), ("subject", &node.kind)],
                ),
                explanation: None,
                lineages: vec![],
                policy,
                history,
                next_actions: vec![
                    action(
                        "accept",
                        command(&[
                            "kaban",
                            "accept",
                            id.as_str(),
                            "--reason",
                            "reviewed",
                            "--path-scope",
                            "**",
                        ]),
                    ),
                    action(
                        "promote",
                        command(&["kaban", "ontology", "promote", &row.group_key, "Entity"]),
                    ),
                    action("forget", command(&["kaban", "forget", id.as_str()])),
                ],
            });
        }
        // a group a person decided (a human winner with machine claims of another value
        // parked `unresolved`, EN-F12c) is settled whatever its rows' dispositions say: the
        // live verdict decides, so no such row renders a `preferred_conflict` here
        let chosen = chosen
            .map(|row| self.store.explain_group(&row.group_key).map(|x| (row, x)))
            .transpose()?
            .filter(|(_, x)| !matches!(x.verdict, AssertionStatus::Winner));
        if let Some((row, x)) = chosen {
            let phrase = subject_phrase(&row.group_key);
            let (status, sentence) = if row.disposition == "unresolved" {
                let reason = x
                    .unresolved_reason
                    .unwrap_or(UnresolvedReason::PreferredConflict);
                (
                    Status::Pending(PendingStatus::Unresolved(reason)),
                    render(
                        "unresolved",
                        &[("reason", reason.as_str()), ("subject", &phrase)],
                    ),
                )
            } else {
                let reason = candidate_reason(&x, Some(&row.reason));
                (
                    Status::Pending(PendingStatus::Candidate(reason)),
                    render(
                        "candidate",
                        &[("reason", reason.as_str()), ("subject", &phrase)],
                    ),
                )
            };
            let mut next_actions = Self::group_actions(&x, Some(id));
            // a candidate group's first action names its leading value as the winner
            // (engine-plan E2.5 `kaban resolve <id> --winner <value>`: ids and keys only,
            // so a value that is not a plain identifier keeps the deprecate action first)
            // and always names its group (RW3-3: a node may hold several candidate groups,
            // one item each, and the action must decide this one); the key is canonical
            // JSON, quoted for the shell by `command` (RW4-1)
            if row.disposition != "unresolved"
                && let Some(value) = winner_value(&x)
            {
                next_actions.insert(
                    0,
                    action(
                        "resolve_winner",
                        command(&[
                            "kaban",
                            "resolve",
                            id.as_str(),
                            "--winner",
                            &value,
                            "--group",
                            &row.group_key,
                        ]),
                    ),
                );
            }
            return Ok(StatusExplanation {
                subject,
                status,
                sentence,
                lineages: lineages_of(&x),
                explanation: Some(x),
                policy,
                history,
                next_actions,
            });
        }
        if self
            .store
            .conflicts_for(std::slice::from_ref(id))?
            .iter()
            .any(|c| c.node_id == node.id)
        {
            return Ok(StatusExplanation {
                subject,
                status: Status::Pending(PendingStatus::Conflict),
                sentence: render("conflict", &[]),
                explanation: None,
                lineages: vec![],
                policy,
                history,
                next_actions: vec![
                    action(
                        "resolve_db",
                        command(&["kaban", "resolve", id.as_str(), "--take", "db"]),
                    ),
                    action(
                        "resolve_file",
                        command(&["kaban", "resolve", id.as_str(), "--take", "file"]),
                    ),
                ],
            });
        }
        if self.anchor_is_stale(&node) {
            return Ok(StatusExplanation {
                subject,
                status: Status::Pending(PendingStatus::Stale),
                sentence: render("stale", &[]),
                explanation: None,
                lineages: vec![],
                policy,
                history,
                // SO-6: the re-target first (`<key>` is the person's to supply: the snapshot
                // no longer holds the old one and nothing here guesses the new), `forget`
                // second
                next_actions: vec![
                    action(
                        "resolve_symbol",
                        command(&["kaban", "resolve", id.as_str(), "--symbol", "<key>"]),
                    ),
                    action("forget", command(&["kaban", "forget", id.as_str()])),
                ],
            });
        }
        // an applied merge (EN-F51 (5); BR-6): the store's redirect decides before the
        // lifecycle does; the pair as node ids, the reversal as the one runnable action
        if let Some(canonical) = self.store.canonical_of(&ws, id)? {
            return Ok(StatusExplanation {
                subject,
                status: Status::Settled(SettledStatus::Merged),
                sentence: render(
                    "merged",
                    &[("subject", id.as_str()), ("canonical", canonical.as_str())],
                ),
                explanation: None,
                lineages: vec![],
                policy,
                history,
                next_actions: vec![action("unmerge", format!("kaban unmerge {id}"))],
            });
        }
        let (status, sentence, next_actions) = match (node.lifecycle, node.governance) {
            (Lifecycle::Tombstoned | Lifecycle::Purged, _) | (_, Governance::Invalidated) => (
                Status::Settled(SettledStatus::Rejected),
                render("rejected", &[]),
                vec![],
            ),
            (_, Governance::Superseded) => (
                Status::Settled(SettledStatus::Superseded),
                render("superseded", &[]),
                vec![],
            ),
            (_, Governance::Accepted) => (
                Status::Settled(SettledStatus::Accepted),
                render("accepted", &[]),
                vec![],
            ),
            (_, Governance::Proposed) => (
                Status::Pending(PendingStatus::Proposed),
                render("proposed", &[]),
                vec![
                    action(
                        "accept",
                        command(&[
                            "kaban",
                            "accept",
                            id.as_str(),
                            "--reason",
                            "reviewed",
                            "--path-scope",
                            "**",
                        ]),
                    ),
                    action("forget", command(&["kaban", "forget", id.as_str()])),
                ],
            ),
            // `Governance::None` and (both enums are `non_exhaustive`) any later variant
            _ => (
                Status::Settled(SettledStatus::Active),
                render("active", &[]),
                vec![],
            ),
        };
        Ok(StatusExplanation {
            subject,
            status,
            sentence,
            explanation: None,
            lineages: vec![],
            policy,
            history,
            next_actions,
        })
    }

    /// Whether `node` is a `CodeRef` whose `symbol` anchor the resolved snapshot lacks
    /// (`why.rs`'s `snapshot_mismatch` leg): only a resolved snapshot can say so.
    fn anchor_is_stale(&self, node: &Node) -> bool {
        if node.kind != "CodeRef" {
            return false;
        }
        let Some(key) = node.props.get("symbol").and_then(Value::as_str) else {
            return false;
        };
        match snapshot::resolve_for_card(self) {
            SnapshotOutcome::Resolved(r) => matches!(r.snapshot.symbol_unique(key), Ok(None)),
            SnapshotOutcome::Absent | SnapshotOutcome::TooNew => false,
        }
    }

    /// The active records of the discovered kind `label` (`x:Org`) in `ws`, from `kind_stats`.
    fn discovered_count(&self, ws: &str, label: &str) -> Result<Option<u32>, ApiError> {
        let local = label.strip_prefix("x:").unwrap_or(label);
        Ok(self
            .store
            .kind_stats(&kaban_core::WorkspaceId::new(ws))?
            .into_iter()
            .find(|k| k.ns == "x" && k.kind == local)
            .map(|k| u32::try_from(k.n_nodes).unwrap_or(u32::MAX)))
    }

    /// A discovered kind: its `kind_stats` row.
    fn discovered_status(&self, label: &str) -> Result<StatusExplanation, ApiError> {
        let ws = self.workspace_str().to_owned();
        let count = self
            .discovered_count(&ws, label)?
            .ok_or_else(|| ApiError::NotFound(label.to_owned()))?;
        let knobs = self.registry().discovered(self.engine());
        Ok(StatusExplanation {
            subject: Addressable::DiscoveredKind(label.to_owned()),
            status: Status::Pending(PendingStatus::Discovered { count }),
            sentence: render(
                "discovered",
                &[("kind", label), ("count", &count.to_string())],
            ),
            explanation: None,
            lineages: vec![],
            policy: PolicyView {
                acceptance: None,
                discovered_policy: Some(policy_name(knobs.rules.policy).to_owned()),
                cap: Some(knobs.rules.cap),
            },
            history: vec![],
            next_actions: vec![
                action(
                    "promote",
                    command(&["kaban", "ontology", "promote", label, "Entity"]),
                ),
                action(
                    "prune",
                    command(&["kaban", "ontology", "prune", "--dry-run"]),
                ),
            ],
        })
    }

    /// `<path>:<offset>`: the ingested `Chunk` of `path` whose byte range covers `offset`
    /// (`props.path`, `props.byte_start`, `props.byte_end`), explained as a node with the
    /// pending span at that offset first (two pending spans of one chunk are two statuses).
    /// One bounded store lookup selects the chunk by path (RW2-6: the other documents' chunks
    /// are never read); the offset is translated into the chunk's own byte space.
    fn span_status(&self, path: &str, offset: u64) -> Result<StatusExplanation, ApiError> {
        let ws = self.workspace_str().to_owned();
        let chunk = self
            .store
            .chunk_covering(&ws, path, offset)?
            .ok_or_else(|| ApiError::NotFound(format!("{path}:{offset}")))?;
        let start = chunk
            .props
            .get("byte_start")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let id = Self::node_id(&chunk.id)?;
        let mut status =
            self.node_status_in(&id, GroupPick::SpanAt(offset.saturating_sub(start)))?;
        status.subject = Addressable::SpanRef {
            path: path.to_owned(),
            offset,
        };
        Ok(status)
    }
}

fn policy_name(p: kaban_core::ontology::meta::DiscoveredPolicyName) -> &'static str {
    use kaban_core::ontology::meta::DiscoveredPolicyName as P;
    match p {
        P::Deny => "deny",
        P::Review => "review",
        P::Allow => "allow",
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion
mod tests {
    use super::*;

    const KEY: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const ID: &str = "01a17a03-5344-7000-8000-00000000000a";

    /// The grammar, every branch and the two collisions (`x:Org` against a path `x:1`; a
    /// 64-hex that is also a valid path string is an `Assertion`).
    #[test]
    fn parse_address_table() {
        let edge = format!("e:{ID}");
        let cases: Vec<(&str, Option<Addressable>)> = vec![
            (KEY, Some(Addressable::Assertion(AssertionKey(KEY.into())))),
            ("x:Org", Some(Addressable::DiscoveredKind("x:Org".into()))),
            (
                "src/a.rs:12",
                Some(Addressable::SpanRef {
                    path: "src/a.rs".into(),
                    offset: 12,
                }),
            ),
            (
                "notes.md:0",
                Some(Addressable::SpanRef {
                    path: "notes.md".into(),
                    offset: 0,
                }),
            ),
            (edge.as_str(), Some(Addressable::Edge(ID.into()))),
            (ID, Some(Addressable::Node(NodeId::parse(ID).unwrap()))),
            // collisions: `x:1` is no identifier and no path; `x:Org` is never a span
            ("x:1", None),
            ("x:org", None),
            ("", None),
            ("src/a.rs:", None),
            ("e:not-an-id", None),
            ("plain", None),
        ];
        for (input, want) in cases {
            let got = parse_address(input).ok();
            assert_eq!(got, want, "{input:?}");
        }
    }

    #[test]
    fn every_template_names_its_status_word_first_with_the_reason_adjacent() {
        for (name, text) in templates() {
            assert!(
                text.starts_with(name) || !text.contains(':'),
                "{name}: {text}"
            );
            if matches!(name, "candidate" | "unresolved") {
                assert!(text.contains("{reason}"), "{name}: {text}");
            }
        }
    }
}
