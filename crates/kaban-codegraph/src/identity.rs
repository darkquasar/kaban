//! Repository identity resolution (stage1-spec G1-S6, §4.1; stage1-plan CG4.1; RS2-4, RS3-3).
//!
//! Pure filesystem code shared by `kaban-index` and `kaban serve` (OD-G1-2): this module spawns
//! nothing and reads no key file. Git outputs (`git rev-parse --git-common-dir`,
//! `git rev-list --max-parents=0 HEAD`) and the 32 key bytes are passed in by the caller.
//!
//! ```text
//!   caller: realpath(repo) ==> walk(canonical)  component by component,
//!                              openat(O_PATH|O_DIRECTORY|O_NOFOLLOW): a symlink is a race
//!   resolve_common_dir(repo, git output) ==> (CommonDirHandle, canonical common dir)
//!   fs_identity(&handle) ==> FsIdentity { dev, ino, btime }   (fstat + statx on the handle)
//!   root_commits(rev-list output) ==> sorted sha40 list
//!   repo_id(&RepoIdKey, gated canonical dir) ==> 32 hex   (HMAC-SHA256, "kaban-repo-id/v1\0")
//!   worktree_reciprocal(worktree, &handle) ==> both directions of a linked worktree agree
//! ```
//!
//! Every error is a fixed class: no path, no git output and no key byte ever appears in a text
//! (`obs-no-sensitive-data`, G1-S8).

use std::fmt;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::{Component, Path, PathBuf};

use hmac::{Hmac, Mac};
use rustix::fs::{AtFlags, Mode, OFlags};
use rustix::io::Errno;
use sha2::Sha256;
use thiserror::Error;
use zeroize::Zeroize;

#[cfg_attr(target_os = "macos", path = "identity/darwin.rs")]
#[cfg_attr(not(target_os = "macos"), path = "identity/linux.rs")]
mod platform;

use platform::WALK_FLAGS;

/// Why a repository identity could not be resolved. Classes only.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum IdentityError {
    /// The path handed to the walk is not absolute (the caller must canonicalise first).
    #[error("path not absolute")]
    NotAbsolute,
    /// A component of the canonical path is a symlink: a race after canonicalisation.
    #[error("symlink component after canonicalisation")]
    SymlinkComponent,
    /// A component is not a directory.
    #[error("path component not a directory")]
    NotDirectory,
    /// A component does not exist.
    #[error("path component not found")]
    NotFound,
    /// Another filesystem failure, by errno class.
    #[error("filesystem failure ({0})")]
    Io(&'static str),
    /// `git rev-parse --git-common-dir` printed nothing usable.
    #[error("common directory output malformed")]
    CommonDirOutput,
    /// The repository has no commit (`git rev-list --max-parents=0 HEAD` printed nothing).
    #[error("repository has no commits")]
    NoCommits,
    /// A root commit line is not forty lowercase hex digits.
    #[error("root commit malformed")]
    MalformedCommit,
    /// The worktree's `.git` and the common directory's `worktrees/<name>/gitdir` disagree.
    #[error("worktree reciprocity failed")]
    WorktreeForged,
}

impl From<Errno> for IdentityError {
    fn from(e: Errno) -> Self {
        match e {
            Errno::LOOP => Self::SymlinkComponent,
            Errno::NOTDIR => Self::NotDirectory,
            Errno::NOENT => Self::NotFound,
            Errno::ACCESS | Errno::PERM => Self::Io("permission_denied"),
            _ => Self::Io("other"),
        }
    }
}

impl From<std::io::Error> for IdentityError {
    fn from(e: std::io::Error) -> Self {
        match e.kind() {
            std::io::ErrorKind::NotFound => Self::NotFound,
            std::io::ErrorKind::NotADirectory => Self::NotDirectory,
            std::io::ErrorKind::PermissionDenied => Self::Io("permission_denied"),
            _ => Self::Io("other"),
        }
    }
}

/// A no-follow directory handle reached by the component walk (`O_PATH` on Linux,
/// read-only on Darwin).
/// Everything that opens inside the directory goes through this handle (`openat`), never
/// through the path again.
#[derive(Debug)]
pub struct DirHandle(OwnedFd);

impl DirHandle {
    /// The `(dev, ino)` of the directory object (`fstat` on the handle).
    ///
    /// # Errors
    /// [`IdentityError::Io`] when `fstat` fails.
    pub fn dev_ino(&self) -> Result<(u64, u64), IdentityError> {
        let st = rustix::fs::fstat(&self.0)?;
        Ok((platform::device(&st), st.st_ino))
    }

