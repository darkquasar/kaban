//! The code-graph tools' control rows and schemas (`code_*` and the `kaban://code-index`
//! resource body): the closed value sets, the row builders and the rows themselves.

use super::{ControlEntry, Direction, Empty, SNAPSHOT_COMMIT, Shape, ToolSchemas, entry};
use crate::code::{
    CodeContextRequest, CodeContextResponse, CodeDefinitionRequest, CodeDefinitionResponse,
    CodeImplementersRequest, CodeIndexResponse, CodeListResponse, CodeMembersRequest,
    CodeNeighboursRequest, CodeNeighboursResponse, CodeReferencesRequest, CodeReferencesResponse,
    CodeTestsForRequest, CodeTestsForResponse,
};
use kaban_codegraph::SCIP_KIND_NAMES;

/// `policy` (stage1-spec §6.1; cg-join-v1 §4; DF-3): `current` when the snapshot's
/// `policy_version` equals the live policy's, `off` when the content scan was off at index
/// time, else `stale`: `kaban_codegraph::Policy::as_str` over its variants
/// (`tests/snapshot_provider.rs` pins the equality).
pub const POLICY_VALUES: &[&str] = &["current", "stale", "off"];

/// `isolation` (`snapshot.isolation`, the resource and the report; stage1-spec §6.1, G1-S5):
/// the `meta.isolation` tiers, `kaban_codegraph::IsolationTier::as_str` over its variants.
pub const ISOLATION_VALUES: &[&str] = &["namespaces", "container", "none"];

/// `kind` of a `CodeNode` (stage1-spec §6.1, G1-S2): `kaban_codegraph::NodeKind::as_str` over
/// its variants (stage 2 adds `overlay`). The memory tools' `kind` (a node kind name) stays
/// content; only the `code_*` tools carry this closed one.
pub const KIND_VALUES: &[&str] = &["symbol", "local", "file"];

/// `label` of a `CodeNode` and of a selector (stage1-spec §6.1): the SCIP kind names plus
/// `File`, pinned in `kaban_codegraph::schema` (CG7.1 default 5).
pub const LABEL_VALUES: &[&str] = SCIP_KIND_NAMES;

/// `language` of a `CodeNode` and `languages[]` of the `kaban://code-index` body (stage1-spec
/// §6.1, G1-F18): the indexed languages of 3.0.0.
pub const LANGUAGE_VALUES: &[&str] = &["go", "typescript", "javascript"];

/// `resources[]` of the `kaban://code-index` body (stage1-spec G1-F18, G1-S5; FIX-E): the
/// read-only resource kinds the indexer's runs were given, `meta.resources` split on `,`:
/// `kaban_index::language::Driver::resource_kinds` over its languages (Go: the module cache
/// and the module-cache toolchain; Rust: the caller's registry and the pinned toolchain).
pub const RESOURCE_VALUES: &[&str] = &["modcache", "registry", "toolchain"];

/// `ranking` of a list response (stage1-spec §6.1, G1-F16): `full` when the complete
/// candidate set was ordered before the cut, `partial` otherwise (not emitted in 3.0.0,
/// OWN-68 SC-10).
pub const RANKING_VALUES: &[&str] = &["full", "partial"];

/// `code_neighbours.edge_types[]` and `edges[].type` (stage1-spec §6.1, §4.3): the closed
/// edge-type names, `kaban_codegraph::EdgeType::ALL` in column order (`tests/code_tools.rs`
/// `edge_type_values_match_schema` pins the equality).
pub const EDGE_TYPE_VALUES: &[&str] = &[
    "CONTAINS",
    "DEFINED_IN",
    "IMPLEMENTS",
    "IMPLEMENTS_MEMBER",
    "REFERENCES",
    "TESTED_BY",
    "TYPE_OF",
];

/// `by_enclosing[].sites[].roles[]` of `code_references` (stage1-spec §6.1): the SCIP
/// `SymbolRole` names, bits 1 to 64 ascending (`Definition` is bit 1; a reference site never
/// carries it).
pub const ROLE_VALUES: &[&str] = &[
    "Definition",
    "Import",
    "WriteAccess",
    "ReadAccess",
    "Generated",
    "Test",
    "ForwardDefinition",
];

/// `tests[].matched_by` of `code_tests_for` (stage1-spec §6.1, G1-F6):
/// `kaban_codegraph::reader::MatchedBy::as_str` over its variants.
pub const MATCHED_BY_VALUES: &[&str] = &["reference", "naming"];

