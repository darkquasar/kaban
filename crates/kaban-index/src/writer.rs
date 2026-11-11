//! Snapshot assembly and deterministic publication (stage1-spec §4.1 file protocol, G1-N5;
//! stage1-plan CG3.2, CG3.3). [`Rows::derive`] composes the derivation modules into the row
//! tables of one snapshot (rows, structural edges, occurrences and `REFERENCES`, `TESTED_BY` in
//! both forms, `ident_terms` re-scanned by the formed-string gate, `symbols_fts` rebuilt);
//! [`publish_admitted`] re-scans every `meta` value (the `indexer:<lang>` text and the
//! caller-supplied strings included, RU1-1) and refuses before any file exists, then creates
//! `<commit40>.cg.db.tmp-<pid>` through the admitted directory handle (`O_CREAT|O_EXCL|
//! O_NOFOLLOW`, 0600, U1 residual 1), writes the tables through `kaban_codegraph::write` into
//! that very inode (the finalized SQLite image is copied to the retained descriptor: no path
//! of the directory is ever spelled, U2 RU2-10), confirms the name still points at it,
//! fsyncs the file, publishes it with a no-overwrite rename relative to the
//! handle and fsyncs the directory through the same handle. A directory `fsync` failure
//! after the rename is [`PublishError::Unsynced`] (`published_unsynced`, U1 residual 3): the
//! snapshot is present at its final name, a `<commit40>.cg.db.unsynced` marker (0600) beside it
//! tells the next `status`/`index` run to re-verify it rather than trust it, and the exit is
//! non-zero. The directory fsync is injectable ([`DirFsync`]) so the failure is testable.
//! [`publish`] is the path-spelled convenience: it canonicalises and re-walks `dir` into a
//! handle (a symlink component refuses) and calls `publish_admitted`. `meta` carries no
//! timestamp and no random value; `VACUUM` runs last inside `write::finish`. `Rows` has no
//! public field (RU1-2): it is built by [`Rows::derive`] and read through accessors.
//!
//! ```text
//!   Admitted A, Option<Admitted B>  ==> Rows::derive ==> Rows { files, symbols, edges, ... }
//!                                                             |
//!   publish_admitted(&DirHandle, &rows, &meta, &policy, dir_fsync)
//!       ==> meta_rows (formed meta scan, a hit is masked and counted)
//!       ==> fd = openat(handle, tmp, EXCL|NOFOLLOW|RDWR, 0600) ==> write::* (retained fd)
//!       ==> statat(handle, tmp, NOFOLLOW) has the same (dev, ino) as fd, else TempSwapped
//!       ==> fsync(fd) ==> renameat_with(handle, tmp, handle, final, NOREPLACE)
//!       ==> dir_fsync(handle) ok ==> Published { name, bytes } | err ==> Unsynced { name } + marker
//! ```

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use kaban_codegraph::identity::DirHandle;
use kaban_codegraph::schema::{Confidence, EdgeType, NodeId, SCHEMA_VERSION, SYMBOLS_FTS_VERSION};
use kaban_codegraph::symbol::{self, Descriptor};
use kaban_codegraph::write::{
    self, EdgeRow, EdgeTypeRow, FileRow, OccurrenceRow, SnippetRow, SymbolRow,
};
use kaban_codegraph::{IdentityError, IsolationTier, WriteError};
use kaban_policy::Policy;
use rustix::fs::{Mode, OFlags, RenameFlags};
use rustix::io::Errno;
use std::os::fd::{AsFd, BorrowedFd};
use thiserror::Error;

use crate::derive::{self, Counts, Dangling};
use crate::gate::{self, Admitted, GateCounts, WarningClass};
use crate::ident_terms;
use crate::ranges::{RangeForm, RangeForms};
use crate::union::{self, UNION_REASON};

/// The `scip` crate pin the schema was read from (`meta.scip_version`).
pub const SCIP_VERSION: &str = "0.10.0";

/// The snapshot's suffixes (stage1-spec §4.1).
const FINAL_SUFFIX: &str = ".cg.db";
const TMP_INFIX: &str = ".tmp-";
/// The marker beside a snapshot whose directory fsync failed (U1 residual 3).
const UNSYNCED_SUFFIX: &str = ".cg.db.unsynced";

/// The directory `fsync` after the rename, injectable for the `published_unsynced` test
/// (U1 residual 3); production passes [`dir_fsync`].
pub type DirFsync = fn(BorrowedFd<'_>) -> Result<(), Errno>;

/// The production directory fsync: the admitted handle is `O_PATH` (no I/O through it), so the
/// directory is reopened as `.` relative to the handle (`O_RDONLY|O_DIRECTORY`, still never
/// through a path string) and that descriptor is fsynced.
///
/// # Errors
/// The errno of the reopen or of `fsync`.
pub fn dir_fsync(fd: BorrowedFd<'_>) -> Result<(), Errno> {
    let dir = rustix::fs::openat(
        fd,
        ".",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    rustix::fs::fsync(&dir)
}

/// A 40-hex commit id, validated once.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Commit(String);

impl Commit {
    /// Accept exactly 40 lowercase hex digits.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        let hex = s.len() == 40
            && s.bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase());
        hex.then(|| Self(s.to_owned()))
    }

    /// The hex text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Commit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The provenance facts of G1-S5 (OWN-29), recorded verbatim in `meta`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provenance {
    /// The tier that ran.
    pub isolation: IsolationTier,
    /// The tier's detail text (gated by the caller as a meta value).
    pub isolation_detail: String,
    /// The kinds of admitted read-only resources the runs were given (`meta.resources`,
    /// comma-joined, sorted: `modcache`, `registry`, `toolchain`; empty when none).
    pub resources: Vec<&'static str>,
    /// The image digest, container tier only.
    pub container_image_digest: Option<String>,
}

/// The caller-supplied `meta` values (stage1-spec §4.1); everything else in `meta` is derived
/// from the rows. No timestamp field exists on purpose (G1-N5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Meta {
    /// The indexed commit (also the file name).
    pub commit: Commit,
    /// `<repo-id>` (a storage key, RS3-3).
    pub repo_id: String,
    /// The policy's version hash.
    pub policy_version: String,
    /// Isolation provenance.
    pub provenance: Provenance,
}

