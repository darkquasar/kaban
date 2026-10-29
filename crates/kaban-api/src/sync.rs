//! `kaban sync`, `kaban rebuild`, `kaban resolve` and the `serve` startup import (core-spec F13,
//! F9b, F5, S1, S5, S6, SR-7; core-plan C5.1 as amended by `reviews/m5-prep.md` M5-2, M5-3,
//! M5-6, M5-8 to M5-11, M5-13, M5-19, M5-23, M5-31, M5-32 and owner decisions OWN-18, OWN-23,
//! OWN-25, OWN-46, OWN-47; engine-plan E1.6): the file side of the project store. The database
//! is authoritative for the machine; `.kaban/<ontology>/<dir>/<id>.md` is the committed
//! serialization, one directory per serialized kind of every loaded ontology.
//!
//! ```text
//!   sync(api, repo, { stage, digest_only, ontologies })
//!     | registry.serialized_kinds()   (ontology, version, dir) per kind with serialize != none
//!     | verified tree + flock   canonical root, no symlinked .kaban/<ontology>/<dir>, one run per repo
//!     | layout_move             pre-engine .kaban/<dir>/ => .kaban/<ontology>/<dir>/ once (layout.rs)
//!     | import(repo)            walk .kaban/<ontology>/<dir>/*.md for every kind (budgeted)
//!     |   raw gate -> split -> gate every string -> typed parse -> ontology@version -> rev rules
//!     |   absence pass: git log --diff-filter=D -- .kaban/  (one bounded call)
//!     | export dirty            dirty_serializable(kinds) minus conflicts -> render -> gate -> temp+rename
//!     |   purged => tombstone, tombstoned => file deleted, else the record file (ontology: n@v)
//!     | digests + .gitignore    .kaban/<ontology>/INDEX.md per selected ontology with digest kinds
//!     | --stage                 git add -A -- .kaban        (GitRunner, 2 s)
//!     | default sink            lease `export` -> run_sink pages -> ack(seq) -> prune(10_000) -> release
//!     v
//!   SyncReport { written, rewritten, tombstones, deleted, withheld, import: ImportReport, .. }
//! ```
//!
//! **Transport completeness (EN-F4 rule).** `sync` always serializes every ontology; the
//! `--ontology` selection and `--digest-only` only decide which digests are generated.
//!
//! **The frontmatter `ontology: <name>@<version>` (OWN-47).** Every file the engine writes
//! names the ontology that declares its kind and the version loaded when it was written; the
//! import refuses a file whose version is not loaded (`ontology_version_unknown`, counted,
//! the file untouched) and validates the record's props against the loaded schema of that
//! kind. A file without the key is a pre-engine file and is read as `adr@1`: the move never
//! rewrites a byte and the second `sync` writes nothing, so those files stay key-less and
//! stay accepted for the shipped `adr` kinds; a key-less file of any other ontology's kind is
//! refused (`MissingKey("ontology")`, counted `skipped`).
//!
//! **Content boundary (S1, M5-2, M5-3, R5-1).** Every string that comes out of a file is host
//! data: the raw text is gated in overlapping windows before the YAML parser sees it (a
//! credential in a comment, a discarded duplicate key or unparsable YAML refuses the file, R5-1),
//! then the split file is gated string by string under the running subcommand's unregistered
//! scope before any typed parse (a credential-class hit skips the file and is audited;
//! redactions apply to the imported strings and the record is imported one rev above both
//! sides so the masked record is re-serialized). Every rendered document (record file,
//! tombstone, digest) passes the windowed output gate (`export::gate_document`, R5-7) before a
//! byte reaches the repository; a hit withholds that file, and length alone never does.
//!
//! **Rev rules (F13).** `file rev > db rev` imports (governance and lifecycle only through the
//! spec's transition tables; an invalid move is a conflict, M5-31); equal rev with a redact hit
//! imports the masked record at `db rev + 1`, dirty (R5-12); equal rev and equal record hash is
//! a no-op; equal rev and a different hash is a `conflicts` row with the file untouched and the
//! record excluded from export until `resolve` (R5-5); `file rev < db rev` marks the record
//! dirty so the next export rewrites the file.
//!
//! **Containment, budgets, one run per repository (R5-2, R5-3, R5-4).** The repository root is
//! canonicalized once; `.kaban` and each record directory must be real directories below it.
//! One import walks at most [`MAX_IMPORT_ENTRIES`] entries and reads at most
//! [`MAX_IMPORT_BYTES`]. Import plus projection run under an exclusive `flock` on `.kaban/`;
//! a run that cannot get it within [`LOCK_WAIT`] defers (`deferred: true`, `resolve` refuses).
//!
//! **Tombstones (OWN-18, OWN-23).** A tombstone file always wins: the local record is purged
//! through the store's purge path (S5 hygiene, audit, `ChangeLog`), a missing row becomes a purged
//! ghost, and a purged record is never rewritten with content. A file that is absent while the
//! database holds a serialized record is tombstoned only when `git log --diff-filter=D` records
//! the deletion; otherwise the record is re-serialized. Outside git, or when git fails, absence
//! never tombstones.
//!
//! **Provenance (OD-15, M5-23).** A file that moves a Decision to `accepted` grants
//! `signer_kind: human-pr` only when the file is committed and unmodified (`git status
//! --porcelain -z -- .kaban` lists no entry for it and HEAD exists); otherwise the record is
//! imported as `proposed` at `file rev + 1`, the file is rewritten on the next sync and
//! `import_warnings` counts it. Conventions and Failures follow the agent tier.
//!
//! **Git output is host content (S1, R5-8).** The deletion listing, the status listing and
//! the author identity are gated before any comparison or event construction; a hit makes
//! that evidence unavailable (nothing tombstoned, acceptances import as `proposed`) or omits
//! the signer, is audited under the running scope with the field `git`, and is never stored
//! or logged. HEAD keeps its exact hexadecimal check; argv stays fixed and content-free.

use crate::api::KabanApi;
use crate::error::ApiError;
use crate::export::{
    DIGEST_CAP, DocumentVerdict, OntologySelection, block_kinds, gate_document, render_digest,
    selected_blocks,
};
use crate::git::GitRunner;
use crate::layout::{INDEX_FILE, MoveReport, layout_move};
use crate::ontology::SerializeDir;
use crate::serialize::{self, OntologyRef, render, render_tombstone};
use crate::types::{Card, WriteReceipt};
use kaban_core::{Ack, Change, Lifecycle, Node, NodeId, Sink, SinkId, StoreError, run_sink};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::Duration;
use thiserror::Error;

/// The kaban-owned directory inside the repository (core-spec F13).
pub const KABAN_DIR: &str = ".kaban";
/// The `.gitignore` kaban writes so a database placed under `.kaban/` is never committed (F18).
const GITIGNORE: &str = "kaban.db*\n";
/// Most records one `sync` exports (the dirty set is re-read on the next run).
const EXPORT_CAP: usize = 10_000;
/// The default sink's cursor and lease name (M5-19; the name `lease_keeper` and the C2.5
/// contenders already use): [`SinkId::EXPORT`].
pub const EXPORT_CONSUMER: &str = SinkId::EXPORT.0;
/// Lease ttl of one synchronous `sync` run (no keeper).
const LEASE_TTL_S: u32 = 60;
/// `ChangeLog` rows kept below the slowest cursor (C2.4 "tail default `10_000`").
const CHANGES_TAIL: u64 = 10_000;
/// `ChangeLog` page size of the default sink's `run_sink` loop.
const CHANGES_PAGE: usize = 2048;
/// Deadline of the import's git calls (`add`, `status`, `log`; M5-19: a 2 s runner, so the
/// pre-commit flow never hangs and a large tree does not fail the evidence check).
const STAGE_TIMEOUT: Duration = Duration::from_secs(2);
/// File mode of every file kaban writes inside the repository (committed text, not a secret
/// store: the database stays 0600, S5).
const FILE_MODE: u32 = 0o644;
/// Largest record file the import reads (a record is a few KiB; this is a safety cap).
const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;
/// Most directory entries one import walks across the three record directories, every entry
/// counted whatever its name or type (R5-3; documented in `docs/developer/SYNC.md`).
pub const MAX_IMPORT_ENTRIES: usize = 50_000;
/// Most record bytes one import reads across the three record directories (R5-3; the sum of
/// the sizes of the files it would read; documented in `docs/developer/SYNC.md`).
pub const MAX_IMPORT_BYTES: u64 = 256 * 1024 * 1024;
/// Largest current `.kaban/<ontology>/INDEX.md` or `.kaban/.gitignore` that `sync` reads to compare
/// against the new rendering (R5-10; `[default]`, documented in `docs/developer/SYNC.md`): a digest of
/// the 10,000-record cap is a few MiB. The size comes from metadata before any byte is read;
/// a larger file is refused with [`SyncError::GeneratedTooLarge`] and left as it is.
pub const MAX_GENERATED_BYTES: u64 = 64 * 1024 * 1024;
/// How long `sync`, `rebuild`, `resolve` and the startup import wait for the repository lock
/// (R5-4) before deferring their work to the holder.
const LOCK_WAIT: Duration = Duration::from_secs(10);
/// Poll interval while waiting for the repository lock.
const LOCK_POLL: Duration = Duration::from_millis(25);

/// Which side of an equal-rev conflict wins (core-spec F13 `--take file|db`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Take {
    /// The file is imported at a rev above both sides and re-serialized at that rev.
    File,
    /// The database record is kept at `rev + 1` and the file rewritten.
    Db,
}

/// Options of one `sync` run (engine-plan E1.6 interface).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncOpts {
    /// Also `git add -A -- .kaban` after the files are written (the pre-commit hook's mode).
    pub stage: bool,
    /// Only the layout move, the digests and the `.gitignore`: no import, no record files, no
    /// default-sink bookkeeping (`kaban sync --digest-only`).
    pub digest_only: bool,
    /// Which ontologies get their `INDEX.md` generated (`--ontology`); the record files are
    /// always written for every ontology.
    pub ontologies: OntologySelection,
}

