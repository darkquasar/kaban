//! Admission policy over gated values (stage1-spec G1-S1; stage1-plan CG3.1).
//!
//! [`admit`] is the per-value rule: a structural hit skips the owning row
//! (`policy_skipped_row`), a free field keeps a redacted value or is dropped on reject
//! (`policy_dropped`), a meta hit is masked and counted (`policy_redacted_meta`, DF-3: never a
//! refusal). [`admit_index`]
//! applies it over a whole [`GatedIndex`]: files, symbols, relationships and occurrences that
//! survive, with the row dependents of a skipped row gone (a relationship or occurrence whose
//! target row was skipped is dropped with the owning row's class and never creates a
//! placeholder). The result borrows from the `GatedIndex`; derivation (CG3.2) works on it and
//! never on a raw value.
//!
//! RU1-2: `Admitted` and its rows have no public field; derivation reads them through accessors
//! and nothing outside this module can build one. RU1-4: the identities the policy rejected are
//! carried in the `Admitted` index (`is_rejected`), so a second run's relationship towards a
//! rejected symbol is never admitted as a union endpoint or a placeholder. The formed-string
//! rules of G1-S1 step 3 ([`formed_structural_ok`], [`formed_free`], [`formed_meta`]) live here
//! too: every string composed after admission (a joined documentation block, a `file:` ref, the
//! `indexer:<lang>` text, a caller-supplied `meta` value) is re-scanned under its class before
//! it is written or reported (RU1-1).

use crate::scip_read::{
    Gated, GatedDocument, GatedIndex, GatedOccurrence, GatedRelationship, GatedSymbolInformation,
    RelationshipFlags,
};
use kaban_policy::Policy;
use serde::Serialize;
use std::collections::HashSet;

/// How a string-typed field is admitted after the gate (G1-S1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FieldClass {
    /// Path- or symbol-bearing (`relative_path`, `symbol`, `enclosing_symbol`, `display_name`,
    /// `Relationship.symbol`, `Occurrence.symbol`, `language`): any hit skips the owning row.
    Structural,
    /// Signature, documentation, snippet-like content: redact keeps the masked value, reject
    /// drops the field.
    Free,
    /// Indexer-reported configuration (`tool_info`, `project_root`): a hit is masked (the
    /// policy's redacted text, or `[policy:<rule-id>]` on a reject hit) and counted.
    Meta,
}

/// The fixed warning classes of the JSON report and `meta.counts` (G1-F7, G1-S1). Serialised as
/// the plain `snake_case` class name (externally tagged unit variants: the wire form is the
/// string `"policy_skipped_row"`, which is what the report and the spec's class vocabulary use).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WarningClass {
    /// A structural hit skipped a whole row and its dependents.
    PolicySkippedRow,
    /// A reject-action hit dropped a free field.
    PolicyDropped,
    /// A meta value was masked (DF-3: redacted and counted, never a refusal).
    PolicyRedactedMeta,
    /// A committed Rust file whose source hit the policy was classified by its path only.
    PolicySkippedSource,
    /// A committed Rust file over the classifier's per-file bound was classified by its path
    /// only (DF-3).
    SourceClassifiedByPath,
}