/// The row tables of one snapshot in the §4.1 order, ready for [`publish`]. Built only by
/// [`Rows::derive`]; read-only afterwards (RU1-2), except the one `snippets` attachment of
/// [`Rows::set_snippets`] (CG8.2: the rows are collected from git after the derivation).
#[derive(Debug, Clone)]
pub struct Rows {
    files: Vec<FileRow>,
    symbols: Vec<SymbolRow>,
    edges: Vec<EdgeRow>,
    occurrences: Vec<OccurrenceRow>,
    snippets: Vec<SnippetRow>,
    indexers: BTreeMap<String, String>,
    range_forms: RangeForms,
    union_reason: Option<&'static str>,
    gate_counts: GateCounts,
    b_gate_counts: Option<GateCounts>,
    counts: Counts,
    /// The rule ids behind the gate's `policy_redacted_meta` (A then B), deduplicated.
    meta_rule_ids: Vec<String>,
    /// The rule ids behind `counts.policy_skipped_source`, deduplicated.
    source_rule_ids: Vec<String>,
    /// The producer's coverage sidecar (R-1; `None` until [`Rows::set_coverage`], and for a
    /// producer that writes none).
    coverage: Option<Coverage>,
    /// The nested Cargo roots the run did not index (backlog 17; `None` until
    /// [`Rows::set_nested_workspaces`]).
    nested_workspaces: Option<Vec<String>>,
}

impl Rows {
    /// Derive every table from admitted run A (and Go run B, G1-F3) under `policy` (the
    /// formed-string re-scan of G1-S1 step 3). B's admission counters are kept apart
    /// (`counts:b_*`, RU1-5); the union's drops are folded into the derivation counters.
    #[must_use]
    pub fn derive(a: &Admitted<'_>, b: Option<&Admitted<'_>>, policy: &Policy) -> Self {
        let union = union::implements(a, b);
        let nodes = derive::nodes(a, &union.set, policy);
        Self::from_nodes(a, b, policy, &union, nodes)
    }

    /// Derive rows with Rust test scopes from the indexed commit, never the working tree
    /// or producer-supplied document text. The CLI calls this only after trust and project-root
    /// validation. Non-Rust documents retain the source-free derivation path.
    ///
    /// # Errors
    /// Refuses unavailable, oversize or malformed Rust source rather than publishing incomplete
    /// test classification (a policy hit skips the file and counts, DF-3). Git retains its
    /// pinned-tool/deadline/FD rules.
    pub fn derive_committed(
        a: &Admitted<'_>,
        b: Option<&Admitted<'_>>,
        policy: &Policy,
        repo: &Path,
        commit: &Commit,
    ) -> Result<Self, crate::IndexError> {
        Self::derive_committed_with(
            a,
            b,
            policy,
            repo,
            commit,
            kaban_core::process::InheritedFds::Refuse,
        )
    }

    /// The whole DB-free pipeline from `index.scip` bytes to rows (S0 §7 I-8; extracted from the
    /// `index` verb): decode and gate run A (and run B), check the project root, admit, derive
    /// with the committed-source test classification, then collect the snippets.
    ///
    /// # Errors
    /// [`DeriveError::Gate`] for decode or classification, [`DeriveError::ProjectRoot`] when an
    /// index names another root.
    pub fn from_scip(
        a: &[u8],
        b: Option<&[u8]>,
        policy: &Policy,
        repo: &Path,
        commit: &Commit,
    ) -> Result<Self, DeriveError> {
        let a = crate::scip_read::decode_and_gate(a, policy).map_err(DeriveError::Gate)?;
        let b = b
            .map(|bytes| crate::scip_read::decode_and_gate(bytes, policy))
            .transpose()
            .map_err(DeriveError::Gate)?;
        for gated in std::iter::once(&a).chain(b.as_ref()) {
            derive::check_project_root(&gated.metadata().project_root_identity, repo)
                .map_err(|_| DeriveError::ProjectRoot)?;
        }
        let admitted_a = gate::admit_index(&a);
        let admitted_b = b.as_ref().map(gate::admit_index);
        let mut rows = Self::derive_committed(&admitted_a, admitted_b.as_ref(), policy, repo, commit)
            .map_err(DeriveError::Gate)?;
        let snippets = crate::snippets::collect(repo, commit.as_str(), &rows, policy);
        rows.set_snippets(snippets);
        Ok(rows)
    }

    /// [`Self::derive_committed`] with an explicit git descriptor policy for in-process
    /// tests running under a build lock. Production uses [`Self::derive_committed`].
    ///
    /// # Errors
    /// As [`Self::derive_committed`].
    pub fn derive_committed_with(
        a: &Admitted<'_>,
        b: Option<&Admitted<'_>>,
        policy: &Policy,
        repo: &Path,
        commit: &Commit,
        inherited_fds: kaban_core::process::InheritedFds,
    ) -> Result<Self, crate::IndexError> {
        let union = union::implements(a, b);
        let mut nodes = derive::nodes(a, &union.set, policy);
        crate::language::classify_committed_tests(&mut nodes, repo, commit, policy, inherited_fds)?;
        Ok(Self::from_nodes(a, b, policy, &union, nodes))
    }

