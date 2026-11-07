//! The `kaban-index` side of repository identity (stage1-plan CG4.1; stage1-spec G1-S6, §4.1):
//! the two git calls through the one-shot runner, the key file through
//! `kaban_policy::open_or_create_key`, the content gate over the canonical path before
//! `repo_id`, and the admission of the per-repository data directory
//! `<KABAN_HOME>/codegraph/<repo-id>` as a validated handle (0700, owned by the user, reached by
//! the symlink-refusing walk) that every later open goes through (U1 residual 1).
//!
//! ```text
//!   git(cwd, args) ==> Exec::Handle(/usr/bin/git | /bin/git | /usr/local/bin/git: the handle
//!                  rule) -c core.fsmonitor=false -c core.hooksPath=/dev/null ... <args>
//!                  env = GIT_QUERY_INTERNAL + LC_ALL=C (no PATH, no HOME), InheritedFds::Refuse
//!   resolve(repo) ==> git rev-parse --git-common-dir ==> identity::resolve_common_dir
//!                 ==> git rev-list --max-parents=0 HEAD ==> identity::root_commits
//!                 ==> Resolved { identity, handle, canonical_common_dir, canonical_repo }
//!   repo_id_gated(policy, key, canonical) ==> Ok(id) | Err(RepoPathRefused)   (exit 4)
//!   admit_repo_dir(home, id) ==> AdmittedDir (DirHandle + path), created 0700 when missing
//! ```
//!
//! Git is a metadata oracle here, never a tool the repository may configure (U2 RU2-4): the
//! executable comes from the fixed candidate list through the wrapper handle rule (a regular
//! ELF file owned by root or the user, writable by neither group nor other, executed through
//! the handle), the caller's `PATH` and `HOME` are not consulted, the system and global
//! configuration are not read, and every execution-capable key of the repository's own
//! configuration is overridden on the command line.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use kaban_codegraph::identity::{
    self, CommonDirHandle, DirHandle, IdentityError, RepoIdKey, RepoIdentity,
};
use kaban_core::process::{Exec, InheritedFds, ProcessError, Spec};
use kaban_policy::Policy;
use thiserror::Error;

