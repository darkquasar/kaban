//! Current-head resolution (stage1-spec G1-F8, G1-S3; cg-join-v1 §4; stage1-plan CG7.1
//! default 2): the one place `kaban-api` learns `HEAD` for the code side. Every call runs
//! `git rev-parse HEAD` (about 1 ms; a same-branch commit rewrites the branch ref, not
//! `.git/HEAD`, so no file mtime is a safe key) and [`HeadCache`] memoises the one `git
//! rev-list --max-count=200 <sha>` under that sha (the sha `rev-parse` returned, never `HEAD`
//! again: a checkout between the two calls must not hand one sha the other's ancestry,
//! RU3-5); both run through [`GitRunner`] (the only spawner); the provider then resolves the
//! snapshot. [`HeadSource::head_input`] is the one head-fetch implementation: `code_*` tools
//! through [`KabanApi::resolve_current`], `remember`'s ref steps through `snapshot_for_refs`,
//! and E2.6's `snapshot.rs` builds its `SnapshotAttach` over `resolve_current` rather than
//! fetching the head itself.
//!
//! ```text
//!   provider? repo_root?  --no-->  NoSnapshot (traced no_provider | no_repo)
//!   pinned head? (KabanApi::with_pinned_head, the bench harness; CG7.3)  --yes-->  that head, no git
//!   git rev-parse HEAD  (every call; not 40 hex => NoSnapshot, traced by class)
//!     ==> HeadCache::get_or_refresh(sha, ancestors)   ancestors: rev-list <sha> (head excluded), once per sha
//!     ==> provider.resolve(repo, head)   TooNewOnly => SnapshotTooNew, else NoSnapshot
//! ```

use std::path::Path;
use std::sync::Arc;

use kaban_codegraph::{HeadCache, HeadInput, NoSnapshot, Resolved};

use super::CodeError;
use crate::api::KabanApi;
use crate::git::{GitError, GitRunner};

/// The ancestor window of G1-F8.
const REV_LIST_MAX: &str = "--max-count=200";

/// The per-process head state: the cached [`HeadInput`](kaban_codegraph::HeadInput) of the
/// last seen sha, or a head pinned at construction. Shared by every clone of the service
/// through an `Arc` (`own-arc-shared`).
#[derive(Debug, Default)]
pub(crate) struct HeadSource {
    cache: HeadCache,
    /// A head fixed when the service was built (`KabanApi::with_pinned_head`, stage1-plan
    /// CG7.3: the bench harness over a `FixedSnapshot` with no repository and no git); returned
    /// before git is consulted. `kaban serve` never pins.
    pinned: Option<Arc<HeadInput>>,
}

/// Why the head fetch produced nothing usable (traced by class, never returned as text).
#[derive(Debug)]
pub(crate) enum FetchError {
    Git(GitError),
    NotHex,
}

impl From<GitError> for FetchError {
    fn from(e: GitError) -> Self {
        Self::Git(e)
    }
}

impl FetchError {
    pub(crate) fn class(&self) -> &'static str {
        match self {
            Self::Git(GitError::Spawn(_)) => "git_spawn",
            Self::Git(GitError::Timeout(_)) => "git_timeout",
            Self::Git(GitError::Status(_)) => "git_status",
            Self::Git(GitError::Output) => "git_output",
            Self::NotHex => "not_hex",
        }
    }
}

/// Exactly 40 lowercase hex digits (a full commit id; `GitRunner::head` admits short ones).
fn is_commit40(s: &str) -> bool {
    s.len() == 40
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

impl HeadSource {
    /// A source pinned to `commit` with no ancestors: [`Self::head_input`] answers it without
    /// spawning anything.
    pub(crate) fn pinned(commit: &str) -> Self {
        Self {
            cache: HeadCache::default(),
            pinned: Some(Arc::new(HeadInput {
                commit: commit.to_owned(),
                ancestors: Vec::new(),
            })),
        }
    }

    /// `rev-parse HEAD` through the runner (G1-S3): the current sha, run on every call.
    fn current(git: &GitRunner, root: &Path) -> Result<String, FetchError> {
        let head = git.run(root, &["rev-parse", "HEAD"])?;
        let commit = head.trim();
        if !is_commit40(commit) {
            return Err(FetchError::NotHex);
        }
        Ok(commit.to_owned())
    }

    /// `rev-list --max-count=200 <commit>` through the runner (G1-S3): the ancestors of
    /// `commit` itself, the head excluded. Run once per sha by [`HeadCache`].
    fn ancestors(git: &GitRunner, root: &Path, commit: &str) -> Result<Vec<String>, FetchError> {
        let listed = git.run(root, &["rev-list", REV_LIST_MAX, commit])?;
        let mut ancestors = Vec::new();
        for line in listed.lines().map(str::trim) {
            if line == commit {
                continue;
            }
            if !is_commit40(line) {
                return Err(FetchError::NotHex);
            }
            ancestors.push(line.to_owned());
        }
        Ok(ancestors)
    }

    /// The current head of `root` with its ancestry: the pinned head when one was set, else
    /// `rev-parse HEAD` on every call and the ancestors of that sha from the cache or one
    /// `rev-list <sha>` (the one head-fetch implementation, shared by the `code_*` tools and
    /// `remember`).
    ///
    /// # Errors
    /// [`FetchError`] by class: git failed, or an output line is not a full commit id.
    pub(crate) fn head_input(
        &self,
        git: &GitRunner,
        root: &Path,
    ) -> Result<Arc<HeadInput>, FetchError> {
        if let Some(pinned) = &self.pinned {
            return Ok(Arc::clone(pinned));
        }
        let commit = Self::current(git, root)?;
        self.cache
            .get_or_refresh(&commit, || Self::ancestors(git, root, &commit))
    }
}

impl KabanApi {
    /// The snapshot of the current `HEAD` (exact, else the nearest ancestor's) through this
    /// process's provider (cg-join-v1 §4, §6).
    ///
    /// # Errors
    /// [`CodeError::NoSnapshot`] without a provider or a repository root, when git cannot
    /// name the head, or when the provider finds nothing; [`CodeError::SnapshotTooNew`] when
    /// every candidate was written by a newer schema.
    pub fn resolve_current(&self) -> Result<Resolved, CodeError> {
        let Some(provider) = self.snapshot_provider.as_ref() else {
            tracing::debug!(class = "no_provider", "code graph not resolved");
            return Err(CodeError::NoSnapshot);
        };
        let Some(root) = self.config.repo_root.as_deref() else {
            tracing::debug!(class = "no_repo", "code graph not resolved");
            return Err(CodeError::NoSnapshot);
        };
        let head = self.head.head_input(&self.git, root).map_err(|e| {
            tracing::debug!(class = e.class(), "head not fetched; no snapshot");
            CodeError::NoSnapshot
        })?;
        provider
            .resolve(self.config.repo.as_deref().unwrap_or(""), &head)
            .map_err(|reason| match reason {
                NoSnapshot::TooNewOnly => CodeError::SnapshotTooNew,
                NoSnapshot::NoDirectory | NoSnapshot::NoFile | NoSnapshot::CorruptOnly => {
                    CodeError::NoSnapshot
                }
            })
    }
}