    fn from_nodes(
        a: &Admitted<'_>,
        b: Option<&Admitted<'_>>,
        policy: &Policy,
        union: &union::Union<'_>,
        mut nodes: derive::Nodes,
    ) -> Self {
        let structural = derive::structural_edges(&nodes, a, &union.set);
        let occurrences = derive::occurrences_and_references(&nodes, a);
        let tested = derive::tested_by(&nodes, &occurrences.references);
        let naming = derive::tested_by_naming(&nodes);
        let edges = merge_edges([structural, occurrences.references, tested, naming]);
        let range_forms = fold_forms(nodes.range_forms, occurrences.range_forms);
        nodes.counts.policy_dropped += fill_ident_terms(&mut nodes.symbols, policy);
        nodes.counts.policy_skipped_row += union.policy_skipped;
        nodes.counts.grammar_skipped_relationships += union.grammar_skipped;
        nodes.counts.malformed_range_skipped += occurrences.malformed_range_skipped;
        let mut indexers = BTreeMap::new();
        let mut languages: Vec<&str> = nodes.files.iter().map(|f| f.language.as_str()).collect();
        languages.sort_unstable();
        languages.dedup();
        for lang in languages {
            indexers.insert(lang.to_owned(), tool_text(a));
        }
        let mut meta_rule_ids: Vec<String> = a.policy_redacted_meta_rules().to_vec();
        for id in b
            .map(Admitted::policy_redacted_meta_rules)
            .unwrap_or_default()
        {
            if !meta_rule_ids.contains(id) {
                meta_rule_ids.push(id.clone());
            }
        }
        Self {
            files: nodes.files,
            symbols: nodes.symbols,
            edges,
            occurrences: occurrences.rows,
            snippets: Vec::new(),
            indexers,
            range_forms,
            union_reason: b.map(|_| UNION_REASON),
            gate_counts: a.counts(),
            b_gate_counts: b.map(Admitted::counts),
            counts: nodes.counts,
            meta_rule_ids,
            source_rule_ids: nodes.source_rule_ids,
            coverage: None,
            nested_workspaces: None,
        }
    }

    /// The rule ids behind `counts:policy_skipped_source` (DF-3; rule ids only).
    #[must_use]
    pub fn policy_skipped_source_rules(&self) -> &[String] {
        &self.source_rule_ids
    }

    /// `files`.
    #[must_use]
    pub fn files(&self) -> &[FileRow] {
        &self.files
    }

    /// `symbols`.
    #[must_use]
    pub fn symbols(&self) -> &[SymbolRow] {
        &self.symbols
    }

    /// `edges`.
    #[must_use]
    pub fn edges(&self) -> &[EdgeRow] {
        &self.edges
    }

    /// `occurrences`.
    #[must_use]
    pub fn occurrences(&self) -> &[OccurrenceRow] {
        &self.occurrences
    }

    /// `snippets` (CG8.2; empty until [`Rows::set_snippets`]).
    #[must_use]
    pub fn snippets(&self) -> &[SnippetRow] {
        &self.snippets
    }

    /// Attach the collected snippet rows (`snippets::collect`, sorted by `symbol_id`) and fold
    /// the collection's counters in: `policy_dropped` into `counts.policy_dropped`, so
    /// [`Rows::warnings`] and `meta.counts:policy_dropped` carry it unchanged, and `skipped`
    /// into `counts.snippet_skipped`.
    pub fn set_snippets(&mut self, collected: crate::snippets::Collected) {
        self.snippets = collected.rows;
        self.counts.policy_dropped += collected.policy_dropped;
        self.counts.snippet_skipped += collected.skipped;
    }

    /// Attach the producer's validated coverage sidecar (R-1): its counters become the
    /// `coverage:<key>` meta rows and its table the one `coverage:per_crate` value.
    pub fn set_coverage(&mut self, coverage: Coverage) {
        self.coverage = Some(coverage);
    }

    /// Name the nested Cargo roots (`detect::nested_workspaces`, relative paths) this run did
    /// not index (backlog 17): their count becomes the `coverage:nested_workspaces_not_indexed`
    /// row and their list the `coverage:nested_workspace_paths` JSON value.
    pub fn set_nested_workspaces(&mut self, paths: Vec<String>) {
        self.nested_workspaces = Some(paths);
    }

    /// `indexer:<lang>` values from the producer's `tool_info`, by language.
    #[must_use]
    pub fn indexers(&self) -> &BTreeMap<String, String> {
        &self.indexers
    }

    /// The range forms read (`meta.scip_range_form`).
    #[must_use]
    pub fn range_forms(&self) -> RangeForms {
        self.range_forms
    }

