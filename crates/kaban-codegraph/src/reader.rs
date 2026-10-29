//! The reader contract (stage1-spec §6 method set plus `nodes_in_file` and `symbol_unique` for
//! the join; cg-join-v1 §6, §9; stage1-plan CG2.2). Every list method returns the **complete**
//! candidate set ordered by `(path, ref)` with no `LIMIT` inside the reader (RV-7): the list
//! limit, `truncated` and the ranking of G1-F16 are the caller's step. The one implementation is
//! `SqliteSnapshot` (`queries/`, CG2.3).

use std::collections::BTreeMap;

use crate::error::ReadError;
use crate::limits::Depth;
use crate::schema::{EdgeType, FileId, NodeId, NodeKind, Range};

/// The `meta` table as a map; the keys of stage1-spec §4.1.
pub type Meta = BTreeMap<String, String>;

/// One `symbols` row joined with its file's path (a `CodeNode` card before projection).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeRow {
    /// `symbols.id`.
    pub id: NodeId,
    /// `symbol | local | file`.
    pub kind: NodeKind,
    /// Exact row identity.
    pub r#ref: String,
    /// The raw SCIP symbol (`None` for a file node).
    pub scip: Option<String>,
    /// The normalised cross-commit key (`kind = symbol` only).
    pub key: Option<String>,
    /// SCIP kind name or `File`.
    pub label: String,
    /// Display name.
    pub name: String,
    /// The defining file.
    pub file_id: Option<FileId>,
    /// `files.path` of `file_id` (`None` for an external placeholder).
    pub path: Option<String>,
    /// `files.language` of `file_id` (`None` for an external placeholder or an empty value);
    /// the `CodeNode.language` card field (stage1-spec §6, CG7.1).
    pub language: Option<String>,
    /// Definition range.
    pub def: Option<Range>,
    /// Enclosing range.
    pub enc: Option<Range>,
    /// Signature text.
    pub signature: Option<String>,
    /// Documentation text.
    pub doc: Option<String>,
    /// The `snippets.text` of this symbol (G1-F17; `None` when none was stored).
    pub snippet: Option<String>,
    /// Defined outside the repository.
    pub external: bool,
    /// Defined in a test file.
    pub is_test: bool,
    /// The identifier split of cg-join-v1 §8.
    pub ident_terms: String,
}

/// One reference site of `references` (stage1-spec §6 `ReferenceSite`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReferenceSite {
    /// The occurrence range.
    pub site: Range,
    /// The document path.
    pub path: String,
    /// SCIP `SymbolRole` bits of the occurrence.
    pub roles: u32,
}

/// The sites of one enclosing node (the file node when the occurrence has no enclosing symbol).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReferenceGroup {
    /// The enclosing node.
    pub enclosing: NodeRow,
    /// Its sites, in `(sl, sc, el, ec)` order.
    pub sites: Vec<ReferenceSite>,
}

/// How a `TESTED_BY` edge was derived (stage1-spec §4.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MatchedBy {
    /// Reference form: the test's enclosing range contains a reference to the symbol.
    Reference,
    /// Naming form: file stems match the language rule (heuristic).
    Naming,
}

impl MatchedBy {
    /// The wire value (`ctl Enum`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Reference => "reference",
            Self::Naming => "naming",
        }
    }
}

/// Which tier of `tests_for` a link comes from (TESTS-1, backlog 20/24).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum How {
    /// A `TESTED_BY` edge of the target itself.
    Direct,
    /// A `TESTED_BY` edge of a caller of the target (one `REFERENCES` hop); `via` names it.
    Via,
    /// A test function whose path or snippet spells every word of the target's name; a
    /// heuristic answered only when the graph has nothing.
    Textual,
}

impl How {
    /// The wire value (`ctl Enum`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Via => "via",
            Self::Textual => "textual",
        }
    }
}

