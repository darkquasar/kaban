//! [`SyncStore`]: the `.kaban/*.md` serialization role trait, with the types only it uses.

use crate::audit::WriteCtx;
use crate::error::Result;
use crate::ids::NodeId;
use crate::model::{ConflictRow, ImportedRecord, Node};

/// The `.kaban/*.md` serialization side of the store (core-spec F13, F9b; core-plan C5.1 and
/// C5.3 as amended by `reviews/m5-prep.md` M5-7, M5-8, M5-15, M5-18 and owner decisions OWN-18,
/// OWN-23, OWN-25): which records need (re)writing, the import of a file's record or
/// tombstone, the equal-rev conflict rows and the `post-commit` stamp. Used by `sync`,
/// `rebuild`, `resolve`, `why` (conflict flag) and `kaban hook post-commit`. New store methods
/// live in a role trait, never the umbrella (M4-26).
pub trait SyncStore: Send + Sync {
    /// The project-scoped records of `workspace_id` whose kind is in `kinds` (the stored names
    /// of the kinds with `governed.serialize != none` in the loaded ontologies, engine-plan
    /// E1.6; the shipped `adr.yaml`: Decisions, Conventions and Failures) and whose file is
    /// stale (M5-18): a non-purged row with `serialized_rev IS NULL OR serialized_rev <> rev`,
    /// or a purged row that was serialized before (`serialized_rev IS NOT NULL`) and whose
    /// tombstone is not yet written (`serialized_rev <> rev`; M5-7: a purged record never
    /// serialized gets no file). Ordered by id, at most `limit`; an empty `kinds` is empty.
    ///
    /// # Errors
    /// Backend errors.
    fn dirty_serializable(
        &self,
        workspace_id: &str,
        kinds: &[&str],
        limit: usize,
    ) -> Result<Vec<Node>>;
    /// The project-scoped records of `workspace_id` whose kind is in `kinds` (as
    /// [`Self::dirty_serializable`]) that have a file to miss (M5-11, added by C5.1b):
    /// `serialized_rev IS NOT NULL` and `active` or `archived` (a tombstoned record has its
    /// file deleted, M5-10; a purged one its tombstone, M5-7). Ordered by id, at most `limit`.
    /// The import's absence pass walks it.
    ///
    /// # Errors
    /// Backend errors.
    fn serialized_records(
        &self,
        workspace_id: &str,
        kinds: &[&str],
        limit: usize,
    ) -> Result<Vec<Node>>;
    /// Record that `id` was written to its file at `rev`: `serialized_rev := rev`, nothing
    /// else (no `rev` bump, no `updated_at` change; the node update trigger still writes one
    /// changes row, which is harmless).
    ///
    /// # Errors
    /// `NotFound` (unknown id) or a backend error.
    fn mark_serialized(&self, id: &NodeId, rev: i64) -> Result<()>;
    /// Write a file's record into the store in one write transaction (M5-18): insert the row,
    /// or update every content column of the stored one, with `rev := file rev` and
    /// `serialized_rev := rev`, `workspace_id` and `policy_version` from `ctx`, `store_scope`
    /// `project`, `content_hash` recomputed, `ctx.audit` appended. On an update the lifecycle
    /// and governance moves go through the spec's transition tables only (§4; M5-31): an
    /// invalid move is `InvalidTransition` and nothing is written (the caller records a
    /// conflict instead); a governance move appends its `events` row (`record.event`, or a
    /// `system` event with reason `import`) and requires an active stored record. A purged
    /// stored id never comes back (F5): `NotFound`, like another tenant's row (S6). An active
    /// record whose dedup identity (workspace, kind, text) another active row already holds
    /// cannot be inserted (the active-only unique index, MD-5) and fails as a backend error.
    ///
    /// # Errors
    /// `NotFound`, `KindMismatch`, `InvalidTransition`, `UnknownKind` or a backend error.
    fn import_record(&self, record: &ImportedRecord, ctx: &WriteCtx) -> Result<Node>;
    /// Import a tombstone (M5-7, M5-8, OWN-18: a tombstone always wins). No stored row: a
    /// purged ghost row of `kind` is inserted at `rev` (`serialized_rev = rev`) so the id
    /// never reappears. A stored purged row: `rev`/`serialized_rev` move up to the file's rev
    /// when it is higher, nothing else. A stored live row: purged in place like
    /// `forget(Purge)` (text, index entry, dedup identity and edges erased, one `op='purge'`
    /// changes row, S5 hygiene), at `max(rev + 1, file rev)` with `serialized_rev` equal to it.
    /// `ctx.audit` (the caller's `Purge` entry) rides in the same transaction. Returns the
    /// row's `rev` afterwards and, for a live row, the outcome of the purge's post-commit
    /// cleanup (SS-03): reported, never an error after the commit.
    ///
    /// # Errors
    /// `NotFound` (another tenant's row), `KindMismatch` or a backend error.
    fn import_purged(
        &self,
        id: &NodeId,
        kind: &str,
        rev: i64,
        ctx: &WriteCtx,
    ) -> Result<ImportedPurge>;
    /// Insert or replace the conflict row of `row.node_id` (M5-5).
    ///
    /// # Errors
    /// Backend errors.
    fn conflict_upsert(&self, row: &ConflictRow) -> Result<()>;
    /// Delete the conflict row of `id`; `true` when one existed.
    ///
    /// # Errors
    /// Backend errors.
    fn conflict_clear(&self, id: &NodeId) -> Result<bool>;
    /// Every conflict row whose node lives in `workspace_id` (S6), ordered by node id.
    ///
    /// # Errors
    /// Backend errors.
    fn conflicts(&self, workspace_id: &str) -> Result<Vec<ConflictRow>>;
    /// The conflict rows among `ids` (M5-6: one bounded query for the cards of a `why`
    /// result), in `ids` order.
    ///
    /// # Errors
    /// Backend errors.
    fn conflicts_for(&self, ids: &[NodeId]) -> Result<Vec<ConflictRow>>;
    /// `kaban hook post-commit` (M5-15, OWN-23; OWN-65): set `commit_sha = sha` on every
    /// non-purged node of `workspace_id` whose `kind` is in `kinds` (the stored names of the
    /// kinds whose effective `index.stamp` is `commit` in the loaded ontologies, engine-spec
    /// §3.1; the E1.6 `kinds: &[&str]` pattern, bound as placeholders) with `commit_sha IS NULL
    /// AND (repo IS NULL OR repo = repo)`, bumping `rev` and `updated_at` (every write bumps
    /// `rev`, F13; each node is stamped once, so the next `sync` rewrites it once). An empty
    /// `kinds` stamps nothing. Returns how many rows were stamped.
    ///
    /// # Errors
    /// Backend errors.
    fn stamp_commit(
        &self,
        workspace_id: &str,
        sha: &str,
        repo: Option<&str>,
        kinds: &[&str],
    ) -> Result<u64>;
    /// Record why a record of a `stamp: commit` kind written now gets no commit sha
    /// (engine-spec §3.1 `index`, OWN-65): one `events` row with `kind = 'unstamped'` and the
    /// reason's fixed text, signer kind `system`. Never an error for the write itself.
    /// Returns the call's terminal `changes.seq`, read inside its transaction (spec 05 SS-10:
    /// the row follows the record's own commit, so the receipt's position is this one).
    ///
    /// # Errors
    /// Backend errors.
    fn record_unstamped(
        &self,
        node: &NodeId,
        workspace_id: &str,
        reason: UnstampedReason,
    ) -> Result<u64>;
    /// `doctor`'s count of `unstamped` events of `workspace_id` per reason.
    ///
    /// # Errors
    /// Backend errors.
    fn unstamped_counts(&self, workspace_id: &str) -> Result<UnstampedCounts>;
    /// Current unstamped, non-purged rows eligible for the commit hook: the workspace,
    /// stamping kinds, and either no repository identity or the requested repository.
    /// An empty kind set is empty. Historical events are not consulted.
    ///
    /// # Errors
    /// Backend errors or an unsupported diagnostic.
    fn unstamped_current_count(
        &self,
        _workspace_id: &str,
        _repo: Option<&str>,
        _kinds: &[&str],
    ) -> Result<u64> {
        Err(crate::StoreError::InvalidName(
            "unstamped_current_count".into(),
            "unsupported",
        ))
    }
}

