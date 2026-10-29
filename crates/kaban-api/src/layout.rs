//! The one-time layout move (engine-spec EN-F4 "golden promise, narrowed and restated",
//! OWN-46; engine-plan E1.6; core-spec F13): the pre-engine `.kaban/<dir>/<id>.md` record
//! files and the `.kaban/DECISIONS.md` digest become `.kaban/<ontology>/<dir>/<id>.md` and
//! `.kaban/<ontology>/INDEX.md` the first time `kaban sync` runs under the engine.
//!
//! ```text
//!   layout_move(repo, registry, git)          caller holds the M5 RepoLock (R5-4), before import
//!     | for every serialized kind (ontology, dir):
//!     |   .kaban/<dir>/<id>.md  ==>  .kaban/<ontology>/<dir>/<id>.md
//!     |     symlink anywhere on either path   => Escapes (R5-2), nothing moved
//!     |     target absent                     => git mv -k (tracked) | rename (untracked)
//!     |     target present, same bytes        => the source goes (git rm | remove)
//!     |     target present, other bytes       => MoveConflict, both files untouched
//!     | .kaban/DECISIONS.md  ==>  .kaban/adr/INDEX.md   (git mv | rename; removed when the
//!     |                                                   new digest already exists)
//!     v
//!   MoveReport { moved, deleted_digest, already, conflicts }
//! ```
//!
//! **What the move never does.** It reads no frontmatter, rewrites no byte and bumps no `rev`:
//! the files are generated and the ids are stable, so the import that follows finds the moved
//! files equal to the stored records (`unchanged`), and a second `sync` finds nothing to move
//! (`already: true`). A file whose stem is not a node id, a non-`.md` entry, a dot file or a
//! non-regular file is not a record and stays where it is (the directory then stays too).
//!
//! **Git semantics.** Inside a repository the move goes through the existing [`GitRunner`]
//! (`git mv -k -- <sources>... <dir>/`, batched; `-k` skips the untracked sources, which are
//! then renamed through the file system), so the index records renames and `git status`
//! shows `R`. The argv holds kaban's own vocabulary only: the `.kaban` literal, ontology and
//! directory names from loaded ontology files (gated at load, EN-S8) and parsed node ids
//! (R5-8: nothing from a record file reaches argv). Git's output is not read. Outside a
//! repository, or when git fails, the move is a plain `rename` and the index is not touched.
//!
//! **Rollback by hand** (documented in `docs/developer/SYNC.md`): `git mv .kaban/adr/decisions
//! .kaban/decisions` (and the other directories), `git mv .kaban/adr/INDEX.md
//! .kaban/DECISIONS.md`; the next engine `sync` moves them forward again.

use crate::git::GitRunner;
use crate::ontology::{OntologyRegistry, SerializeDir};
use crate::sync::{KabanTree, SyncError};
use kaban_core::NodeId;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// The pre-engine digest file name (`.kaban/DECISIONS.md`): the only place the old name
/// survives, so the move can find it.
pub const LEGACY_DIGEST_FILE: &str = "DECISIONS.md";
/// The per-ontology digest file name (`.kaban/<ontology>/INDEX.md`).
pub const INDEX_FILE: &str = "INDEX.md";
/// The ontology the pre-engine layout belongs to: its digest moves to `.kaban/adr/INDEX.md`.
const PRE_ENGINE_ONTOLOGY: &str = "adr";
/// Most sources one `git mv` call carries (argv stays far below any `ARG_MAX`).
const GIT_MV_BATCH: usize = 500;
/// Largest record file the byte comparison of a move target reads (the import's cap).
const MAX_COMPARE_BYTES: u64 = 4 * 1024 * 1024;

/// A move target that already exists with other bytes (never overwritten): the caller
/// records a `conflicts` row keyed by the file id (revs 0, the two files' sha256).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MoveConflict {
    /// The record id (both file names).
    pub id: String,
    /// sha256 of the pre-engine file's bytes.
    pub source_hash: String,
    /// sha256 of the bytes already at the new path.
    pub target_hash: String,
}

/// What one [`layout_move`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[non_exhaustive]
pub struct MoveReport {
    /// Record files moved (or dropped as exact duplicates of their target).
    pub moved: u32,
    /// Whether the pre-engine `.kaban/DECISIONS.md` was moved to or replaced by the digest.
    pub deleted_digest: bool,
    /// Nothing of the pre-engine layout was found: the tree was already in the engine layout
    /// (or empty) and nothing was written.
    pub already: bool,
    /// Targets that existed with other bytes; the sources stay where they are.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conflicts: Vec<MoveConflict>,
}