/// What happens to one gated value under its field class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission<'a> {
    /// Use this value (the original, or the masked value of a free field).
    Admit(&'a str),
    /// A free field with a reject hit: the field is omitted (`policy_dropped`).
    DropField,
    /// A structural hit: the owning row and its dependents are skipped (`policy_skipped_row`).
    SkipOwningRow,
    /// A meta value with a redact-only hit: use the masked text (`policy_redacted_meta`).
    RedactedMeta(&'a str),
    /// A meta value with a reject hit: write [`policy_mask`] of these rule ids instead
    /// (`policy_redacted_meta`); no variant carries the value.
    MaskedMeta(&'a [String]),
}

impl Admission<'_> {
    /// The warning class this admission counts under, if any.
    #[must_use]
    pub fn warning(&self) -> Option<WarningClass> {
        match self {
            Self::Admit(_) => None,
            Self::DropField => Some(WarningClass::PolicyDropped),
            Self::SkipOwningRow => Some(WarningClass::PolicySkippedRow),
            Self::RedactedMeta(_) | Self::MaskedMeta(_) => Some(WarningClass::PolicyRedactedMeta),
        }
    }
}

/// The per-value admission rule of G1-S1.
#[must_use]
pub fn admit(class: FieldClass, g: &Gated) -> Admission<'_> {
    match (class, g) {
        (_, Gated::Pass(s)) | (FieldClass::Free, Gated::Redacted(s)) => {
            Admission::Admit(s.as_str())
        }
        (FieldClass::Free, Gated::Hit { .. }) => Admission::DropField,
        (FieldClass::Structural, Gated::Redacted(_) | Gated::Hit { .. }) => {
            Admission::SkipOwningRow
        }
        (FieldClass::Meta, Gated::Redacted(s)) => Admission::RedactedMeta(s.as_str()),
        (FieldClass::Meta, Gated::Hit { rule_ids }) => Admission::MaskedMeta(rule_ids),
    }
}

/// The fixed text written in place of a meta value with a reject hit: `[policy:<rule-id>]`
/// (several ids comma-joined). Rule ids only, never the value.
#[must_use]
pub fn policy_mask(rule_ids: &[String]) -> String {
    format!("[policy:{}]", rule_ids.join(","))
}

/// The counters behind the warning classes (`meta.counts` and the JSON report).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct GateCounts {
    /// Rows skipped by a structural hit, dependents included one each.
    pub policy_skipped_row: u64,
    /// Free fields dropped by a reject hit.
    pub policy_dropped: u64,
    /// Meta values masked by a hit (redact or reject).
    pub policy_redacted_meta: u64,
}

impl GateCounts {
    fn record(&mut self, a: &Admission<'_>) {
        match a.warning() {
            Some(WarningClass::PolicySkippedRow) => self.policy_skipped_row += 1,
            Some(WarningClass::PolicyDropped) => self.policy_dropped += 1,
            Some(WarningClass::PolicyRedactedMeta) => self.policy_redacted_meta += 1,
            Some(WarningClass::PolicySkippedSource | WarningClass::SourceClassifiedByPath)
            | None => {}
        }
    }

    /// The warning classes with a non-zero count, in the fixed order of the report.
    #[must_use]
    pub fn warnings(&self) -> Vec<WarningClass> {
        let mut out = Vec::new();
        if self.policy_skipped_row > 0 {
            out.push(WarningClass::PolicySkippedRow);
        }
        if self.policy_dropped > 0 {
            out.push(WarningClass::PolicyDropped);
        }
        if self.policy_redacted_meta > 0 {
            out.push(WarningClass::PolicyRedactedMeta);
        }
        out
    }
}

/// The index after admission: what derivation and the writer may use. Read-only outside this
/// module (RU1-2); built only by [`admit_index`].
#[derive(Debug, Clone, PartialEq)]
pub struct Admitted<'a> {
    metadata: AdmittedMetadata,
    files: Vec<AdmittedFile<'a>>,
    external_symbols: Vec<AdmittedSymbol<'a>>,
    counts: GateCounts,
    /// The rule ids of every masked meta value (reject hits name their rules), deduplicated.
    meta_rule_ids: Vec<String>,
    /// The symbol strings of every row the policy skipped (RU1-4: carried forward so a
    /// dependent from another run never resurrects a rejected identity).
    rejected: HashSet<&'a str>,
}

impl<'a> Admitted<'a> {
    /// The producer's `Metadata` (a value with a hit is masked, DF-3).
    #[must_use]
    pub fn metadata(&self) -> &AdmittedMetadata {
        &self.metadata
    }

    /// The rule ids behind `counts.policy_redacted_meta` (reject hits; a redact-only hit is
    /// counted without a rule id). Safe to report.
    #[must_use]
    pub fn policy_redacted_meta_rules(&self) -> &[String] {
        &self.meta_rule_ids
    }

    /// Documents whose path and language passed, in index order.
    #[must_use]
    pub fn files(&self) -> &[AdmittedFile<'a>] {
        &self.files
    }

