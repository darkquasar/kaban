//! The symbol card of `why` (engine-plan E2.6; CG-JOIN v1 §4, §6): how a result matched and
//! the code-graph side it carries.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// How a `why` result matched its target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MatchedBy {
    /// A `path_scope` entry covers the target path.
    PathScope,
    /// The record's text matches the query.
    Query,
    /// The record is linked `ABOUT` or `GOVERNS` to a `CodeRef` whose `symbol` anchor equals
    /// the target key (CG-JOIN v1 §3 predicates).
    Symbol,
    /// The record is reached only through an `EVIDENCE_FOR` edge from such a `CodeRef`.
    Evidence,
}

/// The `snapshot` object of a symbol card (CG-JOIN v1 §4 projection): the resolved commit,
/// whether it is `HEAD`'s own, and `off | current | stale` against the live policy version
/// (`off`: the content scan was disabled at index time). `isolation_tier` and
/// `schema_version` are not on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[schemars(inline)]
pub struct CodeSnapshot {
    /// The snapshot's commit, 40 lowercase hex.
    pub commit: String,
    /// Whether it is the snapshot of the current `HEAD` (else the nearest ancestor's).
    pub exact: bool,
    /// `current` when the snapshot was built under the live denylist policy, `off` when the
    /// content scan was disabled at index time (nothing was gated), else `stale`.
    pub policy: String,
}

/// The `code` field of a symbol card (CG-JOIN v1 §6): exactly one of a resolved snapshot, no
/// snapshot (`snapshot_absent`), every candidate too new (`snapshot_too_new`), a key present in
/// the anchors but absent from the resolved snapshot (`snapshot_mismatch`), or several
/// non-external rows sharing the key (`ambiguous_symbol`); the last two carry the `snapshot`
/// object beside them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CodeField {
    /// A snapshot resolved and holds the key once.
    Snapshot(CodeSnapshot),
    /// No snapshot resolves (`NoSnapshot::{NoDirectory, NoFile, CorruptOnly}`): the exact
    /// anchor leg only, nothing attached.
    SnapshotAbsent,
    /// Every candidate snapshot was written by a newer `kaban-index`: the exact anchor leg
    /// only, nothing attached.
    SnapshotTooNew,
    /// The key is absent from the resolved snapshot.
    SnapshotMismatch {
        /// The snapshot that was consulted.
        snapshot: CodeSnapshot,
    },
    /// More than one `kind = 'symbol' AND external = 0` row shares the key.
    AmbiguousSymbol {
        /// The snapshot that was consulted.
        snapshot: CodeSnapshot,
    },
}
