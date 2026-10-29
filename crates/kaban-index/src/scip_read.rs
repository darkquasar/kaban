//! Protobuf decode of an `index.scip` and the decode-time gate (stage1-spec G1-S1 step 1,
//! RS2-1, RS3-1; stage1-plan CG3.1).
//!
//! The raw `scip::types::Index` lives only inside this module: [`decode_and_gate`] parses the
//! bytes and immediately walks every string-typed field of the pinned schema (`scip` 0.10.0,
//! `scip.proto` as vendored there) through the repository [`Policy`], producing a [`GatedIndex`]
//! whose every string is a [`Gated`] value. Nothing else in the crate sees an ungated string;
//! the one value parsed from the raw text before the gate, the `project_root` identity, leaves
//! as an opaque [`crate::derive::ProjectRoot`] with no textual accessor (df23-6).
//!
//! [`INVENTORY`] is the exhaustive list of those fields with their [`FieldClass`]; the walker
//! names each entry through its constant, so a field that is listed but not walked is a
//! dead-code warning (`-D warnings` fails the build), and the descriptor-walk test in
//! `tests/gate.rs` fails when a string-typed field of the schema is absent from the list. A
//! pin move re-audits the list before it lands (G1-S1).

use crate::error::IndexError;
use crate::gate::FieldClass;
use kaban_policy::{Policy, Verdict};
use protobuf::Message;
use scip::types as raw;

/// One string-typed field of the pinned SCIP schema and the admission class it is gated under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Field {
    /// The protobuf message name as in `scip.proto` (`SymbolInformation`).
    pub message: &'static str,
    /// The field name as in `scip.proto` (`enclosing_symbol`).
    pub field: &'static str,
    /// Structural, free or meta (G1-S1).
    pub class: FieldClass,
}

const fn field(message: &'static str, field: &'static str, class: FieldClass) -> Field {
    Field {
        message,
        field,
        class,
    }
}

// Metadata (meta: kaban-built or indexer-reported configuration; a hit refuses the run)
const TOOL_NAME: Field = field("ToolInfo", "name", FieldClass::Meta);
const TOOL_VERSION: Field = field("ToolInfo", "version", FieldClass::Meta);
const TOOL_ARGUMENTS: Field = field("ToolInfo", "arguments", FieldClass::Meta);
const PROJECT_ROOT: Field = field("Metadata", "project_root", FieldClass::Meta);
// Document
const DOC_LANGUAGE: Field = field("Document", "language", FieldClass::Structural);
const DOC_RELATIVE_PATH: Field = field("Document", "relative_path", FieldClass::Structural);
const DOC_TEXT: Field = field("Document", "text", FieldClass::Free);
// SymbolInformation (documents and external_symbols)
const SYM_SYMBOL: Field = field("SymbolInformation", "symbol", FieldClass::Structural);
const SYM_DOCUMENTATION: Field = field("SymbolInformation", "documentation", FieldClass::Free);
const SYM_DISPLAY_NAME: Field = field("SymbolInformation", "display_name", FieldClass::Structural);
const SYM_ENCLOSING_SYMBOL: Field = field(
    "SymbolInformation",
    "enclosing_symbol",
    FieldClass::Structural,
);
const SIG_LANGUAGE: Field = field("Signature", "language", FieldClass::Free);
const SIG_TEXT: Field = field("Signature", "text", FieldClass::Free);
const REL_SYMBOL: Field = field("Relationship", "symbol", FieldClass::Structural);
// Occurrence (documents and signature occurrences)
const OCC_SYMBOL: Field = field("Occurrence", "symbol", FieldClass::Structural);
const OCC_OVERRIDE_DOCUMENTATION: Field =
    field("Occurrence", "override_documentation", FieldClass::Free);
const DIAG_CODE: Field = field("Diagnostic", "code", FieldClass::Free);
const DIAG_MESSAGE: Field = field("Diagnostic", "message", FieldClass::Free);
const DIAG_SOURCE: Field = field("Diagnostic", "source", FieldClass::Free);

