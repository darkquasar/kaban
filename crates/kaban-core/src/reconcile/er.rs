//! Entity resolution (engine-spec EN-F51; engine-plan E2.10): deterministic blocking and a
//! Fellegi-Sunter score in signed fixed point, pure. Nothing here reads a store: the store
//! builds a [`NodeView`] per node, asks [`blocks`] which pairs to compare, [`score_pair`] for
//! each, [`decide`] for the verdict and [`raw_score`] for the assertion's confidence, and
//! writes what EN-F51 (3)-(5) say. Every number is a [`LogWeight`] or a [`Fixed`]; the
//! floating `[er]` keys cross the seam once, in [`ErPriors::from_config`] (EN-F12a).
//!
//! ```text
//!   [NodeView]  ->  blocks(nodes, fuzzy)  ->  Block{key, members}  ->  pairs()
//!                                                                          |
//!   ErPriors{m, u, thresholds}  ->  score_pair(a, b, p)  ->  ErScore{w, contributions}
//!                                                                          |
//!                                        decide(score, p) -> Match | Possible | None
//!                                        raw_score(score, p) -> Fixed (clamp W / match)
//! ```

use super::{Fixed, FixedError, LogWeight};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

vocabulary!(
    Field,
    "The compared fields of EN-F51 (2); the `[er] m.<field>` / `u.<field>` keys.",
    {
        Title => "title",
        Aliases => "aliases",
        Kind => "kind",
        Tags => "tags",
        PathScope => "path_scope",
        Anchors => "anchors",
        Summary => "summary",
    }
);

vocabulary!(
    Agreement,
    "The agreement level of one field between two nodes.",
    {
        Agree => "agree",
        Disagree => "disagree",
        Missing => "missing",
    }
);

/// What the scorer sees of a node: the ER fields only, every string already in its
/// compared form. `title` is the normalised first line of the text (the `kaban-policy`
/// `norm_term` view, built by the store, which owns that dependency); `anchors` are the
/// `node_anchor` keys as `<anchor_kind>:<key>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeView {
    /// The node id (never a row id in a view that crosses the seam).
    pub id: String,
    /// Workspace: blocks never span two.
    pub workspace_id: String,
    /// Stored kind name.
    pub kind: String,
    /// Normalised title.
    pub title: String,
    /// `props.aliases[]`.
    #[serde(default)]
    pub aliases: Vec<String>,
    /// Free-form tags.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Governing globs.
    #[serde(default)]
    pub path_scope: Vec<String>,
    /// Anchor keys.
    #[serde(default)]
    pub anchors: Vec<String>,
    /// One-line summary.
    #[serde(default)]
    pub summary: Option<String>,
    /// Near-duplicate key (F1).
    #[serde(default)]
    pub dup_key: Option<String>,
}

/// The frozen `[er]` keys (engine-spec §15.1): `m`/`u` per field and the two thresholds,
/// built once from `EngineConfig.er` at the consuming seam.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErPriors {
    /// `P(agree | same entity)` per field.
    pub m: BTreeMap<Field, Fixed>,
    /// `P(agree | different entities)` per field.
    pub u: BTreeMap<Field, Fixed>,
    /// `W >= match_threshold` is a `Match`.
    pub match_threshold: LogWeight,
    /// `possible_threshold <= W < match_threshold` is a `Possible`.
    pub possible_threshold: LogWeight,
}

/// The two prior tables of the `[er]` section, closed so an error can name the table it found
/// an unknown key in without carrying the key (RW4-5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErTable {
    /// `[er.m]`: `P(agree | same entity)`.
    M,
    /// `[er.u]`: `P(agree | different entities)`.
    U,
}

impl ErTable {
    /// The section name, `m` or `u`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::M => "m",
            Self::U => "u",
        }
    }

    /// The path an unknown key of this table reports, `m.<unknown>` or `u.<unknown>`: the key
    /// itself is the caller's bytes and never crosses the error boundary.
    #[must_use]
    pub const fn unknown_path(self) -> &'static str {
        match self {
            Self::M => "m.<unknown>",
            Self::U => "u.<unknown>",
        }
    }
}