    /// Present when run B was unioned in (`meta.union_reason`).
    #[must_use]
    pub fn union_reason(&self) -> Option<&'static str> {
        self.union_reason
    }

    /// Run A's gate counters (CG3.1).
    #[must_use]
    pub fn gate_counts(&self) -> GateCounts {
        self.gate_counts
    }

    /// Run B's gate counters when B was unioned in (RU1-5).
    #[must_use]
    pub fn b_gate_counts(&self) -> Option<GateCounts> {
        self.b_gate_counts
    }

    /// The derivation counters.
    #[must_use]
    pub fn counts(&self) -> Counts {
        self.counts
    }

    /// The warning classes of the run, for the JSON report: A's, B's and the derivation's
    /// non-zero classes, in the fixed order (`policy_redacted_meta` is the publication's: the
    /// formed `meta` rows are scanned there, see [`Published`]).
    #[must_use]
    pub fn warnings(&self) -> Vec<WarningClass> {
        let mut out = Vec::new();
        let skipped = self.gate_counts.policy_skipped_row
            + self.b_gate_counts.map_or(0, |b| b.policy_skipped_row)
            + self.counts.policy_skipped_row;
        let dropped = self.gate_counts.policy_dropped
            + self.b_gate_counts.map_or(0, |b| b.policy_dropped)
            + self.counts.policy_dropped;
        if skipped > 0 {
            out.push(WarningClass::PolicySkippedRow);
        }
        if dropped > 0 {
            out.push(WarningClass::PolicyDropped);
        }
        if self.counts.policy_skipped_source > 0 {
            out.push(WarningClass::PolicySkippedSource);
        }
        if self.counts.source_classified_by_path > 0 {
            out.push(WarningClass::SourceClassifiedByPath);
        }
        out
    }

    /// The `edge_types` derivation table of stage1-spec §4.3, materialised (sorted by type).
    #[must_use]
    pub fn edge_types() -> Vec<EdgeTypeRow> {
        EdgeType::ALL
            .into_iter()
            .map(|t| {
                let (source, confidence) = match t {
                    EdgeType::Contains => (
                        "descriptor grammar of the gated SymbolInformation.symbol",
                        Confidence::Measured,
                    ),
                    EdgeType::DefinedIn => {
                        ("Occurrence.symbol_roles & Definition", Confidence::Measured)
                    }
                    EdgeType::Implements => (
                        "Relationship.is_implementation, endpoints not both members",
                        Confidence::Measured,
                    ),
                    EdgeType::ImplementsMember => (
                        "Relationship.is_implementation, endpoints both Method or MethodSpecification",
                        Confidence::Measured,
                    ),
                    EdgeType::References => (
                        "non-definition occurrence inside the enclosing range of E, else the file node",
                        Confidence::Rule,
                    ),
                    EdgeType::TestedBy => (
                        "reference form: occurrences in test files (rule); naming form: file names (heuristic)",
                        Confidence::Heuristic,
                    ),
                    EdgeType::TypeOf => ("Relationship.is_type_definition", Confidence::Measured),
                };
                EdgeTypeRow {
                    r#type: t,
                    source: source.to_owned(),
                    confidence,
                }
            })
            .collect()
    }

    /// The `meta` rows, sorted by key (stage1-spec §4.1 keys plus `scip_range_form`,
    /// `symbols_fts_version`, `union_reason`, `counts:*` and the producer's `coverage:*`, R-1;
    /// no timestamps). The descriptive values ([`MetaValue::Descriptive`]: the `indexer:<lang>`
    /// join, `isolation_detail`, the image digest) are formed strings re-scanned under the meta
    /// rule (RU1-1); one with a hit is written as its redaction (DF-3) and
    /// `counts:policy_redacted_meta` sums the gate's masked values and these; the
    /// `coverage:per_crate` table is one such value (r1-5: a crate-name hit masks the whole
    /// table). The `coverage:nested_workspace_paths` list gates every raw path the same way
    /// before the list is formed, then the formed string (winB-1: an anchored rule, or one with
    /// a literal newline, matches the raw path and not its quoted, escaped JSON form). The
    /// operational values are written as they are: a masked `isolation`, `commit` or
    /// `schema_version` would fail the reader's admission (df23-4).
    #[must_use]
    pub fn meta_rows(&self, meta: &Meta, policy: &Policy) -> MetaRows {
        let mut rule_ids: Vec<String> = self.meta_rule_ids.clone();
        let mut redacted = 0;
        let mut gate = |text: String| match gate::formed_meta(policy, &text) {
            None => text,
            Some(r) => {
                redacted += 1;
                for id in r.rule_ids {
                    if !rule_ids.contains(&id) {
                        rule_ids.push(id);
                    }
                }
                r.text
            }
        };
        let mut rows: Vec<(String, String)> = self
            .unscanned_meta_rows(meta)
            .into_iter()
            .map(|(key, value)| {
                let text = match value {
                    MetaValue::Operational(text) => text,
                    MetaValue::Descriptive(text) => gate(text),
                    MetaValue::Paths(paths) => {
                        let gated: Vec<String> = paths.into_iter().map(&mut gate).collect();
                        gate(json_list(&gated))
                    }
                };
                (key, text)
            })
            .collect();
        let total = self.gate_counts.policy_redacted_meta
            + self.b_gate_counts.map_or(0, |b| b.policy_redacted_meta)
            + redacted;
        let key = "counts:policy_redacted_meta".to_owned();
        let at = rows.partition_point(|(k, _)| *k < key);
        rows.insert(at, (key, total.to_string()));
        MetaRows {
            rows,
            policy_redacted_meta: total,
            rule_ids,
        }
    }

    /// The `meta` rows before the formed-string scan, each value classed by what it carries.
    fn unscanned_meta_rows(&self, meta: &Meta) -> Vec<(String, MetaValue)> {
        let mut rows: BTreeMap<String, MetaValue> = BTreeMap::new();
        let mut put = |k: &str, v: String| {
            rows.insert(k.to_owned(), MetaValue::Operational(v));
        };
        put("schema_version", SCHEMA_VERSION.to_string());
        put("commit", meta.commit.to_string());
        put("repo_id", meta.repo_id.clone());
        put(
            "languages",
            self.indexers.keys().cloned().collect::<Vec<_>>().join(","),
        );
        put("kaban_index_version", env!("CARGO_PKG_VERSION").to_owned());
        put("scip_version", SCIP_VERSION.to_owned());
        put("policy_version", meta.policy_version.clone());
        put("isolation", meta.provenance.isolation.as_str().to_owned());
        put("resources", meta.provenance.resources.join(","));
        put("scip_range_form", self.range_forms.as_str().to_owned());
        put("symbols_fts_version", SYMBOLS_FTS_VERSION.to_string());
        if let Some(reason) = self.union_reason {
            put("union_reason", reason.to_owned());
        }
        put("counts:files", self.files.len().to_string());
        put("counts:symbols", self.symbols.len().to_string());
        put("counts:edges", self.edges.len().to_string());
        put("counts:occurrences", self.occurrences.len().to_string());
        for (k, v) in self.counts.entries() {
            put(&format!("counts:{k}"), v.to_string());
        }
        put(
            "counts:policy_skipped_row",
            (self.gate_counts.policy_skipped_row + self.counts.policy_skipped_row).to_string(),
        );
        put(
            "counts:policy_dropped",
            (self.gate_counts.policy_dropped + self.counts.policy_dropped).to_string(),
        );
        if let Some(b) = self.b_gate_counts {
            put(
                "counts:b_policy_skipped_row",
                b.policy_skipped_row.to_string(),
            );
            put("counts:b_policy_dropped", b.policy_dropped.to_string());
        }
        if let Some(coverage) = &self.coverage {
            for (k, v) in &coverage.counters {
                put(&format!("coverage:{k}"), v.to_string());
            }
        }
        if let Some(nested) = &self.nested_workspaces {
            put(
                "coverage:nested_workspaces_not_indexed",
                nested.len().to_string(),
            );
        }
        let mut describe = |k: &str, v: String| {
            rows.insert(k.to_owned(), MetaValue::Descriptive(v));
        };
        for (lang, text) in &self.indexers {
            describe(&format!("indexer:{lang}"), text.clone());
        }
        if let Some(coverage) = &self.coverage {
            describe("coverage:per_crate", coverage.per_crate_json());
        }
        describe("isolation_detail", meta.provenance.isolation_detail.clone());
        if let Some(d) = &meta.provenance.container_image_digest {
            describe("container_image_digest", d.clone());
        }
        // repository paths, gated one by one before the list is formed (winB-1)
        if let Some(nested) = &self.nested_workspaces {
            rows.insert(
                "coverage:nested_workspace_paths".to_owned(),
                MetaValue::Paths(nested.clone()),
            );
        }
        rows.into_iter().collect()
    }
}