/// Why `sync`, `import` or `resolve` could not finish. Messages name kaban's own vocabulary
/// and failure classes only, never a path or a value from a file (core-spec S1, S3).
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum SyncError {
    /// The repository has no `.kaban/` directory and the operation needs one.
    #[error("the repository has no .kaban directory")]
    NoKabanDir,
    /// The `.kaban/` tree holds more directory entries or more record bytes than one import
    /// reads (R5-3): nothing was imported.
    #[error("the .kaban tree exceeds the import budget (entries or bytes); nothing was imported")]
    ImportBudget,
    /// `.kaban/`, an ontology directory or one of its record directories is a symlink or
    /// resolves outside the repository (R5-2): nothing under it is read or written.
    #[error("the .kaban directory or a record directory is a symlink or leaves the repository")]
    Escapes,
    /// `--ontology` named an ontology that is not loaded (fixed class, exit 2; the name is
    /// caller data and is not echoed).
    #[error("ontology_unknown: --ontology names an ontology that is not loaded")]
    OntologyUnknown,
    /// A current `.kaban/<ontology>/INDEX.md` or `.kaban/.gitignore` is larger than
    /// [`MAX_GENERATED_BYTES`] (R5-10): it was not read and not replaced.
    #[error(
        "a generated file under .kaban is larger than the replacement cap; it was left as it is"
    )]
    GeneratedTooLarge,
    /// Another kaban held the repository lock for the whole wait (R5-4): `resolve` did
    /// nothing; run it again.
    #[error("another kaban holds the repository; nothing was changed, run the command again")]
    Busy,
    /// A file or directory under `.kaban/` could not be read, created, written or renamed.
    #[error("{what} failed ({kind:?})")]
    Io {
        /// Which step (`creating the record directory`, `writing the record file`, ...).
        what: &'static str,
        /// The I/O error class.
        kind: std::io::ErrorKind,
    },
    /// `git add` (or another required git call) failed.
    #[error("git failed: {0}")]
    Git(#[from] crate::git::GitError),
    /// The record named by `resolve` has no conflict row (or does not exist here).
    #[error("node not found: {0}")]
    NotFound(String),
    /// `resolve --take file` found no file for the record.
    #[error("the record has no file in this repository")]
    NoFile,
    /// `resolve --take file` found a file this kaban cannot read as a record.
    #[error("the record file could not be read as a record: {0}")]
    BadFile(#[from] crate::serialize::SerializeError),
    /// The service refused a string or a request (a denylist hit, a hidden card).
    #[error(transparent)]
    Api(ApiError),
    /// The store refused the write (an invalid transition under `resolve --take file`, ...).
    #[error(transparent)]
    Store(StoreError),
}

impl From<ApiError> for SyncError {
    fn from(e: ApiError) -> Self {
        match e {
            ApiError::NotFound(id) => Self::NotFound(id),
            ApiError::Store(s) => Self::Store(s),
            other => Self::Api(other),
        }
    }
}

impl From<StoreError> for SyncError {
    fn from(e: StoreError) -> Self {
        match e {
            StoreError::NotFound(id) => Self::NotFound(id),
            other => Self::Store(other),
        }
    }
}

impl From<std::io::Error> for SyncError {
    fn from(e: std::io::Error) -> Self {
        Self::Io {
            what: "a file operation",
            kind: e.kind(),
        }
    }
}

/// The seam speaks `ApiError` (`seam::run_tool`): `NotFound` keeps exit 3, a store refusal
/// keeps its own code, every other failure class is a request error (4) with its fixed text.
impl From<SyncError> for ApiError {
    fn from(e: SyncError) -> Self {
        match e {
            SyncError::NotFound(id) => Self::NotFound(id),
            SyncError::Api(a) => a,
            SyncError::Store(s) => Self::Store(s),
            SyncError::OntologyUnknown => Self::Rejected {
                field: "ontology".into(),
                rules: vec!["ontology_unknown".into()],
            },
            other => Self::Invalid(other.to_string()),
        }
    }
}

/// What one import pass did (`kaban rebuild`, the import half of `sync`, the startup import).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ImportReport {
    /// Record and tombstone files read.
    pub files: u32,
    /// Records inserted or updated from a file (`file rev > db rev`, or a fresh id).
    pub imported: u32,
    /// Files whose rev and record hash equal the database's (no-op).
    pub unchanged: u32,
    /// Files whose rev is below the database's: the record is rewritten on the next export.
    pub stale_files: u32,
    /// Tombstone files read.
    pub tombstones: u32,
    /// Live records purged locally by a tombstone file (OWN-18).
    pub purged: u32,
    /// Why the `purged` records are not physically cleaned (SS-03), one entry per distinct
    /// detail in the order first seen: the free-page residue a VACUUM removes, after `busy`
    /// or the failure findings when a post-commit cleanup step did not run through. The
    /// revocations stand; `kaban doctor --optimize --vacuum` completes the cleanup. Empty
    /// when no live record was purged.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cleanup: Vec<String>,
    /// Ids whose file and database disagree at the same rev, or whose file asks for a state
    /// move the spec forbids (M5-31); each has a `conflicts` row.
    #[serde(default)]
    pub conflicts: Vec<String>,
    /// Files skipped because a string hit a reject rule (audited; nothing stored).
    pub rejected: u32,
    /// Files skipped because they could not be read as a record (shape, size, encoding, a
    /// kind or props the loaded ontology does not accept, a missing `ontology` key on a
    /// non-`adr` kind).
    pub skipped: u32,
    /// Files refused because their `ontology: <name>@<version>` names a version this process
    /// has not loaded (`ontology_version_unknown`, OWN-47); the file is untouched.
    #[serde(default)]
    pub version_unknown: u32,
    /// Acceptances a file claimed that the import could not prove (imported `proposed`).
    pub import_warnings: u32,
    /// Serialized records whose file is absent and whose deletion git history records.
    pub tombstoned_by_absence: u32,
    /// Serialized records whose file is absent without a recorded deletion: re-serialized.
    pub reserialized: u32,
    /// Another kaban held the repository lock for the whole wait (R5-4): this pass read and
    /// wrote nothing; the holder's run covers the tree.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub deferred: bool,
    /// The pass's receipt (spec 05 SS-10) at the log head once its files are applied; absent
    /// when the pass ran nothing (no `.kaban/` tree, or `deferred`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receipt: Option<WriteReceipt>,
}

/// Whether this run held the `export` lease for its default-sink bookkeeping (M5-19).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum LeaseState {
    /// The lease was acquired; the cursor was acknowledged and the `ChangeLog` pruned.
    #[default]
    Acquired,
    /// Another holder has it; files were still written, ack and prune were skipped.
    Held,
}

/// What one `sync` run did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SyncReport {
    /// Record files created.
    pub written: u32,
    /// Record files replaced.
    pub rewritten: u32,
    /// Tombstone files written for purged records.
    pub tombstones: u32,
    /// Record files deleted (tombstoned records, M5-10).
    pub deleted: u32,
    /// Files not written because the rendered text hit the denylist (M5-3; a redact hit too:
    /// run `kaban rescan`).
    pub withheld: u32,
    /// Whether any selected `.kaban/<ontology>/INDEX.md` changed.
    pub digest_updated: bool,
    /// Whether `git add -A -- .kaban` ran.
    pub staged: bool,
    /// The default sink's bookkeeping lease.
    pub lease: LeaseState,
    /// The `ChangeLog` position acknowledged for the `export` sink, when the lease was held
    /// (the `kaban sync` JSON keeps the pre-E2.0 field name; E2.11 decides the wire rename).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outbox_acked: Option<u64>,
    /// `ChangeLog` rows pruned (same naming note as `outbox_acked`).
    pub outbox_pruned: u64,
    /// The import half.
    pub import: ImportReport,
    /// The one-time layout move, when this run found a pre-engine tree (engine-plan E1.6).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layout: Option<MoveReport>,
    /// Another kaban held the repository lock for the whole wait (R5-4): this run read and
    /// wrote nothing (no import, no files, no staging); the holder's run covers the tree.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub deferred: bool,
}

/// What `kaban resolve` did.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolveReport {
    /// The record.
    pub id: String,
    /// Which side won.
    pub take: Take,
    /// The record's rev afterwards (above both sides).
    pub rev: i64,
    /// The record as it now reads (projected, S4).
    pub node: Card,
}

/// Move a pre-engine tree once, import the files another clone advanced, write the stale
/// records, the selected digests and the `.gitignore`, stage when asked, then run the default
/// sink's bookkeeping.
///
/// # Errors
/// [`SyncError`]: an unknown `--ontology` name, an unreadable or unwritable `.kaban/` tree,
/// a failed `git add`, a store or service failure. Per-file problems are counted, not
/// returned.
pub fn sync(api: &KabanApi, repo: &Path, opts: &SyncOpts) -> Result<SyncReport, SyncError> {
    let digests = digest_blocks(api, &opts.ontologies)?;
    let kinds = Kinds::from_registry(api);
    let tree = KabanTree::open_or_create(repo)?;
    // import and projection are one critical section per repository (R5-4): a run that
    // cannot get the lock defers everything to the holder, never a partial write
    let Some(_lock) = RepoLock::acquire(&tree.kaban)? else {
        tracing::warn!("another kaban holds the repository; sync deferred");
        return Ok(SyncReport {
            deferred: true,
            ..SyncReport::default()
        });
    };
    let mut report = SyncReport::default();
    // the one-time move, under the same lock and before the import (E1.6): git records the
    // renames inside a repository; a target with other bytes is a conflict row for a human
    let git = repo.join(".git").exists().then(|| GitRunner {
        timeout: STAGE_TIMEOUT,
        ..api.git.clone()
    });
    let moved = layout_move(repo, api.registry(), git.as_ref())?;
    for conflict in &moved.conflicts {
        api.store.conflict_upsert(&kaban_core::ConflictRow {
            node_id: conflict.id.clone(),
            file_rev: 0,
            db_rev: 0,
            file_hash: conflict.source_hash.clone(),
            db_hash: conflict.target_hash.clone(),
            detected_at: now_ts(),
        })?;
    }
    if !moved.already {
        report.layout = Some(moved);
    }
    if !opts.digest_only {
        let pass = import_tree(api, &tree, repo, "sync", &kinds)?;
        report.import = pass.report;
        export_dirty(api, &tree, &kinds, &pass.withheld, &mut report)?;
    }
    report.digest_updated = write_digests(api, &tree, &digests)?;
    write_if_changed(
        &tree.kaban.join(".gitignore"),
        GITIGNORE.as_bytes(),
        "writing .gitignore",
    )?;
    if opts.stage {
        let git = GitRunner {
            timeout: STAGE_TIMEOUT,
            ..api.git.clone()
        };
        // argv hygiene: a fixed pathspec after `--`, nothing from a file reaches argv
        git.run(repo, &["add", "-A", "--", KABAN_DIR])?;
        report.staged = true;
    }
    if !opts.digest_only {
        default_sink_bookkeeping(api, &mut report)?;
    }
    Ok(report)
}