/// Why an `[er]` section does not freeze (the exit-4 class of engine-plan §4: a closed-set
/// key that does not parse, a probability outside `(0, 1)`, a threshold that is not finite
/// or not positive). Carries the table and a known field's path, never a value and never an
/// unknown key (RW4-5: the key is the caller's bytes, out of `Display` and `Debug` alike).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ErConfigError {
    /// A key of `m` or `u` outside [`Field`]; the path is [`ErTable::unknown_path`].
    #[error("er_unknown_field")]
    UnknownField {
        /// The table the unknown key was found in.
        table: ErTable,
    },
    /// An `m` or `u` not strictly inside `(0, 1)` (a zero endpoint is infinite evidence).
    #[error("er_probability_out_of_range")]
    OutOfRange {
        /// `m.<field>` or `u.<field>`.
        path: String,
    },
    /// A threshold that does not convert, or a non-positive `match_threshold`.
    #[error("er_threshold_invalid")]
    Threshold {
        /// `match_threshold` or `possible_threshold`.
        path: String,
    },
}

impl ErPriors {
    /// Freeze the `[er]` section (engine-plan :389: the typed form is built here, once).
    /// # Errors
    /// [`ErConfigError`] for an unknown field, a probability outside `(0, 1)` or a bad
    /// threshold; nothing is clamped.
    pub fn from_config(cfg: &crate::config::Er) -> Result<Self, ErConfigError> {
        let table = |name: ErTable, map: &BTreeMap<String, f64>| {
            let mut out = BTreeMap::new();
            for (key, value) in map {
                // the path names the parsed field, never the caller's key (RW4-5)
                let field = Field::parse(key).ok_or(ErConfigError::UnknownField { table: name })?;
                let path = format!("{}.{}", name.as_str(), field.as_str());
                let fixed = Fixed::from_f64(*value)
                    .map_err(|_| ErConfigError::OutOfRange { path: path.clone() })?;
                if fixed == Fixed::ZERO || fixed == Fixed::ONE {
                    return Err(ErConfigError::OutOfRange { path });
                }
                out.insert(field, fixed);
            }
            Ok(out)
        };
        let threshold = |name: &str, value: f64| {
            LogWeight::from_f64(value).map_err(|_| ErConfigError::Threshold {
                path: name.to_owned(),
            })
        };
        let match_threshold = threshold("match_threshold", cfg.match_threshold)?;
        if match_threshold.0 <= 0 {
            return Err(ErConfigError::Threshold {
                path: "match_threshold".to_owned(),
            });
        }
        Ok(Self {
            m: table(ErTable::M, &cfg.m)?,
            u: table(ErTable::U, &cfg.u)?,
            match_threshold,
            possible_threshold: threshold("possible_threshold", cfg.possible_threshold)?,
        })
    }

    /// The §15.1 defaults (`config::Er::default()`), which freeze by construction.
    #[allow(clippy::expect_used)] // documented invariant: the shipped defaults are in range
    pub fn defaults() -> Self {
        Self::from_config(&crate::config::Er::default()).expect("the [er] defaults freeze")
    }

    /// The signed weight of one field at one level: `log2(m/u)` for agreement,
    /// `log2((1-m)/(1-u))` for disagreement, zero for a missing field or an unconfigured one.
    pub fn weight(&self, field: Field, level: Agreement) -> LogWeight {
        let (Some(m), Some(u)) = (self.m.get(&field), self.u.get(&field)) else {
            return LogWeight(0);
        };
        match level {
            Agreement::Agree => LogWeight::log2_ratio(*m, *u),
            Agreement::Disagree => LogWeight::log2_ratio(Fixed(10_000 - m.0), Fixed(10_000 - u.0)),
            Agreement::Missing => LogWeight(0),
        }
    }
}

