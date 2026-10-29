//! `kaban-index status` and `purge-derived` (stage1-spec G1-F7, G1-S6 revalidation on
//! `status`, §7 `published_unsynced` re-verification, RS-3 derived data lifecycle; stage1-plan
//! CG4.6; U1 residual 3): the trust state of a repository as `revalidate` finds it (a mismatch
//! revokes the mark here too), the provenance of every snapshot in the admitted directory read
//! from its `meta` (`isolation`, `isolation_detail`, `caches`, `languages`), the
//! re-verification of a snapshot whose directory `fsync` failed at publication (the `.unsynced`
//! marker: a fresh open runs `PRAGMA quick_check`; success clears the marker, failure keeps it
//! and reports the snapshot corrupt), and the deletion of every derived file (everything in the
//! directory except the trust mark, which is the user's, not derived).
//!
//! ```text
//!   trust_state(&dir.handle, &identity) ==> Trusted | NotTrusted | Revoked | Malformed | Io
//!   snapshots(&dir) ==> [SnapshotStatus { commit, provenance, bytes, state }]  (sorted by commit)
//!      state: Ok | Reverified (marker cleared) | Corrupt | UnsyncedCorrupt (marker kept) | TooNew | Refused
//!   purge_derived(&dir) ==> Purged { files, directories }   (the `trust` file survives)
//! ```
//!
//! Nothing here echoes a value from a snapshot except the closed provenance vocabulary and the
//! `isolation_detail` kaban itself wrote; file names are `<commit40>` plus fixed suffixes.

use std::path::Path;

use kaban_codegraph::identity::{DirHandle, RepoIdentity};
use kaban_codegraph::sqlite::is_final_name;
use kaban_codegraph::{CodeGraphReader, SnapshotError, SqliteSnapshot};
use rustix::fs::AtFlags;
use serde::Serialize;
use thiserror::Error;

use crate::identity_cli::AdmittedDir;
use crate::trust::{self, TRUST_FILE, TrustError};
use crate::writer::{Commit, clear_unsynced, is_unsynced};

/// The trust state as `status` and `doctor` print it (`revalidate` ran: a mismatch has revoked
/// the mark by the time this value exists).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustState {
    /// The mark exists and the live identity matches it.
    Trusted,
    /// No mark.
    NotTrusted,
    /// The identity changed; the mark was deleted now.
    Revoked,
    /// The record did not parse; deleted now.
    Malformed,
    /// The record could not be read (filesystem class).
    Io,
}

impl TrustState {
    /// The fixed text.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Trusted => "trusted",
            Self::NotTrusted => "not trusted",
            Self::Revoked => "revoked (identity changed; run kaban-index trust again)",
            Self::Malformed => "revoked (record malformed; run kaban-index trust again)",
            Self::Io => "unreadable",
        }
    }
}

/// Revalidate the mark against the live identity (G1-S6: on `status` and `doctor` too).
#[must_use]
pub fn trust_state(dir: &DirHandle, id: &RepoIdentity) -> TrustState {
    match trust::revalidate(dir, id) {
        Ok(()) => TrustState::Trusted,
        Err(TrustError::NotTrusted) => TrustState::NotTrusted,
        Err(TrustError::Revoked) => TrustState::Revoked,
        Err(TrustError::Malformed) => TrustState::Malformed,
        Err(_) => TrustState::Io,
    }
}

/// What a fresh open of a snapshot found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotState {
    /// Opened and read.
    Ok,
    /// Carried the `.unsynced` marker; the fresh open (with `quick_check`) passed and the
    /// marker was cleared.
    Reverified,
    /// `quick_check` failed or `meta` is unreadable (`gc --prune-corrupt` deletes it).
    Corrupt,
    /// Carried the `.unsynced` marker and the fresh open failed: the marker stays, the
    /// snapshot is reported corrupt.
    UnsyncedCorrupt,
    /// `meta.schema_version` is above this reader's.
    TooNew,
    /// The file protocol refused it (a symlink, a group/other mode bit, not a regular file).
    Refused,
}

impl SnapshotState {
    /// The fixed text.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Reverified => "re-verified (unsynced marker cleared)",
            Self::Corrupt => "corrupt (run kaban-index gc --prune-corrupt)",
            Self::UnsyncedCorrupt => "unsynced and corrupt (run kaban-index gc --prune-corrupt)",
            Self::TooNew => "snapshot_too_new",
            Self::Refused => "refused",
        }
    }
}

/// One snapshot of the repository as `status` reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SnapshotStatus {
    /// The commit (the file name).
    pub commit: String,
    /// `meta.isolation` (`None` when unreadable).
    pub isolation: Option<String>,
    /// `meta.isolation_detail`.
    pub isolation_detail: Option<String>,
    /// `meta.resources` (the kinds of admitted resources the runs were given).
    pub resources: Option<String>,
    /// `meta.languages`.
    pub languages: Option<String>,
    /// `meta.policy_version`.
    pub policy_version: Option<String>,
    /// The file's size in bytes.
    pub bytes: u64,
    /// What the fresh open found.
    pub state: SnapshotState,
}