/// The digest blocks of the selected ontologies, grouped per ontology in shipped-file order
/// (an ontology without a `digest: true` kind gets no `INDEX.md`); `OntologyUnknown` when
/// the selection names an unloaded ontology.
fn digest_blocks(
    api: &KabanApi,
    selection: &OntologySelection,
) -> Result<Vec<(String, Vec<crate::ontology::BlockSpec>)>, SyncError> {
    let blocks = selected_blocks(api, selection, |b| b.digest).map_err(|e| match e {
        crate::export::ExportError::OntologyUnknown => SyncError::OntologyUnknown,
        crate::export::ExportError::Store(s) => SyncError::Store(s),
        crate::export::ExportError::Api(a) => SyncError::Api(a),
        other => SyncError::Api(ApiError::Invalid(other.to_string())),
    })?;
    let mut grouped: Vec<(String, Vec<crate::ontology::BlockSpec>)> = Vec::new();
    for block in blocks {
        match grouped.iter_mut().find(|(ns, _)| *ns == block.ontology) {
            Some((_, list)) => list.push(block),
            None => grouped.push((block.ontology.clone(), vec![block])),
        }
    }
    Ok(grouped)
}

/// The serialized kinds of the loaded ontologies (engine-plan E1.6): stored name, ontology,
/// version and directory, read once per run from the registry.
struct Kinds(Vec<(String, SerializeDir)>);

impl Kinds {
    fn from_registry(api: &KabanApi) -> Self {
        Self(api.registry().serialized_kinds())
    }

    /// The stored names, for the store's bound `IN (?, ...)` list.
    fn names(&self) -> Vec<&str> {
        self.0.iter().map(|(k, _)| k.as_str()).collect()
    }

    /// Where a kind serializes, when it gets a file.
    fn target(&self, kind: &str) -> Option<&SerializeDir> {
        self.0.iter().find(|(k, _)| k == kind).map(|(_, t)| t)
    }

    /// Every `(kind, target)`.
    fn iter(&self) -> impl Iterator<Item = (&str, &SerializeDir)> {
        self.0.iter().map(|(k, t)| (k.as_str(), t))
    }
}

/// The repository-relative path of a record file (`.kaban/<ontology>/<dir>/<id>.md`).
fn rel_path(target: &SerializeDir, id: &str) -> String {
    format!("{KABAN_DIR}/{}/{}/{id}.md", target.ontology, target.dir)
}

/// The `ontology: <name>@<version>` stamp of a serialized kind.
fn stamp_of(target: &SerializeDir) -> OntologyRef {
    OntologyRef {
        name: target.ontology.clone(),
        version: target.version,
    }
}

/// Write every stale record of the bound workspace (M5-18 dirty set): the tombstone for a
/// purged record (M5-7), a deletion for a tombstoned one (M5-10), the record file otherwise,
/// each gated before a byte is written (M5-3) and stamped `serialized_rev` afterwards.
fn export_dirty(
    api: &KabanApi,
    tree: &KabanTree,
    kinds: &Kinds,
    withheld: &WithheldFiles,
    report: &mut SyncReport,
) -> Result<(), SyncError> {
    let ws = api.config.workspace.as_str();
    let dirty = api
        .store
        .dirty_serializable(ws, &kinds.names(), EXPORT_CAP)?;
    // a record in unresolved conflict keeps its file as the human left it (F13, R5-5): the
    // dirty database side waits for `resolve`
    let ids: Vec<NodeId> = dirty
        .iter()
        .filter_map(|n| NodeId::parse(&n.id).ok())
        .collect();
    let in_conflict: Vec<String> = api
        .store
        .conflicts_for(&ids)?
        .into_iter()
        .map(|row| row.node_id)
        .collect();
    for node in &dirty {
        let Some(target) = kinds.target(&node.kind) else {
            continue; // the store's filter already excludes these; defensive
        };
        let Ok(id) = NodeId::parse(&node.id) else {
            tracing::warn!("a stored record id is not a UUIDv7; not serialized");
            continue;
        };
        if in_conflict.contains(&node.id) {
            continue;
        }
        // a file this run refused for its ontology version is not rewritten either (OWN-47;
        // E1.7a RE2-4): the row stays dirty until a kaban that reads the file decides
        if withheld.contains(&id) {
            tracing::warn!(
                "a dirty record's file names an unloaded ontology version; not rewritten"
            );
            continue;
        }
        let path = tree.dir(target)?.join(format!("{}.md", id.as_str()));
        match node.lifecycle {
            Lifecycle::Purged => {
                // the tombstone carries the id and the rev only: nothing to redact, but the
                // gate runs like for every file so the rule has no exception
                let text = render_tombstone(id.as_str(), u64::try_from(node.rev).unwrap_or(0));
                if gate_file_text(api, &text)? {
                    write_atomic(&path, text.as_bytes())?;
                    report.tombstones += 1;
                } else {
                    report.withheld += 1;
                    continue;
                }
            }
            Lifecycle::Tombstoned => {
                if remove_if_present(&path)? {
                    report.deleted += 1;
                }
            }
            // `Lifecycle` is `non_exhaustive`: a state this kaban does not know gets the
            // record treatment (its file carries the word, the strict parse decides)
            _ => {
                let events = api.store.events(&id)?;
                let text = match render(node, &events, Some(&stamp_of(target))) {
                    Ok(text) => text,
                    Err(e) => {
                        tracing::warn!(error = %e, "a stored record has no file view; skipped");
                        report.withheld += 1;
                        continue;
                    }
                };
                if !gate_file_text(api, &text)? {
                    report.withheld += 1;
                    continue;
                }
                let existed = path.exists();
                write_atomic(&path, text.as_bytes())?;
                if existed {
                    report.rewritten += 1;
                } else {
                    report.written += 1;
                }
            }
        }
        api.store.mark_serialized(&id, node.rev)?;
    }
    Ok(())
}

/// The gate on the way out (M5-3, R5-7): `true` when `text` may be written as it is. The
/// rendered document (a record file, a tombstone, the digest) is judged in overlapping
/// windows, so it is never withheld for its length alone. A reject hit withholds the file
/// (audited by the gate); a redact hit withholds it too, with a fixed note, because masked
/// bytes at the database's rev would make the two sides disagree at equal rev (`kaban rescan`
/// re-gates the row with `rev + 1`; the next sync then writes it).
fn gate_file_text(api: &KabanApi, text: &str) -> Result<bool, SyncError> {
    match gate_document(api, "sync", "file", text)? {
        DocumentVerdict::Clean => Ok(true),
        DocumentVerdict::Redact => {
            tracing::warn!("a record file was withheld: a redact rule hit; run `kaban rescan`");
            Ok(false)
        }
        DocumentVerdict::Reject(_) => {
            tracing::warn!("a record file was withheld: a reject rule hit");
            Ok(false)
        }
    }
}

/// Render one `.kaban/<ontology>/INDEX.md` per selected ontology with digest kinds from the
/// accepted records and write each when it changed (M5-12, OWN-46); `true` when any changed.
fn write_digests(
    api: &KabanApi,
    tree: &KabanTree,
    digests: &[(String, Vec<crate::ontology::BlockSpec>)],
) -> Result<bool, SyncError> {
    if digests.is_empty() {
        return Ok(false);
    }
    let ws = api.config.workspace.as_str();
    let mut changed = false;
    for (ontology, blocks) in digests {
        let accepted = api.store.briefing(ws, &block_kinds(blocks), DIGEST_CAP)?;
        let text = render_digest(&accepted, blocks);
        if !gate_file_text(api, &text)? {
            tracing::warn!("a digest was withheld");
            continue;
        }
        let dir = tree.component(&[ontology.as_str()])?;
        changed |= write_if_changed(&dir.join(INDEX_FILE), text.as_bytes(), "writing the digest")?;
    }
    Ok(changed)
}

/// The default sink (EN-E10b; engine-plan E2.0): the in-binary projections are applied by the
/// store's own triggers as every row is written, so consuming a page is acknowledging its
/// position. The fence is the runner's ([`run_sink`]), not the sink's.
pub struct DefaultSink<'a> {
    /// The service the sink runs in (held for the projections a later stream may add here).
    #[allow(dead_code)]
    // the E2 default sink has no per-row work yet (projections are triggers)
    api: &'a KabanApi,
}

impl<'a> DefaultSink<'a> {
    /// The default sink of `api`.
    pub fn new(api: &'a KabanApi) -> Self {
        Self { api }
    }
}

impl Sink for DefaultSink<'_> {
    fn id(&self) -> SinkId {
        SinkId::EXPORT
    }
    fn consume(&mut self, changes: &[Change]) -> kaban_core::Result<Ack> {
        // the runner never hands an empty page; the last row is the position applied
        let upto = changes.last().map_or(0, |c| c.seq);
        Ok(Ack { upto })
    }
}