/// Every string-typed field (`string` and `repeated string`) reachable from `scip.Index` in the
/// pinned schema, with its class. The RS3-1 list plus what the descriptor walk reveals
/// (`Document.text`, `Occurrence.override_documentation`, `Diagnostic.{code, source}`,
/// `Signature.language`); `Symbol`, `Package` and `Descriptor` are helper messages not reachable
/// from `Index` and are not part of the index.
pub const INVENTORY: &[Field] = &[
    TOOL_NAME,
    TOOL_VERSION,
    TOOL_ARGUMENTS,
    PROJECT_ROOT,
    DOC_LANGUAGE,
    DOC_RELATIVE_PATH,
    DOC_TEXT,
    SYM_SYMBOL,
    SYM_DOCUMENTATION,
    SYM_DISPLAY_NAME,
    SYM_ENCLOSING_SYMBOL,
    SIG_LANGUAGE,
    SIG_TEXT,
    REL_SYMBOL,
    OCC_SYMBOL,
    OCC_OVERRIDE_DOCUMENTATION,
    DIAG_CODE,
    DIAG_MESSAGE,
    DIAG_SOURCE,
];

/// The gate's verdict on one string (G1-S1: pass, redacted, hit). `Hit` carries rule ids only;
/// no variant carries the original value of a hit. The payload of `Pass` and `Redacted` is a
/// [`Scanned`] string, which only this module can make (RU1-2: a passing value cannot be forged
/// outside the gate).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Gated {
    /// No rule matched; the value as decoded.
    Pass(Scanned),
    /// Redact-action hits only; the masked value. Only a free field may keep it (`admit`).
    Redacted(Scanned),
    /// At least one reject-action hit (credential-class rules always reject, S1), or the
    /// value exceeded the policy's per-field length.
    Hit {
        /// The reject-action rule ids, deduplicated, in scan order.
        rule_ids: Vec<String>,
    },
}

/// A string the gate has scanned: the original of a pass or the masked text of a redaction.
/// Constructed only by [`decode_and_gate`] (private field); read through [`Scanned::as_str`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scanned(String);

impl Scanned {
    /// The scanned text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The decoded index after the decode-time gate: every string is a [`Gated`]. Constructed only
/// by [`decode_and_gate`]; the payload is read through the accessors (RU1-2: no public field,
/// so no caller can replace a document or a metadata value after the gate ran).
#[derive(Debug, Clone, PartialEq)]
pub struct GatedIndex {
    metadata: GatedMetadata,
    documents: Vec<GatedDocument>,
    external_symbols: Vec<GatedSymbolInformation>,
}

impl GatedIndex {
    /// `Index.metadata` (meta class).
    #[must_use]
    pub fn metadata(&self) -> &GatedMetadata {
        &self.metadata
    }

    /// `Index.documents`, in index order.
    #[must_use]
    pub fn documents(&self) -> &[GatedDocument] {
        &self.documents
    }

    /// `Index.external_symbols`, in index order.
    #[must_use]
    pub fn external_symbols(&self) -> &[GatedSymbolInformation] {
        &self.external_symbols
    }
}

/// `scip.Metadata` with its `ToolInfo`, gated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GatedMetadata {
    /// `ToolInfo.name`.
    pub tool_name: Gated,
    /// `ToolInfo.version`.
    pub tool_version: Gated,
    /// `ToolInfo.arguments[]`.
    pub tool_arguments: Vec<Gated>,
    /// `Metadata.project_root`, the text (masked on a hit; published and reported as such).
    pub project_root: Gated,
    /// `Metadata.project_root`, the identity the root check reads: parsed from the text
    /// before the gate, so a masked text cannot turn a valid root into a mismatch (df23-6).
    pub project_root_identity: crate::derive::ProjectRoot,
}