/// Why a record of a `stamp: commit` kind got no commit sha at write time (engine-spec §3.1
/// "Sources that are not repositories", OWN-65): recorded on an `events` row, never an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum UnstampedReason {
    /// The process runs outside any git repository (`repo_root` unset).
    NoRepository,
    /// Historical reason from older writes whose `git rev-parse HEAD` gave nothing.
    /// Ordinary writes no longer probe HEAD or generate this reason.
    NoHistory,
}

impl UnstampedReason {
    /// The fixed `events.reason` text.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NoRepository => "no_repository",
            Self::NoHistory => "no_history",
        }
    }
}

/// What [`SyncStore::import_purged`] left behind: the row's `rev`, and for a live row purged
/// in place the detail of its post-commit cleanup (SS-03, SS-12: the same cleanup as
/// `forget(Purge)`, the index scope included), as `ForgetOutcome::cleanup` words it: the
/// free-page residue until a VACUUM, after `busy` or the failure findings when a cleanup step
/// did not run through. `None` for a ghost or an already purged row: nothing was there to
/// clean. The revocation stands whatever the detail says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportedPurge {
    /// The row's `rev` after the import.
    pub rev: i64,
    /// Why the purge is not physically cleaned, when a live row was purged.
    pub cleanup: Option<String>,
}

/// `doctor`'s `unstamped` line: the `unstamped` events of one workspace per reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct UnstampedCounts {
    /// Rows recorded with `no_repository`.
    pub no_repository: u64,
    /// Rows recorded with `no_history`.
    pub no_history: u64,
}