/// Default-sink bookkeeping (M5-19): under the `export` lease, run the default sink over every
/// row after its cursor and prune; correctness never depends on it (the rev columns decide
/// every write), so a held lease only skips the ack and the prune. The lease is released on
/// every way out except a lost one (R5-11), so the next sync acquires at once instead of
/// waiting out the ttl.
fn default_sink_bookkeeping(api: &KabanApi, report: &mut SyncReport) -> Result<(), SyncError> {
    let owner = format!("sync:{}:{}", std::process::id(), kaban_core::new_id());
    let Some(lease) = api
        .store
        .lease_acquire(EXPORT_CONSUMER, &owner, LEASE_TTL_S)?
    else {
        report.lease = LeaseState::Held;
        return Ok(());
    };
    report.lease = LeaseState::Acquired;
    let outcome = sink_under_lease(api, report, &lease);
    // a lost lease belongs to its successor: releasing it is not ours; every other outcome
    // gives the lease back, and a failed release never masks the bookkeeping's own result
    if report.lease == LeaseState::Acquired
        && let Err(e) = api.store.lease_release(&lease)
    {
        tracing::debug!(error = %e, "the export lease was not released");
    }
    outcome
}

/// The default sink's pages and the prune while the `export` lease is held: the sink is
/// installed first (SS-06: its retention cursor is registered under the lease before any row
/// is consumed, so pruning keeps every later row from the first run on), then `run_sink` from
/// the cursor the previous run left (the log is read from 0; rows below the cursor are gone or
/// acknowledged again, which is harmless), one fenced ack per page, then the prune.
fn sink_under_lease(
    api: &KabanApi,
    report: &mut SyncReport,
    lease: &kaban_core::Lease,
) -> Result<(), SyncError> {
    match api.store.changes_register(EXPORT_CONSUMER, lease.fence) {
        Ok(_) => {}
        Err(StoreError::LeaseLost { .. }) => {
            // a successor took the lease before the sink was installed: its run installs
            report.lease = LeaseState::Held;
            return Ok(());
        }
        Err(e) => return Err(e.into()),
    }
    let mut sink = DefaultSink::new(api);
    let mut after = 0u64;
    loop {
        let page = run_sink(api.store.as_ref(), &mut sink, lease, after, CHANGES_PAGE)?;
        if page.lease_lost {
            // a successor took the lease between the read and the ack: its run acks
            report.lease = LeaseState::Held;
            return Ok(());
        }
        let Some(acked) = page.acked else {
            break; // an empty page: nothing after the cursor
        };
        after = acked;
        report.outbox_acked = Some(acked);
        if page.consumed < CHANGES_PAGE as u64 {
            break;
        }
    }
    report.outbox_pruned = api.store.changes_prune(CHANGES_TAIL)?;
    Ok(())
}

/// Write `bytes` to `path` atomically: a temp file beside it, mode 0644, then a rename.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), SyncError> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let dir = path.parent().ok_or(SyncError::Io {
        what: "locating the record directory",
        kind: std::io::ErrorKind::InvalidInput,
    })?;
    std::fs::create_dir_all(dir).map_err(|e| SyncError::Io {
        what: "creating the record directory",
        kind: e.kind(),
    })?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp: PathBuf = dir.join(format!(".{name}.tmp-{}", std::process::id()));
    let io = |what: &'static str| {
        move |e: std::io::Error| SyncError::Io {
            what,
            kind: e.kind(),
        }
    };
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(FILE_MODE)
        .open(&tmp)
        .map_err(io("creating the temp file"))?;
    let written = f
        .write_all(bytes)
        .and_then(|()| f.sync_all())
        .map_err(io("writing the temp file"));
    drop(f);
    if let Err(e) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    // an existing file created with another umask keeps 0644 after the rename (the temp
    // file's mode is what lands)
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        SyncError::Io {
            what: "renaming the temp file",
            kind: e.kind(),
        }
    })
}

/// [`write_atomic`] only when the current bytes differ; `true` when written. The current file
/// is sized from metadata before it is read (R5-10): a file above [`MAX_GENERATED_BYTES`] is
/// refused untouched, a symlink is never followed (the rename replaces the link, as before),
/// and a file whose size differs from the rendering is replaced without being read at all.
fn write_if_changed(path: &Path, bytes: &[u8], what: &'static str) -> Result<bool, SyncError> {
    let io = move |e: std::io::Error| SyncError::Io {
        what,
        kind: e.kind(),
    };
    let read_back = match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.len() > MAX_GENERATED_BYTES => return Err(SyncError::GeneratedTooLarge),
        Ok(meta) => meta.is_file() && meta.len() == u64::try_from(bytes.len()).unwrap_or(u64::MAX),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => return Err(io(e)),
    };
    if read_back {
        let file = std::fs::File::open(path).map_err(io)?;
        let mut current = Vec::with_capacity(bytes.len());
        std::io::Read::take(file, MAX_GENERATED_BYTES + 1)
            .read_to_end(&mut current)
            .map_err(io)?;
        if current == bytes {
            return Ok(false);
        }
    }
    write_atomic(path, bytes)?;
    Ok(true)
}

/// Delete `path` when it exists; `true` when a file was removed.
fn remove_if_present(path: &Path) -> Result<bool, SyncError> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(SyncError::Io {
            what: "deleting the record file",
            kind: e.kind(),
        }),
    }
}

/// The explicit full import (`kaban rebuild`): every file under `.kaban/`, then the absence
/// pass. `scope` is the running subcommand's seam scope (`sync`, `rebuild`).
///
/// # Errors
/// [`SyncError`]: an unreadable `.kaban/` tree or a store failure. Per-file problems are
/// counted, not returned; a repository without `.kaban/` imports nothing.
pub fn import(api: &KabanApi, repo: &Path, scope: &'static str) -> Result<ImportReport, SyncError> {
    let kinds = Kinds::from_registry(api);
    let Some(tree) = KabanTree::open(repo)? else {
        return Ok(ImportReport::default());
    };
    let Some(_lock) = RepoLock::acquire(&tree.kaban)? else {
        tracing::warn!("another kaban holds the repository; import deferred");
        return Ok(ImportReport {
            deferred: true,
            ..ImportReport::default()
        });
    };
    Ok(import_tree(api, &tree, repo, scope, &kinds)?.report)
}

/// The record ids whose file this import refused for its `ontology: <name>@<version>` (OWN-47):
/// carried to the export half of the same `sync` so the refused file is never rewritten
/// (E1.7a RE2-4). Set semantics over a sorted `Vec` (a few ids at most per run).
#[derive(Debug, Default)]
struct WithheldFiles(Vec<NodeId>);

impl WithheldFiles {
    fn insert(&mut self, id: &NodeId) {
        if let Err(at) = self.0.binary_search(id) {
            self.0.insert(at, id.clone());
        }
    }

    fn contains(&self, id: &NodeId) -> bool {
        self.0.binary_search(id).is_ok()
    }
}

/// One import pass: its report and the files it withheld from this run's export.
struct ImportPass {
    report: ImportReport,
    withheld: WithheldFiles,
}

/// The repository-local interprocess lock (R5-4): an exclusive `flock` on the `.kaban`
/// directory itself, opened without following a link, held for the import plus the
/// projection of one run and released when the guard drops (also when the process dies). No
/// lock file: nothing new to ignore or stage, and the lock cannot be a planted symlink.
struct RepoLock {
    _dir: std::fs::File,
}

impl RepoLock {
    /// The lock, or `None` when another holder kept it for the whole [`LOCK_WAIT`].
    fn acquire(kaban: &Path) -> Result<Option<Self>, SyncError> {
        use rustix::fs::{FlockOperation, Mode, OFlags};
        let io = |what: &'static str| {
            move |e: rustix::io::Errno| SyncError::Io {
                what,
                kind: std::io::Error::from(e).kind(),
            }
        };
        let fd = rustix::fs::open(
            kaban,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(io("opening the .kaban directory"))?;
        let dir = std::fs::File::from(fd);
        let deadline = std::time::Instant::now() + LOCK_WAIT;
        loop {
            match rustix::fs::flock(&dir, FlockOperation::NonBlockingLockExclusive) {
                Ok(()) => return Ok(Some(Self { _dir: dir })),
                Err(rustix::io::Errno::WOULDBLOCK | rustix::io::Errno::INTR) => {
                    if std::time::Instant::now() >= deadline {
                        return Ok(None);
                    }
                    std::thread::sleep(LOCK_POLL);
                }
                Err(e) => return Err(io("locking the .kaban directory")(e)),
            }
        }
    }
}

/// The verified `.kaban/` tree of a repository (R5-2): the repository root canonicalized
/// once, `.kaban` itself not a symlink, and every ontology and record directory checked the
/// same way (not a symlink, resolving below the root) before it is read, written or created.
pub(crate) struct KabanTree {
    /// The canonical repository root.
    pub(crate) root: PathBuf,
    /// `<root>/.kaban`, a real directory.
    pub(crate) kaban: PathBuf,
}

impl KabanTree {
    /// The tree when `.kaban/` exists; `None` when the repository has none (or something that
    /// is not a directory under that name).
    pub(crate) fn open(repo: &Path) -> Result<Option<Self>, SyncError> {
        let root = canonical_root(repo)?;
        let kaban = root.join(KABAN_DIR);
        match std::fs::symlink_metadata(&kaban) {
            Ok(m) if m.is_symlink() => Err(SyncError::Escapes),
            Ok(m) if m.is_dir() => Ok(Some(Self { root, kaban })),
            Ok(_) => Ok(None),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(SyncError::Io {
                what: "opening the .kaban directory",
                kind: e.kind(),
            }),
        }
    }