/// Why the identity of a repository could not be established. Classes only.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum IdentityCliError {
    /// `git` could not run (absent, refused environment, spawn failure); an inherited
    /// descriptor is named for what it is (G7): `git could not run: the kaban-index process
    /// inherited an open file descriptor (fd 9); close it in the caller`.
    #[error("git could not run: {}", git_cause(.0))]
    Git(#[source] ProcessError),
    /// `git` exited non-zero or was killed: not a repository, or no `HEAD`.
    #[error("not a git repository, or it has no commits")]
    NotARepository,
    /// A `git` output was not UTF-8.
    #[error("git output malformed")]
    GitOutput,
    /// The identity resolution refused (walk, worktree, roots).
    #[error(transparent)]
    Identity(#[from] IdentityError),
    /// The canonical common directory hit the content gate: `repo_path_refused` (exit 4);
    /// nothing is derived or persisted from the path.
    #[error("repo_path_refused")]
    RepoPathRefused,
    /// The key file could not be opened or created.
    #[error("codegraph key unavailable")]
    Key,
    /// The data directory for the repository could not be admitted (not 0700, not owned by the
    /// user, a symlink on its path).
    #[error("data directory refused ({0})")]
    DataDir(&'static str),
    /// `KABAN_HOME` and `HOME` are both unset.
    #[error("neither KABAN_HOME nor HOME is set")]
    NoHome,
    /// No git at a trusted location, or the one found is refused by the handle rule (not a
    /// regular ELF file owned by root or the user, or writable by group or other).
    #[error("git_untrusted")]
    GitUntrusted,
    /// `git show <sha>:<path>` exited non-zero (the path is not in that commit) or was killed
    /// (the 2 s deadline or the 1 MiB output cap): the snippet source is skipped (CG8.2).
    #[error("git show failed or was bounded")]
    ShowBounded,
}

/// The cause of a git refusal in plain words: the inherited descriptor is this process's
/// (the message says whose), the other causes keep their class text.
fn git_cause(e: &ProcessError) -> String {
    match e {
        ProcessError::InheritedFd { .. } => format!("the kaban-index process {e}"),
        ProcessError::Spawn(_) | ProcessError::Env(_) => e.to_string(),
    }
}

impl IdentityCliError {
    /// The exit code (§7): every refusal of this module is 4.
    #[must_use]
    pub fn exit_code(&self) -> u8 {
        4
    }
}

/// What `resolve` produced: the identity, the handle and the two canonical paths. The paths are
/// live values read at command time: shown only after the gate, never persisted.
#[derive(Debug)]
pub struct Resolved {
    /// The bound identity.
    pub identity: RepoIdentity,
    /// The common directory handle.
    pub handle: CommonDirHandle,
    /// The checkout directory handle (the symlink-refusing walk of the canonical repository
    /// path, U2 RU2-5): retained to the spawn, where the path must still lead to this object.
    pub repo_handle: DirHandle,
    /// The canonical common directory as a string (the `repo_id` input, gated first).
    pub canonical_common_dir: String,
    /// The canonical repository path.
    pub canonical_repo: PathBuf,
}

/// Deadline of one git call.
const GIT_DEADLINE: Duration = Duration::from_secs(10);

/// The fixed git candidates (U2 RU2-4; the S5.2 handle rule, as the `unshare` wrapper): never
/// the caller's `PATH`.
pub const GIT_CANDIDATES: [&str; 3] = ["/usr/bin/git", "/bin/git", "/usr/local/bin/git"];

/// The configuration every query carries on the command line (it wins over every file): no
/// fsmonitor command, no hooks, no signature verification by an external program. The
/// commands this crate runs (`rev-parse`, `rev-list`, `status --porcelain`, `log --no-walk`,
/// `show <sha>:<path>` for a blob, which streams the object raw and applies no textconv unless
/// asked on the command line) execute nothing else that a repository could configure.
pub const GIT_OVERRIDES: [&str; 6] = [
    "-c",
    "core.fsmonitor=false",
    "-c",
    "core.hooksPath=/dev/null",
    "-c",
    "log.showSignature=false",
];

/// The pinned git handle: the first candidate that exists decides (a refused one is the
/// error, as for the wrappers).
#[cfg(not(target_os = "macos"))]
fn trusted_git() -> Result<Exec, IdentityCliError> {
    let candidates: Vec<PathBuf> = GIT_CANDIDATES.iter().map(PathBuf::from).collect();
    let refs: Vec<&Path> = candidates.iter().map(PathBuf::as_path).collect();
    crate::isolate::wrapper::resolve_wrapper(&refs, None)
        .map(|w| w.exec)
        .map_err(|_| IdentityCliError::GitUntrusted)
}

/// Darwin uses the fixed candidates with native admission and pre-spawn revalidation.
/// Never consult the caller's `PATH`.
#[cfg(target_os = "macos")]
fn trusted_git() -> Result<Exec, IdentityCliError> {
    for candidate in GIT_CANDIDATES {
        let path = Path::new(candidate);
        if !path
            .try_exists()
            .map_err(|_| IdentityCliError::GitUntrusted)?
        {
            continue;
        }
        let tool = crate::tools::admit_executable(path, &[])
            .map_err(|_| IdentityCliError::GitUntrusted)?;
        crate::tools::recheck(&tool).map_err(|_| IdentityCliError::GitUntrusted)?;
        return Ok(Exec::Path(tool.path));
    }
    Err(IdentityCliError::GitUntrusted)
}

/// Run `git <args>` in `cwd` through the runner with the production descriptor policy
/// ([`InheritedFds::Refuse`]): the pinned executable, [`GIT_OVERRIDES`], the
/// `runner::git_query_env` environment (no `PATH`, no `HOME`), stdin null, 1 MiB caps. Shared
/// with the `index` preconditions and `gc` (CG4.6): the one way this crate runs git.
///
/// # Errors
/// [`IdentityCliError::GitUntrusted`], [`IdentityCliError::Git`],
/// [`IdentityCliError::NotARepository`] (non-zero exit or killed), [`IdentityCliError::GitOutput`].
pub fn git(cwd: &Path, args: &[&str]) -> Result<String, IdentityCliError> {
    git_with(cwd, args, InheritedFds::Refuse)
}

/// [`git`] with an explicit descriptor policy: a library test launched under `flock(1)` passes
/// [`InheritedFds::Tolerate`]; production callers use [`git`].
///
/// # Errors
/// As [`git`].
pub fn git_with(
    cwd: &Path,
    args: &[&str],
    inherited_fds: InheritedFds,
) -> Result<String, IdentityCliError> {
    let exec = trusted_git()?;
    let env = crate::runner::git_query_env().map_err(IdentityCliError::Git)?;
    let mut command_line: Vec<OsString> = GIT_OVERRIDES.iter().map(OsString::from).collect();
    command_line.extend(args.iter().map(OsString::from));
    let outcome = kaban_core::process::run(Spec {
        exec,
        args: command_line,
        cwd,
        env,
        deadline: GIT_DEADLINE,
        stdout_cap: 1 << 20,
        stderr_cap: 1 << 20,
        monitors: Vec::new(),
        join_bound: Duration::from_secs(2),
        inherited_fds,
    })
    .map_err(IdentityCliError::Git)?;
    if outcome.killed.is_some() || !outcome.status.is_some_and(|s| s.success()) {
        return Err(IdentityCliError::NotARepository);
    }
    String::from_utf8(outcome.stdout).map_err(|_| IdentityCliError::GitOutput)
}

/// Deadline of one `git show <sha>:<path>` (CG8.2): a blob that cannot be streamed in this time
/// is skipped, the run continues.
const GIT_SHOW_DEADLINE: Duration = Duration::from_secs(2);

/// The bytes of the blob at `path` in commit `sha` (`git show <sha>:<path>`, G1-F17, G1-F7:
/// never the working tree) through the same pinned git, overrides, environment and descriptor
/// policy as [`git`], with a 2 s deadline and a 1 MiB stdout cap. Bytes come back as they are:
/// the caller slices and decides about UTF-8 (`snippets::collect`).
///
/// # Errors
/// [`IdentityCliError::GitUntrusted`], [`IdentityCliError::Git`],
/// [`IdentityCliError::ShowBounded`] (non-zero exit, the deadline or the cap).
pub fn git_show(repo: &Path, sha: &str, path: &str) -> Result<Vec<u8>, IdentityCliError> {
    git_show_with(repo, sha, path, InheritedFds::Refuse)
}

/// [`git_show`] with an explicit descriptor policy (as [`git_with`]: an in-process test under
/// `cargo-slot.sh` or `flock(1)` passes [`InheritedFds::Tolerate`]; production uses
/// [`git_show`]).
///
/// # Errors
/// As [`git_show`].
pub fn git_show_with(
    repo: &Path,
    sha: &str,
    path: &str,
    inherited_fds: InheritedFds,
) -> Result<Vec<u8>, IdentityCliError> {
    let exec = trusted_git()?;
    let env = crate::runner::git_query_env().map_err(IdentityCliError::Git)?;
    let mut command_line: Vec<OsString> = GIT_OVERRIDES.iter().map(OsString::from).collect();
    command_line.push(OsString::from("show"));
    command_line.push(OsString::from(format!("{sha}:{path}")));
    let outcome = kaban_core::process::run(Spec {
        exec,
        args: command_line,
        cwd: repo,
        env,
        deadline: GIT_SHOW_DEADLINE,
        stdout_cap: 1 << 20,
        stderr_cap: 1 << 20,
        monitors: Vec::new(),
        join_bound: Duration::from_secs(2),
        inherited_fds,
    })
    .map_err(IdentityCliError::Git)?;
    if outcome.killed.is_some() || !outcome.status.is_some_and(|s| s.success()) {
        return Err(IdentityCliError::ShowBounded);
    }
    Ok(outcome.stdout)
}

/// Resolve the identity of the repository at `repo` (G1-S6): canonical walk, common directory
/// through git, `(dev, ino, btime)` of the handle, sorted root commits, worktree reciprocity.
///
/// # Errors
/// The classes of [`IdentityCliError`]; `NoCommits` surfaces as
/// `Identity(IdentityError::NoCommits)` with the fixed text "repository has no commits".
pub fn resolve(repo: &Path) -> Result<Resolved, IdentityCliError> {
    resolve_with(repo, InheritedFds::Refuse)
}

/// [`resolve`] with an explicit descriptor policy for its git calls (see [`git_with`]).
///
/// # Errors
/// As [`resolve`].
pub fn resolve_with(
    repo: &Path,
    inherited_fds: InheritedFds,
) -> Result<Resolved, IdentityCliError> {
    let canonical_repo = std::fs::canonicalize(repo).map_err(IdentityError::from)?;
    let repo_handle = identity::walk(&canonical_repo)?;
    let common_out = git_with(
        &canonical_repo,
        &["rev-parse", "--git-common-dir"],
        inherited_fds,
    )?;
    let (handle, canonical_common) = identity::resolve_common_dir(&canonical_repo, &common_out)?;
    let roots_out = git_with(
        &canonical_repo,
        &["rev-list", "--max-parents=0", "HEAD"],
        inherited_fds,
    )?;
    let root_commits = identity::root_commits(&roots_out)?;
    identity::worktree_reciprocal(&canonical_repo, &handle)?;
    let fs = identity::fs_identity(&handle)?;
    let canonical_common_dir = canonical_common
        .to_str()
        .ok_or(IdentityCliError::GitOutput)?
        .to_owned();
    Ok(Resolved {
        identity: RepoIdentity { fs, root_commits },
        handle,
        repo_handle,
        canonical_common_dir,
        canonical_repo,
    })
}

/// `$KABAN_HOME` or `~/.kaban`.
///
/// # Errors
/// [`IdentityCliError::NoHome`].
pub fn kaban_home() -> Result<PathBuf, IdentityCliError> {
    if let Some(h) = std::env::var_os("KABAN_HOME") {
        return Ok(PathBuf::from(h));
    }
    std::env::var_os("HOME")
        .map(|h| PathBuf::from(h).join(".kaban"))
        .ok_or(IdentityCliError::NoHome)
}

/// The dedicated key `<home>/keys/codegraph.key` (OD-G1-2): created 0600 `O_EXCL` when absent.
///
/// # Errors
/// [`IdentityCliError::Key`].
pub fn open_key(home: &Path) -> Result<RepoIdKey, IdentityCliError> {
    let key = kaban_policy::open_or_create_key(&home.join("keys"), "codegraph")
        .map_err(|_| IdentityCliError::Key)?;
    Ok(RepoIdKey::new(*key))
}

/// Gate the canonical common directory, then derive `<repo-id>` (§4.1): any hit, reject or
/// redact class, is `repo_path_refused` and nothing is derived (RU2-15, OWN-75: a path kaban
/// would have to mask is not a path it keys on).
///
/// # Errors
/// [`IdentityCliError::RepoPathRefused`].
pub fn repo_id_gated(
    policy: &Policy,
    key: &RepoIdKey,
    canonical_common_dir: &str,
) -> Result<String, IdentityCliError> {
    let v = policy.scan(canonical_common_dir);
    if v.rejected || !v.hits.is_empty() {
        return Err(IdentityCliError::RepoPathRefused);
    }
    Ok(identity::repo_id(key, canonical_common_dir))
}

/// The live path as `trust`, `status` and `doctor` show it: scanned, redact hits masked, a
/// reject hit replaced by the fixed marker. Never persisted.
#[must_use]
pub fn display_path(policy: &Policy, path: &Path) -> String {
    let text = path.to_string_lossy();
    let verdict = policy.scan(&text);
    if verdict.rejected {
        "[removed: repo_path]".to_owned()
    } else if verdict.redacted.is_empty() {
        text.into_owned()
    } else {
        verdict.redacted
    }
}

/// The admitted per-repository data directory: a walked handle plus its path (for messages
/// and for the names of files opened through the handle).
#[derive(Debug)]
pub struct AdmittedDir {
    /// The validated handle every open goes through.
    pub handle: DirHandle,
    /// `<home>/codegraph/<repo-id>`, canonical.
    pub path: PathBuf,
}

/// Create (0700) when missing and admit `<home>/codegraph/<repo-id>`: canonicalised, re-walked
/// component by component (a symlink anywhere refuses), owned by the invoking user, mode 0700.
///
/// # Errors
/// [`IdentityCliError::DataDir`] with `symlink`, `not_owner`, `mode`, `create`; the walk classes.
pub fn admit_repo_dir(home: &Path, repo_id: &str) -> Result<AdmittedDir, IdentityCliError> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    let dir = home.join("codegraph").join(repo_id);
    // the parents must not be symlinks either: refuse before creating anything under them
    for ancestor in dir.ancestors().skip(1) {
        if ancestor
            .symlink_metadata()
            .is_ok_and(|m| m.file_type().is_symlink())
        {
            return Err(IdentityCliError::DataDir("symlink"));
        }
    }
    if dir
        .symlink_metadata()
        .is_ok_and(|m| m.file_type().is_symlink())
    {
        return Err(IdentityCliError::DataDir("symlink"));
    }
    if !dir.exists() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)
            .map_err(|_| IdentityCliError::DataDir("create"))?;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|_| IdentityCliError::DataDir("create"))?;
    }
    let canonical = std::fs::canonicalize(&dir).map_err(IdentityError::from)?;
    if canonical != dir {
        return Err(IdentityCliError::DataDir("symlink"));
    }
    let handle = identity::walk(&canonical)?;
    let (uid, mode) = handle.owner_and_mode()?;
    if uid != rustix::process::geteuid().as_raw() {
        return Err(IdentityCliError::DataDir("not_owner"));
    }
    if mode & 0o077 != 0 {
        return Err(IdentityCliError::DataDir("mode"));
    }
    Ok(AdmittedDir {
        handle,
        path: canonical,
    })
}
