//! The `serve`-side wiring of the code-graph snapshot provider (stage1-plan CG5.2; stage1-spec
//! G1-F8 serve side, G1-S7; cg-join-v1 §1, §4): read `<KABAN_HOME>/keys/codegraph.key`,
//! derive `<repo-id>` from the gated canonical common directory with the same key and rule as
//! `kaban-index` (CG4.1 `identity`), and hand the service a `DiscoverProvider` over
//! `<KABAN_HOME>/codegraph/<repo-id>`. Reads only: nothing here creates the key, the directory
//! or any file; the one subprocess is `git rev-parse --git-common-dir` through [`GitRunner`].
//!
//! ```text
//!   keys/codegraph.key: absent --> NoKey (no provider)
//!     one open O_NOFOLLOW|O_NONBLOCK, fstat on the descriptor (identity::open_private_file):
//!     symlink | not a regular file | owner or mode & 0o077 | read of 33 bytes != 32 --> KeyRefused (exit 4)
//!   repo_root: none --> NoRepo
//!   git rev-parse --git-common-dir ==> resolve_common_dir (handle walk, G1-S6) --fail--> NoRepo
//!   policy.scan(canonical): any hit --> Refused (reject or redact class: RU2-15, OWN-75)
//!   repo_id(key, canonical) ==> absolute(<home>/codegraph/<repo-id>) ==> DiscoverProvider --> Ready
//! ```
//!
//! The outcome is recorded as this process's [`State`] and announced by `serve` alone
//! ([`announce`], one `codegraph provider` line at `info` beside `kaban ready`): a CLI
//! command's stderr is its fixed text only, whatever `RUST_LOG` says (core S-series fixed-error
//! contract; CG5.2a). The line carries the state only: no path, no id, no key.

use std::fmt;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use kaban_api::GitRunner;
use kaban_codegraph::identity::{PrivateFileError, open_private_file};
use kaban_codegraph::{DiscoverProvider, RepoIdKey, SnapshotProvider, repo_id, resolve_common_dir};
use zeroize::Zeroizing;

/// The key file name under `<KABAN_HOME>/keys` (OD-G1-2; `kaban-index` creates it, `serve`
/// only reads it).
const KEY_NAME: &str = "codegraph.key";

/// The key length `kaban-index` writes.
const KEY_LEN: usize = 32;

/// The read bound of the key file: `KEY_LEN + 1`, one byte more than the key tells an
/// oversize file apart without reading it (RU3-4).
const KEY_READ_CAP: u64 = 33;

/// The provider's startup state, one per process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum State {
    /// A provider over `<home>/codegraph/<repo-id>`.
    Ready,
    /// No key file: no provider, nothing created.
    NoKey,
    /// No enclosing repository, or its common directory could not be resolved.
    NoRepo,
    /// The canonical common directory was rejected by the content gate.
    Refused,
}

impl State {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::NoKey => "no_key",
            Self::NoRepo => "no_repo",
            Self::Refused => "refused",
        }
    }
}

/// The state [`wire`] reached in this process (set once, by the first `open_api`).
static STATE: OnceLock<State> = OnceLock::new();

/// Log the state once for `serve` (`kaban ready`'s neighbour); a no-op before [`wire`] ran.
pub(crate) fn announce() {
    if let Some(state) = STATE.get() {
        tracing::info!(state = state.as_str(), "codegraph provider");
    }
}

/// What `open_api` hands `KabanApi::with_snapshot_provider`.
pub(crate) struct Wiring {
    /// `<KABAN_HOME>/codegraph/<repo-id>`; need not exist yet.
    pub(crate) dir: PathBuf,
    /// The provider over `dir`.
    pub(crate) provider: Arc<dyn SnapshotProvider>,
}

impl fmt::Debug for Wiring {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Wiring")
            .field("dir", &self.dir)
            .finish_non_exhaustive()
    }
}

/// Why the key file was refused (stage1-spec S5, as the audit key): the class only, never the
/// path. A config error (exit 4) at startup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KeyRefused {
    /// The path is a symbolic link.
    Symlink,
    /// Not a regular file.
    NotRegularFile,
    /// Readable by group or others.
    Mode,
    /// Not exactly 32 bytes.
    Length,
    /// The file exists but could not be inspected or read.
    Io,
}

impl KeyRefused {
    const fn class(self) -> &'static str {
        match self {
            Self::Symlink => "symlink",
            Self::NotRegularFile => "not_regular_file",
            Self::Mode => "mode",
            Self::Length => "length",
            Self::Io => "io",
        }
    }
}

