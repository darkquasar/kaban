//! Row and edge derivation over admitted values (stage1-spec §4.1 rows, §4.3 edges, G1-F1,
//! F2, F3, F5; stage1-plan CG3.2). Everything here starts from an [`Admitted`] index: the gate
//! ran before any string reaches this module (RS2-1), and the grammar (`symbol::parse`) is the
//! first typed check on a gated value (G1-S1 step 2).
//!
//! ```text
//!   Admitted A (+ union::implements(A, B)) ==> nodes() ==> Nodes { files, symbols, counts }
//!                                                   |
//!                                   structural_edges() ==> CONTAINS, DEFINED_IN, IMPLEMENTS,
//!                                                          IMPLEMENTS_MEMBER, TYPE_OF
//!                        occurrences_and_references() ==> occurrences, REFERENCES (weight = sites)
//!                        tested_by() / tested_by_naming() ==> TESTED_BY (reference, naming)
//! ```
//!
//! Rows are sorted the §4.1 way (`files` by path bytes, `symbols` by `ref` bytes) and ids are
//! the 1-based positions, so the same input yields the same bytes (G1-N5). Placeholder rows
//! (`kind = symbol`, `external = 1`, no file) exist for every edge endpoint and every occurrence
//! symbol without a `SymbolInformation`, so nothing dangles (§4.1; coordinator decision at GM3).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};

use kaban_codegraph::check_repo_relative_path;
use kaban_codegraph::schema::{
    EdgeType, FileId, NodeId, NodeKind, ROLE_DEFINITION, ROLE_TEST, Range,
};
use kaban_codegraph::symbol::{self, Descriptor, Symbol};
use kaban_codegraph::write::{EdgeRow, FileRow, OccurrenceRow, SymbolRow};
use kaban_policy::Policy;
use protobuf::EnumFull;
use scip::types::symbol_information::Kind;

use crate::gate::{self, Admitted, AdmittedFile, AdmittedOccurrence, AdmittedSymbol};
use crate::ranges::{self, RangeForms};
use crate::scip_read::RelationshipFlags;
use crate::union::ImplementsSet;

/// `signature` bound in bytes (G1-F2).
pub const MAX_SIGNATURE_BYTES: usize = 1024;
/// `doc` bound in bytes (G1-F2).
pub const MAX_DOC_BYTES: usize = 4096;
/// The fixed truncation marker appended to a bounded `signature` or `doc` (G1-F2).
pub const TRUNCATION_MARKER: &str = " [truncated]";

/// The derivation counters (`meta.counts:*`, the JSON report).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    /// A second `SymbolInformation` with the same symbol: the first in document order stays.
    pub duplicates_skipped: u64,
    /// Rows skipped by the grammar or the path rule, dependents dropped for it one each.
    pub grammar_skipped_rows: u64,
    /// Rows skipped because a formed string (`ref`, `scip`, `key`, `name`, `path`) hit the
    /// policy on the re-scan of G1-S1 step 3.
    pub policy_skipped_row: u64,
    /// Placeholder rows created for endpoints without a `SymbolInformation`.
    pub placeholder_rows: u64,
    /// Symbols whose effective label came from the descriptor (producer emitted
    /// `UnspecifiedKind`).
    pub label_from_descriptor: u64,
    /// Files whose language came from the extension (producer emitted no `language`).
    pub language_from_extension: u64,
    /// Free fields emptied by the formed-string re-scan (G1-S1 step 3): `ident_terms`, a joined
    /// documentation block, a bounded signature (RU1-1).
    pub policy_dropped: u64,
    /// Producer ranges refused by the ordering rule (`ranges::MalformedRange`, RU1-7): the site
    /// or the enclosing range is skipped, one each.
    pub malformed_range_skipped: u64,
    /// Implementation relationships dropped because an endpoint fails the grammar (the union,
    /// RU1-5); the endpoint's missing placeholder row is counted under `grammar_skipped_rows`.
    pub grammar_skipped_relationships: u64,
    /// Symbols that got no snippet because their file's `git show` timed out, exited non-zero,
    /// exceeded the 1 MiB cap, held a NUL byte, or because the sliced lines were not UTF-8 or
    /// lay past the end of the blob (CG8.2; the `snippet_skipped` warning and report count).
    /// Not in [`Counts::entries`]: that is the persisted `meta.counts:*` set, pinned by the
    /// fixture goldens; this counter describes the run, not the snapshot.
    pub snippet_skipped: u64,
    /// Rust files whose committed source hit the policy (DF-3): classified by their path only,
    /// no in-file `#[cfg(test)]`/`#[test]` facts; counted, never a refusal.
    pub policy_skipped_source: u64,
    /// Rust files over the classifier's per-file bound (raw open delimiters, tokens; DF-3):
    /// classified by their path only, counted, never a refusal.
    pub source_classified_by_path: u64,
}

impl Counts {
    /// The counters as `(key, value)` pairs under their `counts:` names, sorted by key.
    #[must_use]
    pub fn entries(&self) -> BTreeMap<&'static str, u64> {
        BTreeMap::from([
            ("duplicates_skipped", self.duplicates_skipped),
            (
                "grammar_skipped_relationships",
                self.grammar_skipped_relationships,
            ),
            ("grammar_skipped_rows", self.grammar_skipped_rows),
            ("label_from_descriptor", self.label_from_descriptor),
            ("language_from_extension", self.language_from_extension),
            ("malformed_range_skipped", self.malformed_range_skipped),
            ("placeholder_rows", self.placeholder_rows),
            ("policy_dropped", self.policy_dropped),
            ("policy_skipped_row", self.policy_skipped_row),
            ("policy_skipped_source", self.policy_skipped_source),
            ("source_classified_by_path", self.source_classified_by_path),
        ])
    }
}