    /// `external_symbols` rows that passed.
    #[must_use]
    pub fn external_symbols(&self) -> &[AdmittedSymbol<'a>] {
        &self.external_symbols
    }

    /// The counters.
    #[must_use]
    pub fn counts(&self) -> GateCounts {
        self.counts
    }

    /// Every surviving `SymbolInformation` row, documents first, then `external_symbols`.
    pub fn symbols(&self) -> impl Iterator<Item = &AdmittedSymbol<'a>> {
        self.files
            .iter()
            .flat_map(|f| f.symbols.iter())
            .chain(self.external_symbols.iter())
    }

    /// `true` when the policy skipped a row whose symbol string is `symbol` (RU1-4).
    #[must_use]
    pub fn is_rejected(&self, symbol: &str) -> bool {
        self.rejected.contains(symbol)
    }
}

/// The gated `Metadata` after admission (the writer's `indexer:<lang>` text); a value with a
/// hit holds its masked text (DF-3). The root check reads the identity parsed before the gate
/// (`GatedMetadata::project_root_identity`, df23-6), not this text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdmittedMetadata {
    tool_name: String,
    tool_version: String,
    tool_arguments: Vec<String>,
    project_root: String,
}

impl AdmittedMetadata {
    /// `ToolInfo.name`.
    #[must_use]
    pub fn tool_name(&self) -> &str {
        &self.tool_name
    }

    /// `ToolInfo.version`.
    #[must_use]
    pub fn tool_version(&self) -> &str {
        &self.tool_version
    }

    /// `ToolInfo.arguments[]`.
    #[must_use]
    pub fn tool_arguments(&self) -> &[String] {
        &self.tool_arguments
    }

    /// `Metadata.project_root`.
    #[must_use]
    pub fn project_root(&self) -> &str {
        &self.project_root
    }
}

/// One surviving document.
#[derive(Debug, Clone, PartialEq)]
pub struct AdmittedFile<'a> {
    relative_path: &'a str,
    language: &'a str,
    symbols: Vec<AdmittedSymbol<'a>>,
    occurrences: Vec<AdmittedOccurrence<'a>>,
}

impl<'a> AdmittedFile<'a> {
    /// `Document.relative_path`.
    #[must_use]
    pub fn relative_path(&self) -> &'a str {
        self.relative_path
    }

    /// `Document.language`.
    #[must_use]
    pub fn language(&self) -> &'a str {
        self.language
    }

    /// Surviving `SymbolInformation` rows.
    #[must_use]
    pub fn symbols(&self) -> &[AdmittedSymbol<'a>] {
        &self.symbols
    }

    /// Surviving occurrences (symbol passed and its row, if any, survived).
    #[must_use]
    pub fn occurrences(&self) -> &[AdmittedOccurrence<'a>] {
        &self.occurrences
    }
}

/// One surviving `SymbolInformation` row.
#[derive(Debug, Clone, PartialEq)]
pub struct AdmittedSymbol<'a> {
    symbol: &'a str,
    enclosing_symbol: Option<&'a str>,
    display_name: Option<&'a str>,
    kind: scip::types::symbol_information::Kind,
    documentation: Vec<&'a str>,
    signature: Option<&'a str>,
    relationships: Vec<AdmittedRelationship<'a>>,
}

impl<'a> AdmittedSymbol<'a> {
    /// `SymbolInformation.symbol`.
    #[must_use]
    pub fn symbol(&self) -> &'a str {
        self.symbol
    }

    /// `SymbolInformation.enclosing_symbol` when non-empty.
    #[must_use]
    pub fn enclosing_symbol(&self) -> Option<&'a str> {
        self.enclosing_symbol
    }

    /// `SymbolInformation.display_name` when non-empty.
    #[must_use]
    pub fn display_name(&self) -> Option<&'a str> {
        self.display_name
    }

    /// `SymbolInformation.kind`.
    #[must_use]
    pub fn kind(&self) -> scip::types::symbol_information::Kind {
        self.kind
    }

    /// Surviving `documentation[]` elements (redacted ones masked; dropped ones omitted).
    #[must_use]
    pub fn documentation(&self) -> &[&'a str] {
        &self.documentation
    }

    /// `signature_documentation.text` when present and not dropped.
    #[must_use]
    pub fn signature(&self) -> Option<&'a str> {
        self.signature
    }

    /// Relationships whose target passed the gate and whose target row was not skipped.
    #[must_use]
    pub fn relationships(&self) -> &[AdmittedRelationship<'a>] {
        &self.relationships
    }
}

