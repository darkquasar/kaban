//! The snapshot schema (stage1-spec §4.1 exactly, plus `symbols.ident_terms` and `symbols_fts`
//! of cg-join-v1 §8) and the closed vocabularies its columns hold (stage1-plan CG2.2).
//!
//! Columns listed in cg-join-v1 §8 are stable within `CG-JOIN v1`; everything else (`snippets`,
//! `edge_types`, `enc_*`, `signature`, `doc`, `lines`) is v4-internal.

use std::fmt;
use std::str::FromStr;

use crate::error::ReadError;

/// The reader's schema version; a snapshot whose `meta.schema_version` major is above it is
/// `SnapshotTooNew` (stage1-spec §7).
pub const SCHEMA_VERSION: u32 = 1;

/// `meta.symbols_fts_version` written beside the FTS table (cg-join-v1 §8).
pub const SYMBOLS_FTS_VERSION: u32 = 1;

/// The fixed page size of every snapshot (G1-N5).
pub const PAGE_SIZE: u32 = 4096;

/// The DDL of stage1-spec §4.1 with the `ident_terms` column and `symbols_fts` (cg-join-v1 §8).
/// `PRAGMA page_size` is set by `write::begin` before this runs (it must precede the first page).
pub const DDL: &str = "\
CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT);
CREATE TABLE edge_types(type TEXT PRIMARY KEY, source TEXT, confidence TEXT);
CREATE TABLE files(id INTEGER PRIMARY KEY, path TEXT UNIQUE, language TEXT, is_test INTEGER, lines INTEGER);
CREATE TABLE symbols(id INTEGER PRIMARY KEY, kind TEXT, ref TEXT UNIQUE, scip TEXT NULL, key TEXT NULL, label TEXT, name TEXT,
        file_id INTEGER NULL, def_sl INTEGER NULL, def_sc INTEGER NULL, def_el INTEGER NULL, def_ec INTEGER NULL,
        enc_sl INTEGER NULL, enc_sc INTEGER NULL, enc_el INTEGER NULL, enc_ec INTEGER NULL,
        signature TEXT NULL, doc TEXT NULL, external INTEGER, is_test INTEGER,
        ident_terms TEXT NOT NULL DEFAULT '');
CREATE INDEX symbols_key ON symbols(key);
CREATE INDEX symbols_label_name ON symbols(label, name);
CREATE INDEX symbols_file_id ON symbols(file_id);
CREATE INDEX symbols_name ON symbols(name);
CREATE TABLE edges(src INTEGER, dst INTEGER, type TEXT, weight INTEGER, PRIMARY KEY(src, type, dst));
CREATE INDEX edges_dst_type_src ON edges(dst, type, src);
CREATE TABLE occurrences(symbol_id INTEGER, file_id INTEGER, sl INTEGER, sc INTEGER, el INTEGER, ec INTEGER, roles INTEGER, enclosing_id INTEGER NULL);
CREATE INDEX occurrences_symbol_file ON occurrences(symbol_id, file_id);
CREATE INDEX occurrences_file_sl ON occurrences(file_id, sl);
CREATE TABLE snippets(symbol_id INTEGER PRIMARY KEY, text TEXT);
CREATE VIRTUAL TABLE symbols_fts USING fts5(name, key, label, ident_terms, content='symbols', content_rowid='id', tokenize='unicode61');
";

/// A `symbols.id`: every edge endpoint, occurrence symbol and enclosing node (RS-4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeId(pub i64);

/// A `files.id`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FileId(pub i64);

/// A source range in SCIP coordinates (0-based lines and columns, end exclusive).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Range {
    /// Start line.
    pub sl: u32,
    /// Start column.
    pub sc: u32,
    /// End line.
    pub el: u32,
    /// End column.
    pub ec: u32,
}