    /// The tree, creating `.kaban/` as a real directory when it is absent (`sync`). `mkdir`
    /// never follows a link: a symlink under that name is refused, not used.
    fn open_or_create(repo: &Path) -> Result<Self, SyncError> {
        if let Some(tree) = Self::open(repo)? {
            return Ok(tree);
        }
        let root = canonical_root(repo)?;
        let kaban = root.join(KABAN_DIR);
        match std::fs::create_dir(&kaban) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => {
                return Err(SyncError::Io {
                    what: "creating the .kaban directory",
                    kind: e.kind(),
                });
            }
        }
        Self::open(repo)?.ok_or(SyncError::Io {
            what: "opening the .kaban directory",
            kind: std::io::ErrorKind::NotADirectory,
        })
    }

    /// The record directory of a serialized kind (`.kaban/<ontology>/<dir>`), verified
    /// component by component.
    fn dir(&self, target: &SerializeDir) -> Result<PathBuf, SyncError> {
        self.component(&[target.ontology.as_str(), target.dir.as_str()])
    }

    /// The path `.kaban/<parts...>`, every component verified: one plain path component
    /// (never empty, `.`, `..`, absolute or containing a separator: E1.7a RE2-3, judged before
    /// the file system is asked, so an absent path cannot leave the tree either), not a
    /// symlink and, when it exists, resolving below the root (R5-2, applied to the ontology
    /// directories and the record directories alike). An absent component is created later by
    /// the write, under the verified parent.
    pub(crate) fn component(&self, parts: &[&str]) -> Result<PathBuf, SyncError> {
        if parts.is_empty() || !parts.iter().all(|part| is_plain_component(part)) {
            return Err(SyncError::Escapes);
        }
        let mut path = self.kaban.clone();
        for part in parts {
            path.push(part);
            match std::fs::symlink_metadata(&path) {
                Ok(m) if m.is_symlink() => return Err(SyncError::Escapes),
                Ok(_) => {
                    let real = std::fs::canonicalize(&path).map_err(|e| SyncError::Io {
                        what: "locating the record directory",
                        kind: e.kind(),
                    })?;
                    if !real.starts_with(&self.root) {
                        return Err(SyncError::Escapes);
                    }
                }
                // nothing below an absent component exists either: the rest is just a path
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return Ok(parts
                        .iter()
                        .fold(self.kaban.clone(), |p, part| p.join(part)));
                }
                Err(e) => {
                    return Err(SyncError::Io {
                        what: "locating the record directory",
                        kind: e.kind(),
                    });
                }
            }
        }
        Ok(path)
    }

    /// `path` relative to the repository root, `/`-separated (the argv form of a kaban path).
    pub(crate) fn rel(&self, path: &Path) -> String {
        path.strip_prefix(&self.root)
            .unwrap_or(path)
            .to_string_lossy()
            .into_owned()
    }
}

/// Is `part` exactly one normal path component (`Component::Normal`, nothing else)? `""`,
/// `.`, `..`, `/x`, `a/b` and `a\0` are not.
fn is_plain_component(part: &str) -> bool {
    let mut components = Path::new(part).components();
    matches!(components.next(), Some(std::path::Component::Normal(c)) if c == part)
        && components.next().is_none()
        && !part.contains('\0')
}

/// The canonical repository root (every containment check compares against it).
fn canonical_root(repo: &Path) -> Result<PathBuf, SyncError> {
    std::fs::canonicalize(repo).map_err(|e| SyncError::Io {
        what: "locating the repository",
        kind: e.kind(),
    })
}

/// [`import`] over a verified tree.
fn import_tree(
    api: &KabanApi,
    tree: &KabanTree,
    repo: &Path,
    scope: &'static str,
    kinds: &Kinds,
) -> Result<ImportPass, SyncError> {
    let mut report = ImportReport::default();
    let mut withheld = WithheldFiles::default();
    // the whole listing first, under one entry count and one byte budget (R5-3), so an
    // oversized tree is refused before a byte of it is read or a row written
    let mut budget = Budget::default();
    let mut listing = Vec::new();
    for (kind, target) in kinds.iter() {
        let files = record_files(&tree.dir(target)?, &mut report, &mut budget)?;
        listing.push((kind, target, files));
    }
    let evidence = Evidence::gather(api, scope, repo);
    let mut present: Vec<String> = Vec::new();
    for (kind, target, files) in listing {
        for (id, path) in files {
            report.files += 1;
            let rel = rel_path(target, id.as_str());
            let outcome = import_file(api, scope, &path, &rel, kind, target, &id, &evidence);
            if matches!(outcome, FileOutcome::VersionUnknown) {
                withheld.insert(&id);
            }
            apply_outcome(api, scope, &id, kind, outcome, &mut report)?;
            present.push(rel);
        }
    }
    absence_pass(api, scope, repo, kinds, &present, &mut report)?;
    // the pass's receipt (SS-10), at the one boundary `sync` and `import` share: the pass is
    // one transaction per file, so its position is the head once every file is applied
    // (under the repository lock no second pass runs; another writer of the database may
    // have moved the head past the pass's own rows, an upper bound of them)
    // the pass's transactions are closed: the barrier runs here, once per pass (SS-10)
    report.receipt = Some(api.await_searchable(api.committed_receipt(None)));
    Ok(ImportPass { report, withheld })
}

/// The absence rule (F13 as narrowed by OWN-23/M5-11): a serialized record whose file is
/// absent is tombstoned only when git history records the deletion of that path (one bounded
/// `git log --diff-filter=D` per import); otherwise it is marked dirty and re-serialized by
/// the next export. Outside a repository, or when git fails, nothing is tombstoned.
fn absence_pass(
    api: &KabanApi,
    scope: &'static str,
    repo: &Path,
    kinds: &Kinds,
    present: &[String],
    report: &mut ImportReport,
) -> Result<(), SyncError> {
    let ws = api.config.workspace.as_str();
    let serialized = api
        .store
        .serialized_records(ws, &kinds.names(), EXPORT_CAP)?;
    let absent: Vec<(&Node, String)> = serialized
        .iter()
        .filter_map(|n| {
            let rel = rel_path(kinds.target(&n.kind)?, &n.id);
            (!present.contains(&rel)).then_some((n, rel))
        })
        .collect();
    if absent.is_empty() {
        return Ok(());
    }
    let deleted_in_history = recorded_deletions(api, scope, repo);
    for (node, rel) in absent {
        let Ok(id) = NodeId::parse(&node.id) else {
            continue;
        };
        if deleted_in_history
            .as_ref()
            .is_some_and(|d| d.contains(&rel))
        {
            let mut ctx = api.write_ctx(None);
            let mut entry = api.entry_for(
                &ctx,
                kaban_core::AuditOp::Forget,
                scope,
                "node",
                id.as_str(),
                vec![],
            );
            entry.node_id = Some(id.to_string());
            ctx.audit.push(entry);
            api.store
                .forget(&id, kaban_core::ForgetMode::Tombstone, &ctx)?;
            api.export_audit(&ctx.audit);
            report.tombstoned_by_absence += 1;
        } else {
            // no rev is 0: the record is dirty until the next export writes its file again
            api.store.mark_serialized(&id, 0)?;
            report.reserialized += 1;
        }
    }
    Ok(())
}

/// Every `.kaban/` path a commit ever deleted, or `None` when that cannot be known (not a
/// repository, git absent, slow or failing, or a denylist hit in the listing, R5-8): the
/// `Uncheckable` class never tombstones.
fn recorded_deletions(api: &KabanApi, scope: &'static str, repo: &Path) -> Option<Vec<String>> {
    if !repo.join(".git").exists() {
        return None;
    }
    let git = GitRunner {
        timeout: STAGE_TIMEOUT,
        ..api.git.clone()
    };
    match git.run(
        repo,
        &[
            "log",
            "--diff-filter=D",
            "--name-only",
            "--format=",
            "--",
            KABAN_DIR,
        ],
    ) {
        Ok(out) if gate_git_output(api, scope, &out) => Some(
            out.lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(str::to_owned)
                .collect(),
        ),
        Ok(_) => None,
        Err(e) => {
            tracing::debug!(error = %e, "git log unavailable; absence tombstones nothing");
            None
        }
    }
}

/// The content gate on what git prints (S1, R5-8): paths and author identities come from the
/// repository and are host content like any file. `true` when `out` may be compared or
/// stored; on any hit the caller treats the evidence as unavailable (a reject hit is audited
/// under `scope`/`git`; an audit failure is treated as a hit too, fail-closed). Nothing of the
/// output is logged. The argv that produced it stays fixed and content-free.
fn gate_git_output(api: &KabanApi, scope: &'static str, out: &str) -> bool {
    match gate_document(api, scope, "git", out) {
        Ok(DocumentVerdict::Clean) => true,
        Ok(DocumentVerdict::Redact | DocumentVerdict::Reject(_)) => {
            tracing::warn!("git output hit the denylist; the evidence is treated as unavailable");
            false
        }
        Err(e) => {
            tracing::warn!(error = %e, "git output could not be gated; the evidence is treated as unavailable");
            false
        }
    }
}

/// What one import has walked so far (R5-3): every directory entry, and the bytes of the
/// files it would read.
#[derive(Default)]
struct Budget {
    entries: usize,
    bytes: u64,
}

impl Budget {
    /// Count one entry; `ImportBudget` past the cap.
    fn entry(&mut self) -> Result<(), SyncError> {
        self.entries += 1;
        if self.entries > MAX_IMPORT_ENTRIES {
            tracing::warn!(cap = MAX_IMPORT_ENTRIES, "import entry budget exceeded");
            return Err(SyncError::ImportBudget);
        }
        Ok(())
    }

    /// Count the bytes of a file the import would read; `ImportBudget` past the cap.
    fn file(&mut self, len: u64) -> Result<(), SyncError> {
        self.bytes = self.bytes.saturating_add(len);
        if self.bytes > MAX_IMPORT_BYTES {
            tracing::warn!(cap = MAX_IMPORT_BYTES, "import byte budget exceeded");
            return Err(SyncError::ImportBudget);
        }
        Ok(())
    }
}