/// One test link of `tests_for` (stage1-spec §6 `TestLink`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestLink {
    /// The test function, method or test file node.
    pub test: NodeRow,
    /// The derivation form.
    pub matched_by: MatchedBy,
    /// `true` for the naming form.
    pub heuristic: bool,
    /// The tier.
    pub how: How,
    /// The caller the test reaches the target through (`How::Via` only).
    pub via: Option<NodeRow>,
}

/// The target of `tests_for`: a symbol (reference form) or a file (naming form).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TestsTarget {
    /// `code_tests_for(symbol)`.
    Symbol(NodeId),
    /// `code_tests_for(path)`.
    File(FileId),
}

/// One edge of a `neighbours` result, by node refs (stage1-spec §6 `Edge`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NeighbourEdge {
    /// Source ref.
    pub from: String,
    /// The edge type.
    pub r#type: EdgeType,
    /// Target ref.
    pub to: String,
}

/// The bounded BFS result of `neighbours`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NeighbourGraph {
    /// Every node reached, the start node first, then by `(depth, ref)`.
    pub nodes: Vec<NodeRow>,
    /// Every edge expanded, in expansion order.
    pub edges: Vec<NeighbourEdge>,
    /// A per-hub or total traversal bound cut the expansion.
    pub truncated: bool,
    /// Edges to document-local symbols (`kind = local`) left out because locals were not asked
    /// for (G4); 0 when they were.
    pub locals_hidden: usize,
}

/// The reader methods of stage1-spec §6 and cg-join-v1 §9.
pub trait CodeGraphReader {
    /// The `meta` table.
    ///
    /// # Errors
    /// [`ReadError::Sql`].
    fn meta(&self) -> Result<Meta, ReadError>;