/// The node tables of one snapshot with the lookups edge derivation needs.
#[derive(Debug, Clone)]
pub struct Nodes {
    /// `files` rows, sorted by path, ids positional.
    pub files: Vec<FileRow>,
    /// `symbols` rows, sorted by `ref`, ids positional (file nodes included).
    pub symbols: Vec<SymbolRow>,
    /// The counters.
    pub counts: Counts,
    /// The SCIP range forms the definition ranges came in (folded into `meta.scip_range_form`).
    pub range_forms: RangeForms,
    /// Committed-source test ranges; retained for reference-only items without symbols.
    pub(crate) test_scopes: BTreeMap<FileId, Vec<Range>>,
    /// The rule ids behind `counts.policy_skipped_source`, deduplicated (DF-3; safe to report).
    pub(crate) source_rule_ids: Vec<String>,
    by_ref: HashMap<String, NodeId>,
    file_by_path: HashMap<String, FileId>,
    /// `per_file[file_id - 1]` is the file node of that file.
    per_file: Vec<NodeId>,
    /// `effective[id - 1]` is the label the derivation table sees for that row.
    effective: Vec<Kind>,
}

impl Nodes {
    /// The node with this exact `ref`.
    #[must_use]
    pub fn id(&self, r#ref: &str) -> Option<NodeId> {
        self.by_ref.get(r#ref).copied()
    }

    /// The node of a symbol string occurring in `path` (`local N` symbols are namespaced by
    /// document, G1-F1 spike rule).
    #[must_use]
    pub fn symbol_id(&self, path: &str, scip: &str) -> Option<NodeId> {
        self.id(&ref_of(path, scip))
    }

    /// The file row of `path`.
    #[must_use]
    pub fn file(&self, path: &str) -> Option<FileId> {
        self.file_by_path.get(path).copied()
    }

    /// The file node of a file row.
    #[must_use]
    pub fn file_node(&self, file: FileId) -> Option<NodeId> {
        usize::try_from(file.0)
            .ok()
            .and_then(|i| i.checked_sub(1))
            .and_then(|i| self.per_file.get(i))
            .copied()
    }

    /// The `files` row of `file`.
    #[must_use]
    pub fn file_row(&self, file: FileId) -> Option<&FileRow> {
        usize::try_from(file.0)
            .ok()
            .and_then(|i| i.checked_sub(1))
            .and_then(|i| self.files.get(i))
    }

    /// The row of a node id.
    #[must_use]
    pub fn row(&self, id: NodeId) -> Option<&SymbolRow> {
        usize::try_from(id.0)
            .ok()
            .and_then(|i| i.checked_sub(1))
            .and_then(|i| self.symbols.get(i))
    }

    /// `true` when `id` is a row of this table (the debug assertion of `structural_edges`).
    #[must_use]
    pub fn contains(&self, id: NodeId) -> bool {
        self.row(id).is_some()
    }

    /// The effective label of a row (`effective_label`; `File` for a file node).
    #[must_use]
    pub fn effective_label(&self, id: NodeId) -> Option<Kind> {
        usize::try_from(id.0)
            .ok()
            .and_then(|i| i.checked_sub(1))
            .and_then(|i| self.effective.get(i))
            .copied()
    }
}

/// The `ref` of a symbol string seen in document `path` (§4.1).
#[must_use]
pub fn ref_of(path: &str, scip: &str) -> String {
    if scip.starts_with("local ") {
        format!("local:{path}:{scip}")
    } else {
        format!("scip:{scip}")
    }
}

/// The `ref` of a file node.
#[must_use]
pub fn file_ref(path: &str) -> String {
    format!("file:{path}")
}

// ---------------------------------------------------------------------------------------------
// G1-F5: the derivation table

/// The edge type a relationship yields, from the two endpoint labels and the four relationship
/// flags, or `None` (stage1-spec §4.3 rows `IMPLEMENTS`, `IMPLEMENTS_MEMBER`, `TYPE_OF`; no
/// `EXTENDS`, no `OVERRIDES`, OD-4). Exhaustive: an implementation between two method-like
/// members is `IMPLEMENTS_MEMBER`; between two members of any other mix (a field on either
/// side) it is nothing; otherwise `IMPLEMENTS`. A type-definition flag without an
/// implementation flag is `TYPE_OF`.
#[must_use]
pub fn derive_edge_type(src: Kind, dst: Kind, flags: RelationshipFlags) -> Option<EdgeType> {
    if flags.is_implementation() {
        return match (member_class(src), member_class(dst)) {
            (MemberClass::MethodLike, MemberClass::MethodLike) => Some(EdgeType::ImplementsMember),
            (MemberClass::NotMember, _) | (_, MemberClass::NotMember) => Some(EdgeType::Implements),
            (MemberClass::Field, _) | (_, MemberClass::Field) => None,
        };
    }
    if flags.is_type_definition() {
        return Some(EdgeType::TypeOf);
    }
    None
}

/// The §4.3 member classes of a label.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MemberClass {
    /// `Method`, `MethodSpecification`, `TraitMethod`, `StaticMethod`.
    MethodLike,
    /// `Field`.
    Field,
    /// Everything else.
    NotMember,
}

const fn member_class(k: Kind) -> MemberClass {
    match k {
        Kind::Method | Kind::MethodSpecification | Kind::TraitMethod | Kind::StaticMethod => {
            MemberClass::MethodLike
        }
        Kind::Field => MemberClass::Field,
        _ => MemberClass::NotMember,
    }
}