/// The `<id>.md` files of one record directory, sorted by name, each with its parsed id (a
/// file whose stem is not a node id is counted `skipped`; nothing but a plain `.md` file is
/// read). A missing directory holds no files. Every entry and every readable file's size
/// count against `budget`.
fn record_files(
    dir: &Path,
    report: &mut ImportReport,
    budget: &mut Budget,
) -> Result<Vec<(NodeId, PathBuf)>, SyncError> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(SyncError::Io {
                what: "reading the record directory",
                kind: e.kind(),
            });
        }
    };
    let mut files = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| SyncError::Io {
            what: "reading the record directory",
            kind: e.kind(),
        })?;
        budget.entry()?;
        let path = entry.path();
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            report.skipped += 1;
            continue;
        };
        if name.starts_with('.') {
            continue; // a temp file of a concurrent writer, never a record
        }
        let Some(stem) = name.strip_suffix(".md") else {
            report.skipped += 1;
            continue;
        };
        // `DirEntry::metadata` does not follow a symlink: a link is not a regular file here
        let Ok(meta) = entry.metadata() else {
            report.skipped += 1;
            continue;
        };
        if !meta.is_file() {
            report.skipped += 1;
            continue;
        }
        let Ok(id) = NodeId::parse(stem) else {
            report.skipped += 1;
            continue;
        };
        if meta.len() > MAX_FILE_BYTES {
            report.skipped += 1; // never read (`read_bounded` agrees); only the entry counts
            continue;
        }
        budget.file(meta.len())?;
        files.push((id, path));
    }
    files.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
    Ok(files)
}

/// What the working tree can prove about committed bytes (M5-23), gathered once per import:
/// the paths `git status --porcelain -z --untracked-files=all -- .kaban` lists (modified,
/// staged or untracked), HEAD, and the author identity when it is recorded. `None` when the
/// evidence class is unavailable (not a repository, no HEAD, git failed, or a denylist hit in
/// the listing, R5-8): every acceptance then imports as `proposed`. An author that hits the
/// denylist is omitted (the event keeps `human-pr` and the commit), never stored.
struct Evidence {
    listed: Option<Vec<String>>,
    head: Option<String>,
    author: Option<String>,
}

impl Evidence {
    fn gather(api: &KabanApi, scope: &'static str, repo: &Path) -> Self {
        if !repo.join(".git").exists() {
            return Self::none();
        }
        // the import is not latency-bound like `why`: the bounded runner gets the `git add`
        // deadline, so a slow `status` on a large tree does not demote committed acceptances
        let git = GitRunner {
            timeout: STAGE_TIMEOUT,
            ..api.git.clone()
        };
        let Some(head) = git.head(repo) else {
            return Self::none();
        };
        let listed = match git.run(
            repo,
            &[
                "status",
                "--porcelain",
                "-z",
                "--untracked-files=all",
                "--",
                KABAN_DIR,
            ],
        ) {
            // the listing is gated before any path is compared (R5-8); the NUL separators
            // are replaced so the scan sees one line per entry
            Ok(out) if gate_git_output(api, scope, &out.replace('\0', "\n")) => {
                porcelain_paths(&out)
            }
            Ok(_) => return Self::none(),
            Err(e) => {
                tracing::debug!(error = %e, "git status unavailable; acceptances import as proposed");
                return Self::none();
            }
        };
        let author = if api.config.record_signer_identity {
            git.run(repo, &["log", "-1", "--format=%ae"])
                .ok()
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
                // gated before it can become `NewEvent.signer` (R5-8): a hit omits the signer
                .filter(|s| gate_git_output(api, scope, s))
        } else {
            None
        };
        Self {
            listed: Some(listed),
            head: Some(head),
            author,
        }
    }

    fn none() -> Self {
        Self {
            listed: None,
            head: None,
            author: None,
        }
    }

    /// The proof that the file at `rel` (repository-relative) is committed and unmodified:
    /// HEAD exists and `git status` lists nothing for it. `None` otherwise.
    fn proof(&self, rel: &str) -> Option<Proof> {
        match (&self.listed, &self.head) {
            (Some(listed), Some(head)) if !listed.iter().any(|p| p == rel) => Some(Proof {
                head: head.clone(),
                author: self.author.clone(),
            }),
            _ => None,
        }
    }
}

/// What a `human-pr` acceptance event records (OD-15): the commit the bytes are in and the
/// author identity when `record_signer_identity` is on.
struct Proof {
    head: String,
    author: Option<String>,
}

/// The paths of a `--porcelain -z` listing: `XY <path>` entries, and for a rename the bare
/// old path that follows; both land in the set (over-inclusion only denies evidence).
fn porcelain_paths(out: &str) -> Vec<String> {
    out.split('\0')
        .filter(|chunk| !chunk.is_empty())
        .map(|chunk| match chunk.as_bytes() {
            [_, _, b' ', ..] => chunk[3..].to_owned(),
            _ => chunk.to_owned(),
        })
        .collect()
}

/// What one file asked for, after the gate and the typed parse.
enum FileOutcome {
    /// A record file: the gated record, whether a redaction changed a string, and whether the
    /// file's acceptance claim is backed by committed bytes.
    Record {
        record: Box<serialize::SerializedRecord>,
        redacted: bool,
        proof: Option<Proof>,
    },
    /// A tombstone file at `rev`.
    Tombstone { rev: u64 },
    /// A reject-class hit: skipped and audited.
    Rejected,
    /// Unreadable or malformed: skipped.
    Skipped,
    /// The file names an ontology version this process has not loaded (OWN-47): refused,
    /// the file untouched.
    VersionUnknown,
}

/// Read, gate and parse one file (M5-2): the split file becomes one JSON value for the
/// pre-parse scan under `scope`; then every string is gated (redactions apply), then the
/// strict typed parse runs on the gated values. Nothing from the file is logged or echoed.
#[allow(clippy::too_many_arguments)] // one call site; the file's identity is these parts
fn import_file(
    api: &KabanApi,
    scope: &'static str,
    path: &Path,
    rel: &str,
    dir_kind: &str,
    target: &SerializeDir,
    id: &NodeId,
    evidence: &Evidence,
) -> FileOutcome {
    let Some(text) = read_bounded(path) else {
        return FileOutcome::Skipped;
    };
    // the byte-complete gate (S1, R5-1): the raw text, comments and all, is judged before the
    // YAML parser can drop anything; scanned in overlapping windows so a long but clean file
    // is not a false refusal. A reject hit refuses the file and is audited under `scope`.
    let mut raw_ctx = api.write_ctx(None);
    if let Err(e) = api.gate_joined(&mut raw_ctx, scope, "file", &text) {
        tracing::warn!(error = %e, "a record file hit a reject rule before parse; skipped");
        return FileOutcome::Rejected;
    }
    let raw = match serialize::split(&text) {
        Ok(raw) => raw,
        Err(e) => {
            tracing::debug!(error = %e, "a record file could not be split; skipped");
            return FileOutcome::Skipped;
        }
    };
    let value = serde_json::json!({ "frontmatter": raw.frontmatter, "body": raw.body });
    if let Err(e) = api.prepare_args(scope, &value) {
        tracing::warn!(error = %e, "a record file hit a reject rule; skipped");
        return FileOutcome::Rejected;
    }
    let mut ctx = api.write_ctx(None);
    let mut red = Vec::new();
    let gated = api
        .gate_json(&mut ctx, scope, "frontmatter", &raw.frontmatter, &mut red)
        .and_then(|frontmatter| {
            let body = api.gate(&mut ctx, scope, "body", &raw.body, &mut red)?;
            api.flush_audit(&mut ctx)?;
            Ok(serialize::RawFile { frontmatter, body })
        });
    let gated = match gated {
        Ok(gated) => gated,
        Err(e) => {
            tracing::warn!(error = %e, "a record file was refused by the gate; skipped");
            return FileOutcome::Rejected;
        }
    };
    let parsed = match serialize::parse_frontmatter(gated) {
        Ok(parsed) => parsed,
        Err(e) => {
            tracing::debug!(error = %e, "a record file is not a record; skipped");
            return FileOutcome::Skipped;
        }
    };
    match parsed {
        serialize::Parsed::Tombstone { id: file_id, rev } => {
            if &file_id != id {
                return FileOutcome::Skipped; // the file name is the id (F13)
            }
            FileOutcome::Tombstone { rev }
        }
        serialize::Parsed::Record(record) => {
            if &record.id != id || record.kind != dir_kind {
                return FileOutcome::Skipped;
            }
            match check_ontology(api, &record, target) {
                Ok(()) => {}
                Err(e @ serialize::SerializeError::OntologyVersionUnknown) => {
                    tracing::warn!(error = %e, class = "ontology_version_unknown", "a record file names an unloaded ontology version; refused");
                    return FileOutcome::VersionUnknown;
                }
                Err(e) => {
                    tracing::debug!(error = %e, "a record file does not fit its ontology; skipped");
                    return FileOutcome::Skipped;
                }
            }
            FileOutcome::Record {
                record,
                redacted: !red.is_empty(),
                proof: evidence.proof(rel),
            }
        }
    }
}

/// The ontology rule of an imported record (OWN-47; see the module doc): the file's
/// `ontology: <name>@<version>` (a pre-engine file without it reads as `adr@1`, accepted for
/// the shipped `adr` kinds only) must name the directory's ontology at a loaded version,
/// and the props must fit the loaded `props_schema` of the kind.
fn check_ontology(
    api: &KabanApi,
    record: &serialize::SerializedRecord,
    target: &SerializeDir,
) -> Result<(), serialize::SerializeError> {
    let stamp = match &record.ontology {
        Some(stamp) => stamp.clone(),
        None if target.ontology == OntologyRef::pre_engine().name => OntologyRef::pre_engine(),
        None => return Err(serialize::SerializeError::MissingKey("ontology")),
    };
    if stamp.name != target.ontology {
        return Err(serialize::SerializeError::BadValue("ontology"));
    }
    if !api.registry().has_version(&stamp.name, stamp.version) {
        return Err(serialize::SerializeError::OntologyVersionUnknown);
    }
    crate::ontology::validate_kind_props(api.registry(), &record.kind, &record.props)
        .map_err(|_| serialize::SerializeError::BadValue("props"))
}