/// Move a pre-engine `.kaban/` tree into the `.kaban/<ontology>/...` layout once (see the
/// module doc). The caller holds the repository lock and runs this before the import.
/// `git` is the runner to record the renames with when `repo` is a git repository (`None`
/// outside one: plain renames).
///
/// # Errors
/// [`SyncError::Escapes`] when `.kaban`, a pre-engine record directory or a component of a
/// target path is a symlink or resolves outside the repository (nothing is moved);
/// [`SyncError::Io`] when a rename or a directory creation fails part-way (the files already
/// moved stay moved; the next `sync` continues).
pub fn layout_move(
    repo: &Path,
    registry: &OntologyRegistry,
    git: Option<&GitRunner>,
) -> Result<MoveReport, SyncError> {
    let Some(tree) = KabanTree::open(repo)? else {
        return Ok(MoveReport {
            already: true,
            ..MoveReport::default()
        });
    };
    let mut report = MoveReport::default();
    for (_, target) in registry.serialized_kinds() {
        move_dir(&tree, &target, git, &mut report)?;
    }
    move_digest(&tree, git, &mut report)?;
    report.already = report.moved == 0 && !report.deleted_digest && report.conflicts.is_empty();
    Ok(report)
}

/// The record files of one pre-engine directory into its ontology directory.
fn move_dir(
    tree: &KabanTree,
    target: &SerializeDir,
    git: Option<&GitRunner>,
    report: &mut MoveReport,
) -> Result<(), SyncError> {
    let old_dir = tree.component(&[target.dir.as_str()])?;
    let sources = pre_engine_records(&old_dir)?;
    if sources.is_empty() {
        return Ok(());
    }
    let new_dir = tree.component(&[target.ontology.as_str(), target.dir.as_str()])?;
    std::fs::create_dir_all(&new_dir).map_err(|e| SyncError::Io {
        what: "creating the ontology directory",
        kind: e.kind(),
    })?;
    let mut pending: Vec<(NodeId, PathBuf)> = Vec::new();
    for (id, source) in sources {
        let destination = new_dir.join(format!("{}.md", id.as_str()));
        match std::fs::symlink_metadata(&destination) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => pending.push((id, source)),
            Err(e) => {
                return Err(SyncError::Io {
                    what: "locating the move target",
                    kind: e.kind(),
                });
            }
            Ok(meta) => {
                // the target exists: an exact duplicate lets the source go, anything else is
                // a conflict for a human and both files stay (never overwritten)
                let source_bytes = read_regular(&source)?;
                let target_bytes = if meta.is_file() {
                    read_regular(&destination)?
                } else {
                    None
                };
                match (source_bytes, target_bytes) {
                    (Some(s), Some(t)) if s == t => {
                        remove_source(tree, &source, git)?;
                        report.moved += 1;
                    }
                    (s, t) => report.conflicts.push(MoveConflict {
                        id: id.to_string(),
                        source_hash: sha256(s.as_deref()),
                        target_hash: sha256(t.as_deref()),
                    }),
                }
            }
        }
    }
    report.moved += rename_all(tree, &pending, &new_dir, git)?;
    // an emptied pre-engine directory goes; one that still holds something stays
    let _ = std::fs::remove_dir(&old_dir);
    Ok(())
}

/// The pre-engine digest into `.kaban/adr/INDEX.md`, or away when the new digest exists.
fn move_digest(
    tree: &KabanTree,
    git: Option<&GitRunner>,
    report: &mut MoveReport,
) -> Result<(), SyncError> {
    let old = tree.kaban.join(LEGACY_DIGEST_FILE);
    match std::fs::symlink_metadata(&old) {
        Ok(meta) if meta.is_file() => {}
        Ok(meta) if meta.is_symlink() => return Err(SyncError::Escapes),
        Ok(_) => return Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => {
            return Err(SyncError::Io {
                what: "locating the digest",
                kind: e.kind(),
            });
        }
    }
    let ontology_dir = tree.component(&[PRE_ENGINE_ONTOLOGY])?;
    std::fs::create_dir_all(&ontology_dir).map_err(|e| SyncError::Io {
        what: "creating the ontology directory",
        kind: e.kind(),
    })?;
    let new = ontology_dir.join(INDEX_FILE);
    if new.exists() {
        remove_source(tree, &old, git)?;
    } else {
        rename_one(tree, &old, &new, git)?;
    }
    report.deleted_digest = true;
    Ok(())
}