/// One field's signed contribution to a pair's score (stored on the identity assertion as
/// `{field, level, w}`, EN-F51 (4)).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErContribution {
    /// The compared field.
    pub field: Field,
    /// The level observed.
    pub level: Agreement,
    /// The field's weight at that level.
    pub w: LogWeight,
}

/// A pair's Fellegi-Sunter score: the total and the waterfall behind it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErScore {
    /// `W = sum w_f`, accumulated in `i64` and carried as a `LogWeight` (seven bounded
    /// terms never leave `i32`).
    pub w: LogWeight,
    /// One entry per [`Field::ALL`], in that order.
    pub contributions: Vec<ErContribution>,
}

/// The verdict on a pair (EN-F51 (3)).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErDecision {
    /// `W >= match_threshold`: an identity row is written.
    Match,
    /// `possible_threshold <= W < match_threshold`: a `merge` candidate for review.
    Possible,
    /// Below `possible_threshold`: nothing is written.
    None,
}

fn level_of_sets(a: &[String], b: &[String]) -> Agreement {
    if a.is_empty() || b.is_empty() {
        return Agreement::Missing;
    }
    if a.iter().any(|x| b.contains(x)) {
        Agreement::Agree
    } else {
        Agreement::Disagree
    }
}

fn level_of_text(a: &str, b: &str) -> Agreement {
    if a.is_empty() || b.is_empty() {
        Agreement::Missing
    } else if a == b {
        Agreement::Agree
    } else {
        Agreement::Disagree
    }
}

/// The agreement level of one field between two views: equal non-empty text agrees, any
/// shared element of two non-empty lists agrees, an empty side is `Missing`.
pub fn agreement(field: Field, a: &NodeView, b: &NodeView) -> Agreement {
    match field {
        Field::Title => level_of_text(&a.title, &b.title),
        Field::Kind => level_of_text(&a.kind, &b.kind),
        Field::Summary => level_of_text(
            a.summary.as_deref().unwrap_or(""),
            b.summary.as_deref().unwrap_or(""),
        ),
        Field::Aliases => level_of_sets(&a.aliases, &b.aliases),
        Field::Tags => level_of_sets(&a.tags, &b.tags),
        Field::PathScope => level_of_sets(&a.path_scope, &b.path_scope),
        Field::Anchors => level_of_sets(&a.anchors, &b.anchors),
    }
}

/// Score one pair: the per-field weights of [`ErPriors::weight`] summed in `i64`.
pub fn score_pair(a: &NodeView, b: &NodeView, p: &ErPriors) -> ErScore {
    let mut sum = 0_i64;
    let contributions = Field::ALL
        .iter()
        .map(|&field| {
            let level = agreement(field, a, b);
            let w = p.weight(field, level);
            sum += i64::from(w.0);
            ErContribution { field, level, w }
        })
        .collect();
    let w = LogWeight(i32::try_from(sum).unwrap_or(if sum < 0 { i32::MIN } else { i32::MAX }));
    ErScore { w, contributions }
}

/// The verdict: `>=` on the `LogWeight` integers, no rounding.
pub fn decide(score: &ErScore, p: &ErPriors) -> ErDecision {
    if score.w >= p.match_threshold {
        ErDecision::Match
    } else if score.w >= p.possible_threshold {
        ErDecision::Possible
    } else {
        ErDecision::None
    }
}

/// The assertion's confidence, `clamp(W / match_threshold, 0..=1)`: a negative `W` is zero.
/// # Errors
/// A non-positive `match_threshold` (never from [`ErPriors::from_config`]).
pub fn raw_score(score: &ErScore, p: &ErPriors) -> Result<Fixed, FixedError> {
    LogWeight::raw_score(i64::from(score.w.0), p.match_threshold)
}