/// One surviving relationship.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdmittedRelationship<'a> {
    target: &'a str,
    flags: RelationshipFlags,
}

impl<'a> AdmittedRelationship<'a> {
    /// The target symbol.
    #[must_use]
    pub fn target(&self) -> &'a str {
        self.target
    }

    /// The relationship's kind flags.
    #[must_use]
    pub fn flags(&self) -> RelationshipFlags {
        self.flags
    }
}

/// One surviving occurrence.
#[derive(Debug, Clone, PartialEq)]
pub struct AdmittedOccurrence<'a> {
    symbol: &'a str,
    range: &'a [i32],
    symbol_roles: i32,
    enclosing_range: &'a [i32],
    typed_range: Option<&'a scip::types::occurrence::Typed_range>,
    typed_enclosing_range: Option<&'a scip::types::occurrence::Typed_enclosing_range>,
}

impl<'a> AdmittedOccurrence<'a> {
    /// The occurrence's symbol.
    #[must_use]
    pub fn symbol(&self) -> &'a str {
        self.symbol
    }

    /// `Occurrence.range`.
    #[must_use]
    pub fn range(&self) -> &'a [i32] {
        self.range
    }

    /// `Occurrence.symbol_roles`.
    #[must_use]
    pub fn symbol_roles(&self) -> i32 {
        self.symbol_roles
    }

    /// `Occurrence.enclosing_range`.
    #[must_use]
    pub fn enclosing_range(&self) -> &'a [i32] {
        self.enclosing_range
    }

    /// `Occurrence.typed_range`.
    #[must_use]
    pub fn typed_range(&self) -> Option<&'a scip::types::occurrence::Typed_range> {
        self.typed_range
    }

    /// `Occurrence.typed_enclosing_range`.
    #[must_use]
    pub fn typed_enclosing_range(
        &self,
    ) -> Option<&'a scip::types::occurrence::Typed_enclosing_range> {
        self.typed_enclosing_range
    }
}

// ---------------------------------------------------------------------------------------------
// formed strings (G1-S1 step 3, RU1-1)

/// A formed structural string (`ref`, `scip`, `key`, `name`, `path`, a `file:` ref, a file
/// node's name) passes when the policy reports nothing at all; a hit skips the owning row.
#[must_use]
pub fn formed_structural_ok(policy: &Policy, s: &str) -> bool {
    let v = policy.scan(s);
    !v.rejected && v.hits.is_empty()
}

/// A formed free string (a joined and bounded documentation block, a bounded signature) under
/// the free-field rule: `Some(text)` keeps it (the masked text on a redact-only hit), `None`
/// drops the field (`policy_dropped`).
#[must_use]
pub fn formed_free(policy: &Policy, s: &str) -> Option<String> {
    let v = policy.scan_windows(s, policy.file.defaults.max_field_chars);
    if v.rejected {
        return None;
    }
    if v.hits.is_empty() {
        Some(s.to_owned())
    } else {
        Some(v.redacted)
    }
}

/// What a formed meta string with a hit becomes (DF-3): the text to write instead and the rule
/// ids behind it, never the value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetaRedaction {
    /// The masked text of a redact-only hit, or [`policy_mask`] of the reject rules.
    pub text: String,
    /// Every rule id that hit, deduplicated, in scan order.
    pub rule_ids: Vec<String>,
}