/// The `<id>.md` record files directly under a pre-engine directory, sorted by id; an absent
/// directory holds none. Only regular files whose stem is a node id count (a dot file, a
/// link, a directory or another name is not a record and is left alone).
fn pre_engine_records(dir: &Path) -> Result<Vec<(NodeId, PathBuf)>, SyncError> {
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
        let name = entry.file_name();
        let Some(stem) = name
            .to_str()
            .filter(|n| !n.starts_with('.'))
            .and_then(|n| n.strip_suffix(".md"))
        else {
            continue;
        };
        let Ok(id) = NodeId::parse(stem) else {
            continue;
        };
        // `DirEntry::metadata` does not follow a symlink: a link is not a record
        if !entry.metadata().is_ok_and(|m| m.is_file()) {
            continue;
        }
        files.push((id, entry.path()));
    }
    files.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
    Ok(files)
}

/// Move `pending` into `new_dir`: `git mv -k` in batches when inside git (the untracked
/// sources are skipped by `-k` and renamed afterwards), plain renames otherwise. Returns how
/// many files landed.
fn rename_all(
    tree: &KabanTree,
    pending: &[(NodeId, PathBuf)],
    new_dir: &Path,
    git: Option<&GitRunner>,
) -> Result<u32, SyncError> {
    if let Some(git) = git {
        let dest = tree.rel(new_dir);
        for batch in pending.chunks(GIT_MV_BATCH) {
            let rels: Vec<String> = batch.iter().map(|(_, p)| tree.rel(p)).collect();
            let mut argv: Vec<&str> = vec!["mv", "-k", "--"];
            argv.extend(rels.iter().map(String::as_str));
            argv.push(&dest);
            if let Err(e) = git.run(&tree.root, &argv) {
                // outside git's reach (git absent, slow, or not a work tree): the file
                // system move below still lands the files, without index entries
                tracing::debug!(error = %e, "git mv unavailable; the layout move renames instead");
            }
        }
    }
    let mut moved = 0u32;
    for (id, source) in pending {
        let destination = new_dir.join(format!("{}.md", id.as_str()));
        if source.exists() && !destination.exists() {
            std::fs::rename(source, &destination).map_err(|e| SyncError::Io {
                what: "moving the record file",
                kind: e.kind(),
            })?;
        }
        moved += 1;
    }
    Ok(moved)
}

/// Move one file: `git mv` inside git (falling back to a rename), a rename otherwise.
fn rename_one(
    tree: &KabanTree,
    old: &Path,
    new: &Path,
    git: Option<&GitRunner>,
) -> Result<(), SyncError> {
    if let Some(git) = git {
        let (from, to) = (tree.rel(old), tree.rel(new));
        if git
            .run(&tree.root, &["mv", "-k", "--", &from, &to])
            .is_err()
        {
            tracing::debug!("git mv unavailable; the layout move renames instead");
        }
    }
    if old.exists() && !new.exists() {
        std::fs::rename(old, new).map_err(|e| SyncError::Io {
            what: "moving the digest",
            kind: e.kind(),
        })?;
    }
    Ok(())
}

/// Drop a source whose target already holds its bytes: `git rm -q` inside git (so the index
/// records the deletion), then the file itself when it is still there.
fn remove_source(
    tree: &KabanTree,
    source: &Path,
    git: Option<&GitRunner>,
) -> Result<(), SyncError> {
    if let Some(git) = git {
        let rel = tree.rel(source);
        if git
            .run(&tree.root, &["rm", "-q", "--ignore-unmatch", "--", &rel])
            .is_err()
        {
            tracing::debug!("git rm unavailable; the layout move removes the file instead");
        }
    }
    match std::fs::remove_file(source) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(SyncError::Io {
            what: "deleting the record file",
            kind: e.kind(),
        }),
    }
}

/// The bytes of a regular file within the cap (`None` when it is not one or is larger).
fn read_regular(path: &Path) -> Result<Option<Vec<u8>>, SyncError> {
    let meta = std::fs::symlink_metadata(path).map_err(|e| SyncError::Io {
        what: "locating the record file",
        kind: e.kind(),
    })?;
    if !meta.is_file() || meta.len() > MAX_COMPARE_BYTES {
        return Ok(None);
    }
    std::fs::read(path).map(Some).map_err(|e| SyncError::Io {
        what: "reading the record file",
        kind: e.kind(),
    })
}

/// sha256 hex of `bytes` (an empty string for an unreadable side).
fn sha256(bytes: Option<&[u8]>) -> String {
    bytes.map_or_else(String::new, kaban_policy::sha256_hex)
}