    /// Open the child directory `name` (one component, no slash) through this handle with the
    /// same flags; a symlink or a non-directory is refused.
    ///
    /// # Errors
    /// The walk classes of [`IdentityError`].
    pub fn child(&self, name: &std::ffi::OsStr) -> Result<Self, IdentityError> {
        match rustix::fs::openat(&self.0, name, WALK_FLAGS, Mode::empty()) {
            Ok(fd) => Ok(Self(fd)),
            // `O_PATH|O_NOFOLLOW` opens a symlink itself and `O_DIRECTORY` then fails with
            // ENOTDIR: tell the race apart from a plain file without following anything
            Err(Errno::NOTDIR) => {
                let st = rustix::fs::statat(&self.0, name, AtFlags::SYMLINK_NOFOLLOW)?;
                if rustix::fs::FileType::from_raw_mode(st.st_mode) == rustix::fs::FileType::Symlink
                {
                    Err(IdentityError::SymlinkComponent)
                } else {
                    Err(IdentityError::NotDirectory)
                }
            }
            Err(e) => Err(e.into()),
        }
    }

    /// The parent directory (`..`) through this handle.
    ///
    /// # Errors
    /// The walk classes of [`IdentityError`].
    pub fn parent(&self) -> Result<Self, IdentityError> {
        Ok(Self(rustix::fs::openat(
            &self.0,
            "..",
            WALK_FLAGS,
            Mode::empty(),
        )?))
    }

    /// Owner uid and permission bits of the directory (for the admission of a data directory:
    /// owned by the user, `0700`).
    ///
    /// # Errors
    /// [`IdentityError::Io`] when `fstat` fails.
    pub fn owner_and_mode(&self) -> Result<(u32, u32), IdentityError> {
        let st = rustix::fs::fstat(&self.0)?;
        Ok((st.st_uid, platform::mode(&st) & 0o7777))
    }
}

impl AsFd for DirHandle {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

/// Re-walk an already canonical absolute path component by component with
/// no-follow directory opens: a component that is a symlink now is a race after
/// canonicalisation and refuses with [`IdentityError::SymlinkComponent`].
///
/// # Errors
/// [`IdentityError::NotAbsolute`], the walk classes.
pub fn walk(canonical: &Path) -> Result<DirHandle, IdentityError> {
    let mut components = canonical.components();
    if components.next() != Some(Component::RootDir) {
        return Err(IdentityError::NotAbsolute);
    }
    let mut handle = DirHandle(rustix::fs::open("/", WALK_FLAGS, Mode::empty())?);
    for component in components {
        match component {
            Component::Normal(name) => handle = handle.child(name)?,
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(IdentityError::NotAbsolute);
            }
        }
    }
    Ok(handle)
}

/// Why a private file was not admitted by [`open_private_file`]. Classes only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum PrivateFileError {
    /// No object at the path.
    #[error("file not found")]
    NotFound,
    /// The path names a symbolic link (`O_NOFOLLOW` refused it).
    #[error("file is a symbolic link")]
    Symlink,
    /// The object is not a regular file (a FIFO, a directory, a device, a socket).
    #[error("not a regular file")]
    NotRegularFile,
    /// The file is owned by another user.
    #[error("file not owned by the caller")]
    NotOwner,
    /// A group or other permission bit is set.
    #[error("file mode refused")]
    Mode,
    /// Another filesystem failure, by errno class.
    #[error("filesystem failure ({0})")]
    Io(&'static str),
}

const PRIVATE_FILE_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::NOFOLLOW)
    .union(OFlags::NONBLOCK)
    .union(OFlags::CLOEXEC);

/// One admitted open of a private regular file (the key file `kaban serve` reads; RU3-4):
/// `open(O_RDONLY|O_NOFOLLOW|O_NONBLOCK|O_CLOEXEC)` by path, then `fstat` on the descriptor:
/// a regular file, owned by the effective uid, with no group or other permission bit. The
/// caller reads the returned `File`, so the object the checks saw is the object read; a
/// replacement after the open reaches nothing. `O_NONBLOCK` makes a FIFO's open return at once
/// (the `fstat` then refuses it); it changes nothing for the admitted case, since a regular
/// file never blocks a read.
///
/// # Errors
/// [`PrivateFileError`] by class; the path is never part of the error.
pub fn open_private_file(path: &Path) -> Result<std::fs::File, PrivateFileError> {
    let fd = match rustix::fs::open(path, PRIVATE_FILE_FLAGS, Mode::empty()) {
        Ok(fd) => fd,
        Err(Errno::NOENT) => return Err(PrivateFileError::NotFound),
        Err(Errno::LOOP) => return Err(PrivateFileError::Symlink),
        Err(Errno::NXIO) => return Err(PrivateFileError::NotRegularFile),
        Err(Errno::ACCESS | Errno::PERM) => return Err(PrivateFileError::Io("permission_denied")),
        Err(_) => return Err(PrivateFileError::Io("other")),
    };
    let st = rustix::fs::fstat(&fd).map_err(|_| PrivateFileError::Io("other"))?;
    if !rustix::fs::FileType::from_raw_mode(st.st_mode).is_file() {
        return Err(PrivateFileError::NotRegularFile);
    }
    if st.st_uid != rustix::process::geteuid().as_raw() {
        return Err(PrivateFileError::NotOwner);
    }
    if st.st_mode & 0o077 != 0 {
        return Err(PrivateFileError::Mode);
    }
    Ok(std::fs::File::from(fd))
}