/// A formed meta string (`indexer:<lang>`, every `meta` value at publication) under the meta
/// rule: `None` when clean, else the redaction to write in its place (`policy_redacted_meta`).
#[must_use]
pub fn formed_meta(policy: &Policy, s: &str) -> Option<MetaRedaction> {
    let v = policy.scan(s);
    if !v.rejected && v.hits.is_empty() {
        return None;
    }
    let mut rule_ids: Vec<String> = v.reject_rules;
    for h in v.hits {
        if !rule_ids.contains(&h.rule) {
            rule_ids.push(h.rule);
        }
    }
    let text = if v.rejected {
        policy_mask(&rule_ids)
    } else {
        v.redacted
    };
    Some(MetaRedaction { text, rule_ids })
}

/// Apply the admission policy over a gated index.
///
/// Meta values are masked first (a hit is counted, never a refusal). Then every document: a path
/// or language hit skips the file with everything in it; a symbol row whose `symbol`,
/// `enclosing_symbol` or `display_name` hit is skipped with its relationships and occurrences;
/// relationships and occurrences pointing at a skipped row are dropped; free fields follow
/// [`admit`]. `Document.text`, diagnostics and signature occurrences never enter a snapshot and
/// produce no row and no count.
#[must_use]
pub fn admit_index(g: &GatedIndex) -> Admitted<'_> {
    let mut counts = GateCounts::default();
    let mut meta_rule_ids = Vec::new();
    let metadata = admit_metadata(g, &mut counts, &mut meta_rule_ids);

    // first pass: the symbol strings of every row that will be skipped, index-wide, so that a
    // relationship or occurrence towards one of them is dropped wherever it appears
    let mut skipped: HashSet<&str> = HashSet::new();
    for d in g.documents() {
        let file_skipped = structural_hit(&d.relative_path) || structural_hit(&d.language);
        for s in &d.symbols {
            if (file_skipped || row_skipped(s))
                && let Gated::Pass(sym) = &s.symbol
            {
                skipped.insert(sym.as_str());
            }
        }
    }
    for s in g.external_symbols() {
        if row_skipped(s)
            && let Gated::Pass(sym) = &s.symbol
        {
            skipped.insert(sym.as_str());
        }
    }

    let mut files = Vec::new();
    for d in g.documents() {
        if let Some(f) = admit_file(d, &skipped, &mut counts) {
            files.push(f);
        }
    }
    let mut external_symbols = Vec::new();
    for s in g.external_symbols() {
        if let Some(row) = admit_symbol(s, &skipped, &mut counts) {
            external_symbols.push(row);
        }
    }
    Admitted {
        metadata,
        files,
        external_symbols,
        counts,
        meta_rule_ids,
        rejected: skipped,
    }
}

/// The meta values under the meta rule (DF-3): a hit keeps the masked text or [`policy_mask`],
/// counted under `policy_redacted_meta`; the reject rule ids are collected into `rule_ids`.
fn admit_metadata(
    g: &GatedIndex,
    counts: &mut GateCounts,
    rule_ids: &mut Vec<String>,
) -> AdmittedMetadata {
    let mut value = |v: &Gated| -> String {
        let a = admit(FieldClass::Meta, v);
        counts.record(&a);
        match a {
            Admission::Admit(s) | Admission::RedactedMeta(s) => s.to_owned(),
            Admission::MaskedMeta(ids) => {
                for id in ids {
                    if !rule_ids.contains(id) {
                        rule_ids.push(id.clone());
                    }
                }
                policy_mask(ids)
            }
            Admission::DropField | Admission::SkipOwningRow => String::new(),
        }
    };
    let m = g.metadata();
    AdmittedMetadata {
        tool_name: value(&m.tool_name),
        tool_version: value(&m.tool_version),
        tool_arguments: m.tool_arguments.iter().map(&mut value).collect(),
        project_root: value(&m.project_root),
    }
}

fn structural_hit(g: &Gated) -> bool {
    admit(FieldClass::Structural, g) == Admission::SkipOwningRow
}

/// A symbol row is skipped when any of its structural fields hit.
fn row_skipped(s: &GatedSymbolInformation) -> bool {
    structural_hit(&s.symbol)
        || structural_hit(&s.enclosing_symbol)
        || structural_hit(&s.display_name)
}