/// `scip.Document`, gated; `occurrences` and `symbols` keep their order.
#[derive(Debug, Clone, PartialEq)]
pub struct GatedDocument {
    /// `Document.language`.
    pub language: Gated,
    /// `Document.relative_path`.
    pub relative_path: Gated,
    /// `Document.text`: gated for completeness; never enters a snapshot (G1-S1).
    pub text: Gated,
    /// `Document.position_encoding` as reported.
    pub position_encoding: raw::PositionEncoding,
    /// `Document.occurrences`.
    pub occurrences: Vec<GatedOccurrence>,
    /// `Document.symbols`.
    pub symbols: Vec<GatedSymbolInformation>,
}

/// `scip.SymbolInformation`, gated.
#[derive(Debug, Clone, PartialEq)]
pub struct GatedSymbolInformation {
    /// `SymbolInformation.symbol`.
    pub symbol: Gated,
    /// `SymbolInformation.documentation[]`.
    pub documentation: Vec<Gated>,
    /// `SymbolInformation.relationships`.
    pub relationships: Vec<GatedRelationship>,
    /// `SymbolInformation.kind` (unknown values fold to `UnspecifiedKind`).
    pub kind: raw::symbol_information::Kind,
    /// `SymbolInformation.display_name`.
    pub display_name: Gated,
    /// `SymbolInformation.signature_documentation`, when present.
    pub signature_documentation: Option<GatedSignature>,
    /// `SymbolInformation.enclosing_symbol`.
    pub enclosing_symbol: Gated,
}

/// `scip.Signature` (`signature_documentation`), gated.
#[derive(Debug, Clone, PartialEq)]
pub struct GatedSignature {
    /// `Signature.language`.
    pub language: Gated,
    /// `Signature.text`.
    pub text: Gated,
    /// `Signature.occurrences`.
    pub occurrences: Vec<GatedOccurrence>,
}

/// `scip.Relationship`, gated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GatedRelationship {
    /// `Relationship.symbol` (the target).
    pub symbol: Gated,
    /// The relationship's kind flags.
    pub flags: RelationshipFlags,
}

/// The four independent kind flags of `scip.Relationship` as one small set.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct RelationshipFlags(u8);

impl RelationshipFlags {
    const REFERENCE: u8 = 1;
    const IMPLEMENTATION: u8 = 1 << 1;
    const TYPE_DEFINITION: u8 = 1 << 2;
    const DEFINITION: u8 = 1 << 3;

    fn from_raw(r: &raw::Relationship) -> Self {
        let mut bits = 0;
        for (on, bit) in [
            (r.is_reference, Self::REFERENCE),
            (r.is_implementation, Self::IMPLEMENTATION),
            (r.is_type_definition, Self::TYPE_DEFINITION),
            (r.is_definition, Self::DEFINITION),
        ] {
            if on {
                bits |= bit;
            }
        }
        Self(bits)
    }

    /// The flag set from its bits, in schema order: `is_reference` = 1, `is_implementation` =
    /// 2, `is_type_definition` = 4, `is_definition` = 8; higher bits are ignored (the CG3.2
    /// derivation-table test enumerates all 16 combinations; additive, GM3 deviation 2).
    #[must_use]
    pub const fn from_bits(bits: u8) -> Self {
        Self(
            bits & (Self::REFERENCE
                | Self::IMPLEMENTATION
                | Self::TYPE_DEFINITION
                | Self::DEFINITION),
        )
    }

    /// `Relationship.is_reference`.
    #[must_use]
    pub fn is_reference(self) -> bool {
        self.0 & Self::REFERENCE != 0
    }

    /// `Relationship.is_implementation`.
    #[must_use]
    pub fn is_implementation(self) -> bool {
        self.0 & Self::IMPLEMENTATION != 0
    }

