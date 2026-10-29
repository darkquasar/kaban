//! Snapshot resolution behind one trait (cg-join-v1 §4, SD-4; stage1-spec G1-F8, G1-S7;
//! stage1-plan CG5.2): [`SnapshotProvider`] is what the engine's adapter
//! (`kaban-api/src/snapshot.rs`, E2.6) calls with what it learned from git; [`DiscoverProvider`]
//! is the production implementation over [`discover`](crate::discover::discover) and the LRU of
//! two ([`SnapshotCache`]); [`HeadCache`] memoises one [`HeadInput`] per head sha so
//! `rev-list` runs once per head change (`rev-parse HEAD` runs on every call). This crate
//! spawns nothing (G1-S3): the sha and the fetch are the caller's.
//!
//! ```text
//!   DiscoverProvider::resolve(_, head)
//!     identity::walk(dir)  every component O_PATH|O_DIRECTORY|O_NOFOLLOW  --fail-->  NoDirectory (traced)
//!       <repo-id> and <codegraph>: owned by the euid, no group/other write bit, and the same
//!       (dev, ino) pair as at the first admission (RU3-3)        --fail-->  NoDirectory (traced)
//!       <repo-id> itself absent (purge-derived, gc): not_found and the retained pair cleared,
//!       so the recreated directory is admitted on the next resolve (OWN-81 (4)); any other
//!       failure, a swap in place (directory_replaced) included, keeps the pair
//!     discover(dir, head)  ==> (path, commit)
//!     cache.get_or_open(path)  --Err(class)-->  traced, candidate dropped, discover again
//!       (a cached entry is reused only at the generation it was admitted at, RU3-2)
//!     ref <== the lease's meta           --incomplete-->  traced, candidate dropped
//!     validate_for_attach(path) --Err(class)-->  traced, candidate dropped, discover again
//!       attach generation != lease generation --> traced generation_changed, candidate dropped
//!     ==> Resolved { snapshot (request-scoped lease), ref, attach }   one generation
//! ```
//!
//! The trait is deliberately not sealed (`api-sealed-trait` weighed): the engine's tests and
//! [`FixedSnapshot`](crate::testing::FixedSnapshot) implement it, and its one method is the
//! whole contract.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

pub use crate::discover::{HeadInput, NoSnapshot, SnapshotRef};
use crate::discover::{SnapshotCache, discover, skip_class, snapshot_ref};
use crate::error::SnapshotError;
use crate::identity::{self, DirHandle, IdentityError};
use crate::reader::CodeGraphReader;
use crate::sqlite::{AttachUri, SqliteSnapshot, validate_for_attach};

/// What a successful resolution hands the caller (cg-join-v1 §4): the open snapshot as a
/// request-scoped lease (RG2-6: drop it with the request, never keep it across requests), its
/// identity, and the admitted attach URI the engine's read connection uses.
#[derive(Debug)]
pub struct Resolved {
    /// The open read-only snapshot (a lease on the provider's cache entry, or a fresh open).
    pub snapshot: Arc<SqliteSnapshot>,
    /// The resolved commit and the `meta` fields the responses project.
    pub r#ref: SnapshotRef,
    /// The admitted `ATTACH` parameter for the same file.
    pub attach: AttachUri,
}

/// The one resolution entry point (cg-join-v1 §4, SD-4). `repo_identity` is `ApiConfig.repo`,
/// the join's repository identity (SD-5); the production provider is already bound to one
/// repository's directory and accepts it unused.
pub trait SnapshotProvider: Send + Sync {
    /// Resolve the snapshot for `head` (exact, else the nearest ancestor with one).
    ///
    /// # Errors
    /// [`NoSnapshot`] with the closed reason.
    fn resolve(&self, repo_identity: &str, head: &HeadInput) -> Result<Resolved, NoSnapshot>;
}

/// The production provider: [`discover`] over `<codegraph>/<repo-id>` plus the LRU of two open
/// snapshots (G1-F8, the fixed spec constant). One per process, behind the `Arc<dyn
/// SnapshotProvider>` the service holds (`own-arc-shared`); the state is the cache and the
/// directory identities of the first admission, each locked for its check alone, never across
/// a request.
#[derive(Debug)]
pub struct DiscoverProvider {
    dir: PathBuf,
    cache: Mutex<SnapshotCache>,
    /// The `(dev, ino)` of `<repo-id>` and `<codegraph>` at the first admission (RU3-3): a
    /// later resolve that finds other objects at the same path refuses, unless a resolve in
    /// between found `<repo-id>` itself absent, which clears the pair (OWN-81 (4)).
    identity: Mutex<Option<DirIdentity>>,
}

/// The directory objects behind `<codegraph>/<repo-id>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DirIdentity {
    repo: (u64, u64),
    codegraph: (u64, u64),
}

/// Why [`DiscoverProvider::admit_dir`] refused: the traced class (never a path) and whether
/// this refusal reset the retained identity (OWN-81 (4): only `<repo-id>`'s own absence does).
#[derive(Debug, Clone, Copy)]
struct Refused {
    class: &'static str,
    reset: bool,
}