    /// The node with this exact `ref`.
    ///
    /// # Errors
    /// [`ReadError`].
    fn node(&self, r#ref: &str) -> Result<Option<NodeRow>, ReadError>;

    /// The node with this id.
    ///
    /// # Errors
    /// [`ReadError`].
    fn node_by_id(&self, id: NodeId) -> Result<Option<NodeRow>, ReadError>;

    /// The one `kind = 'symbol' AND external = 0` row with this key; `Ambiguous` when several
    /// share it (cg-join-v1 §6).
    ///
    /// # Errors
    /// [`ReadError::Ambiguous`], [`ReadError::Sql`].
    fn symbol_unique(&self, key: &str) -> Result<Option<NodeRow>, ReadError>;

    /// Every node defined in the file at `path`, by `(path, ref)`.
    ///
    /// # Errors
    /// [`ReadError`].
    fn nodes_in_file(&self, path: &str) -> Result<Vec<NodeRow>, ReadError>;

    /// Exact, case-sensitive `(label, name)` matches, by `(path, ref)`.
    ///
    /// # Errors
    /// [`ReadError`].
    fn by_label_name(&self, label: &str, name: &str) -> Result<Vec<NodeRow>, ReadError>;

    /// Every row with the exact, case-sensitive display `name`, whatever its label, kind or
    /// `external` flag, by `(path, ref)`.
    ///
    /// # Errors
    /// [`ReadError`].
    fn by_name(&self, name: &str) -> Result<Vec<NodeRow>, ReadError>;

    /// Nodes whose definition range touches `line` (0-based) in the file at `path`, by
    /// `(path, ref)`. Ranges are end-exclusive: `[sl:sc, el:ec)` touches `sl..=el` when `ec > 0`
    /// and `sl..el` when `ec == 0`; an empty range at column 0 touches nothing (RG2-5).
    ///
    /// # Errors
    /// [`ReadError`].
    fn by_path_line(&self, path: &str, line: u32) -> Result<Vec<NodeRow>, ReadError>;

    /// Nodes of `file` whose definition range touches any `(from, to)` line interval (inclusive,
    /// 0-based; `from > to` is empty and matches nothing), by `(path, ref)`, under the same
    /// half-open rule as `by_path_line`.
    ///
    /// # Errors
    /// [`ReadError`].
    fn nodes_in_ranges(
        &self,
        file: FileId,
        ranges: &[(u32, u32)],
    ) -> Result<Vec<NodeRow>, ReadError>;

    /// `IMPLEMENTS` sources of `id`, external rows only when asked, by `(path, ref)`.
    ///
    /// # Errors
    /// [`ReadError`].
    fn implementers(&self, id: NodeId, include_external: bool) -> Result<Vec<NodeRow>, ReadError>;

    /// Direct `CONTAINS` children of `id`, restricted to `labels` when non-empty, by `(path, ref)`.
    ///
    /// # Errors
    /// [`ReadError`].
    fn members(&self, id: NodeId, labels: &[&str]) -> Result<Vec<NodeRow>, ReadError>;

    /// The top-level symbols of `file` (G3): `kind = 'symbol'` rows defined in it with no
    /// `CONTAINS` parent among the other symbols of the same file, restricted to `labels` when
    /// non-empty, in definition range order.
    ///
    /// # Errors
    /// [`ReadError`].
    fn file_members(&self, file: FileId, labels: &[&str]) -> Result<Vec<NodeRow>, ReadError>;

    /// Reference sites of `id` (occurrences without the definition role) grouped by enclosing
    /// node, groups by `(path, ref)`; sites in test files only when asked.
    ///
    /// # Errors
    /// [`ReadError`].
    fn references(&self, id: NodeId, include_tests: bool)
    -> Result<Vec<ReferenceGroup>, ReadError>;

    /// `TESTED_BY` targets of a symbol (reference form) or a file (naming form), by `(path, ref)`.
    ///
    /// # Errors
    /// [`ReadError`].
    fn tests_for(&self, target: TestsTarget) -> Result<Vec<TestLink>, ReadError>;

    /// The `via` tier (TESTS-1): the `TESTED_BY` targets of the first `intermediates` callers
    /// of `id` (non-test, non-external functions or methods with a `REFERENCES` edge to it, by
    /// `(path, ref)`; a re-exporting module is not a caller), each link naming its caller in
    /// `via`; by caller, then `(path, ref)`.
    ///
    /// # Errors
    /// [`ReadError`].
    fn tests_via(&self, id: NodeId, intermediates: usize) -> Result<Vec<TestLink>, ReadError>;

    /// The `textual` tier (TESTS-1): test functions and methods whose path or stored snippet
    /// contain every one of `words` (ASCII case-insensitive), by `(path, ref)`; `matched_by:
    /// naming`, heuristic. Empty when `words` is empty.
    ///
    /// # Errors
    /// [`ReadError`].
    fn tests_textual(&self, words: &[&str]) -> Result<Vec<TestLink>, ReadError>;

    /// Bounded BFS from `id` over `edge_types` (every type when empty) in both directions, up to
    /// `depth`, with the `Limits` traversal bounds; deterministic by `(depth, ref)`. Each distinct
    /// `(src, type, dst)` edge is returned once (RG2-3). Edges to document-local symbols
    /// (`kind = local`: scip-go's and rust-analyzer's `local N`, named by the id) are left out
    /// and counted in `locals_hidden` unless `include_locals` (G4); hidden edges never count
    /// against the bounds.
    ///
    /// # Errors
    /// [`ReadError::OutOfRange`] (`depth`) when `depth` exceeds this reader's own
    /// `Limits::depth_max` (RG2-4); [`ReadError`] otherwise.
    fn neighbours_with(
        &self,
        id: NodeId,
        depth: Depth,
        edge_types: &[EdgeType],
        include_locals: bool,
    ) -> Result<NeighbourGraph, ReadError>;

    /// [`neighbours_with`](Self::neighbours_with) over every node, locals included (the
    /// complete graph the goldens check).
    ///
    /// # Errors
    /// As `neighbours_with`.
    fn neighbours(
        &self,
        id: NodeId,
        depth: Depth,
        edge_types: &[EdgeType],
    ) -> Result<NeighbourGraph, ReadError> {
        self.neighbours_with(id, depth, edge_types, true)
    }
}