/// `tests[].how` of `code_tests_for` and `code_context` (TESTS-1):
/// `kaban_codegraph::How::as_str` over its variants.
pub const HOW_VALUES: &[&str] = &["direct", "via", "textual"];

/// The `snapshot` object every `code_*` response carries (stage1-spec G1-F8, §6.1).
const fn snapshot_rows(tool: &'static str) -> [ControlEntry; 3] {
    [
        entry(
            tool,
            Direction::Response,
            "snapshot.commit",
            SNAPSHOT_COMMIT,
        ),
        entry(
            tool,
            Direction::Response,
            "snapshot.policy",
            Shape::Enum(POLICY_VALUES),
        ),
        entry(
            tool,
            Direction::Response,
            "snapshot.isolation",
            Shape::Enum(ISOLATION_VALUES),
        ),
    ]
}

/// The enums of a `CodeNode` card under `prefix` (`results[]` or `definition`).
const fn card_rows(tool: &'static str, prefix: &CardPath) -> [ControlEntry; 3] {
    [
        entry(
            tool,
            Direction::Response,
            prefix.kind,
            Shape::Enum(KIND_VALUES),
        ),
        entry(
            tool,
            Direction::Response,
            prefix.label,
            Shape::Enum(LABEL_VALUES),
        ),
        entry(
            tool,
            Direction::Response,
            prefix.language,
            Shape::Enum(LANGUAGE_VALUES),
        ),
    ]
}

/// The three enum paths of a card at one position of a response.
struct CardPath {
    kind: &'static str,
    label: &'static str,
    language: &'static str,
}

/// A card inside `results[]` (the list tools).
const LIST_CARD: CardPath = CardPath {
    kind: "results[].kind",
    label: "results[].label",
    language: "results[].language",
};

/// The `definition` card (`code_definition`).
const DEFINITION_CARD: CardPath = CardPath {
    kind: "definition.kind",
    label: "definition.label",
    language: "definition.language",
};

/// The rows of one list tool's response: the snapshot, the card enums, `ranking`.
const fn list_rows(tool: &'static str) -> [ControlEntry; 7] {
    let snapshot = snapshot_rows(tool);
    let card = card_rows(tool, &LIST_CARD);
    [
        snapshot[0],
        snapshot[1],
        snapshot[2],
        card[0],
        card[1],
        card[2],
        entry(
            tool,
            Direction::Response,
            "ranking",
            Shape::Enum(RANKING_VALUES),
        ),
    ]
}

/// The enclosing card of a reference group (`code_references`).
const ENCLOSING_CARD: CardPath = CardPath {
    kind: "by_enclosing[].enclosing.kind",
    label: "by_enclosing[].enclosing.label",
    language: "by_enclosing[].enclosing.language",
};

/// The test card of a test link (`code_tests_for`).
const TEST_CARD: CardPath = CardPath {
    kind: "tests[].test.kind",
    label: "tests[].test.label",
    language: "tests[].test.language",
};

/// A card inside `nodes[]` (`code_neighbours`).
const NODE_CARD: CardPath = CardPath {
    kind: "nodes[].kind",
    label: "nodes[].label",
    language: "nodes[].language",
};

const CODE_IMPLEMENTERS_RESPONSE: [ControlEntry; 7] = list_rows("code_implementers");
const CODE_MEMBERS_RESPONSE: [ControlEntry; 7] = list_rows("code_members");
const CODE_DEFINITION_SNAPSHOT: [ControlEntry; 3] = snapshot_rows("code_definition");
const CODE_DEFINITION_CARD: [ControlEntry; 3] = card_rows("code_definition", &DEFINITION_CARD);
const CODE_REFERENCES_SNAPSHOT: [ControlEntry; 3] = snapshot_rows("code_references");
const CODE_REFERENCES_CARD: [ControlEntry; 3] = card_rows("code_references", &ENCLOSING_CARD);
const CODE_TESTS_FOR_SNAPSHOT: [ControlEntry; 3] = snapshot_rows("code_tests_for");
const CODE_TESTS_FOR_CARD: [ControlEntry; 3] = card_rows("code_tests_for", &TEST_CARD);
const CODE_NEIGHBOURS_SNAPSHOT: [ControlEntry; 3] = snapshot_rows("code_neighbours");
const CODE_NEIGHBOURS_CARD: [ControlEntry; 3] = card_rows("code_neighbours", &NODE_CARD);
const CODE_INDEX_SNAPSHOT: [ControlEntry; 3] = snapshot_rows("code_index");