impl Refused {
    /// A refusal that keeps the retained identity.
    const fn keep(class: &'static str) -> Self {
        Self {
            class,
            reset: false,
        }
    }
}

/// The traced class of a directory admission failure (never a path).
fn walk_class(e: &IdentityError) -> &'static str {
    match e {
        IdentityError::NotAbsolute => "not_absolute",
        IdentityError::SymlinkComponent => "symlink_refused",
        IdentityError::NotDirectory => "not_directory",
        IdentityError::NotFound => "not_found",
        IdentityError::Io(class) => class,
        _ => "other",
    }
}

impl DiscoverProvider {
    /// A provider over `dir` (`<KABAN_HOME>/codegraph/<repo-id>`, absolute). The directory need
    /// not exist: resolution reports [`NoSnapshot::NoDirectory`] until `kaban-index` creates
    /// it. Nothing is opened or created here.
    #[must_use]
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            cache: Mutex::new(SnapshotCache::new()),
            identity: Mutex::new(None),
        }
    }

    /// Admit the directory chain before anything under it is discovered (G1-S7, RS-5; RU3-3):
    /// every component of `dir` is walked with `O_PATH|O_DIRECTORY|O_NOFOLLOW` (a symlink
    /// anywhere refuses; a relative `dir` refuses as `not_absolute`), then `<repo-id>` (the
    /// handle) and `<codegraph>` (its parent) must each be owned by the effective uid with no
    /// group or other write bit (`kaban-index` creates them `0700`, U2), and their `(dev, ino)`
    /// must equal the pair retained at the first admission. One failure resets that pair
    /// (OWN-81 (4)): `<repo-id>` itself absent while its parent is there (`purge-derived`, gc),
    /// traced `not_found`, so the directory `kaban-index` recreates at the path is admitted on
    /// the next resolve without a restart. Every other failure, another object at the path
    /// (`directory_replaced`) included, leaves the pair as it is: a swap the provider never saw
    /// as an absence stays refused until restart. The error is the traced class plus whether the
    /// pair was reset.
    fn admit_dir(&self) -> Result<(), Refused> {
        let repo = self.walk_repo()?;
        let codegraph = repo.parent().map_err(|e| Refused::keep(walk_class(&e)))?;
        let euid = rustix::process::geteuid().as_raw();
        let private = |handle: &DirHandle| -> Result<(u64, u64), Refused> {
            let (uid, mode) = handle
                .owner_and_mode()
                .map_err(|e| Refused::keep(walk_class(&e)))?;
            if uid != euid {
                return Err(Refused::keep("not_owner"));
            }
            if mode & 0o022 != 0 {
                return Err(Refused::keep("mode_refused"));
            }
            handle.dev_ino().map_err(|e| Refused::keep(walk_class(&e)))
        };
        let now = DirIdentity {
            repo: private(&repo)?,
            codegraph: private(&codegraph)?,
        };
        let mut known = self.identity.lock().unwrap_or_else(PoisonError::into_inner);
        match *known {
            Some(first) if first != now => Err(Refused::keep("directory_replaced")),
            Some(_) => Ok(()),
            None => {
                *known = Some(now);
                Ok(())
            }
        }
    }

    /// The `<repo-id>` handle: the walk up to its parent, then the one `openat` of the final
    /// component, so the directory's own absence (`NotFound` on that open, the parent present)
    /// is told apart from a missing or refused ancestor. That absence clears the retained
    /// identity before the class is returned (OWN-81 (4)); nothing else does. A `dir` without a
    /// final component (`/`) is walked whole and never resets.
    fn walk_repo(&self) -> Result<DirHandle, Refused> {
        let Some((codegraph, name)) = self.dir.parent().zip(self.dir.file_name()) else {
            return identity::walk(&self.dir).map_err(|e| Refused::keep(walk_class(&e)));
        };
        let codegraph = identity::walk(codegraph).map_err(|e| Refused::keep(walk_class(&e)))?;
        match codegraph.child(name) {
            Ok(repo) => Ok(repo),
            Err(IdentityError::NotFound) => {
                *self.identity.lock().unwrap_or_else(PoisonError::into_inner) = None;
                Err(Refused {
                    class: "not_found",
                    reset: true,
                })
            }
            Err(e) => Err(Refused::keep(walk_class(&e))),
        }
    }

    /// The directory this provider resolves in.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// A lease on the open snapshot at `path` from the LRU (the lock is held for the open only).
    fn lease(&self, path: &Path) -> Result<Arc<SqliteSnapshot>, &'static str> {
        self.cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get_or_open(path)
            .map_err(skip_class)
    }

    /// One generation per `Resolved` (RU3-2): the lease, the `SnapshotRef` read from that
    /// lease's own `meta`, and an attach admitted at the same file generation as the lease.
    /// The error is the traced skip class.
    fn one_generation(
        &self,
        path: &Path,
        commit: &str,
    ) -> Result<(Arc<SqliteSnapshot>, SnapshotRef, AttachUri), &'static str> {
        let snapshot = self.lease(path)?;
        let meta = snapshot.meta().map_err(|_| "corrupt")?;
        let r#ref =
            snapshot_ref(&meta, snapshot.schema_version(), commit).ok_or("meta_incomplete")?;
        let attach = validate_for_attach(path).map_err(skip_class)?;
        if attach.generation() != snapshot.generation() {
            return Err(skip_class(SnapshotError::Io {
                class: "generation_changed",
            }));
        }
        Ok((snapshot, r#ref, attach))
    }
}