/// The label the derivation table sees: the SCIP kind when the producer emitted one, else the
/// member class read from the descriptor grammar (`name().` is a `Method`; a `name.` term
/// directly under a `name#` type is a `Field`; anything else stays `UnspecifiedKind`).
/// scip-typescript 0.4.0 emits `UnspecifiedKind` throughout; the `symbols.label` column keeps
/// the kind verbatim and this fallback is counted in `counts:label_from_descriptor`
/// (coordinator decision at GM3).
#[must_use]
pub fn effective_label(kind: Kind, last: Option<&Descriptor>, parent: Option<&Descriptor>) -> Kind {
    if kind != Kind::UnspecifiedKind {
        return kind;
    }
    match (last, parent) {
        (Some(Descriptor::Method { .. }), _) => Kind::Method,
        (Some(Descriptor::Term(_)), Some(Descriptor::Type(_))) => Kind::Field,
        _ => Kind::UnspecifiedKind,
    }
}

/// [`effective_label`] over a parsed symbol.
#[must_use]
pub fn label_of(kind: Kind, sym: &Symbol) -> Kind {
    let n = sym.descriptors.len();
    let last = sym.descriptors.last();
    let parent = n.checked_sub(2).and_then(|i| sym.descriptors.get(i));
    effective_label(kind, last, parent)
}

/// The `symbols.label` text: the SCIP kind name as in `scip.proto`, or `File`.
#[must_use]
pub fn label_text(kind: Kind) -> String {
    kind.descriptor().name().to_owned()
}

// ---------------------------------------------------------------------------------------------
// languages and test files

/// `files.language`: the document's gated `language`, else by extension (`.go`, `.ts`/`.tsx`,
/// `.js`/`.jsx`, `.rs`; coordinator decision at GM3), else empty.
#[must_use]
pub fn language_of(path: &str, declared: &str) -> (String, bool) {
    if !declared.is_empty() {
        return (declared.to_owned(), false);
    }
    let ext = Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    let lang = match ext {
        "go" => "go",
        "ts" | "tsx" => "typescript",
        "js" | "jsx" => "javascript",
        "rs" => "rust",
        _ => "",
    };
    (lang.to_owned(), !lang.is_empty())
}

/// The per-language test-file rule of stage1-spec §4.3: Go `*_test.go`; TypeScript and
/// JavaScript `*.test.<ext>`, `*.spec.<ext>`, or a `__tests__` path component;
/// Rust `.rs` sources below a `tests/` directory (including workspace packages).
#[must_use]
pub fn is_test_file(path: &str, language: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    match language.to_ascii_lowercase().as_str() {
        "go" => name.ends_with("_test.go"),
        "rust" => {
            Path::new(name)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("rs"))
                && path.split('/').any(|part| part == "tests")
        }
        "typescript" | "javascript" | "ts" | "js" => {
            let stem_marked = ["ts", "tsx", "js", "jsx"].iter().any(|ext| {
                name.ends_with(&format!(".test.{ext}")) || name.ends_with(&format!(".spec.{ext}"))
            });
            stem_marked || path.split('/').any(|c| c == "__tests__")
        }
        _ => false,
    }
}

// ---------------------------------------------------------------------------------------------
// nodes

/// One candidate row before sorting and id assignment.
struct Pending {
    kind: NodeKind,
    r#ref: String,
    scip: String,
    key: Option<String>,
    label: String,
    name: String,
    path: Option<String>,
    def: Option<Range>,
    enc: Option<Range>,
    signature: Option<String>,
    doc: Option<String>,
    external: bool,
    is_test: bool,
    effective: Kind,
}

/// Derive the `files` and `symbols` rows of run A (every table but edges and occurrences).
///
/// `implements` is the relationship set of `union::implements` (A alone for one-run languages,
/// A and B for Go, G1-F3): its endpoints get placeholders here so that `structural_edges` never
/// dangles. `policy` re-scans every formed string (`path`, `ref`, `scip`, `key`, `name`) before
/// it enters a row (G1-S1 step 3).
#[must_use]
pub fn nodes(a: &Admitted<'_>, implements: &ImplementsSet<'_>, policy: &Policy) -> Nodes {
    let mut b = Builder {
        policy,
        counts: Counts::default(),
        range_forms: RangeForms::default(),
        files: Vec::new(),
        pending: Vec::new(),
        seen: HashSet::new(),
        skipped: HashSet::new(),
    };
    b.files(a.files());
    for f in a.files() {
        if b.file_ok(f.relative_path()) {
            for s in f.symbols() {
                b.symbol(s, Some(f));
            }
        }
    }
    for s in a.external_symbols() {
        b.symbol(s, None);
    }
    b.definition_ranges(a);
    b.placeholders(a, implements);
    b.nodes_of_files();
    b.finish()
}

struct Builder<'p> {
    policy: &'p Policy,
    counts: Counts,
    range_forms: RangeForms,
    files: Vec<FileRow>,
    pending: Vec<Pending>,
    /// `ref`s with a pending row (duplicate detection).
    seen: HashSet<String>,
    /// `ref`s whose row was skipped (grammar, path rule, re-scan): dependents are dropped.
    skipped: HashSet<String>,
}