/// A card inside `callers[]` (`code_context`).
const CALLERS_CARD: CardPath = CardPath {
    kind: "callers[].kind",
    label: "callers[].label",
    language: "callers[].language",
};

/// A card inside `callees[]` (`code_context`).
const CALLEES_CARD: CardPath = CardPath {
    kind: "callees[].kind",
    label: "callees[].label",
    language: "callees[].language",
};

const CODE_CONTEXT_SNAPSHOT: [ControlEntry; 3] = snapshot_rows("code_context");
const CODE_CONTEXT_DEFINITION: [ControlEntry; 3] = card_rows("code_context", &DEFINITION_CARD);
const CODE_CONTEXT_CALLERS: [ControlEntry; 3] = card_rows("code_context", &CALLERS_CARD);
const CODE_CONTEXT_CALLEES: [ControlEntry; 3] = card_rows("code_context", &CALLEES_CARD);
const CODE_CONTEXT_TEST: [ControlEntry; 3] = card_rows("code_context", &TEST_CARD);

/// The code-graph tools' rows of [`super::REGISTRY`].
pub(super) const ROWS: &[ControlEntry] = &[
    // code-graph tools (stage1-spec §6.1, G1-S2; CG7.1): the selector's `label` and
    // `code_members.labels[]` on the way in, the snapshot ref and the card enums on the way out
    entry(
        "code_implementers",
        Direction::Request,
        "target.label",
        Shape::Enum(LABEL_VALUES),
    ),
    entry(
        "code_members",
        Direction::Request,
        "target.label",
        Shape::Enum(LABEL_VALUES),
    ),
    entry(
        "code_members",
        Direction::Request,
        "labels[]",
        Shape::Enum(LABEL_VALUES),
    ),
    entry(
        "code_definition",
        Direction::Request,
        "target.label",
        Shape::Enum(LABEL_VALUES),
    ),
    CODE_IMPLEMENTERS_RESPONSE[0],
    CODE_IMPLEMENTERS_RESPONSE[1],
    CODE_IMPLEMENTERS_RESPONSE[2],
    CODE_IMPLEMENTERS_RESPONSE[3],
    CODE_IMPLEMENTERS_RESPONSE[4],
    CODE_IMPLEMENTERS_RESPONSE[5],
    CODE_IMPLEMENTERS_RESPONSE[6],
    CODE_MEMBERS_RESPONSE[0],
    CODE_MEMBERS_RESPONSE[1],
    CODE_MEMBERS_RESPONSE[2],
    CODE_MEMBERS_RESPONSE[3],
    CODE_MEMBERS_RESPONSE[4],
    CODE_MEMBERS_RESPONSE[5],
    CODE_MEMBERS_RESPONSE[6],
    CODE_DEFINITION_SNAPSHOT[0],
    CODE_DEFINITION_SNAPSHOT[1],
    CODE_DEFINITION_SNAPSHOT[2],
    CODE_DEFINITION_CARD[0],
    CODE_DEFINITION_CARD[1],
    CODE_DEFINITION_CARD[2],
    // CG7.2: the selector's `label` and `code_neighbours.edge_types[]` on the way in; the
    // snapshot ref, the card enums at each card position, `roles[]`, `matched_by` and the edge
    // `type` on the way out
    entry(
        "code_references",
        Direction::Request,
        "target.label",
        Shape::Enum(LABEL_VALUES),
    ),
    entry(
        "code_tests_for",
        Direction::Request,
        "target.label",
        Shape::Enum(LABEL_VALUES),
    ),
    entry(
        "code_neighbours",
        Direction::Request,
        "target.label",
        Shape::Enum(LABEL_VALUES),
    ),
    entry(
        "code_neighbours",
        Direction::Request,
        "edge_types[]",
        Shape::Enum(EDGE_TYPE_VALUES),
    ),
    CODE_REFERENCES_SNAPSHOT[0],
    CODE_REFERENCES_SNAPSHOT[1],
    CODE_REFERENCES_SNAPSHOT[2],
    CODE_REFERENCES_CARD[0],
    CODE_REFERENCES_CARD[1],
    CODE_REFERENCES_CARD[2],
    entry(
        "code_references",
        Direction::Response,
        "by_enclosing[].sites[].roles[]",
        Shape::Enum(ROLE_VALUES),
    ),
    CODE_TESTS_FOR_SNAPSHOT[0],
    CODE_TESTS_FOR_SNAPSHOT[1],
    CODE_TESTS_FOR_SNAPSHOT[2],
    CODE_TESTS_FOR_CARD[0],
    CODE_TESTS_FOR_CARD[1],
    CODE_TESTS_FOR_CARD[2],
    entry(
        "code_tests_for",
        Direction::Response,
        "tests[].matched_by",
        Shape::Enum(MATCHED_BY_VALUES),
    ),
    entry(
        "code_tests_for",
        Direction::Response,
        "tests[].how",
        Shape::Enum(HOW_VALUES),
    ),
    CODE_NEIGHBOURS_SNAPSHOT[0],
    CODE_NEIGHBOURS_SNAPSHOT[1],
    CODE_NEIGHBOURS_SNAPSHOT[2],
    CODE_NEIGHBOURS_CARD[0],
    CODE_NEIGHBOURS_CARD[1],
    CODE_NEIGHBOURS_CARD[2],
    entry(
        "code_neighbours",
        Direction::Response,
        "edges[].type",
        Shape::Enum(EDGE_TYPE_VALUES),
    ),
    // CG7.3: the `kaban://code-index` body (resource only: no request side)
    CODE_INDEX_SNAPSHOT[0],
    CODE_INDEX_SNAPSHOT[1],
    CODE_INDEX_SNAPSHOT[2],
    entry(
        "code_index",
        Direction::Response,
        "languages[]",
        Shape::Enum(LANGUAGE_VALUES),
    ),
    entry(
        "code_index",
        Direction::Response,
        "resources[]",
        Shape::Enum(RESOURCE_VALUES),
    ),
    // CG9.1 `code_context` (stage1-spec G1-F14, §6.1): the selector's `label` on the way in;
    // the snapshot ref, the card enums at the four card positions, `tests[].matched_by` and
    // `last_change.commit` on the way out
    // (`approximate` is a bool, `author_date` and `summary` are content: no row)
    entry(
        "code_context",
        Direction::Request,
        "target.label",
        Shape::Enum(LABEL_VALUES),
    ),
    CODE_CONTEXT_SNAPSHOT[0],
    CODE_CONTEXT_SNAPSHOT[1],
    CODE_CONTEXT_SNAPSHOT[2],
    CODE_CONTEXT_DEFINITION[0],
    CODE_CONTEXT_DEFINITION[1],
    CODE_CONTEXT_DEFINITION[2],
    CODE_CONTEXT_CALLERS[0],
    CODE_CONTEXT_CALLERS[1],
    CODE_CONTEXT_CALLERS[2],
    CODE_CONTEXT_CALLEES[0],
    CODE_CONTEXT_CALLEES[1],
    CODE_CONTEXT_CALLEES[2],
    CODE_CONTEXT_TEST[0],
    CODE_CONTEXT_TEST[1],
    CODE_CONTEXT_TEST[2],
    entry(
        "code_context",
        Direction::Response,
        "tests[].matched_by",
        Shape::Enum(MATCHED_BY_VALUES),
    ),
    entry(
        "code_context",
        Direction::Response,
        "tests[].how",
        Shape::Enum(HOW_VALUES),
    ),
    entry(
        "code_context",
        Direction::Response,
        "last_change.commit",
        SNAPSHOT_COMMIT,
    ),
];

/// The code-graph tools' entries of [`super::tool_schemas`].
pub(super) fn schemas() -> Vec<ToolSchemas> {
    vec![
        // code graph CG7.1 (behind the `kaban` crate's `code-graph` feature; the adapter test
        // runs with default features)
        super::schemas::<CodeImplementersRequest, CodeListResponse>("code_implementers"),
        super::schemas::<CodeMembersRequest, CodeListResponse>("code_members"),
        super::schemas::<CodeDefinitionRequest, CodeDefinitionResponse>("code_definition"),
        // code graph CG7.2
        super::schemas::<CodeReferencesRequest, CodeReferencesResponse>("code_references"),
        super::schemas::<CodeTestsForRequest, CodeTestsForResponse>("code_tests_for"),
        super::schemas::<CodeNeighboursRequest, CodeNeighboursResponse>("code_neighbours"),
        // code graph CG7.3: the `kaban://code-index` resource body (`RESOURCE_ONLY`)
        super::schemas::<Empty, CodeIndexResponse>("code_index"),
        // code graph CG9.1
        super::schemas::<CodeContextRequest, CodeContextResponse>("code_context"),
    ]
}
