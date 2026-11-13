//! The content/control field-class seam (core-spec S1 and S4 as amended by owner decision
//! OWN-10; core-plan task C3.0).
//!
//! Every string leaf of every tool's request and response is either *content* (the default:
//! scanned by the denylist on the way in and on the way out) or *control*: a value kaban itself
//! shapes (node ids, kaban-issued tokens, hashes, commit SHAs, closed enum values). A control
//! entry names the tool, the direction, the JSON path pattern and an exact [`Shape`]; a value
//! that passes its shape is validated instead of scanned, a value that fails it is treated as
//! content and scanned (so a canary under a control key is still caught, and the typed parse then
//! rejects the shape). Secret entries are kaban-issued secrets: never logged, audited raw, echoed
//! in error text or returned unless the entry is the issuing response field.
//!
//! The registry is the single declaration. Adapters never consult it directly: they call
//! [`crate::KabanApi::prepare_args`] (request side) and [`crate::KabanApi::gate_response`]
//! (response side), which look entries up by path while walking the JSON tree; the typed
//! handlers and the read projections gate the same strings by name through `ControlPath`
//! (review RC-1), so one lookup decides on both sides. Object keys are literal: a key spelling a
//! path pattern is escaped and never aliases an entry ([`append_key`], review RC-2). Anything
//! not listed stays content (fail closed). `tests/control.rs` walks the schemars schema of every
//! tool and fails on a stale entry (one that points at no string path).

mod code;
mod ingest;
mod memory;

pub use code::{
    EDGE_TYPE_VALUES, HOW_VALUES, ISOLATION_VALUES, KIND_VALUES, LABEL_VALUES, LANGUAGE_VALUES,
    MATCHED_BY_VALUES, POLICY_VALUES, RANKING_VALUES, RESOURCE_VALUES, ROLE_VALUES,
};
pub use ingest::{DIMS, DTYPE_VALUES};
pub use memory::{LAYER_VALUES, METHOD_VALUES};

use kaban_core::NodeId;
use schemars::{JsonSchema, Schema, schema_for};

/// Which side of a tool call a string sits on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Direction {
    /// Raw request arguments, before typed deserialization.
    Request,
    /// The serialized result, before it reaches the wire.
    Response,
}

/// Exact validator of a control value. Closed set: a new validator is a new variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    /// A node (or edge) id: hyphenated lowercase `UUIDv7` ([`NodeId::parse`]).
    NodeId,
    /// Lowercase hex of `min..=max` digits (commit SHAs 7..=64, hashes 64).
    Hex {
        /// Minimum number of hex digits.
        min: usize,
        /// Maximum number of hex digits.
        max: usize,
    },
    /// A kaban-issued one-time token: exactly 64 lowercase hex digits.
    Token64,
    /// One of a closed set of literal values.
    Enum(&'static [&'static str]),
    /// A non-negative integer in `min..=max` (`embed_submit.model.dims`, EN-E6). JSON numbers
    /// are never scanned (the seam walkers judge strings only), so the row is the declaration
    /// of the field's class and its bounds; a string at such a path passes only when it spells
    /// a decimal integer inside the bounds, and the typed parse then refuses the string anyway.
    Int {
        /// Smallest admitted value.
        min: u64,
        /// Largest admitted value.
        max: u64,
    },
}

impl Shape {
    /// Whether `s` has exactly this shape.
    pub fn accepts(self, s: &str) -> bool {
        match self {
            Self::NodeId => NodeId::parse(s).is_ok(),
            Self::Hex { min, max } => (min..=max).contains(&s.len()) && is_lower_hex(s),
            Self::Token64 => s.len() == 64 && is_lower_hex(s),
            Self::Enum(values) => values.contains(&s),
            Self::Int { min, max } => s.parse::<u64>().is_ok_and(|n| (min..=max).contains(&n)),
        }
    }
}

fn is_lower_hex(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// One declared control field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ControlEntry {
    /// Tool name as the adapter dispatches it.
    pub tool: &'static str,
    /// Request or response side.
    pub direction: Direction,
    /// JSON path pattern from the root of the arguments or the result: object keys joined with
    /// `.`, `[]` for any array element (`links[].to`, `results[].id`, `policy.version`).
    pub path: &'static str,
    /// The exact validator.
    pub shape: Shape,
    /// A kaban-issued secret (see the module docs): never logged, audited raw, echoed in
    /// error text or returned, except by the one issuing response entry.
    pub secret: bool,
    /// The response field that hands a secret to the caller once (review RC-5). Only
    /// meaningful with `secret`; a secret response entry without it is masked on the way out.
    pub issuing: bool,
}

const fn entry(
    tool: &'static str,
    direction: Direction,
    path: &'static str,
    shape: Shape,
) -> ControlEntry {
    ControlEntry {
        tool,
        direction,
        path,
        shape,
        secret: false,
        issuing: false,
    }
}