/// What one resolution pass over a workspace did (`SqliteStore::er_resolve`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErReport {
    /// Pairs the blocks named and the scorer compared.
    pub compared: u64,
    /// `Match` pairs whose two nodes share a canonical after the pass.
    pub matched: u64,
    /// `Possible` pairs left for review as `merge` candidates.
    pub possible: u64,
    /// `Match` pairs the final verdict kept apart: a human distinction on the pair
    /// (`identity_conflict`) or bridging its components, or a reversed merge (`unmerge`).
    pub conflicts: u64,
}

/// What `kaban unmerge <id>` restored (EN-F51 (5)).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnmergeOutcome {
    /// The node named.
    pub id: String,
    /// The canonical the named node was redirected to, when it was the merged one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canonical_id: Option<String>,
    /// The nodes whose redirect the reversal removed or moved (the named node, or the
    /// members of the cluster it was the canonical of).
    pub restored: Vec<String>,
    /// The identity claims now `deprecated` under the given reason.
    pub deprecated: Vec<String>,
}

/// Why two nodes share a block (EN-F51 (1)).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockKey {
    /// Equal `dup_key`.
    DupKey(String),
    /// Equal normalised title.
    Title(String),
    /// A shared anchor key.
    Anchor(String),
    /// A fuzzy identity neighbourhood around one node (EN-F24, through [`FuzzyIndex`] only).
    Fuzzy(String),
}

/// One block: the key and the indices (into the `nodes` slice given to [`blocks`]) of its
/// members, ascending. Blocks of one member are never returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    /// The workspace every member belongs to.
    pub workspace_id: String,
    /// Why the members are compared.
    pub key: BlockKey,
    /// Member indices, ascending.
    pub members: Vec<usize>,
}

/// The fuzzy identity neighbourhood of EN-F24 (the text index's fuzzy terms, E3.4). No
/// implementation ships yet: `blocks(.., None)` everywhere (spec 05 SS-15).
///
/// Any implementation indexes the ontology-declared identity fields of a node only, never
/// general body text.
pub trait FuzzyIndex {
    /// The ids of the nodes whose identity-field fuzzy similarity to `view` clears the
    /// index's threshold, in the view's workspace. `Ok` with an empty list is an answer (no
    /// neighbour); an index that cannot answer returns [`FuzzyUnavailable`], never an empty
    /// list.
    ///
    /// # Errors
    /// [`FuzzyUnavailable`] when the index is missing, stale or failed.
    fn neighbours(&self, view: &NodeView) -> Result<Vec<String>, FuzzyUnavailable>;
}

/// A [`FuzzyIndex`] could not answer (spec 05 SS-15): distinct from an empty neighbourhood,
/// so blocking fails rather than silently comparing fewer pairs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("fuzzy_unavailable")]
pub struct FuzzyUnavailable;

/// Block the nodes per workspace: exact `dup_key`, equal normalised title, shared anchor key
/// and, through `fuzzy`, the EN-F24 neighbourhoods. Exhaustive within a block, never across
/// workspaces; deterministic (ascending by workspace, key, member index).
///
/// # Errors
/// [`FuzzyUnavailable`] when `fuzzy` cannot answer for some node; never with `None`.
pub fn blocks(
    nodes: &[NodeView],
    fuzzy: Option<&dyn FuzzyIndex>,
) -> Result<Vec<Block>, FuzzyUnavailable> {
    let mut by_key: BTreeMap<(&str, BlockKey), BTreeSet<usize>> = BTreeMap::new();
    let mut by_id: BTreeMap<(&str, &str), usize> = BTreeMap::new();
    for (i, n) in nodes.iter().enumerate() {
        by_id.insert((n.workspace_id.as_str(), n.id.as_str()), i);
        if let Some(d) = &n.dup_key
            && !d.is_empty()
        {
            by_key
                .entry((&n.workspace_id, BlockKey::DupKey(d.clone())))
                .or_default()
                .insert(i);
        }
        if !n.title.is_empty() {
            by_key
                .entry((&n.workspace_id, BlockKey::Title(n.title.clone())))
                .or_default()
                .insert(i);
        }
        for a in &n.anchors {
            by_key
                .entry((&n.workspace_id, BlockKey::Anchor(a.clone())))
                .or_default()
                .insert(i);
        }
    }
    if let Some(index) = fuzzy {
        for (i, n) in nodes.iter().enumerate() {
            let near: Vec<usize> = index
                .neighbours(n)?
                .iter()
                .filter_map(|id| by_id.get(&(n.workspace_id.as_str(), id.as_str())).copied())
                .filter(|j| *j != i)
                .collect();
            if near.is_empty() {
                continue;
            }
            let members = by_key
                .entry((&n.workspace_id, BlockKey::Fuzzy(n.id.clone())))
                .or_default();
            members.insert(i);
            members.extend(near);
        }
    }
    Ok(by_key
        .into_iter()
        .filter(|(_, members)| members.len() >= 2)
        .map(|((ws, key), members)| Block {
            workspace_id: ws.to_owned(),
            key,
            members: members.into_iter().collect(),
        })
        .collect())
}