impl SnapshotProvider for DiscoverProvider {
    fn resolve(&self, _repo_identity: &str, head: &HeadInput) -> Result<Resolved, NoSnapshot> {
        // `discover` admits a candidate by opening it once; the cached open and the attach
        // validation run the same checks again and can only fail when the file changed under
        // us. Such a candidate is skipped like a corrupt one (traced, never a panic): it is
        // removed from a working copy of the head and discovery reruns, at most once per
        // candidate. The `SnapshotRef` discovery read is not used: the one that goes out is
        // read from the leased connection, and the attach must be of the lease's generation
        // (RU3-2), so a same-name republication between the steps skips the candidate rather
        // than mixing two files in one `Resolved`.
        // G1-S7 (RS-5): the directory chain is admitted before anything is opened; `discover`
        // itself follows the directory (`metadata`), so the walk, the private-directory rule
        // and the identity re-check live here (RU3-3). `reset` tells the serve log a
        // `purge-derived` (the identity cleared, the re-index will be served) from a swap in
        // place (refused until restart), OWN-81 (4).
        if let Err(refused) = self.admit_dir() {
            tracing::debug!(
                class = refused.class,
                reset = refused.reset,
                "snapshot directory refused"
            );
            return Err(NoSnapshot::NoDirectory);
        }
        let mut working = head.clone();
        let mut skipped = false;
        for _ in 0..=head.ancestors.len() {
            let (path, discovered) = match discover(&self.dir, &working) {
                Ok(found) => found,
                Err(NoSnapshot::NoFile) if skipped => return Err(NoSnapshot::CorruptOnly),
                Err(reason) => return Err(reason),
            };
            let exact = discovered.commit == head.commit;
            match self.one_generation(&path, &discovered.commit) {
                Ok((snapshot, mut r#ref, attach)) => {
                    r#ref.exact = exact;
                    return Ok(Resolved {
                        snapshot,
                        r#ref,
                        attach,
                    });
                }
                Err(class) => {
                    tracing::debug!(class, exact, "snapshot skipped after discovery");
                    skipped = true;
                    if !drop_candidate(&mut working, &discovered.commit) {
                        return Err(NoSnapshot::CorruptOnly);
                    }
                }
            }
        }
        Err(NoSnapshot::CorruptOnly)
    }
}

/// Remove `commit` from the working head: the head commit is replaced by the nearest ancestor
/// (`false` when none is left), an ancestor is dropped from the list.
fn drop_candidate(working: &mut HeadInput, commit: &str) -> bool {
    if working.commit == commit {
        if working.ancestors.is_empty() {
            return false;
        }
        working.commit = working.ancestors.remove(0);
        return true;
    }
    working.ancestors.retain(|a| a != commit);
    true
}

/// One cached [`HeadInput`] per process (G1-F8: the ancestors of one head sha, keyed by that
/// sha). The caller resolves the current sha on every call (`rev-parse HEAD`, about 1 ms) and
/// supplies the ancestor fetch; a commit's ancestors never change, so a hit on the same sha is
/// always current. The fetch's error type is the caller's (this crate does not know
/// `GitError`, cg-join-v1 §2).
#[derive(Debug, Default)]
pub struct HeadCache {
    inner: Mutex<Option<Arc<HeadInput>>>,
}

impl HeadCache {
    /// An empty cache.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The cached head when its commit is `commit`, else run `ancestors` once and cache the
    /// new head under `commit` (any other sha refetches, an earlier one too: the cache holds
    /// one). A failed fetch leaves the cache unchanged. Concurrent callers serialise on the
    /// fetch, so a head change costs one `rev-list`, not one per request.
    ///
    /// # Errors
    /// The fetch's error, unchanged.
    pub fn get_or_refresh<E>(
        &self,
        commit: &str,
        ancestors: impl FnOnce() -> Result<Vec<String>, E>,
    ) -> Result<Arc<HeadInput>, E> {
        let mut slot = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(head) = slot.as_ref()
            && head.commit == commit
        {
            return Ok(Arc::clone(head));
        }
        let head = Arc::new(HeadInput {
            commit: commit.to_owned(),
            ancestors: ancestors()?,
        });
        *slot = Some(Arc::clone(&head));
        Ok(head)
    }
}