/// Why [`Rows::from_scip`] failed.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum DeriveError {
    /// Decode or committed-source classification failed.
    #[error("index decode or classification failed")]
    Gate(crate::IndexError),
    /// An index names another project root.
    #[error("project root mismatch")]
    ProjectRoot,
}

/// A formed JSON list meta value (compact, the items in their order).
fn json_list<T: serde::Serialize>(items: &[T]) -> String {
    serde_json::to_string(items).unwrap_or_else(|_| "[]".to_owned())
}

/// The Rust producer's coverage sidecar (`coverage.json` beside `index.scip`,
/// `tools/kaban-rust-scip/src/semantic.rs` `Coverage`; R-1), validated: a JSON object whose
/// keys are the producer's counters ([`Coverage::KEYS`]) with non-negative integer values, plus
/// `per_crate`, an array of `{crate, declarations, block_items_unique, block_item_lines}` rows
/// with Cargo-shaped crate names. Anything else (an unknown key, a string where a count goes, a
/// file over [`Coverage::MAX_BYTES`]) refuses the whole sidecar: the run then publishes no
/// `coverage:*` row and warns `coverage_invalid`, never a wrong figure. The counters are
/// operational metadata (never redacted); the table is descriptive (its names are text).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Coverage {
    counters: BTreeMap<String, u64>,
    per_crate: Vec<CrateCoverage>,
}

/// One row of the producer's `per_crate` table: exactly these keys (r1-6; an unknown one
/// refuses the sidecar like an unknown counter does).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CrateCoverage {
    /// The Cargo package name.
    #[serde(rename = "crate")]
    pub name: String,
    /// Published declarations of the package.
    pub declarations: u64,
    /// Distinct skipped block items in its files.
    pub block_items_unique: u64,
    /// Lines inside those items, each once.
    pub block_item_lines: u64,
}

/// Why a coverage sidecar was refused (the fixed class only; no value is echoed).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum CoverageError {
    /// Larger than [`Coverage::MAX_BYTES`].
    #[error("coverage sidecar too large")]
    TooLarge,
    /// Not a JSON object.
    #[error("coverage sidecar is not a JSON object")]
    NotObject,
    /// A key outside [`Coverage::KEYS`] and `per_crate`.
    #[error("coverage sidecar has an unknown key")]
    UnknownKey,
    /// A counter that is not a non-negative integer.
    #[error("coverage sidecar counter is not an integer")]
    NotInteger,
    /// `per_crate` is not an array of well-formed rows, or has too many.
    #[error("coverage sidecar per_crate table is malformed")]
    Table,
}

impl Coverage {
    /// The sidecar is a few hundred bytes plus one row per package: a megabyte bounds it.
    pub const MAX_BYTES: u64 = 1 << 20;
    /// At most this many `per_crate` rows (a workspace of more packages is a different tool).
    pub const MAX_CRATES: usize = 4096;
    /// The producer's counters, in its order.
    pub const KEYS: [&'static str; 16] = [
        "targets_skipped",
        "proc_macros_not_built",
        "contexts",
        "declarations",
        "block_items_skipped",
        "block_item_references",
        "shared_file_contexts_skipped",
        "incomplete_modules",
        "divergent_bodies",
        "cfg_divergent_declarations",
        "tokens_without_symbol",
        "positions_skipped",
        "scip_bytes",
        "block_items_unique",
        "block_item_lines",
        "derived_impls_generic_skipped",
    ];

    /// Parse and validate the sidecar's bytes.
    ///
    /// # Errors
    /// [`CoverageError`] by class; nothing of the file is kept on a refusal.
    pub fn parse(bytes: &[u8]) -> Result<Self, CoverageError> {
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > Self::MAX_BYTES {
            return Err(CoverageError::TooLarge);
        }
        let value: serde_json::Value =
            serde_json::from_slice(bytes).map_err(|_| CoverageError::NotObject)?;
        let object = value.as_object().ok_or(CoverageError::NotObject)?;
        let mut counters = BTreeMap::new();
        let mut per_crate = Vec::new();
        for (key, value) in object {
            if key == "per_crate" {
                let rows = value.as_array().ok_or(CoverageError::Table)?;
                if rows.len() > Self::MAX_CRATES {
                    return Err(CoverageError::Table);
                }
                for row in rows {
                    let row: CrateCoverage =
                        serde_json::from_value(row.clone()).map_err(|_| CoverageError::Table)?;
                    if !is_cargo_name(&row.name) {
                        return Err(CoverageError::Table);
                    }
                    per_crate.push(row);
                }
                continue;
            }
            if !Self::KEYS.contains(&key.as_str()) {
                return Err(CoverageError::UnknownKey);
            }
            let count = value.as_u64().ok_or(CoverageError::NotInteger)?;
            counters.insert(key.clone(), count);
        }
        Ok(Self {
            counters,
            per_crate,
        })
    }

    /// The counters present, by key.
    #[must_use]
    pub fn counters(&self) -> &BTreeMap<String, u64> {
        &self.counters
    }

    /// The table as the one `coverage:per_crate` meta value (compact JSON, the rows in the
    /// producer's order).
    #[must_use]
    pub fn per_crate_json(&self) -> String {
        json_list(&self.per_crate)
    }
}