/// The closed `symbols.label` vocabulary (stage1-spec §6.1 `label`, G1-S2): every
/// `SymbolInformation.Kind` name of the pinned `scip.proto` 0.10.0, in declaration order. `File`
/// (the file node's label) is one of them. `kaban-api` never links `scip`, so the list is pinned
/// here and `kaban-index/tests/labels.rs` asserts it against `Kind::enum_descriptor()`.
pub const SCIP_KIND_NAMES: &[&str] = &[
    "UnspecifiedKind",
    "AbstractMethod",
    "Accessor",
    "Array",
    "Assertion",
    "AssociatedType",
    "Attribute",
    "Axiom",
    "Boolean",
    "Class",
    "Concept",
    "Constant",
    "Constructor",
    "Contract",
    "DataFamily",
    "Delegate",
    "Enum",
    "EnumMember",
    "Error",
    "Event",
    "Extension",
    "Fact",
    "Field",
    "File",
    "Function",
    "Getter",
    "Grammar",
    "Instance",
    "Interface",
    "Key",
    "Lang",
    "Lemma",
    "Library",
    "Macro",
    "Method",
    "MethodAlias",
    "MethodReceiver",
    "MethodSpecification",
    "Message",
    "Mixin",
    "Modifier",
    "Module",
    "Namespace",
    "Null",
    "Number",
    "Object",
    "Operator",
    "Package",
    "PackageObject",
    "Parameter",
    "ParameterLabel",
    "Pattern",
    "Predicate",
    "Property",
    "Protocol",
    "ProtocolMethod",
    "PureVirtualMethod",
    "Quasiquoter",
    "SelfParameter",
    "Setter",
    "Signature",
    "SingletonClass",
    "SingletonMethod",
    "StaticDataMember",
    "StaticEvent",
    "StaticField",
    "StaticMethod",
    "StaticProperty",
    "StaticVariable",
    "String",
    "Struct",
    "Subscript",
    "Tactic",
    "Theorem",
    "ThisParameter",
    "Trait",
    "TraitMethod",
    "Type",
    "TypeAlias",
    "TypeClass",
    "TypeClassMethod",
    "TypeFamily",
    "TypeParameter",
    "Union",
    "Value",
    "Variable",
];

/// `symbols.kind` (stage1-spec §4.1: one tagged node table).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NodeKind {
    /// A global SCIP symbol (`ref = "scip:" || scip`).
    Symbol,
    /// A document-local symbol (`ref = "local:" || path || ":" || scip`).
    Local,
    /// A file node (`ref = "file:" || path`).
    File,
}

impl NodeKind {
    /// The column value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Symbol => "symbol",
            Self::Local => "local",
            Self::File => "file",
        }
    }
}

impl FromStr for NodeKind {
    type Err = ReadError;

    fn from_str(s: &str) -> Result<Self, ReadError> {
        match s {
            "symbol" => Ok(Self::Symbol),
            "local" => Ok(Self::Local),
            "file" => Ok(Self::File),
            _ => Err(ReadError::Malformed { column: "kind" }),
        }
    }
}

impl fmt::Display for NodeKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// `edges.type` (stage1-spec §4.3; no `EXTENDS`, OD-4). The variants are declared in the byte
/// order of their stored text, so the derived `Ord` is the §4.1 column order the writer checks
/// (`TESTED_BY` before `TYPE_OF`; RG2-2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum EdgeType {
    /// Enclosing symbol -> member (descriptor grammar).
    Contains,
    /// Symbol -> file node of its definition.
    DefinedIn,
    /// Implementation -> interface (`Relationship.is_implementation`, non-members).
    Implements,
    /// Member -> interface member (`Relationship.is_implementation`, both members).
    ImplementsMember,
    /// Enclosing symbol or file -> referenced symbol, `weight` = site count.
    References,
    /// Symbol -> test function (reference form) or file -> test file (naming form).
    TestedBy,
    /// Symbol -> its type definition.
    TypeOf,
}

impl EdgeType {
    /// Every edge type, in column (stored text) order.
    pub const ALL: [EdgeType; 7] = [
        Self::Contains,
        Self::DefinedIn,
        Self::Implements,
        Self::ImplementsMember,
        Self::References,
        Self::TestedBy,
        Self::TypeOf,
    ];

    /// The column value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Contains => "CONTAINS",
            Self::DefinedIn => "DEFINED_IN",
            Self::Implements => "IMPLEMENTS",
            Self::ImplementsMember => "IMPLEMENTS_MEMBER",
            Self::References => "REFERENCES",
            Self::TypeOf => "TYPE_OF",
            Self::TestedBy => "TESTED_BY",
        }
    }
}

impl FromStr for EdgeType {
    type Err = ReadError;

    fn from_str(s: &str) -> Result<Self, ReadError> {
        Self::ALL
            .into_iter()
            .find(|t| t.as_str() == s)
            .ok_or(ReadError::Malformed { column: "type" })
    }
}

impl fmt::Display for EdgeType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// `edge_types.confidence` (stage1-spec §4.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Confidence {
    /// Verified on the spike corpus or a committed fixture with an independent oracle.
    Measured,
    /// Defined by the spec, verified on fixtures only.
    Rule,
    /// May be wrong by construction; flagged in responses.
    Heuristic,
}

impl Confidence {
    /// The column value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Measured => "measured",
            Self::Rule => "rule",
            Self::Heuristic => "heuristic",
        }
    }
}

/// The SCIP occurrence role bit for a definition (`SymbolRole.Definition`); a site with this
/// bit set is not a reference (stage1-spec §4.3 `REFERENCES`).
pub const ROLE_DEFINITION: u32 = 1;

/// The standard SCIP `SymbolRole.Test` occurrence bit. A test-only site need not have
/// its own definition or test enclosing symbol; test exclusion also checks this bit.
pub const ROLE_TEST: u32 = 32;