impl fmt::Display for KeyRefused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "codegraph key refused ({})", self.class())
    }
}

impl std::error::Error for KeyRefused {}

/// Build the provider for this process, or `None`, and record the [`State`] for
/// [`announce`]. Nothing is logged here (CLI commands share this path).
///
/// # Errors
/// [`KeyRefused`] when a key file exists but is not a private 32-byte regular file.
pub(crate) fn wire(
    home: &Path,
    repo_root: Option<&Path>,
    policy: &kaban_policy::Policy,
    git: &GitRunner,
) -> Result<Option<Wiring>, KeyRefused> {
    let (state, wiring) = build(home, repo_root, policy, git)?;
    let _ = STATE.set(state);
    Ok(wiring)
}

fn build(
    home: &Path,
    repo_root: Option<&Path>,
    policy: &kaban_policy::Policy,
    git: &GitRunner,
) -> Result<(State, Option<Wiring>), KeyRefused> {
    let Some(key) = read_key(&home.join("keys").join(KEY_NAME))? else {
        return Ok((State::NoKey, None));
    };
    let Some(root) = repo_root else {
        return Ok((State::NoRepo, None));
    };
    // a git or walk failure is `NoRepo`; its class is not logged (a CLI command's stderr is
    // its fixed text only), `serve` announces the state
    let Ok(canonical) = common_dir(root, git) else {
        return Ok((State::NoRepo, None));
    };
    // the content gate on the one path-derived input (§4.1, as `kaban-index` gates it): any
    // hit, reject or redact class, derives nothing (RU2-15, OWN-75)
    let v = policy.scan(&canonical);
    if v.rejected || !v.hits.is_empty() {
        return Ok((State::Refused, None));
    }
    // the provider walks its directory component by component from `/` (RU3-3), so the data
    // home is made absolute here (no symlink following) and a relative `KABAN_HOME` still works
    let dir = home.join("codegraph").join(repo_id(&key, &canonical));
    let dir = std::path::absolute(&dir).unwrap_or(dir);
    let provider: Arc<dyn SnapshotProvider> = Arc::new(DiscoverProvider::new(dir.clone()));
    Ok((State::Ready, Some(Wiring { dir, provider })))
}

/// The key at `path`: `None` when absent, the key when it is a private regular file of 32
/// bytes, refused otherwise. One admitted open (`O_NOFOLLOW|O_NONBLOCK`, the checks on the
/// descriptor: [`open_private_file`], RU3-4), then at most [`KEY_READ_CAP`] bytes read into
/// zeroizing memory and moved into the zero-on-drop [`RepoIdKey`]; an owner mismatch is the
/// `mode` class.
fn read_key(path: &Path) -> Result<Option<RepoIdKey>, KeyRefused> {
    let file = match open_private_file(path) {
        Ok(file) => file,
        Err(PrivateFileError::NotFound) => return Ok(None),
        Err(PrivateFileError::Symlink) => return Err(KeyRefused::Symlink),
        Err(PrivateFileError::NotRegularFile) => return Err(KeyRefused::NotRegularFile),
        Err(PrivateFileError::NotOwner | PrivateFileError::Mode) => return Err(KeyRefused::Mode),
        Err(_) => return Err(KeyRefused::Io),
    };
    let mut bytes = Zeroizing::new(Vec::new());
    file.take(KEY_READ_CAP)
        .read_to_end(&mut bytes)
        .map_err(|_| KeyRefused::Io)?;
    if bytes.len() != KEY_LEN {
        return Err(KeyRefused::Length);
    }
    let mut key = Zeroizing::new([0u8; KEY_LEN]);
    key.copy_from_slice(&bytes);
    Ok(Some(RepoIdKey::new(*key)))
}

/// The canonical common directory of the repository at `root` as a gated string: one `git
/// rev-parse --git-common-dir`, then the symlink-refusing handle walk of G1-S6. The error is
/// the failure class (kept for a future `doctor` line; not logged).
fn common_dir(root: &Path, git: &GitRunner) -> Result<String, &'static str> {
    let out = git
        .run(root, &["rev-parse", "--git-common-dir"])
        .map_err(|e| match e {
            kaban_api::GitError::Spawn(_) => "git_spawn",
            kaban_api::GitError::Timeout(_) => "git_timeout",
            kaban_api::GitError::Status(_) => "git_status",
            kaban_api::GitError::Output => "git_output",
        })?;
    let (_handle, canonical) = resolve_common_dir(root, &out).map_err(|_| "common_dir")?;
    canonical
        .to_str()
        .map(str::to_owned)
        .ok_or("common_dir_not_utf8")
}