/// The distinct unordered pairs `(i, j)`, `i < j`, the blocks ask to compare: a pair in two
/// blocks is compared once; a pair outside every block never.
pub fn pairs(blocks: &[Block]) -> BTreeSet<(usize, usize)> {
    let mut out = BTreeSet::new();
    for b in blocks {
        for (x, &i) in b.members.iter().enumerate() {
            for &j in &b.members[x + 1..] {
                out.insert((i.min(j), i.max(j)));
            }
        }
    }
    out
}

/// A pair of node ids as a brief names it (`uncertain_pairs[]`, `pair_judgements[]`,
/// EN-F51 (6)); `new` orders the two so `(a, b)` and `(b, a)` are one pair.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PairRef {
    /// The lesser id.
    pub a: String,
    /// The greater id.
    pub b: String,
}

impl PairRef {
    /// The canonical order of two ids.
    pub fn new(x: &str, y: &str) -> Self {
        let (a, b) = if x <= y { (x, y) } else { (y, x) };
        Self {
            a: a.to_owned(),
            b: b.to_owned(),
        }
    }
}

/// One returned `pair_judgements[]` entry: the pair and whether the judge calls it one entity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairJudgement {
    /// The judged pair (any order; compared canonically).
    pub pair: PairRef,
    /// The judge's verdict.
    pub same: bool,
}

/// The fixed skip reason of a judgement on a pair the brief did not ask about.
pub const NOT_REQUESTED: &str = "not_requested";

/// What [`judge_filter`] keeps and what it drops.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JudgeFilter {
    /// Judgements on requested pairs, in returned order, one per pair (the first wins).
    pub accepted: Vec<PairJudgement>,
    /// Dropped entries with their reason (`not_requested`, or a repeat of a kept pair).
    pub skipped: Vec<(PairJudgement, &'static str)>,
}

/// The fixed skip reason of a second judgement on an already kept pair.
pub const DUPLICATE: &str = "duplicate";

/// Keep only the judgements on `requested` pairs (EN-F51 (6): a `pair_judgements[]` entry for
/// a pair outside `uncertain_pairs[]` is `skipped: not_requested`). Pure; E3.9 owns the
/// transport that turns the kept entries into `layer: llm, method: er_judge` claims.
pub fn judge_filter(requested: &[PairRef], returned: &[PairJudgement]) -> JudgeFilter {
    let asked: BTreeSet<PairRef> = requested.iter().map(|p| PairRef::new(&p.a, &p.b)).collect();
    let mut seen = BTreeSet::new();
    let mut out = JudgeFilter::default();
    for j in returned {
        let canonical = PairRef::new(&j.pair.a, &j.pair.b);
        if !asked.contains(&canonical) {
            out.skipped.push((j.clone(), NOT_REQUESTED));
        } else if !seen.insert(canonical) {
            out.skipped.push((j.clone(), DUPLICATE));
        } else {
            out.accepted.push(j.clone());
        }
    }
    out
}