/// `snapshot_commit`, `snapshot.commit`, `last_change.commit`, `range.from`, `range.to`
/// (stage1-spec §6.1, G1-S2): a commit a caller names or a response reports, 7 to 64 lowercase
/// hex. Admission of user input before resolution only (SO-2): a stored or compared value is
/// the resolved 40 hex (v4 CG5.1). The registry rows land with the first tool carrying each
/// path (`remember.refs[].snapshot_commit` in CG5.1, the `code_*` fields in E2.6 and GM7).
pub const SNAPSHOT_COMMIT: Shape = Shape::Hex { min: 7, max: 64 };

/// Every control field of every tool. Kind and relationship names, workspace names,
/// `path_scope` entries, `CodeRef` fields and every free-form string are content (S1) and must not
/// appear here. The one `CodeRef` exception is `refs[].snapshot_commit`: a commit id kaban
/// canonicalises to 40 hex before it is stored (SO-2, v4 CG5.1); `symbol` stays content.
/// On the `code_*` tools (stage1-spec §6.1, G1-S2) the closed enums `kind`,
/// `label`, `language`, `ranking`, `policy`, `isolation`, `roles[]`, `matched_by`, the edge
/// `type` and `edge_types[]`, and the `snapshot.commit` hex are control; `symbol`, `ref`, `raw`,
/// `name`, `path`, `current_path`, `signature`, `doc`, `snippet` and the edge `from`/`to` refs
/// are content (no `ScipSymbol` shape exists: the grammar is a typed handler step). The
/// `kaban://code-index` resource is gated as `code_index` (G1-F18, CG7.3): its snapshot rows,
/// `languages[]` and `caches` are control; the indexer texts, versions and detail are content.
pub const REGISTRY: &[ControlEntry] = &REGISTRY_ARRAY;

/// The row families of [`REGISTRY`] in registry order: the memory tools, the docs-ingest and
/// embedding tools, then the code-graph tools.
const FAMILIES: &[&[ControlEntry]] = &[memory::ROWS, ingest::ROWS, code::ROWS];

const REGISTRY_LEN: usize = total(FAMILIES);

const REGISTRY_ARRAY: [ControlEntry; REGISTRY_LEN] = concat(FAMILIES);

/// Number of rows over `families`.
const fn total(families: &[&[ControlEntry]]) -> usize {
    let mut n = 0;
    let mut i = 0;
    while i < families.len() {
        n += families[i].len();
        i += 1;
    }
    n
}

/// `families` flattened in order into one array of exactly `N` rows (`N` is [`total`]).
const fn concat<const N: usize>(families: &[&[ControlEntry]]) -> [ControlEntry; N] {
    let mut rows = [entry("", Direction::Request, "", Shape::NodeId); N];
    let mut k = 0;
    let mut i = 0;
    while i < families.len() {
        let family = families[i];
        let mut j = 0;
        while j < family.len() {
            rows[k] = family[j];
            k += 1;
            j += 1;
        }
        i += 1;
    }
    assert!(k == N, "REGISTRY_LEN must equal the row count of FAMILIES");
    rows
}

/// Characters that have a meaning in a path pattern (`.` joins keys, `[]` marks an array
/// element) plus the escape itself.
const PATH_META: [char; 4] = ['.', '[', ']', '\\'];

/// Append object key `key` to `path` as one segment. Keys are literal: every path
/// metacharacter inside a key is prefixed with `\`, so a caller-chosen key such as
/// `"links[].to"` becomes `links\[\]\.to` and can never equal a registered path (no
/// registered path contains `\`; `tests/control.rs` asserts that). Shared by the seam walkers
/// and the completeness test's schema walker (review RC-2).
pub fn append_key(path: &mut String, key: &str) {
    if !path.is_empty() {
        path.push('.');
    }
    if key.contains(PATH_META) {
        for c in key.chars() {
            if PATH_META.contains(&c) {
                path.push('\\');
            }
            path.push(c);
        }
    } else {
        path.push_str(key);
    }
}

/// Append an array element marker to `path`.
pub fn append_index(path: &mut String) {
    path.push_str("[]");
}

/// The control entry at `path` of `tool`'s request or response, if any.
pub fn control_entry(
    tool: &str,
    direction: Direction,
    path: &str,
) -> Option<&'static ControlEntry> {
    REGISTRY
        .iter()
        .find(|e| e.direction == direction && e.tool == tool && e.path == path)
}

/// Whether `s` is a control value at `path`: there is an entry and its shape accepts `s`.
fn passes(tool: &str, direction: Direction, path: &str, s: &str) -> bool {
    control_entry(tool, direction, path).is_some_and(|e| e.shape.accepts(s))
}

/// Registry coordinates of one string a typed handler or a read projection gates by name
/// (`link.from`, a traversal edge id): the same lookup the seam walkers make, so a value the
/// pre-parse scan validated is not scanned again by the handler, and a response id is not
/// scanned by the projection (review RC-1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ControlPath {
    tool: &'static str,
    direction: Direction,
    path: &'static str,
}

impl ControlPath {
    /// A request-side path of `tool`.
    pub(crate) const fn request(tool: &'static str, path: &'static str) -> Self {
        Self {
            tool,
            direction: Direction::Request,
            path,
        }
    }

    /// A response-side path of `tool`.
    pub(crate) const fn response(tool: &'static str, path: &'static str) -> Self {
        Self {
            tool,
            direction: Direction::Response,
            path,
        }
    }

