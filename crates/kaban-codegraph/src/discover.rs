//! Snapshot discovery (stage1-spec G1-F8; cg-join-v1 §4; stage1-plan CG2.3): the exact
//! `<dir>/<head.commit>.cg.db` when present and valid, else the first `head.ancestors` entry with
//! a valid snapshot; a candidate that fails admission is skipped with a traced class, and the
//! reason when nothing resolves is the contract's closed [`NoSnapshot`]. [`HeadInput`] and
//! [`SnapshotRef`] live here (the first task that uses them); `provider.rs` (CG5.2) re-exports
//! them beside `SnapshotProvider`. [`SnapshotCache`] is the LRU of two cached snapshots that
//! `DiscoverProvider` owns (leases are request-scoped, see the type).
//!
//! ```text
//!   head.commit ==> exact?  ==> admitted?  ==> (path, SnapshotRef { exact: true })
//!        |                       | skipped (too new | corrupt | refused), traced
//!        v                       v
//!   head.ancestors[0..] in order ==> first admitted ==> (path, SnapshotRef { exact: false })
//!   none: NoDirectory | NoFile | TooNewOnly (wins) | CorruptOnly
//! ```

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::error::SnapshotError;
use crate::reader::{CodeGraphReader, Meta};
use crate::sqlite::{FINAL_SUFFIX, FileGeneration, SqliteSnapshot};

/// The isolation tier recorded in `meta.isolation` (stage1-spec G1-S5, OWN-29).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IsolationTier {
    /// Linux user, network and mount namespaces.
    Namespaces,
    /// A container runtime.
    Container,
    /// Native, no isolation.
    None,
}

impl IsolationTier {
    /// The `meta.isolation` value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Namespaces => "namespaces",
            Self::Container => "container",
            Self::None => "none",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "namespaces" => Some(Self::Namespaces),
            "container" => Some(Self::Container),
            "none" => Some(Self::None),
            _ => Option::None,
        }
    }
}

/// The resolved snapshot's identity (cg-join-v1 §4, SD-4), read from `meta`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotRef {
    /// 40 lowercase hex, the file's commit.
    pub commit: String,
    /// `commit == head.commit`.
    pub exact: bool,
    /// `meta.isolation`.
    pub isolation_tier: IsolationTier,
    /// `meta.schema_version` major.
    pub schema_version: u32,
    /// `meta.policy_version`; the caller compares it with the live policy (`current | stale`).
    pub policy_version: String,
}

/// What the caller learned from git (cg-join-v1 §4): `kaban-codegraph` spawns nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadInput {
    /// `rev-parse HEAD`, 40 lowercase hex.
    pub commit: String,
    /// `rev-list --max-count=200 <commit>` (the sha above, so a checkout between the two git
    /// calls cannot mix two heads; RU3-5), head excluded, nearest first, 40 lowercase hex each.
    pub ancestors: Vec<String>,
}

/// Why nothing resolved (cg-join-v1 §4, closed; the card projection of §6 depends on it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NoSnapshot {
    /// No `<repo-id>` directory (or no key).
    NoDirectory,
    /// The directory exists but no candidate file does, for head or any ancestor.
    NoFile,
    /// Every existing candidate was skipped as `SnapshotTooNew` (wins over `CorruptOnly`).
    TooNewOnly,
    /// Every existing candidate was skipped as `Corrupt` or refused by admission.
    CorruptOnly,
}