/// The commits that have a final snapshot name in `dir`, sorted.
#[must_use]
pub fn final_commits(dir: &Path) -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<String> = rd
        .flatten()
        .filter(|e| is_final_name(&e.file_name()))
        .filter_map(|e| {
            e.file_name()
                .to_str()
                .and_then(|n| n.strip_suffix(".cg.db"))
                .map(str::to_owned)
        })
        .collect();
    out.sort();
    out
}

/// Every snapshot of the directory with its provenance and state (the `published_unsynced`
/// re-verification happens here: a marked snapshot is opened afresh and the marker cleared on
/// success).
#[must_use]
pub fn snapshots(dir: &AdmittedDir) -> Vec<SnapshotStatus> {
    final_commits(&dir.path)
        .into_iter()
        .map(|commit| snapshot_status(dir, &commit))
        .collect()
}

/// The status of one snapshot (see [`snapshots`]).
#[must_use]
pub fn snapshot_status(dir: &AdmittedDir, commit: &str) -> SnapshotStatus {
    let path = dir.path.join(format!("{commit}.cg.db"));
    let bytes = std::fs::symlink_metadata(&path).map_or(0, |m| m.len());
    let marked = Commit::parse(commit).is_some_and(|c| is_unsynced(&dir.handle, &c));
    let mut status = SnapshotStatus {
        commit: commit.to_owned(),
        isolation: None,
        isolation_detail: None,
        resources: None,
        languages: None,
        policy_version: None,
        bytes,
        state: SnapshotState::Corrupt,
    };
    match SqliteSnapshot::open_ro(&path).and_then(|s| s.meta().map_err(|_| SnapshotError::Corrupt))
    {
        Ok(meta) => {
            status.isolation = meta.get("isolation").cloned();
            status.isolation_detail = meta.get("isolation_detail").cloned();
            status.resources = meta.get("resources").cloned();
            status.languages = meta.get("languages").cloned();
            status.policy_version = meta.get("policy_version").cloned();
            status.state = if marked {
                match Commit::parse(commit).map(|c| clear_unsynced(&dir.handle, &c)) {
                    Some(Ok(())) => SnapshotState::Reverified,
                    _ => SnapshotState::UnsyncedCorrupt,
                }
            } else {
                SnapshotState::Ok
            };
        }
        Err(SnapshotError::SnapshotTooNew { .. }) => status.state = SnapshotState::TooNew,
        Err(
            SnapshotError::SymlinkRefused
            | SnapshotError::ModeRefused
            | SnapshotError::NotRegularFile
            | SnapshotError::TempNameRefused,
        ) => status.state = SnapshotState::Refused,
        // `Corrupt`, `Io` and any class a later reader adds: not readable here
        Err(_) => {
            status.state = if marked {
                SnapshotState::UnsyncedCorrupt
            } else {
                SnapshotState::Corrupt
            };
        }
    }
    status
}

/// Whether the snapshot of `commit` is present, and if so what a fresh open finds (the `index`
/// `already_indexed` path: an existing name is never overwritten; a marked one is re-verified
/// first).
#[must_use]
pub fn existing(dir: &AdmittedDir, commit: &Commit) -> Option<SnapshotState> {
    let name = format!("{commit}.cg.db");
    rustix::fs::statat(&dir.handle, name, AtFlags::SYMLINK_NOFOLLOW)
        .ok()
        .map(|_| snapshot_status(dir, commit.as_str()).state)
}

/// What `purge_derived` removed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Purged {
    /// Files unlinked (snapshots, logs, markers, temp files).
    pub files: usize,
    /// Directories removed with their contents (scratch, caches).
    pub directories: usize,
}

/// Why a purge did not complete.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum PurgeError {
    /// An entry could not be removed; the errno class.
    #[error("purge: {0}")]
    Remove(&'static str),
}

/// Delete every derived file of the repository directory (RS-3): every entry except the trust
/// mark. Files go through the admitted handle (`unlinkat`); directories (scratch `run-<pid>`,
/// caches) are removed with their contents.
///
/// # Errors
/// [`PurgeError::Remove`] on the first entry that could not be removed (the rest is left).
pub fn purge_derived(dir: &AdmittedDir) -> Result<Purged, PurgeError> {
    let rd = std::fs::read_dir(&dir.path).map_err(|_| PurgeError::Remove("read_dir"))?;
    let mut purged = Purged::default();
    for entry in rd.flatten() {
        let name = entry.file_name();
        if name == TRUST_FILE {
            continue;
        }
        let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
        if is_dir {
            std::fs::remove_dir_all(entry.path()).map_err(|_| PurgeError::Remove("dir"))?;
            purged.directories += 1;
        } else {
            rustix::fs::unlinkat(&dir.handle, name.as_os_str(), AtFlags::empty())
                .map_err(|_| PurgeError::Remove("unlink"))?;
            purged.files += 1;
        }
    }
    Ok(purged)
}
