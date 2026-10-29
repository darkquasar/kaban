//! Store-level types owned by no single role trait.

use crate::ids::NodeId;

/// One outstanding review item of a node (engine-spec EN-F5a, EN-F14a; engine-plan E2.5,
/// dispositions RE4-9): the `node_candidate` row's admission state (`reason =
/// discovered_kind`, keyed by the `x:` label) or one of the node's conflict groups whose
/// claims are still `candidate` or `unresolved`. A node with several outstanding groups has
/// one row per group, in `(rid, group_key)` order; the discovered row is listed on its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewRow {
    /// The node under review (the span's chunk for a span group).
    pub node: NodeId,
    /// The node's stored kind.
    pub kind: String,
    /// `discovered_kind`, `below_margin`, `governed_semantics`, `merge`; `unresolved` for a
    /// group whose claims are `unresolved` (the explanation names the conflict).
    pub reason: String,
    /// The group key (EN-F12a canonical JSON), or the `x:` label of a discovered row.
    pub group_key: String,
    /// The group's `assertion_status.disposition` (`candidate` or `unresolved`; `candidate`
    /// for the discovered row).
    pub disposition: String,
    /// Unix seconds since the node entered review.
    pub since: i64,
}

/// Permission bits (`mode & 0o777`) of the database file and its sidecars as a diagnostic
/// open found them (M4-19): read with `stat` before the connection existed and never
/// tightened, so they report what an operator would see. `None` is an absent sidecar; the
/// main file is present whenever an open succeeded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DatabaseFileModes {
    /// The database file.
    pub db: u32,
    /// The `-wal` sidecar, when present.
    pub wal: Option<u32>,
    /// The `-shm` sidecar, when present.
    pub shm: Option<u32>,
}

impl DatabaseFileModes {
    /// The one mode every database file must have (core-spec S5).
    pub const OWNER_ONLY: u32 = 0o600;

    /// Whether every present file is owner-only.
    #[must_use]
    pub fn all_owner_only(&self) -> bool {
        [Some(self.db), self.wal, self.shm]
            .into_iter()
            .flatten()
            .all(|mode| mode == Self::OWNER_ONLY)
    }
}