    /// Whether `s` is a control value here (validated, not scanned).
    pub(crate) fn accepts(self, s: &str) -> bool {
        passes(self.tool, self.direction, self.path, s)
    }
}

/// The input and output schema of one tool, for the completeness check.
#[derive(Debug, Clone)]
pub struct ToolSchemas {
    /// Tool name.
    pub tool: &'static str,
    /// Schema of the request arguments.
    pub input: Schema,
    /// Schema of the serialized result.
    pub output: Schema,
}

/// Tools without arguments take an empty object.
#[derive(Debug, Default, serde::Deserialize, JsonSchema)]
struct Empty {}

fn schemas<I: JsonSchema, O: JsonSchema>(tool: &'static str) -> ToolSchemas {
    ToolSchemas {
        tool,
        input: schema_for!(I),
        output: schema_for!(O),
    }
}

/// Entries of [`tool_schemas`] that are resource bodies gated through the registry and not
/// tools: `crates/kaban/tests/mcp_control.rs` subtracts them before comparing with the live
/// `tools/list` (review RC-4; CG7.3).
pub const RESOURCE_ONLY: &[&str] = &["code_index"];

/// Entries of [`tool_schemas`] whose shape is reserved ahead of the tool (engine-spec EN-F50,
/// EN-E6; engine-plan E2.11): the rows and schemas are declared so the completeness checks
/// cover them now, but no handler exists and `tools/list` does not advertise them until the
/// filling task lands (`embed_submit`: E3.7). `crates/kaban/tests/mcp_control.rs` subtracts
/// them like [`RESOURCE_ONLY`].
pub const RESERVED: &[&str] = &["embed_submit"];

/// Entries of [`tool_schemas`] that are CLI verbs going through the seam (`run_tool`) and are
/// not MCP tools (E2.10 boundary review BR-8): machine layer maintenance is CLI-only.
/// Subtracted by `crates/kaban/tests/mcp_control.rs` like [`RESOURCE_ONLY`].
pub const CLI_ONLY: &[&str] = &["forget_layer", "reindex_layer"];

/// The schemas of every tool the adapters expose (the same request and response types the MCP
/// adapter dispatches on), plus the [`RESOURCE_ONLY`] bodies, the [`RESERVED`] shapes and the
/// [`CLI_ONLY`] verbs. `crates/kaban/tests/mcp_control.rs` asserts that the live `tools/list`
/// names equal this list minus those three exactly (review RC-4), so a tool added to the router
/// without a row here fails that test, and each tool-adding task extends this list. No entry is
/// named `ask`: the tool is absent in 3.0.0, not stubbed (OWN-68 SC-02; `tests/control.rs`).
pub fn tool_schemas() -> Vec<ToolSchemas> {
    memory::schemas()
        .into_iter()
        .chain(ingest::schemas())
        .chain(code::schemas())
        .collect()
}

/// Position of a scan inside a tool's request or response: tracks the JSON path so string
/// leaves can be matched against the registry. A walk without a scope treats every string as
/// content.
#[derive(Debug, Default)]
pub(crate) struct Walk {
    scope: Option<(String, Direction)>,
    path: String,
}

impl Walk {
    /// A walk over `tool`'s request or response.
    pub(crate) fn scoped(tool: &str, direction: Direction) -> Self {
        Self {
            scope: Some((tool.to_owned(), direction)),
            path: String::new(),
        }
    }

    /// Descend into object key `k`; returns the mark for [`Self::pop`].
    pub(crate) fn push_key(&mut self, k: &str) -> usize {
        let mark = self.path.len();
        if self.scope.is_some() {
            append_key(&mut self.path, k);
        }
        mark
    }

    /// Descend into an array element; returns the mark for [`Self::pop`].
    pub(crate) fn push_index(&mut self) -> usize {
        let mark = self.path.len();
        if self.scope.is_some() {
            append_index(&mut self.path);
        }
        mark
    }

    /// Return to the position recorded by a push.
    pub(crate) fn pop(&mut self, mark: usize) {
        self.path.truncate(mark);
    }

    /// Whether the string at the current position is a control value that passes its shape
    /// (and so is validated, not scanned).
    pub(crate) fn passes_control(&self, s: &str) -> bool {
        let Some((tool, direction)) = &self.scope else {
            return false;
        };
        passes(tool, *direction, &self.path, s)
    }

    /// Document-stream content in a next response, not a control value or scalar input.
    pub(crate) fn is_ingest_stream(&self) -> bool {
        matches!(&self.scope, Some((tool, Direction::Response)) if tool == "ingest_next")
            && matches!(self.path.as_str(), "chunk.text" | "chunks[].text")
    }

    /// Whether the current position is a secret control entry that is not the issuing
    /// response (review RC-5): such a value must not leave, whatever it holds.
    pub(crate) fn withholds_secret(&self) -> bool {
        let Some((tool, direction)) = &self.scope else {
            return false;
        };
        control_entry(tool, *direction, &self.path).is_some_and(|e| e.secret && !e.issuing)
    }
}
