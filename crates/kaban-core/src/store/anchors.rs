//! The CG-JOIN v1 attach and anchor types (engine-plan E2.6) that cross into the store.

use crate::model::WorkspaceFilter;

/// The admitted code-graph snapshot as it crosses into the store (CG-JOIN v1 §2, SO-3; engine
/// E2.6): the sealed [`AttachHandle`] of the admitted file, the snapshot's commit and schema
/// major, and the opaque file generation the admission inspected (RU3-2: a same-commit
/// republication is another generation and re-attaches). Built only by
/// [`SnapshotAttach::from_admitted`], from one admission's evidence, in exactly one production
/// place, `kaban-api/src/snapshot.rs`, from a `Resolved`; `kaban-core` knows no codegraph type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotAttach {
    /// The admitted read-only attachment target, opaque to core.
    pub handle: AttachHandle,
    /// `SnapshotRef.commit`, 40 lowercase hex.
    pub commit: String,
    /// `meta.schema_version` major as admitted.
    pub schema_version: u32,
    /// The file generation the admission inspected, rendered opaquely; compared for equality
    /// only: against the attached one, and by the owning adapter against the file's current
    /// generation before an attach (a stale handle attaches nothing, SS-25).
    pub generation: String,
}

impl SnapshotAttach {
    /// The one constructor (spec 05 SS-25): an admission's evidence, together. "Admitted"
    /// means the file passed `kaban_codegraph::validate_for_attach` in this process (a regular
    /// owner-only file under its final name, a readable image at a supported schema):
    /// `encoded` is the attachment target that admission rendered for the owning store adapter
    /// (its `AttachUri`, verbatim), `commit` the resolved `SnapshotRef.commit` of the same
    /// file, `schema_version` the major and `generation` the file generation that admission
    /// read. The handle exists only inside the attach built here, so a bare string never
    /// becomes one. Callers: the production adapter `kaban-api/src/snapshot.rs` (the one wrap
    /// site, pinned by `kaban-api/tests/crate_graph.rs`) and the test twins that admit a
    /// fixture the same way.
    #[must_use]
    pub fn from_admitted(
        encoded: String,
        commit: String,
        schema_version: u32,
        generation: String,
    ) -> Self {
        Self {
            handle: AttachHandle(encoded),
            commit,
            schema_version,
            generation,
        }
    }
}

/// The admitted snapshot's attachment target, sealed (spec 05 SS-25): the value
/// `kaban_codegraph::AttachUri` encoded for the owning store adapter, carried through core
/// unread. Core neither builds nor inspects its syntax, and `Debug` withholds it. It is minted
/// only with the admission's other evidence, by [`SnapshotAttach::from_admitted`] (production:
/// `kaban-api/src/snapshot.rs`), and read in one place (the SQLite adapter's attach
/// statement); `kaban-api/tests/crate_graph.rs` pins both.
///
/// The value cannot be built from a literal:
///
/// ```compile_fail
/// let _ = kaban_core::AttachHandle(String::new());
/// ```
#[derive(Clone, PartialEq, Eq)]
pub struct AttachHandle(String);

impl AttachHandle {
    /// The admitted value, for the owning adapter's attach statement only.
    #[must_use]
    pub fn admitted_value(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for AttachHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AttachHandle(<withheld>)")
    }
}

/// The CG-JOIN v1 §3 predicate set of every anchor join, resolved by the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AnchorPredicates<'a> {
    /// The read workspace (S6).
    pub workspace: &'a WorkspaceFilter,
    /// `ApiConfig.repo`, the join's repository identity (SD-5; never the keyed `<repo-id>`).
    pub repo: &'a str,
    /// The resolved snapshot commit, 40 hex; `None` when no snapshot resolved (the scope
    /// predicate then keeps the `NULL` rows only).
    pub commit: Option<&'a str>,
}

/// One row of [`MemoryStore::snapshot_symbols`](super::MemoryStore::snapshot_symbols) (CG-JOIN
/// v1 §7): the stable `symbols` columns the engine reads plus the defining file's path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotSymbol {
    /// `symbols.key`, the normalised cross-commit key.
    pub key: String,
    /// `symbols.name`.
    pub name: String,
    /// `symbols.label`.
    pub label: String,
    /// `files.path` of the defining file.
    pub path: String,
    /// `symbols.ident_terms` (CG-JOIN v1 §8).
    pub ident_terms: String,
}

/// A page request of [`MemoryStore::snapshot_symbols`](super::MemoryStore::snapshot_symbols):
/// rows with `id > after_id`, at most `limit` of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SymbolPage {
    /// The last id of the previous page (`0` for the first).
    pub after_id: i64,
    /// Page size; callers pass [`SNAPSHOT_SYMBOL_PAGE`].
    pub limit: u32,
}

/// The structural page size of
/// [`MemoryStore::snapshot_symbols`](super::MemoryStore::snapshot_symbols) (CG-JOIN v1 §7; not a
/// config key).
pub const SNAPSHOT_SYMBOL_PAGE: u32 = 4096;