/// The file's text, when it is a regular UTF-8 file within the cap.
fn read_bounded(path: &Path) -> Option<String> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_FILE_BYTES {
        return None;
    }
    std::fs::read_to_string(path).ok()
}

/// Apply the rev rules for one file against the stored record (F13, M5-2, M5-8, M5-23,
/// M5-31).
fn apply_outcome(
    api: &KabanApi,
    scope: &'static str,
    id: &NodeId,
    kind: &str,
    outcome: FileOutcome,
    report: &mut ImportReport,
) -> Result<(), SyncError> {
    let ws = api.config.workspace.as_str();
    let stored = api.store.get(id, true)?;
    // another tenant's row under this id is invisible (S6): the file is not ours to apply
    let stored = match stored {
        Some(n) if n.workspace_id != ws => {
            tracing::warn!("a record file names a record of another workspace; skipped");
            report.skipped += 1;
            return Ok(());
        }
        other => other,
    };
    match outcome {
        FileOutcome::Rejected => report.rejected += 1,
        FileOutcome::Skipped => report.skipped += 1,
        FileOutcome::VersionUnknown => report.version_unknown += 1,
        FileOutcome::Tombstone { rev } => {
            // a tombstone always wins, whatever the revs (OWN-23/M5-8): the live record is
            // purged through the store's purge path with its audit row, a purged one moves
            // up, a missing one becomes a ghost
            let live = stored.is_some_and(|n| n.lifecycle != Lifecycle::Purged);
            let mut ctx = api.write_ctx(None);
            if live {
                let mut entry = api.entry_for(
                    &ctx,
                    kaban_core::AuditOp::Purge,
                    scope,
                    "node",
                    id.as_str(),
                    vec![],
                );
                entry.node_id = Some(id.to_string());
                ctx.audit.push(entry);
            }
            let imported =
                api.store
                    .import_purged(id, kind, i64::try_from(rev).unwrap_or(i64::MAX), &ctx)?;
            api.export_audit(&ctx.audit);
            report.tombstones += 1;
            if live {
                report.purged += 1;
            }
            // the purge committed; its cleanup is the report's to carry, never a rejection
            if let Some(detail) = imported.cleanup
                && !report.cleanup.contains(&detail)
            {
                report.cleanup.push(detail);
            }
        }
        FileOutcome::Record {
            record,
            redacted,
            proof,
        } => {
            let file_rev = i64::try_from(record.rev).unwrap_or(i64::MAX);
            let Some(db) = stored else {
                import_record(api, &record, None, file_rev, redacted, proof, report)?;
                return Ok(());
            };
            if db.lifecycle == Lifecycle::Purged {
                // a purged record is never rewritten with content (OWN-23): the content file
                // (a stale branch, or the file the purge has not yet replaced) is replaced by
                // the tombstone on the export half; the record is marked dirty so that
                // happens even when the tombstone had been written once already
                if db.serialized_rev == Some(db.rev) {
                    let stamp = if file_rev == db.rev {
                        file_rev - 1
                    } else {
                        file_rev
                    };
                    api.store.mark_serialized(id, stamp)?;
                }
                report.stale_files += 1;
                return Ok(());
            }
            match file_rev.cmp(&db.rev) {
                std::cmp::Ordering::Greater => {
                    match import_record(api, &record, Some(&db), file_rev, redacted, proof, report)
                    {
                        Ok(()) => {}
                        // the file asks for a state move the spec forbids (M5-31): a conflict
                        // for a human, both sides untouched
                        Err(SyncError::Store(StoreError::InvalidTransition(e))) => {
                            tracing::warn!(error = %e, "a record file asks for an invalid transition; conflict recorded");
                            record_conflict(api, id, &record, &db, report)?;
                        }
                        Err(e) => return Err(e),
                    }
                }
                std::cmp::Ordering::Equal if redacted => {
                    // the policy correction comes before the hash rule (M5-2, R5-12): the
                    // masked record is stored at `db rev + 1`, dirty, and the next export
                    // rewrites the file with the mask; no conflict for a human to settle
                    match import_record(api, &record, Some(&db), file_rev, true, proof, report) {
                        Ok(()) => {}
                        Err(SyncError::Store(StoreError::InvalidTransition(e))) => {
                            tracing::warn!(error = %e, "a record file asks for an invalid transition; conflict recorded");
                            record_conflict(api, id, &record, &db, report)?;
                        }
                        Err(e) => return Err(e),
                    }
                }
                std::cmp::Ordering::Equal => {
                    let db_hash = db_record_hash(&db);
                    let file_hash = serialize::record_hash(&record);
                    if file_hash == db_hash {
                        report.unchanged += 1;
                    } else {
                        record_conflict(api, id, &record, &db, report)?;
                    }
                }
                std::cmp::Ordering::Less => {
                    // the database is ahead: the file is rewritten on the next export
                    api.store.mark_serialized(id, file_rev)?;
                    report.stale_files += 1;
                }
            }
        }
    }
    Ok(())
}

/// Which side `resolve` is applying, after the file side was read and gated.
enum Resolution {
    /// The stored record, re-stamped one rev above both sides.
    Db,
    /// The file's gated record, imported one rev above both sides.
    File {
        record: Box<serialize::SerializedRecord>,
        proof: Option<Proof>,
    },
    /// The file is a tombstone: the local purge.
    Tombstone,
}

/// The fixed refusal of a `resolve` whose file the output gate would withhold.
fn withheld_file() -> SyncError {
    SyncError::Api(ApiError::Rejected {
        field: "file".into(),
        rules: vec!["file-withheld".into()],
    })
}

/// The record hash of a stored node's file view (`rev`, events and timestamps take no part).
fn db_record_hash(db: &Node) -> String {
    serialize::SerializedRecord::from_node(db, &[])
        .map(|r| serialize::record_hash(&r))
        .unwrap_or_default()
}

/// Upsert the `conflicts` row of `id` (M5-5) and report it; the file stays untouched.
fn record_conflict(
    api: &KabanApi,
    id: &NodeId,
    file: &serialize::SerializedRecord,
    db: &Node,
    report: &mut ImportReport,
) -> Result<(), SyncError> {
    api.store.conflict_upsert(&kaban_core::ConflictRow {
        node_id: id.to_string(),
        file_rev: i64::try_from(file.rev).unwrap_or(i64::MAX),
        db_rev: db.rev,
        file_hash: serialize::record_hash(file),
        db_hash: db_record_hash(db),
        detected_at: now_ts(),
    })?;
    report.conflicts.push(id.to_string());
    Ok(())
}

/// Unix seconds now.
fn now_ts() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// Write a file's record into the store at `file_rev` (M5-18), or at `file_rev + 1` and
/// dirty when a redaction changed a string (M5-2) or an acceptance claim could not be proved
/// (M5-23), so the corrected record is re-serialized on the next export.
fn import_record(
    api: &KabanApi,
    record: &serialize::SerializedRecord,
    db: Option<&Node>,
    file_rev: i64,
    redacted: bool,
    proof: Option<Proof>,
    report: &mut ImportReport,
) -> Result<(), SyncError> {
    let mut governance = record.governance;
    let mut event = None;
    let mut correct = redacted;
    let moves_to =
        |to: kaban_core::Governance| db.is_none_or(|n| n.governance != to) && governance == to;
    let governed = api.governed();
    if moves_to(kaban_core::Governance::Accepted) {
        let last_accepted = record.events.iter().rev().find(|e| e.kind == "accepted");
        let reason = last_accepted.map_or_else(|| "import".to_owned(), |e| e.reason.clone());
        // the proof requirement follows the record's governance (core-spec F4 tiers through
        // the loaded set, EN-F4; E1.7a RE2-5): every `acceptance: human` kind needs the git
        // proof, an `acceptance: agent` kind follows the agent rule; a kind the set does not
        // govern cannot be serialized, so it fails closed on the human side
        let human_tier = governed.acceptance(&record.kind)
            != Some(kaban_core::ontology::meta::Acceptance::Agent);
        if human_tier {
            if let Some(proof) = proof {
                // the acceptance proof (OD-15): committed bytes, HEAD, the author when recorded
                event = Some(kaban_core::NewEvent {
                    reason,
                    path_scope: record.path_scope.clone(),
                    signer: proof.author,
                    signer_kind: kaban_core::SignerKind::HumanPr,
                    host: None,
                    session_id: None,
                    commit_sha: Some(proof.head),
                });
            } else {
                governance = kaban_core::Governance::Proposed;
                report.import_warnings += 1;
                correct = true;
                tracing::warn!(
                    "a record file claims an acceptance the working tree cannot prove; imported as proposed"
                );
            }
        } else {
            // the agent tier (F4; the shipped Convention and Failure): no commit check
            event = Some(kaban_core::NewEvent {
                reason,
                path_scope: record.path_scope.clone(),
                signer_kind: kaban_core::SignerKind::Agent,
                ..Default::default()
            });
        }
    }
    let rev = if correct {
        file_rev.saturating_add(1)
    } else {
        file_rev
    };
    let imported = to_imported(record, &governed, governance, rev, event);
    let ctx = api.write_ctx(None).with_repo(api.config.repo.clone());
    api.store.import_record(&imported, &ctx)?;
    if correct {
        // the file on disk is at `file_rev`; the store is one above: dirty until rewritten
        api.store.mark_serialized(&record.id, file_rev)?;
    }
    report.imported += 1;
    Ok(())
}