/// A Cargo package name: 1 to 64 ASCII letters, digits, `-` or `_`.
fn is_cargo_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// A `meta` value by what it carries (df23-4). Operational metadata is typed by its producer
/// (the 40-hex commit, a tier or cache name, a version, a count, the `repo_id` storage key) and
/// is read back by the reader's admission (`discover::snapshot_ref` parses `commit`,
/// `isolation` and `policy_version`), so it is written as it is. Descriptive text (the
/// producer's `indexer:<lang>` tool text, the tier's detail text, the image digest) can carry
/// anything and is scanned under the meta rule before publication. The nested workspace paths
/// are kept as items so each raw path is scanned before the list is formed, then the formed
/// string (winB-1).
#[derive(Debug, Clone, PartialEq, Eq)]
enum MetaValue {
    Operational(String),
    Descriptive(String),
    Paths(Vec<String>),
}

/// Every edge list into one, sorted `(src, type, dst)` (ids are `ref`-order positions and
/// `EdgeType`'s `Ord` is its stored-text order, so this is the §4.1 order); distinct types never
/// collide, so no weight is summed here.
fn merge_edges<const N: usize>(lists: [Vec<EdgeRow>; N]) -> Vec<EdgeRow> {
    let mut all: BTreeMap<(NodeId, EdgeType, NodeId), u32> = BTreeMap::new();
    for e in lists.into_iter().flatten() {
        all.entry((e.src, e.r#type, e.dst))
            .and_modify(|w| *w = (*w).max(e.weight))
            .or_insert(e.weight);
    }
    all.into_iter()
        .map(|((src, ty, dst), weight)| EdgeRow {
            src,
            dst,
            r#type: ty,
            weight,
        })
        .collect()
}

/// The forms of the definition ranges and of the sites, folded.
fn fold_forms(a: RangeForms, b: RangeForms) -> RangeForms {
    match b {
        RangeForms::None => a,
        RangeForms::Typed => a.with(RangeForm::Typed),
        RangeForms::Legacy => a.with(RangeForm::Legacy),
        RangeForms::Mixed => a.with(RangeForm::Typed).with(RangeForm::Legacy),
    }
}

/// `symbols.ident_terms` from the gated `name`, the last descriptor of `key` (the symbol
/// string for a local, nothing for a file node) and `label`, re-scanned as a formed string
/// (G1-S1 step 3): a hit empties the field. Returns the number emptied (`policy_dropped`).
fn fill_ident_terms(symbols: &mut [SymbolRow], policy: &Policy) -> u64 {
    let mut dropped = 0;
    for row in symbols {
        let last = row
            .key
            .as_deref()
            .or(row.scip.as_deref())
            .and_then(|s| symbol::parse(s).ok())
            .and_then(|sym| sym.descriptors.last().map(descriptor_text))
            .unwrap_or_default();
        let terms = ident_terms::build(&row.name, &last, &row.label);
        let verdict = policy.scan(&terms);
        if verdict.rejected || !verdict.hits.is_empty() {
            dropped += 1;
            row.ident_terms = String::new();
        } else {
            row.ident_terms = terms;
        }
    }
    dropped
}

/// One descriptor in its grammar spelling (the identifier with its suffix).
fn descriptor_text(d: &Descriptor) -> String {
    match d {
        Descriptor::Namespace(n) => format!("{n}/"),
        Descriptor::Type(n) => format!("{n}#"),
        Descriptor::Term(n) => format!("{n}."),
        Descriptor::Method {
            name,
            disambiguator,
        } => format!("{name}({disambiguator})."),
        Descriptor::TypeParameter(n) => format!("[{n}]"),
        Descriptor::Parameter(n) => format!("({n})"),
        Descriptor::Meta(n) => format!("{n}:"),
        Descriptor::Local(n) => n.clone(),
        Descriptor::Macro(n) => format!("{n}!"),
    }
}

/// `indexer:<lang>` text: `<tool> <version> <argv...>` from run A's gated `tool_info`.
fn tool_text(a: &Admitted<'_>) -> String {
    let m = a.metadata();
    let mut parts = vec![m.tool_name(), m.tool_version()];
    parts.extend(m.tool_arguments().iter().map(String::as_str));
    parts.join(" ")
}

/// Why a publication failed. The path is not part of the text: the caller knows it.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum PublishError {
    /// The final name already exists (`RENAME_NOREPLACE` refused; the temp file is removed).
    #[error("snapshot already published")]
    Exists,
    /// The temp name already exists (a concurrent writer with the same pid, or a leftover).
    #[error("temp file exists")]
    TempExists,
    /// The file at the temp name after the write is not the inode created through the
    /// directory handle (replaced during the write): nothing is published.
    #[error("temp file replaced during the write")]
    TempSwapped,
    /// A filesystem call failed (open, fsync, rename), as its errno class.
    #[error("filesystem: {0}")]
    Fs(#[source] Errno),
    /// A write helper refused or SQLite failed.
    #[error("write: {0}")]
    Write(#[from] WriteError),
    /// An edge endpoint without a row (a derivation bug; nothing is written).
    #[error("{0}")]
    Dangling(#[from] Dangling),
    /// The rename succeeded but the directory `fsync` after it failed (`published_unsynced`,
    /// U1 residual 3): the snapshot exists under `name` in the admitted directory and may not
    /// be durable; the `.unsynced` marker beside it makes the next run re-verify it. Exit
    /// non-zero.
    #[error("published_unsynced: {source}")]
    Unsynced {
        /// The final name of the snapshot that was published (`<commit40>.cg.db`).
        name: String,
        /// The fsync errno.
        #[source]
        source: Errno,
    },
    /// The path-spelled `dir` could not be walked into an admitted handle (a symlink component,
    /// not a directory).
    #[error("directory refused: {0}")]
    Dir(#[from] IdentityError),
}

impl PublishError {
    /// The fixed class text of this failure (the JSON report's `class`).
    #[must_use]
    pub const fn class(&self) -> &'static str {
        match self {
            Self::Exists => "snapshot_exists",
            Self::TempExists => "temp_exists",
            Self::TempSwapped => "temp_swapped",
            Self::Fs(_) => "fs",
            Self::Write(_) => "write",
            Self::Dangling(_) => "dangling",
            Self::Unsynced { .. } => "published_unsynced",
            Self::Dir(_) => "data_dir_refused",
        }
    }

    /// The process exit code of G1-F7/§7: every publication failure is 1 (`published_unsynced`
    /// included: the run is not `ok`; a meta policy hit is masked, never a failure, DF-3).
    #[must_use]
    pub const fn exit_code(&self) -> u8 {
        1
    }
}

/// Whether `<dir>/<commit40>.cg.db` carries the `.unsynced` marker of a publication whose
/// directory fsync failed: the snapshot is present but the next run re-verifies it (opens it
/// afresh, `quick_check` memo not trusted) before using it.
#[must_use]
pub fn is_unsynced(dir: &DirHandle, commit: &Commit) -> bool {
    rustix::fs::statat(
        dir,
        format!("{commit}{UNSYNCED_SUFFIX}"),
        rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
    )
    .is_ok()
}

/// Remove the `.unsynced` marker of `commit` after a successful re-verification.
///
/// # Errors
/// [`PublishError::Fs`] when the unlink fails for a reason other than absence.
pub fn clear_unsynced(dir: &DirHandle, commit: &Commit) -> Result<(), PublishError> {
    match rustix::fs::unlinkat(
        dir,
        format!("{commit}{UNSYNCED_SUFFIX}"),
        rustix::fs::AtFlags::empty(),
    ) {
        Ok(()) | Err(Errno::NOENT) => Ok(()),
        Err(e) => Err(PublishError::Fs(e)),
    }
}

impl From<Errno> for PublishError {
    fn from(e: Errno) -> Self {
        Self::Fs(e)
    }
}

/// Best-effort removal of the created temp file unless publication succeeded. Preserve a
/// name known to refer to a replacement; this identity check and unlink are not atomic.
struct TempGuard<'a> {
    dir: &'a DirHandle,
    name: &'a str,
    identity: rustix::fs::Stat,
    armed: bool,
}

impl Drop for TempGuard<'_> {
    fn drop(&mut self) {
        if self.armed
            && rustix::fs::statat(self.dir, self.name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW)
                .is_ok_and(|stat| {
                    (stat.st_dev, stat.st_ino) == (self.identity.st_dev, self.identity.st_ino)
                })
        {
            let _ = rustix::fs::unlinkat(self.dir, self.name, rustix::fs::AtFlags::empty());
        }
    }
}

/// Publish `rows` as `<dir>/<commit40>.cg.db` through the §4.1 file protocol and return the
/// final path (the canonical directory plus the published name). `dir` is spelled as a path
/// here: it is canonicalised and re-walked component by component into a handle (a symlink
/// component refuses, U1 residual 2) and the publication goes through [`publish_admitted`]
/// with the production directory fsync.
///
/// # Errors
/// [`PublishError`]: the walk, an existing final or temp name, a filesystem
/// error, a write helper refusal (best-effort cleanup of the created temp file), `Unsynced`.
pub fn publish(
    dir: &Path,
    rows: &Rows,
    meta: &Meta,
    policy: &Policy,
) -> Result<PathBuf, PublishError> {
    let canonical = std::fs::canonicalize(dir).map_err(IdentityError::from)?;
    let handle = kaban_codegraph::identity::walk(&canonical)?;
    let published = publish_admitted(&handle, rows, meta, policy, dir_fsync)?;
    Ok(canonical.join(published.name))
}

/// The `meta` rows ready to write (DF-3): every value re-scanned, hits replaced by their
/// redaction; the count and the rule ids for the report (rule ids only).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetaRows {
    /// `(key, value)` sorted by key, `counts:policy_redacted_meta` included.
    pub rows: Vec<(String, String)>,
    /// Masked meta values, the gate's (A and B) plus the formed rows'.
    pub policy_redacted_meta: u64,
    /// The rule ids behind them, deduplicated.
    pub rule_ids: Vec<String>,
}

/// What [`publish_admitted`] produced: the final name inside the admitted directory, the
/// size of the published file (`fstat` of the written inode) and the meta redactions (DF-3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Published {
    /// `<commit40>.cg.db`.
    pub name: String,
    /// The published file's size in bytes.
    pub bytes: u64,
    /// Masked meta values (`counts:policy_redacted_meta`).
    pub policy_redacted_meta: u64,
    /// Their rule ids, deduplicated (for `policy_redacted_meta_per_indexer`).
    pub policy_redacted_meta_rules: Vec<String>,
}