/// The handle of the canonical common directory of a repository.
#[derive(Debug)]
pub struct CommonDirHandle(DirHandle);

impl CommonDirHandle {
    /// The underlying walked handle.
    #[must_use]
    pub fn dir(&self) -> &DirHandle {
        &self.0
    }
}

/// Canonicalise `repo` once (`realpath`), re-walk it, interpret the trimmed output of
/// `git rev-parse --git-common-dir` (absolute, or relative to the canonical repository),
/// canonicalise and re-walk that too. Returns the handle and the canonical common directory
/// path the caller must gate before deriving anything from it (§4.1).
///
/// # Errors
/// [`IdentityError::CommonDirOutput`] on an empty output, the walk classes.
pub fn resolve_common_dir(
    repo: &Path,
    git_common_dir_output: &str,
) -> Result<(CommonDirHandle, PathBuf), IdentityError> {
    let canonical_repo = std::fs::canonicalize(repo)?;
    let _repo_handle = walk(&canonical_repo)?;
    let line = git_common_dir_output
        .lines()
        .next()
        .map(str::trim)
        .filter(|s| !s.is_empty() && !s.contains('\0'))
        .ok_or(IdentityError::CommonDirOutput)?;
    let common = canonical_repo.join(line);
    let canonical_common = std::fs::canonicalize(common)?;
    let handle = walk(&canonical_common)?;
    Ok((CommonDirHandle(handle), canonical_common))
}

/// The filesystem identity of a directory object: device, inode and birth time where the
/// filesystem reports one (Linux `statx STATX_BTIME` or Darwin native birthtime;
/// compared only when both sides have it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FsIdentity {
    /// `st_dev`.
    pub dev: u64,
    /// `st_ino`.
    pub ino: u64,
    /// Birth time in nanoseconds since the epoch, `None` when the filesystem has none.
    pub btime: Option<i128>,
}

impl FsIdentity {
    /// Whether two identities name the same directory object: `dev` and `ino` equal, and the
    /// birth time equal when both sides have one (a side without one is not compared).
    #[must_use]
    pub fn matches(&self, other: &Self) -> bool {
        self.dev == other.dev
            && self.ino == other.ino
            && match (self.btime, other.btime) {
                (Some(a), Some(b)) => a == b,
                _ => true,
            }
    }
}

/// Descriptor-bound metadata, never a path lookup (`fstat`, plus Linux `statx`).
///
/// # Errors
/// [`IdentityError::Io`] when identity metadata fails (unavailable birth time is `None`).
pub fn fs_identity(h: &CommonDirHandle) -> Result<FsIdentity, IdentityError> {
    let (dev, ino) = h.0.dev_ino()?;
    let btime = platform::birthtime(&h.0);
    Ok(FsIdentity { dev, ino, btime })
}

/// The identity the trust mark binds: the common directory object and the sorted root commits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoIdentity {
    /// The common directory object.
    pub fs: FsIdentity,
    /// Every root commit (`git rev-list --max-parents=0 HEAD`), forty lowercase hex, sorted.
    pub root_commits: Vec<String>,
}

/// Parse the output of `git rev-list --max-parents=0 HEAD`: one forty-hex line per root,
/// returned sorted and de-duplicated.
///
/// # Errors
/// [`IdentityError::NoCommits`] on no line, [`IdentityError::MalformedCommit`] on a line that
/// is not forty lowercase hex digits.
pub fn root_commits(rev_list_output: &str) -> Result<Vec<String>, IdentityError> {
    let mut roots: Vec<String> = rev_list_output
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|l| {
            if l.len() == 40
                && l.bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            {
                Ok(l.to_owned())
            } else {
                Err(IdentityError::MalformedCommit)
            }
        })
        .collect::<Result<_, _>>()?;
    if roots.is_empty() {
        return Err(IdentityError::NoCommits);
    }
    roots.sort();
    roots.dedup();
    Ok(roots)
}