fn admit_file<'a>(
    d: &'a GatedDocument,
    skipped: &HashSet<&str>,
    counts: &mut GateCounts,
) -> Option<AdmittedFile<'a>> {
    let path = admit(FieldClass::Structural, &d.relative_path);
    let language = admit(FieldClass::Structural, &d.language);
    let (Admission::Admit(relative_path), Admission::Admit(language)) = (path, language) else {
        counts.record(&Admission::SkipOwningRow);
        return None;
    };
    let symbols = d
        .symbols
        .iter()
        .filter_map(|s| admit_symbol(s, skipped, counts))
        .collect();
    let occurrences = d
        .occurrences
        .iter()
        .filter_map(|o| admit_occurrence(o, skipped, counts))
        .collect();
    Some(AdmittedFile {
        relative_path,
        language,
        symbols,
        occurrences,
    })
}

fn admit_symbol<'a>(
    s: &'a GatedSymbolInformation,
    skipped: &HashSet<&str>,
    counts: &mut GateCounts,
) -> Option<AdmittedSymbol<'a>> {
    let (Admission::Admit(symbol), Admission::Admit(enclosing), Admission::Admit(display)) = (
        admit(FieldClass::Structural, &s.symbol),
        admit(FieldClass::Structural, &s.enclosing_symbol),
        admit(FieldClass::Structural, &s.display_name),
    ) else {
        counts.record(&Admission::SkipOwningRow);
        return None;
    };
    let documentation = s
        .documentation
        .iter()
        .filter_map(|doc| admit_free(doc, counts))
        .collect();
    let signature = s
        .signature_documentation
        .as_ref()
        .and_then(|sig| admit_free(&sig.text, counts));
    let relationships = s
        .relationships
        .iter()
        .filter_map(|r| admit_relationship(r, skipped, counts))
        .collect();
    Some(AdmittedSymbol {
        symbol,
        enclosing_symbol: non_empty(enclosing),
        display_name: non_empty(display),
        kind: s.kind,
        documentation,
        signature,
        relationships,
    })
}

fn admit_free<'a>(g: &'a Gated, counts: &mut GateCounts) -> Option<&'a str> {
    let a = admit(FieldClass::Free, g);
    counts.record(&a);
    match a {
        Admission::Admit(s) => Some(s),
        Admission::DropField
        | Admission::SkipOwningRow
        | Admission::RedactedMeta(_)
        | Admission::MaskedMeta(_) => None,
    }
}

fn admit_relationship<'a>(
    r: &'a GatedRelationship,
    skipped: &HashSet<&str>,
    counts: &mut GateCounts,
) -> Option<AdmittedRelationship<'a>> {
    let target = admit_target(&r.symbol, skipped, counts)?;
    Some(AdmittedRelationship {
        target,
        flags: r.flags,
    })
}

fn admit_occurrence<'a>(
    o: &'a GatedOccurrence,
    skipped: &HashSet<&str>,
    counts: &mut GateCounts,
) -> Option<AdmittedOccurrence<'a>> {
    let symbol = admit_target(&o.symbol, skipped, counts)?;
    Some(AdmittedOccurrence {
        symbol,
        range: &o.range,
        symbol_roles: o.symbol_roles,
        enclosing_range: &o.enclosing_range,
        typed_range: o.typed_range.as_ref(),
        typed_enclosing_range: o.typed_enclosing_range.as_ref(),
    })
}

/// A symbol reference (relationship target, occurrence symbol) survives when it passed the gate
/// and names no skipped row; otherwise it is dropped under the owning row's class.
fn admit_target<'a>(
    g: &'a Gated,
    skipped: &HashSet<&str>,
    counts: &mut GateCounts,
) -> Option<&'a str> {
    match admit(FieldClass::Structural, g) {
        Admission::Admit(sym) if !skipped.contains(sym) => Some(sym),
        _ => {
            counts.record(&Admission::SkipOwningRow);
            None
        }
    }
}

fn non_empty(s: &str) -> Option<&str> {
    (!s.is_empty()).then_some(s)
}