    /// `Relationship.is_type_definition`.
    #[must_use]
    pub fn is_type_definition(self) -> bool {
        self.0 & Self::TYPE_DEFINITION != 0
    }

    /// `Relationship.is_definition`.
    #[must_use]
    pub fn is_definition(self) -> bool {
        self.0 & Self::DEFINITION != 0
    }
}

/// `scip.Occurrence`, gated; the integer ranges and roles as reported.
#[derive(Debug, Clone, PartialEq)]
pub struct GatedOccurrence {
    /// `Occurrence.range` (3 or 4 integers, SCIP convention).
    pub range: Vec<i32>,
    /// `Occurrence.symbol`.
    pub symbol: Gated,
    /// `Occurrence.symbol_roles` bitset.
    pub symbol_roles: i32,
    /// `Occurrence.override_documentation[]`.
    pub override_documentation: Vec<Gated>,
    /// `Occurrence.diagnostics`.
    pub diagnostics: Vec<GatedDiagnostic>,
    /// `Occurrence.enclosing_range`.
    pub enclosing_range: Vec<i32>,
    /// `Occurrence.typed_range`, when present (no strings).
    pub typed_range: Option<raw::occurrence::Typed_range>,
    /// `Occurrence.typed_enclosing_range`, when present (no strings).
    pub typed_enclosing_range: Option<raw::occurrence::Typed_enclosing_range>,
}

/// `scip.Diagnostic`, gated; never enters a snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GatedDiagnostic {
    /// `Diagnostic.code`.
    pub code: Gated,
    /// `Diagnostic.message`.
    pub message: Gated,
    /// `Diagnostic.source`.
    pub source: Gated,
}

/// Decode `bytes` as a `scip.Index` and gate every string of [`INVENTORY`] through `policy`.
///
/// Free fields are scanned in windows of the policy's per-field length (a documentation block
/// or `Document.text` may exceed it); structural and meta fields are scanned whole, so an
/// oversize value is a hit (`input-too-long`) and its row is skipped.
///
/// # Errors
/// [`IndexError::Decode`] when the bytes are not a `scip.Index`; the protobuf error text is
/// not carried.
pub fn decode_and_gate(bytes: &[u8], policy: &Policy) -> Result<GatedIndex, IndexError> {
    let index = raw::Index::parse_from_bytes(bytes).map_err(|_| IndexError::Decode)?;
    let gate = Gate { policy };
    Ok(gate.index(&index))
}

/// The walker: one method per message, each string through [`Gate::value`] under its field.
struct Gate<'p> {
    policy: &'p Policy,
}