/// The resolution rule of the module doc.
///
/// # Errors
/// [`NoSnapshot`] with the reason.
pub fn discover(dir: &Path, head: &HeadInput) -> Result<(PathBuf, SnapshotRef), NoSnapshot> {
    if !std::fs::metadata(dir).is_ok_and(|m| m.is_dir()) {
        return Err(NoSnapshot::NoDirectory);
    }
    let mut saw_too_new = false;
    let mut saw_skipped = false;
    let candidates = std::iter::once(&head.commit).chain(head.ancestors.iter());
    for commit in candidates {
        let path = dir.join(format!("{commit}{FINAL_SUFFIX}"));
        if std::fs::symlink_metadata(&path).is_err() {
            continue;
        }
        match admit_candidate(&path, commit) {
            Ok(mut r#ref) => {
                r#ref.exact = *commit == head.commit;
                return Ok((path, r#ref));
            }
            Err(class) => {
                tracing::debug!(class, exact = (*commit == head.commit), "snapshot skipped");
                if class == "snapshot_too_new" {
                    saw_too_new = true;
                } else {
                    saw_skipped = true;
                }
            }
        }
    }
    Err(if saw_too_new {
        NoSnapshot::TooNewOnly
    } else if saw_skipped {
        NoSnapshot::CorruptOnly
    } else {
        NoSnapshot::NoFile
    })
}

/// Open and read the `SnapshotRef` of one candidate; the error is the traced class.
pub(crate) fn admit_candidate(path: &Path, commit: &str) -> Result<SnapshotRef, &'static str> {
    let snap = SqliteSnapshot::open_ro(path).map_err(skip_class)?;
    let meta = snap.meta().map_err(|_| "corrupt")?;
    snapshot_ref(&meta, snap.schema_version(), commit).ok_or("meta_incomplete")
}

/// The five SD-4 fields from `meta`; `None` when a required key is missing, `meta.commit`
/// disagrees with the file name or `isolation` is outside its set.
pub(crate) fn snapshot_ref(meta: &Meta, schema_version: u32, commit: &str) -> Option<SnapshotRef> {
    let meta_commit = meta.get("commit")?;
    if meta_commit != commit {
        return None;
    }
    Some(SnapshotRef {
        commit: commit.to_owned(),
        exact: false,
        isolation_tier: IsolationTier::parse(meta.get("isolation")?)?,
        schema_version,
        policy_version: meta.get("policy_version")?.clone(),
    })
}

/// The traced class of an admission failure (shared with `provider.rs`).
pub(crate) fn skip_class(e: SnapshotError) -> &'static str {
    match e {
        SnapshotError::SnapshotTooNew { .. } => "snapshot_too_new",
        SnapshotError::Corrupt => "corrupt",
        SnapshotError::SymlinkRefused => "symlink_refused",
        SnapshotError::ModeRefused => "mode_refused",
        SnapshotError::NotRegularFile => "not_regular_file",
        SnapshotError::TempNameRefused => "temp_name_refused",
        SnapshotError::Io { class } => class,
    }
}

/// The LRU of two open snapshots keyed by path (G1-F8 "at most two snapshots open per process",
/// read with the cg-join-v1 §4 lease clause, RG2-6): the cache bounds its **cached** connections
/// at [`Self::CAPACITY`]; the `Arc<SqliteSnapshot>` a caller receives is a **request-scoped
/// lease** that may transiently keep an evicted connection alive until it drops. Eviction drops
/// the cache's reference; the connection closes with the last holder. A third open is admitted
/// only after the least recently used entry was released, so the cache itself never holds three
/// (a failed open has already released that slot). Provider code (CG5.2) must not retain leases
/// across requests.
#[derive(Debug, Default)]
pub struct SnapshotCache {
    /// Most recently used last.
    entries: Vec<(PathBuf, Arc<SqliteSnapshot>)>,
}

impl SnapshotCache {
    /// The G1-F8 ceiling, a fixed spec constant and not a config key (cg-join-v1 §4, SO-11).
    pub const CAPACITY: usize = 2;

    /// An empty cache.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A lease on the open snapshot at `path`: the cached one when the path still has the file
    /// generation it was admitted at (RU3-2: a same-name republication is another file; the
    /// stale entry is dropped and the path reopened, a lease a caller still holds on the old
    /// generation stays valid until it drops), or a new one opened after the least recently
    /// used entry was released when the cache was full.
    ///
    /// # Errors
    /// [`SnapshotError`] from `open_ro` (the released slot stays free).
    pub fn get_or_open(&mut self, path: &Path) -> Result<Arc<SqliteSnapshot>, SnapshotError> {
        if let Some(pos) = self.entries.iter().position(|(p, _)| p == path) {
            let entry = self.entries.remove(pos);
            if FileGeneration::current(path).is_ok_and(|now| now == entry.1.generation()) {
                let snap = Arc::clone(&entry.1);
                self.entries.push(entry);
                return Ok(snap);
            }
            tracing::debug!(class = "generation_changed", "cached snapshot dropped");
            drop(entry);
        }
        if self.entries.len() >= Self::CAPACITY {
            // release before opening: the cache never holds a third connection
            drop(self.entries.remove(0));
        }
        let snap = Arc::new(SqliteSnapshot::open_ro(path)?);
        self.entries.push((path.to_path_buf(), Arc::clone(&snap)));
        Ok(snap)
    }

    /// Whether `path` is currently open in the cache.
    #[must_use]
    pub fn contains(&self, path: &Path) -> bool {
        self.entries.iter().any(|(p, _)| p == path)
    }

    /// Cached snapshots held (leases a caller still holds are not counted).
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// No snapshot held.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}