impl Builder<'_> {
    /// A formed structural string passes the re-scan when the policy reports nothing at all.
    fn formed_ok(&self, s: &str) -> bool {
        gate::formed_structural_ok(self.policy, s)
    }

    /// A formed free string under the free-field rule; a drop is counted (`policy_dropped`).
    fn formed_free(&mut self, s: &str) -> Option<String> {
        let kept = gate::formed_free(self.policy, s);
        if kept.is_none() {
            self.counts.policy_dropped += 1;
        }
        kept
    }

    fn file_ok(&self, path: &str) -> bool {
        self.files.iter().any(|f| f.path == path)
    }

    fn files(&mut self, files: &[AdmittedFile<'_>]) {
        for f in files {
            let path = f.relative_path();
            if check_repo_relative_path(path).is_err() {
                self.counts.grammar_skipped_rows += 1;
                continue;
            }
            // the path, the file node's `ref` and its `name` are the formed strings of a file
            // (RU1-1: a rule may match `file:drivers/x.go` and not `drivers/x.go`)
            let formed = [path, &file_ref(path), basename(path)];
            if formed.into_iter().any(|v| !self.formed_ok(v)) {
                self.counts.policy_skipped_row += 1;
                continue;
            }
            if self.file_ok(path) {
                self.counts.duplicates_skipped += 1;
                continue;
            }
            let (language, from_ext) = language_of(path, f.language());
            if from_ext {
                self.counts.language_from_extension += 1;
            }
            let is_test = is_test_file(path, &language);
            self.files.push(FileRow {
                id: FileId(0),
                path: path.to_owned(),
                language,
                is_test,
                lines: lines_seen(f.occurrences()),
            });
        }
        self.files
            .sort_by(|x, y| x.path.as_bytes().cmp(y.path.as_bytes()));
        for (i, f) in self.files.iter_mut().enumerate() {
            f.id = FileId(i64::try_from(i + 1).unwrap_or(i64::MAX));
        }
    }

    fn symbol(&mut self, s: &AdmittedSymbol<'_>, file: Option<&AdmittedFile<'_>>) {
        let path = file.map(AdmittedFile::relative_path);
        let r#ref = ref_of(path.unwrap_or(""), s.symbol());
        let Ok(parsed) = symbol::parse(s.symbol()) else {
            self.counts.grammar_skipped_rows += 1;
            self.skipped.insert(r#ref);
            return;
        };
        if self.seen.contains(&r#ref) || self.skipped.contains(&r#ref) {
            self.counts.duplicates_skipped += 1;
            return;
        }
        let kind = node_kind(&parsed);
        let key = (kind == NodeKind::Symbol)
            .then(|| symbol::normalise_version(s.symbol()).ok())
            .flatten();
        let name = symbol::name(&parsed).to_owned();
        let label_kind = label_of(s.kind(), &parsed);
        if label_kind != s.kind() {
            self.counts.label_from_descriptor += 1;
        }
        let formed = [
            Some(r#ref.as_str()),
            Some(s.symbol()),
            key.as_deref(),
            Some(&name),
        ];
        if formed.into_iter().flatten().any(|v| !self.formed_ok(v)) {
            self.counts.policy_skipped_row += 1;
            self.skipped.insert(r#ref);
            return;
        }
        let is_test = file.is_some_and(|f| {
            self.files
                .iter()
                .any(|row| row.path == f.relative_path() && row.is_test)
        });
        // the joined documentation and the bounded signature are formed strings: re-scanned
        // under the free-field rule before they enter the row (RU1-1)
        let signature = s
            .signature()
            .and_then(|t| self.formed_free(&bounded(t, MAX_SIGNATURE_BYTES)));
        let doc = (!s.documentation().is_empty())
            .then(|| bounded(&s.documentation().join("\n\n"), MAX_DOC_BYTES))
            .and_then(|joined| self.formed_free(&joined));
        self.seen.insert(r#ref.clone());
        self.pending.push(Pending {
            kind,
            r#ref,
            scip: s.symbol().to_owned(),
            key,
            label: label_text(s.kind()),
            name,
            path: path.map(str::to_owned),
            def: None,
            enc: None,
            signature,
            doc,
            external: file.is_none(),
            is_test,
            effective: label_kind,
        });
    }

    /// The first definition occurrence of each row, in document order, gives `def` and `enc`;
    /// its SCIP `Test` role makes the row `is_test` (a producer's positive test fact stands
    /// whatever the committed-source classifier later adds or cannot add, df23-5).
    fn definition_ranges(&mut self, a: &Admitted<'_>) {
        let mut index: HashMap<&str, usize> = self
            .pending
            .iter()
            .enumerate()
            .map(|(i, p)| (p.r#ref.as_str(), i))
            .collect();
        let mut done: Vec<(usize, Range, Option<Range>, bool)> = Vec::new();
        let mut forms = self.range_forms;
        let mut malformed = 0;
        for f in a.files() {
            if !self.file_ok(f.relative_path()) {
                continue;
            }
            for o in f.occurrences() {
                let roles = roles(o);
                if roles & ROLE_DEFINITION == 0 {
                    continue;
                }
                let r#ref = ref_of(f.relative_path(), o.symbol());
                let Some(i) = index.remove(r#ref.as_str()) else {
                    continue;
                };
                // a malformed site skips the definition (the row stays, without `def`); a
                // malformed enclosing range skips `enc` only (RU1-7)
                let range = match ranges::occurrence_range(o) {
                    Ok(Some((range, form))) => {
                        forms = forms.with(form);
                        range
                    }
                    Ok(None) => continue,
                    Err(ranges::MalformedRange) => {
                        malformed += 1;
                        continue;
                    }
                };
                let enc = match ranges::enclosing(o) {
                    Ok(Some((r, form))) => {
                        forms = forms.with(form);
                        Some(r)
                    }
                    Ok(None) => None,
                    Err(ranges::MalformedRange) => {
                        malformed += 1;
                        None
                    }
                };
                done.push((i, range, enc, roles & ROLE_TEST != 0));
            }
        }
        self.range_forms = forms;
        self.counts.malformed_range_skipped += malformed;
        for (i, def, enc, test_role) in done {
            if let Some(p) = self.pending.get_mut(i) {
                p.def = Some(def);
                p.enc = enc;
                p.is_test |= test_role;
            }
        }
    }

    /// Placeholders for every endpoint without a row: relationship targets and union pairs
    /// (§4.1), `CONTAINS` parents of the repository's own rows (spike rule) and occurrence
    /// symbols (G1-F4, coordinator decision at GM3). One level only: a placeholder or an
    /// external row gets no parent of its own (`CONTAINS` is derived for repository rows).
    fn placeholders(&mut self, a: &Admitted<'_>, implements: &ImplementsSet<'_>) {
        let mut candidates: BTreeSet<String> = BTreeSet::new();
        for (src, dst, _) in implements {
            candidates.insert(ref_of("", src));
            candidates.insert(ref_of("", dst));
        }
        for f in a.files() {
            if !self.file_ok(f.relative_path()) {
                continue;
            }
            for s in f.symbols() {
                for r in s.relationships() {
                    if derive_edge_type(Kind::UnspecifiedKind, Kind::UnspecifiedKind, r.flags())
                        .is_some()
                    {
                        candidates.insert(ref_of(f.relative_path(), r.target()));
                    }
                }
            }
            for o in f.occurrences() {
                candidates.insert(ref_of(f.relative_path(), o.symbol()));
            }
        }
        for p in &self.pending {
            if p.kind == NodeKind::Symbol
                && !p.external
                && let Ok(sym) = symbol::parse(&p.scip)
                && let Some(parent) = symbol::parent(&sym)
            {
                candidates.insert(ref_of("", &symbol::format(&parent)));
            }
        }
        for r#ref in candidates {
            if self.seen.contains(&r#ref) || self.skipped.contains(&r#ref) {
                continue;
            }
            let scip = scip_of_ref(&r#ref);
            let Ok(parsed) = symbol::parse(scip) else {
                // an occurrence-only or relationship-target symbol that fails the grammar: a
                // counted skip like any row (RU1-5)
                self.counts.grammar_skipped_rows += 1;
                self.skipped.insert(r#ref);
                continue;
            };
            let kind = node_kind(&parsed);
            let key = (kind == NodeKind::Symbol)
                .then(|| symbol::normalise_version(scip).ok())
                .flatten();
            let name = symbol::name(&parsed).to_owned();
            let formed = [
                Some(r#ref.as_str()),
                Some(scip),
                key.as_deref(),
                Some(&name),
            ];
            if formed.into_iter().flatten().any(|v| !self.formed_ok(v)) {
                self.counts.policy_skipped_row += 1;
                self.skipped.insert(r#ref);
                continue;
            }
            self.counts.placeholder_rows += 1;
            self.seen.insert(r#ref.clone());
            self.pending.push(Pending {
                kind,
                scip: scip.to_owned(),
                r#ref,
                key,
                label: label_text(Kind::UnspecifiedKind),
                name,
                path: None,
                def: None,
                enc: None,
                signature: None,
                doc: None,
                external: true,
                is_test: false,
                effective: label_of(Kind::UnspecifiedKind, &parsed),
            });
        }
    }

    /// The file nodes; their `ref` and `name` passed the formed-string re-scan in `files`.
    fn nodes_of_files(&mut self) {
        for f in &self.files {
            let name = basename(&f.path).to_owned();
            self.pending.push(Pending {
                kind: NodeKind::File,
                r#ref: file_ref(&f.path),
                scip: String::new(),
                key: None,
                label: "File".to_owned(),
                name,
                path: Some(f.path.clone()),
                def: None,
                enc: None,
                signature: None,
                doc: None,
                external: false,
                is_test: f.is_test,
                effective: Kind::File,
            });
        }
    }

    fn finish(mut self) -> Nodes {
        let file_by_path: HashMap<String, FileId> =
            self.files.iter().map(|f| (f.path.clone(), f.id)).collect();
        self.pending
            .sort_by(|x, y| x.r#ref.as_bytes().cmp(y.r#ref.as_bytes()));
        let mut by_ref = HashMap::with_capacity(self.pending.len());
        let mut per_file = vec![NodeId(0); self.files.len()];
        let effective: Vec<Kind> = self.pending.iter().map(|p| p.effective).collect();
        let symbols: Vec<SymbolRow> = self
            .pending
            .into_iter()
            .enumerate()
            .map(|(i, p)| {
                let id = NodeId(i64::try_from(i + 1).unwrap_or(i64::MAX));
                let file_id = p
                    .path
                    .as_deref()
                    .and_then(|path| file_by_path.get(path))
                    .copied();
                if p.kind == NodeKind::File
                    && let Some(f) = file_id
                    && let Some(slot) = usize::try_from(f.0)
                        .ok()
                        .and_then(|n| per_file.get_mut(n - 1))
                {
                    *slot = id;
                }
                by_ref.insert(p.r#ref.clone(), id);
                SymbolRow {
                    id,
                    kind: p.kind,
                    r#ref: p.r#ref,
                    scip: (p.kind != NodeKind::File).then_some(p.scip),
                    key: p.key,
                    label: p.label,
                    name: p.name,
                    file_id,
                    def: p.def,
                    enc: p.enc,
                    signature: p.signature,
                    doc: p.doc,
                    external: p.external,
                    is_test: p.is_test,
                    ident_terms: String::new(),
                }
            })
            .collect();
        Nodes {
            files: self.files,
            symbols,
            counts: self.counts,
            range_forms: self.range_forms,
            test_scopes: BTreeMap::new(),
            source_rule_ids: Vec::new(),
            by_ref,
            file_by_path,
            per_file,
            effective,
        }
    }
}

/// `Occurrence.symbol_roles` as the reader's unsigned bit set (a negative value has no bits).
pub(crate) fn roles(o: &AdmittedOccurrence<'_>) -> u32 {
    u32::try_from(o.symbol_roles()).unwrap_or(0)
}

/// The last path component (`files.path` is repository-relative with `/` separators).
fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn node_kind(sym: &Symbol) -> NodeKind {
    if sym.scheme == "local" {
        NodeKind::Local
    } else {
        NodeKind::Symbol
    }
}

/// The symbol string inside a `scip:` or `local:<path>:` ref built by [`ref_of`].
fn scip_of_ref(r#ref: &str) -> &str {
    r#ref
        .strip_prefix("scip:")
        .or_else(|| r#ref.rfind(":local ").map(|i| &r#ref[i + 1..]))
        .unwrap_or(r#ref)
}

/// The largest end line seen in the document's well-formed occurrences plus one (`files.lines`;
/// the document text is never read, G1-S1; malformed ranges are counted where they are skipped,
/// not here).
fn lines_seen(occurrences: &[AdmittedOccurrence<'_>]) -> u32 {
    occurrences
        .iter()
        .flat_map(|o| {
            ranges::occurrence_range(o)
                .ok()
                .flatten()
                .map(|(r, _)| r.el)
                .into_iter()
                .chain(ranges::enclosing(o).ok().flatten().map(|(r, _)| r.el))
        })
        .max()
        .map_or(0, |el| el + 1)
}

/// Bound `text` to `max` bytes at a character boundary, appending the fixed marker when cut.
#[must_use]
pub fn bounded(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut cut = max.saturating_sub(TRUNCATION_MARKER.len());
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    let mut out = String::with_capacity(max);
    out.push_str(&text[..cut]);
    out.push_str(TRUNCATION_MARKER);
    out
}

// ---------------------------------------------------------------------------------------------
// structural edges

/// `CONTAINS`, `DEFINED_IN`, `IMPLEMENTS`, `IMPLEMENTS_MEMBER` and `TYPE_OF` (stage1-spec §4.3),
/// sorted by `(src, type, dst)` with `weight = 1`. Relationship edges come from `implements`
/// (the union set) and from A's type-definition relationships. A dependent whose endpoint has
/// no row (its owning row was skipped by the gate, the grammar or the path rule) is dropped
/// under that row's class, which was counted once when the row was skipped (G1-S1 step 2:
/// "the count is 1"). Self-loops are dropped (G1-F3).
#[must_use]
pub fn structural_edges(
    nodes: &Nodes,
    a: &Admitted<'_>,
    implements: &ImplementsSet<'_>,
) -> Vec<EdgeRow> {
    let mut set: BTreeSet<(NodeId, EdgeType, NodeId)> = BTreeSet::new();
    let mut push = |src: Option<NodeId>, ty: EdgeType, dst: Option<NodeId>| {
        if let (Some(s), Some(d)) = (src, dst)
            && s != d
        {
            set.insert((s, ty, d));
        }
    };

    // CONTAINS: parent -> child, parent = the symbol minus its last descriptor (repository rows;
    // external rows and placeholders have no file and get no parent chain)
    for row in &nodes.symbols {
        if row.kind != NodeKind::Symbol || row.external {
            continue;
        }
        if let Some(scip) = row.scip.as_deref()
            && let Ok(sym) = symbol::parse(scip)
            && let Some(parent) = symbol::parent(&sym)
        {
            let parent_ref = ref_of("", &symbol::format(&parent));
            push(nodes.id(&parent_ref), EdgeType::Contains, Some(row.id));
        }
    }
    // DEFINED_IN: symbol -> file node of the document holding the definition occurrence
    for f in a.files() {
        let Some(file) = nodes.file(f.relative_path()) else {
            continue;
        };
        let file_node = nodes.file_node(file);
        for o in f.occurrences() {
            if roles(o) & ROLE_DEFINITION == 0 {
                continue;
            }
            push(
                nodes.symbol_id(f.relative_path(), o.symbol()),
                EdgeType::DefinedIn,
                file_node,
            );
        }
        // TYPE_OF and any other non-IMPLEMENTS relationship edge: from A only (G1-F3)
        for s in f.symbols() {
            for r in s.relationships() {
                if r.flags().is_implementation() {
                    continue;
                }
                if let Some(ty) =
                    derive_edge_type(Kind::UnspecifiedKind, Kind::UnspecifiedKind, r.flags())
                {
                    push(
                        nodes.symbol_id(f.relative_path(), s.symbol()),
                        ty,
                        nodes.symbol_id(f.relative_path(), r.target()),
                    );
                }
            }
        }
    }
    // IMPLEMENTS / IMPLEMENTS_MEMBER: the union set
    for (src, dst, ty) in implements {
        push(nodes.symbol_id("", src), *ty, nodes.symbol_id("", dst));
    }
    let edges: Vec<EdgeRow> = set
        .into_iter()
        .map(|(src, ty, dst)| EdgeRow {
            src,
            dst,
            r#type: ty,
            weight: 1,
        })
        .collect();
    debug_assert!(check_no_dangling(&nodes.symbols, &edges).is_ok());
    edges
}

// ---------------------------------------------------------------------------------------------
// occurrences, REFERENCES, TESTED_BY

/// The occurrence table and the `REFERENCES` edges derived from it.
#[derive(Debug, Clone, Default)]
pub struct Occurrences {
    /// `occurrences` rows, sorted `(file_id, range, symbol_id, roles)`, distinct.
    pub rows: Vec<OccurrenceRow>,
    /// `REFERENCES` edges, `weight` = site count, sorted `(src, dst)`.
    pub references: Vec<EdgeRow>,
    /// The SCIP range forms the sites came in.
    pub range_forms: RangeForms,
    /// Sites skipped for a malformed range (RU1-7).
    pub malformed_range_skipped: u64,
}

/// Every occurrence of a symbol with a row becomes an `occurrences` row (definition sites
/// included, so the table is the complete site list; the reader filters the definition role);
/// each non-definition site is attributed to the innermost symbol E of the same document whose
/// enclosing range (`typed_enclosing_range` over `enclosing_range`, `[EN-amend]`) contains it,
/// else to the document's file node, and aggregated into `E -> S REFERENCES` with `weight` =
/// the count of the distinct sites (stage1-spec §4.3, G1-F4; RU1-6: a producer record that
/// repeats a site adds no weight, so the weight equals the reader's grouped site count). A site
/// of a symbol without a row (its row was skipped) is dropped under that row's class; a site
/// without a range is dropped; a malformed range is a counted skip (RU1-7).
#[must_use]
pub fn occurrences_and_references(nodes: &Nodes, a: &Admitted<'_>) -> Occurrences {
    let mut sites: BTreeSet<(FileId, Range, NodeId, u32, Option<NodeId>)> = BTreeSet::new();
    let mut weights: BTreeMap<(NodeId, NodeId), u32> = BTreeMap::new();
    let mut forms = RangeForms::default();
    let mut malformed = 0;
    for f in a.files() {
        let Some(file) = nodes.file(f.relative_path()) else {
            continue;
        };
        let Some(file_node) = nodes.file_node(file) else {
            continue;
        };
        let enclosers = enclosers_of(nodes, file);
        let production_file = !nodes.file_row(file).is_some_and(|row| row.is_test);
        for o in f.occurrences() {
            let Some(symbol) = nodes.symbol_id(f.relative_path(), o.symbol()) else {
                continue;
            };
            let (range, form) = match ranges::occurrence_range(o) {
                Ok(Some(site)) => site,
                Ok(None) => continue,
                Err(ranges::MalformedRange) => {
                    malformed += 1;
                    continue;
                }
            };
            forms = forms.with(form);
            let mut roles = roles(o);
            // a site inside a committed test scope, or a reference in a production file that
            // resolves to a test-only symbol (a `src/` file classified by its path only keeps
            // the producer's `Test` roles on its definitions, df23-5), is a test reference
            // whatever the producer tagged; in a test file the file's classification already
            // says so, and a definition site keeps the producer's roles
            let in_test_scope = nodes
                .test_scopes
                .get(&file)
                .is_some_and(|scopes| scopes.iter().any(|scope| ranges::contains(*scope, range)));
            let to_test_symbol = production_file
                && roles & ROLE_DEFINITION == 0
                && nodes
                    .row(symbol)
                    .is_some_and(|s| s.kind != NodeKind::File && s.is_test);
            if in_test_scope || to_test_symbol {
                roles |= ROLE_TEST;
            }
            let enclosing = innermost(&enclosers, range);
            let new_site = sites.insert((file, range, symbol, roles, enclosing));
            if new_site && roles & ROLE_DEFINITION == 0 {
                let src = enclosing.unwrap_or(file_node);
                if src != symbol {
                    *weights.entry((src, symbol)).or_insert(0) += 1;
                }
            }
        }
    }
    Occurrences {
        rows: sites
            .into_iter()
            .map(
                |(file_id, range, symbol_id, roles, enclosing_id)| OccurrenceRow {
                    symbol_id,
                    file_id,
                    range,
                    roles,
                    enclosing_id,
                },
            )
            .collect(),
        references: weights
            .into_iter()
            .map(|((src, dst), weight)| EdgeRow {
                src,
                dst,
                r#type: EdgeType::References,
                weight,
            })
            .collect(),
        range_forms: forms,
        malformed_range_skipped: malformed,
    }
}

/// The `(enclosing range, node)` pairs of the symbols defined in `file` that carry one.
fn enclosers_of(nodes: &Nodes, file: FileId) -> Vec<(Range, NodeId)> {
    nodes
        .symbols
        .iter()
        .filter(|s| s.file_id == Some(file) && s.kind != NodeKind::File)
        .filter_map(|s| s.enc.map(|enc| (enc, s.id)))
        .collect()
}

/// The innermost encloser containing `site`: the smallest span, ties by the lower id.
fn innermost(enclosers: &[(Range, NodeId)], site: Range) -> Option<NodeId> {
    enclosers
        .iter()
        .filter(|(enc, _)| ranges::contains(*enc, site))
        .min_by_key(|(enc, id)| {
            (
                (enc.el.saturating_sub(enc.sl), enc.ec.wrapping_sub(enc.sc)),
                *id,
            )
        })
        .map(|(_, id)| *id)
}

/// `TESTED_BY`, reference form (stage1-spec §4.3): `S -> T` for every `REFERENCES` edge
/// `T -> S` whose `T` is a function/method (effective label) classified as test code,
/// including committed-source Rust test scopes in mixed production files.
#[must_use]
pub fn tested_by(nodes: &Nodes, references: &[EdgeRow]) -> Vec<EdgeRow> {
    let mut set: BTreeSet<(NodeId, NodeId)> = BTreeSet::new();
    for e in references {
        let Some(t) = nodes.row(e.src) else {
            continue;
        };
        let is_test_fn = t.is_test
            && t.kind != NodeKind::File
            && matches!(
                nodes.effective_label(e.src),
                Some(Kind::Function | Kind::Method | Kind::StaticMethod | Kind::TraitMethod)
            );
        if is_test_fn && e.dst != e.src {
            set.insert((e.dst, e.src));
        }
    }
    set.into_iter()
        .map(|(src, dst)| EdgeRow {
            src,
            dst,
            r#type: EdgeType::TestedBy,
            weight: 1,
        })
        .collect()
}

/// `TESTED_BY`, naming form (heuristic): file node `F -> TF` when `TF` is one of
/// [`test_file_for`]`(F)` and exists in the snapshot.
#[must_use]
pub fn tested_by_naming(nodes: &Nodes) -> Vec<EdgeRow> {
    let mut set: BTreeSet<(NodeId, NodeId)> = BTreeSet::new();
    for f in &nodes.files {
        for candidate in test_file_for(&f.path, &f.language) {
            if let (Some(src), Some(dst)) = (
                nodes.file_node(f.id),
                nodes.file(&candidate).and_then(|t| nodes.file_node(t)),
            ) && src != dst
            {
                set.insert((src, dst));
            }
        }
    }
    set.into_iter()
        .map(|(src, dst)| EdgeRow {
            src,
            dst,
            r#type: EdgeType::TestedBy,
            weight: 1,
        })
        .collect()
}

/// The test-file names the per-language naming rule pairs with `path` (stage1-spec §4.3): Go
/// `x.go` -> `x_test.go`; TypeScript and JavaScript `x.<ext>` -> `x.test.<ext>`, `x.spec.<ext>`,
/// `__tests__/x.<ext>` beside it. A test file itself, or another language, pairs with nothing.
#[must_use]
pub fn test_file_for(path: &str, language: &str) -> Vec<String> {
    if is_test_file(path, language) {
        return Vec::new();
    }
    let (dir, name) = path.rsplit_once('/').map_or(("", path), |(d, n)| (d, n));
    let Some((stem, ext)) = name.rsplit_once('.') else {
        return Vec::new();
    };
    let join = |d: &str, n: String| if d.is_empty() { n } else { format!("{d}/{n}") };
    match language.to_ascii_lowercase().as_str() {
        "go" if ext == "go" => vec![join(dir, format!("{stem}_test.go"))],
        "typescript" | "javascript" | "ts" | "js" if matches!(ext, "ts" | "tsx" | "js" | "jsx") => {
            vec![
                join(dir, format!("{stem}.test.{ext}")),
                join(dir, format!("{stem}.spec.{ext}")),
                join(dir, format!("__tests__/{name}")),
            ]
        }
        _ => Vec::new(),
    }
}

/// An edge endpoint that is not a row (a writer bug: placeholders must exist, §4.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dangling {
    /// The missing endpoint.
    pub id: NodeId,
    /// The edge type.
    pub r#type: EdgeType,
}

impl fmt::Display for Dangling {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "dangling {} endpoint id {}", self.r#type, self.id.0)
    }
}

impl std::error::Error for Dangling {}

/// Every endpoint of `edges` is a row of `symbols` (ids are the 1-based positions, §4.1);
/// debug-asserted by the derivation and checked again by the writer before it inserts.
///
/// # Errors
/// The first dangling endpoint.
pub fn check_no_dangling(symbols: &[SymbolRow], edges: &[EdgeRow]) -> Result<(), Dangling> {
    let rows = i64::try_from(symbols.len()).unwrap_or(i64::MAX);
    for e in edges {
        for id in [e.src, e.dst] {
            if id.0 < 1 || id.0 > rows {
                return Err(Dangling {
                    id,
                    r#type: e.r#type,
                });
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// project root

/// The gated `Metadata.project_root` does not name the repository root (consumed by CG4.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProjectRootMismatch;

impl fmt::Display for ProjectRootMismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("project_root_mismatch")
    }
}

impl std::error::Error for ProjectRootMismatch {}

/// The identity of `Metadata.project_root`, parsed from the producer's text (a `file://` URI
/// or a plain path, trailing `/` dropped) before the gate masks it (df23-6): the root is
/// validated as an identity and the text is redacted for publication, independently. The
/// path is never shown: no accessor, `Debug` is opaque; [`check_project_root`] is the only
/// question it answers.
#[derive(Clone, PartialEq, Eq)]
pub struct ProjectRoot(PathBuf);

impl ProjectRoot {
    /// Parse the producer's `project_root` text.
    #[must_use]
    pub fn parse(text: &str) -> Self {
        let path = text.strip_prefix("file://").unwrap_or(text);
        Self(PathBuf::from(path.trim_end_matches('/')))
    }
}

impl fmt::Debug for ProjectRoot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ProjectRoot(..)")
    }
}

/// `project_root` must end in `repo_root`: the indexer ran on the repository the snapshot is
/// named for.
///
/// # Errors
/// [`ProjectRootMismatch`]; the values are not echoed.
pub fn check_project_root(
    project_root: &ProjectRoot,
    repo_root: &Path,
) -> Result<(), ProjectRootMismatch> {
    if project_root.0.ends_with(repo_root) && !repo_root.as_os_str().is_empty() {
        Ok(())
    } else {
        Err(ProjectRootMismatch)
    }
}