/// Publish `rows` as `<commit40>.cg.db` inside the admitted directory `dir` (U1 residual 1 and
/// U2 RU2-10: the handle is the only spelling of the directory; the temp file is created
/// through it, the finalized SQLite image is copied to that created descriptor, the name
/// is confirmed to still point at that inode, and the rename and the directory fsync go
/// through the handle; no caller string is involved). The `meta` values are re-scanned under
/// `policy` first (RU1-1): a hit refuses before the temp file. `dir_fsync` is the directory
/// fsync after the rename ([`dir_fsync`] in production): its failure is `Unsynced` (U1
/// residual 3).
///
/// # Errors
/// [`PublishError`]: a meta policy hit, an existing final or temp name, a filesystem error, a
/// write helper refusal (best-effort cleanup of the created temp file), `Unsynced`.
pub fn publish_admitted(
    dir: &DirHandle,
    rows: &Rows,
    meta: &Meta,
    policy: &Policy,
    dir_fsync: DirFsync,
) -> Result<Published, PublishError> {
    derive::check_no_dangling(&rows.symbols, &rows.edges)?;
    let scanned = rows.meta_rows(meta, policy);
    let meta_rows = &scanned.rows;
    let final_name = format!("{}{FINAL_SUFFIX}", meta.commit);
    let tmp_name = format!(
        "{}{FINAL_SUFFIX}{TMP_INFIX}{}",
        meta.commit,
        std::process::id()
    );

    let created = rustix::fs::openat(
        dir,
        &tmp_name,
        OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::RDWR | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR,
    )
    .map_err(|e| match e {
        Errno::EXIST => PublishError::TempExists,
        other => PublishError::Fs(other),
    })?;
    let created_stat = rustix::fs::fstat(&created)?;
    let mut guard = TempGuard {
        dir,
        name: &tmp_name,
        identity: created_stat,
        armed: true,
    };

    // Fault injection at the filesystem boundary; production has no hook or callback.
    #[cfg(test)]
    tests::AFTER_TEMP_CREATE.with(|hook| {
        if let Some(replace) = hook.borrow_mut().take() {
            replace();
        }
    });

    // SQL owns no output path. The finalized image is copied only to this retained
    // descriptor, regardless of directory renames or basename replacement (U2 RU2-10).
    write_all(created.as_fd(), rows, meta_rows)?;

    // the name in the admitted directory still points at the inode SQLite wrote
    let named_stat = rustix::fs::statat(dir, &tmp_name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW)?;
    if (named_stat.st_dev, named_stat.st_ino) != (guard.identity.st_dev, guard.identity.st_ino) {
        return Err(PublishError::TempSwapped);
    }
    rustix::fs::fsync(&created)?;
    let bytes = u64::try_from(rustix::fs::fstat(&created)?.st_size).unwrap_or(0);
    drop(created);

    rustix::fs::renameat_with(dir, &tmp_name, dir, &final_name, RenameFlags::NOREPLACE).map_err(
        |e| match e {
            Errno::EXIST => PublishError::Exists,
            other => PublishError::Fs(other),
        },
    )?;
    guard.armed = false;

    if let Err(source) = dir_fsync(dir.as_fd()) {
        // present but possibly not durable: leave the marker (best effort) and report
        let marker = format!("{}{UNSYNCED_SUFFIX}", meta.commit);
        let _ = rustix::fs::openat(
            dir,
            marker,
            OFlags::CREATE | OFlags::NOFOLLOW | OFlags::WRONLY | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        );
        return Err(PublishError::Unsynced {
            name: final_name,
            source,
        });
    }
    Ok(Published {
        name: final_name,
        bytes,
        policy_redacted_meta: scanned.policy_redacted_meta,
        policy_redacted_meta_rules: scanned.rule_ids,
    })
}