impl Gate<'_> {
    fn value(&self, f: Field, s: &str) -> Gated {
        let verdict = match f.class {
            FieldClass::Free => self
                .policy
                .scan_windows(s, self.policy.file.defaults.max_field_chars),
            FieldClass::Structural | FieldClass::Meta => self.policy.scan(s),
        };
        gated_from(f.class, verdict, s)
    }

    fn values(&self, f: Field, ss: &[String]) -> Vec<Gated> {
        ss.iter().map(|s| self.value(f, s)).collect()
    }

    fn index(&self, i: &raw::Index) -> GatedIndex {
        GatedIndex {
            metadata: self.metadata(&i.metadata),
            documents: i.documents.iter().map(|d| self.document(d)).collect(),
            external_symbols: i.external_symbols.iter().map(|s| self.symbol(s)).collect(),
        }
    }

    fn metadata(&self, m: &raw::Metadata) -> GatedMetadata {
        GatedMetadata {
            tool_name: self.value(TOOL_NAME, &m.tool_info.name),
            tool_version: self.value(TOOL_VERSION, &m.tool_info.version),
            tool_arguments: self.values(TOOL_ARGUMENTS, &m.tool_info.arguments),
            project_root: self.value(PROJECT_ROOT, &m.project_root),
            project_root_identity: crate::derive::ProjectRoot::parse(&m.project_root),
        }
    }

    fn document(&self, d: &raw::Document) -> GatedDocument {
        GatedDocument {
            language: self.value(DOC_LANGUAGE, &d.language),
            relative_path: self.value(DOC_RELATIVE_PATH, &d.relative_path),
            text: self.value(DOC_TEXT, &d.text),
            position_encoding: d.position_encoding.enum_value_or_default(),
            occurrences: d.occurrences.iter().map(|o| self.occurrence(o)).collect(),
            symbols: d.symbols.iter().map(|s| self.symbol(s)).collect(),
        }
    }

    fn symbol(&self, s: &raw::SymbolInformation) -> GatedSymbolInformation {
        GatedSymbolInformation {
            symbol: self.value(SYM_SYMBOL, &s.symbol),
            documentation: self.values(SYM_DOCUMENTATION, &s.documentation),
            relationships: s
                .relationships
                .iter()
                .map(|r| self.relationship(r))
                .collect(),
            kind: s.kind.enum_value_or_default(),
            display_name: self.value(SYM_DISPLAY_NAME, &s.display_name),
            signature_documentation: s
                .signature_documentation
                .as_ref()
                .map(|sig| self.signature(sig)),
            enclosing_symbol: self.value(SYM_ENCLOSING_SYMBOL, &s.enclosing_symbol),
        }
    }

    fn signature(&self, sig: &raw::Signature) -> GatedSignature {
        GatedSignature {
            language: self.value(SIG_LANGUAGE, &sig.language),
            text: self.value(SIG_TEXT, &sig.text),
            occurrences: sig.occurrences.iter().map(|o| self.occurrence(o)).collect(),
        }
    }

    fn relationship(&self, r: &raw::Relationship) -> GatedRelationship {
        GatedRelationship {
            symbol: self.value(REL_SYMBOL, &r.symbol),
            flags: RelationshipFlags::from_raw(r),
        }
    }

    fn occurrence(&self, o: &raw::Occurrence) -> GatedOccurrence {
        GatedOccurrence {
            range: o.range.clone(),
            symbol: self.value(OCC_SYMBOL, &o.symbol),
            symbol_roles: o.symbol_roles,
            override_documentation: self
                .values(OCC_OVERRIDE_DOCUMENTATION, &o.override_documentation),
            diagnostics: o.diagnostics.iter().map(|d| self.diagnostic(d)).collect(),
            enclosing_range: o.enclosing_range.clone(),
            typed_range: o.typed_range.clone(),
            typed_enclosing_range: o.typed_enclosing_range.clone(),
        }
    }

    fn diagnostic(&self, d: &raw::Diagnostic) -> GatedDiagnostic {
        GatedDiagnostic {
            code: self.value(DIAG_CODE, &d.code),
            message: self.value(DIAG_MESSAGE, &d.message),
            source: self.value(DIAG_SOURCE, &d.source),
        }
    }
}

/// `Verdict` to `Gated`. A reject-action hit is a `Hit` with its rule ids. A redact-only hit
/// keeps the masked text for a free field; for a structural or meta field it is a `Hit` too
/// (G1-S1: redaction is for free fields only, an identity or a configuration value is never
/// partially rewritten), carrying every hit's rule id. No hit passes the original.
fn gated_from(class: FieldClass, v: Verdict, original: &str) -> Gated {
    if v.rejected {
        return Gated::Hit {
            rule_ids: v.reject_rules,
        };
    }
    if v.hits.is_empty() {
        return Gated::Pass(Scanned(original.to_owned()));
    }
    match class {
        FieldClass::Free => Gated::Redacted(Scanned(v.redacted)),
        FieldClass::Structural | FieldClass::Meta => {
            let mut rule_ids: Vec<String> = Vec::new();
            for h in v.hits {
                if !rule_ids.contains(&h.rule) {
                    rule_ids.push(h.rule);
                }
            }
            Gated::Hit { rule_ids }
        }
    }
}