/// The store's view of a gated record (M5-18): canonical scope, the near-duplicate key
/// recomputed (F1) for a kind whose mixin says `dedup: title+scope` in `governed` (the
/// shipped Decision, a loaded `Runbook`; E1.7a RE2-7) through the function `remember` uses,
/// provenance as the file carries it.
fn to_imported(
    record: &serialize::SerializedRecord,
    governed: &kaban_core::GovernedSet,
    governance: kaban_core::Governance,
    rev: i64,
    event: Option<kaban_core::NewEvent>,
) -> kaban_core::ImportedRecord {
    let path_scope = crate::currency::canonical_path_scope(&record.path_scope);
    let dedups = governed
        .governed(&record.kind)
        .is_some_and(|g| g.dedup == kaban_core::ontology::meta::Dedup::TitleScope);
    let dup_key = dedups
        .then(|| crate::coderef::dup_key(record.summary.as_deref(), &record.text, &path_scope));
    kaban_core::ImportedRecord {
        id: record.id.clone(),
        kind: record.kind.clone(),
        text: record.text.clone(),
        summary: record.summary.clone(),
        tags: record.tags.clone(),
        confidence: record.confidence,
        importance: record.importance,
        lifecycle: record.lifecycle,
        governance,
        path_scope,
        props: record.props.clone(),
        rev,
        created_at: record.created_at.clone(),
        updated_at: record.updated_at.clone(),
        valid_from: record.valid_from.clone(),
        valid_to: record.valid_to.clone(),
        accepted_commit: record.accepted_commit.clone(),
        agent_id: record.provenance.agent_id.clone(),
        session_id: record.provenance.session_id.clone(),
        repo: record.provenance.repo.clone(),
        commit_sha: record.provenance.commit_sha.clone(),
        dup_key,
        event,
    }
}

/// The `serve` startup import (M5-13): [`import`] in the process repository when it has a
/// `.kaban/` directory, `None` otherwise (no repository, no directory).
///
/// # Errors
/// As [`import`].
pub fn startup_import(api: &KabanApi) -> Result<Option<ImportReport>, SyncError> {
    let Some(root) = api.config.repo_root.as_deref() else {
        return Ok(None);
    };
    if !root.join(KABAN_DIR).is_dir() {
        return Ok(None);
    }
    import(api, root, "sync").map(Some)
}

/// Resolve an equal-rev conflict (M5-32): `Db` bumps the record to `rev + 1` and rewrites the
/// file; `File` imports the file at `max(file rev, db rev) + 1` and re-serializes it. Both
/// clear the conflict row and return the projected record. The prospective file is gated
/// before the database moves, and the row is cleared only after the file is written and
/// stamped (R5-6); a write failure leaves the record dirty at its new rev with the row kept.
///
/// # Errors
/// [`SyncError::NotFound`] when `id` has no conflict row, [`SyncError::NoFile`] or
/// [`SyncError::BadFile`] for `File`, a store `InvalidTransition` when the file asks for a
/// forbidden governance move, [`SyncError`] otherwise.
pub fn resolve(
    api: &KabanApi,
    repo: &Path,
    id: &NodeId,
    take: Take,
) -> Result<ResolveReport, SyncError> {
    let ws = api.config.workspace.as_str();
    let row = api
        .store
        .conflicts_for(std::slice::from_ref(id))?
        .into_iter()
        .next()
        .ok_or_else(|| SyncError::NotFound(id.to_string()))?;
    let db = api
        .store
        .get(id, true)?
        .filter(|n| n.workspace_id == ws && n.lifecycle != Lifecycle::Purged)
        .ok_or_else(|| SyncError::NotFound(id.to_string()))?;
    let kinds = Kinds::from_registry(api);
    let target = kinds
        .target(&db.kind)
        .ok_or_else(|| SyncError::NotFound(id.to_string()))?;
    let tree = KabanTree::open(repo)?.ok_or(SyncError::NoKabanDir)?;
    let _lock = RepoLock::acquire(&tree.kaban)?.ok_or(SyncError::Busy)?;
    let path = tree.dir(target)?.join(format!("{}.md", id.as_str()));
    let rev = row.file_rev.max(db.rev).saturating_add(1);
    let ctx = api.write_ctx(None).with_repo(api.config.repo.clone());
    // 1. what the resolution would write, judged before the database moves (R5-6): a reject
    //    or redact hit refuses the resolution with both sides and the row as they were
    let side = match take {
        Take::Db => Resolution::Db,
        Take::File => {
            // the file, gated like an import (M5-2); the store's transition tables still
            // decide (M5-31)
            let evidence = Evidence::gather(api, "resolve", repo);
            let rel = rel_path(target, id.as_str());
            if !path.is_file() {
                return Err(SyncError::NoFile);
            }
            match import_file(api, "resolve", &path, &rel, &db.kind, target, id, &evidence) {
                FileOutcome::Record { record, proof, .. } => Resolution::File { record, proof },
                FileOutcome::Tombstone { .. } => Resolution::Tombstone,
                FileOutcome::Rejected => {
                    return Err(SyncError::Api(ApiError::Rejected {
                        field: "file".into(),
                        rules: vec!["file-rejected".into()],
                    }));
                }
                FileOutcome::Skipped => return Err(SyncError::NoFile),
                FileOutcome::VersionUnknown => {
                    return Err(SyncError::BadFile(
                        serialize::SerializeError::OntologyVersionUnknown,
                    ));
                }
            }
        }
    };
    let stamp = stamp_of(target);
    let prospective = match &side {
        Resolution::Db => {
            let mut own = serialize::SerializedRecord::from_node(&db, &api.store.events(id)?)?
                .with_ontology(stamp.clone());
            own.rev = u64::try_from(rev).unwrap_or(u64::MAX);
            serialize::render_record(&own)
        }
        Resolution::File { record, .. } => {
            let mut own = (**record).clone().with_ontology(stamp.clone());
            own.rev = u64::try_from(rev).unwrap_or(u64::MAX);
            serialize::render_record(&own)
        }
        Resolution::Tombstone => render_tombstone(id.as_str(), u64::try_from(rev).unwrap_or(0)),
    };
    if !gate_file_text(api, &prospective)? {
        return Err(withheld_file());
    }
    // 2. the database: one rev above both sides (every write bumps `rev`, F13)
    let node = match side {
        Resolution::Db => {
            let own = serialize::SerializedRecord::from_node(&db, &[])?;
            let imported = kaban_core::ImportedRecord {
                dup_key: db.dup_key.clone(),
                ..to_imported(&own, &api.governed(), db.governance, rev, None)
            };
            api.store.import_record(&imported, &ctx)?
        }
        Resolution::File { record, proof } => {
            // counts are not reported by `resolve`; the warning path still demotes an
            // unprovable acceptance claim (OD-15)
            let mut counts = ImportReport::default();
            import_record(api, &record, Some(&db), rev, false, proof, &mut counts)?;
            api.store
                .get(id, true)?
                .ok_or_else(|| SyncError::NotFound(id.to_string()))?
        }
        Resolution::Tombstone => {
            // a tombstone always wins anyway (OWN-23); taking it is the local purge
            let mut ctx = api.write_ctx(None);
            let mut entry = api.entry_for(
                &ctx,
                kaban_core::AuditOp::Purge,
                "resolve",
                "node",
                id.as_str(),
                vec![],
            );
            entry.node_id = Some(id.to_string());
            ctx.audit.push(entry);
            let imported = api.store.import_purged(id, &db.kind, rev, &ctx)?;
            if let Some(detail) = imported.cleanup {
                // `resolve` reports the record, not a pass: the cleanup detail is traced
                // (content-free: busy, findings, the residue), the revocation stands
                tracing::info!(detail = %detail, "the resolved tombstone's purge is not physically cleaned");
            }
            api.export_audit(&ctx.audit);
            api.store
                .get(id, true)?
                .ok_or_else(|| SyncError::NotFound(id.to_string()))?
        }
    };
    // 3. the file at the new rev, then the stamp, then the row (R5-6): a failure here leaves
    //    the record dirty at its new rev with the row in place, so a later `resolve` (or, for
    //    a tombstone, the next sync) finishes the job; nothing is reported as settled
    let written = render(&node, &api.store.events(id)?, Some(&stamp))
        .map_err(SyncError::from)
        .and_then(|text| {
            if gate_file_text(api, &text)? {
                write_atomic(&path, text.as_bytes())
            } else {
                Err(withheld_file())
            }
        });
    if let Err(e) = written {
        tracing::warn!(
            "resolve could not write the record file; the conflict stays and the record is dirty"
        );
        api.store
            .mark_serialized(id, db.serialized_rev.unwrap_or(0))?;
        return Err(e);
    }
    api.store.mark_serialized(id, node.rev)?;
    api.store.conflict_clear(id)?;
    let card = api.project(&node, true, None).ok_or_else(|| {
        // the write happened; the card is hidden by a reject rule like every read of it
        SyncError::Api(ApiError::Rejected {
            field: "node".into(),
            rules: vec!["read-hidden".into()],
        })
    })?;
    Ok(ResolveReport {
        id: id.to_string(),
        take,
        rev: node.rev,
        node: card,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion
mod tests {
    use super::*;

    /// E1.7a RE2-3: the tree helper refuses anything but plain single components, before the
    /// file system is consulted, so a missing path cannot resolve outside `.kaban/` either.
    #[test]
    fn the_tree_helper_refuses_multi_component_and_absolute_parts_even_when_absent() {
        let scratch = std::env::temp_dir().join(format!("kaban-sync-tree-{}", std::process::id()));
        let repo = scratch.join("repo");
        std::fs::create_dir_all(repo.join(KABAN_DIR)).unwrap();
        let tree = KabanTree::open(&repo).unwrap().expect(".kaban exists");
        let outside = scratch.join("outside");
        for parts in [
            vec![outside.to_str().unwrap()],
            vec!["adr", outside.to_str().unwrap()],
            vec!["../outside"],
            vec!["adr", "../../outside"],
            vec!["a/b"],
            vec!["adr", "a/b"],
            vec![""],
            vec!["."],
            vec![".."],
            vec![],
        ] {
            let err = tree.component(&parts).unwrap_err();
            assert!(matches!(err, SyncError::Escapes), "{parts:?}: {err:?}");
        }
        assert!(!outside.exists());
        // a plain absent component is the joined path under `.kaban/`
        let absent = tree.component(&["adr", "things"]).unwrap();
        assert_eq!(absent, repo.join(KABAN_DIR).join("adr").join("things"));
        assert!(!absent.exists());
        std::fs::remove_dir_all(&scratch).unwrap();
    }
}