/// Every table through the write helpers, in the §4.1 order, into the created descriptor's
/// inode; the connection closes on return.
fn write_all(
    created: BorrowedFd<'_>,
    rows: &Rows,
    meta_rows: &[(String, String)],
) -> Result<(), PublishError> {
    let snapshot = write::CreatedSnapshot::open(created)?;
    let conn = snapshot.connection();
    write::begin(conn)?;
    write::insert_files(conn, &rows.files)?;
    write::insert_symbols(conn, &rows.symbols)?;
    write::insert_edges(conn, &rows.edges)?;
    write::insert_occurrences(conn, &rows.occurrences)?;
    write::insert_snippets(conn, &rows.snippets)?;
    write::insert_edge_types(conn, &Rows::edge_types())?;
    let borrowed: Vec<(&str, &str)> = meta_rows
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    write::insert_meta(conn, &borrowed)?;
    write::rebuild_fts(conn)?;
    snapshot.finish()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use std::cell::RefCell;

    std::thread_local! {
        pub(super) static AFTER_TEMP_CREATE: RefCell<Option<Box<dyn FnOnce()>>> = const { RefCell::new(None) };
    }

    #[test]
    fn publication_failure_preserves_a_replaced_temporary_name() {
        struct Scratch(PathBuf);
        impl Drop for Scratch {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        for symlink in [false, true] {
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let scratch = Scratch(
                std::env::temp_dir()
                    .join(format!("kaban-publication-{}-{nonce}", std::process::id())),
            );
            std::fs::create_dir(&scratch.0).unwrap();
            let directory = std::fs::canonicalize(&scratch.0).unwrap();
            let policy = Policy::empty();
            let bytes = include_bytes!("../../../fixtures/codegraph/go-drivers/index.scip");
            let gated = crate::decode_and_gate(bytes, &policy).unwrap();
            let admitted = crate::admit_index(&gated);
            let rows = Rows::derive(&admitted, None, &policy);
            let meta = Meta {
                commit: Commit::parse(&"a1".repeat(20)).unwrap(),
                repo_id: "0123456789abcdef0123456789abcdef".to_owned(),
                policy_version: "test".to_owned(),
                provenance: Provenance {
                    isolation: IsolationTier::None,
                    isolation_detail: "test".to_owned(),
                    resources: Vec::new(),
                    container_image_digest: None,
                },
            };
            let temp = directory.join(format!("{}.cg.db.tmp-{}", meta.commit, std::process::id()));
            let retained = directory.join("retained-inode");
            let sentinel = directory.join("sentinel");
            std::fs::write(&sentinel, b"replacement sentinel").unwrap();
            let replacement = temp.clone();
            let displaced = retained.clone();
            let target = sentinel.clone();
            AFTER_TEMP_CREATE.with(|hook| {
                *hook.borrow_mut() = Some(Box::new(move || {
                    std::fs::rename(&replacement, displaced).unwrap();
                    if symlink {
                        std::os::unix::fs::symlink(target, replacement).unwrap();
                    } else {
                        std::fs::write(replacement, b"replacement sentinel").unwrap();
                    }
                }));
            });

            let error = publish(&directory, &rows, &meta, &policy).unwrap_err();
            assert!(matches!(error, PublishError::TempSwapped), "{error:?}");
            assert_eq!(
                std::fs::read(&temp).unwrap(),
                b"replacement sentinel",
                "cleanup must preserve the replacement"
            );
            assert_eq!(std::fs::read(&sentinel).unwrap(), b"replacement sentinel");
            assert!(
                std::fs::metadata(&retained).unwrap().len() > 0,
                "writing stayed on the created inode"
            );
            assert!(!directory.join(format!("{}.cg.db", meta.commit)).exists());
        }
    }
}
