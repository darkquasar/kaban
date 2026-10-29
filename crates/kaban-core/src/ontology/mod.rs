//! Ontologies as data (engine-spec §3.1, EN-F1, EN-F7, EN-S11; engine-plan E1.2): the file form
//! ([`OntologyFile`], [`meta`]), qualified and stored names, and the resolution of a set of files
//! into one [`Ontology`] ([`resolve`]): `extends` visibility, bare-name lookup order, single
//! `is_a` inheritance, mixins in list order and the conflict rules. Nothing here reaches a
//! store, the network or a `$ref`: `prefixes` are opaque strings and `extends` names only
//! loaded namespaces.

pub mod meta;
pub mod resolve;

pub use meta::{
    DirName, IdentifierError, Index, KindBody, KindName, Ns, OntologyFile, RelBody, RelName, Stamp,
};
pub use resolve::{NamespaceSet, Ontology, Qualified, resolve, split_stored, stored_name};

use crate::validator::SchemaError;

/// Why an ontology failed to load or resolve (EN-F7). `Display` is the fixed text of the list
/// [`ONTOLOGY_ERROR_TEXTS`]; the payload is a class and the dotted path of the field (a map key
/// is its own path element), never an offending value, so `Debug` is safe to log (E1.3a). The
/// conflict and builtin variants carry the declared qualified name the spec asks for (EN-F1).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OntologyError {
    /// Two declarations of one `(ns, name)` with different bodies, an `is_a` cycle, a child
    /// retyping or removing an inherited property, a mixin naming `is_a` or `mixins`, a key set
    /// by two mixins.
    #[error("ontology_conflict")]
    Conflict {
        /// The offending qualified name.
        name: Qualified,
        /// Which rule (`duplicate`, `is_a_cycle`, `retype`, `mixin_is_a`, `mixin_key`, ...).
        detail: String,
    },
    /// A namespace, kind or rel name failing its regex.
    #[error("invalid_identifier")]
    InvalidIdentifier {
        /// The dotted path of the field (`ontology`, `extends`, `kinds.<name>`,
        /// `rels.<name>.from`, ...).
        path: String,
    },
    /// A `props_schema` keyword outside the EN-F3 subset (`$ref` included).
    #[error("schema_unsupported:{keyword}")]
    Unsupported {
        /// The keyword.
        keyword: String,
    },
    /// A malformed `props_schema` (`schema_invalid:<rule>`) or a malformed file
    /// (`schema_invalid:file:<dotted path>`).
    #[error("schema_invalid:{rule}")]
    SchemaInvalid {
        /// The rule or `file:<path>`.
        rule: String,
    },
    /// A value outside its engine-spec §3.1 admission range (`version >= 1`,
    /// `rank.expand_depth <= 4`, question `depth <= 3`, `priors`, `min_confidence` and
    /// `confidence_required` in `[0, 1]`): `schema_invalid:range:<dotted path>`.
    #[error("schema_invalid:range:{path}")]
    OutOfRange {
        /// The dotted path of the field (never its value).
        path: String,
    },
    /// `extends` or a qualified name refers to a namespace that is neither in the files nor
    /// loaded.
    #[error("unknown_namespace")]
    UnknownNamespace {
        /// The dotted path of the reference.
        path: String,
    },
    /// A bare name resolvable in two namespaces.
    #[error("ambiguous_kind")]
    AmbiguousKind {
        /// The dotted path of the reference.
        path: String,
    },
    /// A kind reference no namespace in the lookup order declares.
    #[error("unknown_kind")]
    UnknownKind {
        /// The dotted path of the reference.
        path: String,
    },
    /// A rel reference no namespace declares.
    #[error("unknown_rel")]
    UnknownRel {
        /// The dotted path of the reference.
        path: String,
    },
    /// A user file redefining a builtin (raised by the E1.3 seam).
    #[error("ontology_builtin_immutable")]
    BuiltinImmutable(Qualified),
    /// The discovered-labels cap is reached (raised by E1.8).
    #[error("discovered_cap_reached")]
    DiscoveredCapReached,
}

/// The EN-F7 texts, exit 4 each; `<kw>` and `<rule>` are templates. A test iterates every
/// `Display` against this list.
pub const ONTOLOGY_ERROR_TEXTS: &[&str] = &[
    "ontology_conflict",
    "invalid_identifier",
    "schema_unsupported:<kw>",
    "schema_invalid:<rule>",
    "ontology_builtin_immutable",
    "unknown_kind",
    "unknown_rel",
    "unknown_namespace",
    "ambiguous_kind",
    "discovered_cap_reached",
];

impl From<SchemaError> for OntologyError {
    fn from(e: SchemaError) -> Self {
        match e {
            SchemaError::Unsupported { keyword, .. } => OntologyError::Unsupported { keyword },
            SchemaError::Invalid { rule, .. } => OntologyError::SchemaInvalid { rule },
        }
    }
}