/// The 32 key bytes of `~/.kaban/keys/codegraph.key`, read by the binaries and handed in here;
/// zeroed on drop; `Debug` prints no byte.
pub struct RepoIdKey([u8; 32]);

impl RepoIdKey {
    /// Wrap the key bytes the caller read.
    #[must_use]
    pub fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
}

impl Drop for RepoIdKey {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl fmt::Debug for RepoIdKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RepoIdKey([redacted])")
    }
}

/// `hmac_hex(key, "kaban-repo-id/v1\0" || canonical_common_dir)[0..32]` (§4.1, RS3-3): a
/// storage key, never an authorisation. The caller passes the path only after it passed the
/// content gate.
#[must_use]
pub fn repo_id(key: &RepoIdKey, gated_canonical_common_dir: &str) -> String {
    // Invariant: HMAC accepts keys of any length, so new_from_slice cannot fail.
    #[allow(clippy::expect_used)]
    let mut mac = <Hmac<Sha256>>::new_from_slice(&key.0).expect("hmac accepts any key length");
    mac.update(b"kaban-repo-id/v1\0");
    mac.update(gated_canonical_common_dir.as_bytes());
    let bytes = mac.finalize().into_bytes();
    let mut hex = String::with_capacity(32);
    for b in &bytes[..16] {
        use fmt::Write;
        // Invariant: writing two hex digits into a String cannot fail.
        let _ = write!(hex, "{b:02x}");
    }
    hex
}

/// Reciprocal validation of the directory being indexed against the common directory handle
/// (G1-S6). Main worktree: `<worktree>/.git` must be the common directory object itself.
/// Linked worktree: `<worktree>/.git` is a `gitdir: <path>` file whose target walks to a
/// directory under `<common>/worktrees/`, and that directory's `gitdir` file must walk back to
/// the `(dev, ino)` of `<worktree>`; either direction failing refuses.
///
/// # Errors
/// [`IdentityError::WorktreeForged`] on any disagreement, the walk classes.
pub fn worktree_reciprocal(worktree: &Path, h: &CommonDirHandle) -> Result<(), IdentityError> {
    let canonical = std::fs::canonicalize(worktree)?;
    let worktree_handle = walk(&canonical)?;
    let dot_git = canonical.join(".git");
    let meta = std::fs::symlink_metadata(&dot_git)?;
    let common_id = h.0.dev_ino()?;
    if meta.is_dir() {
        // main worktree: `.git` is the common directory
        return if walk(&dot_git)?.dev_ino()? == common_id {
            Ok(())
        } else {
            Err(IdentityError::WorktreeForged)
        };
    }
    if !meta.is_file() {
        return Err(IdentityError::WorktreeForged);
    }
    let text = std::fs::read_to_string(&dot_git)?;
    let gitdir = text
        .lines()
        .next()
        .and_then(|l| l.strip_prefix("gitdir:"))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or(IdentityError::WorktreeForged)?;
    let gitdir =
        std::fs::canonicalize(canonical.join(gitdir)).map_err(|_| IdentityError::WorktreeForged)?;
    let gitdir_handle = walk(&gitdir)?;
    // direction one: the gitdir sits directly under `<common>/worktrees`
    let worktrees =
        h.0.child(std::ffi::OsStr::new("worktrees"))
            .map_err(|_| IdentityError::WorktreeForged)?;
    if gitdir_handle.parent()?.dev_ino()? != worktrees.dev_ino()? {
        return Err(IdentityError::WorktreeForged);
    }
    // direction two: `<gitdir>/gitdir` names `<worktree>/.git`; its directory is the worktree
    let back = std::fs::read_to_string(gitdir.join("gitdir"))
        .map_err(|_| IdentityError::WorktreeForged)?;
    let back = back.lines().next().map(str::trim).filter(|s| !s.is_empty());
    let Some(back) = back else {
        return Err(IdentityError::WorktreeForged);
    };
    let back_dir = Path::new(back)
        .parent()
        .ok_or(IdentityError::WorktreeForged)?;
    let back_dir = std::fs::canonicalize(back_dir).map_err(|_| IdentityError::WorktreeForged)?;
    if walk(&back_dir)?.dev_ino()? != worktree_handle.dev_ino()? {
        return Err(IdentityError::WorktreeForged);
    }
    Ok(())
}
