//! The trust mark (stage1-spec G1-S6; stage1-plan CG4.1; RS2-2, RS2-4): one file `trust` in the
//! admitted per-repository directory, 0600, created `O_EXCL`, holding identity material and no
//! path or other free string; revalidation deletes the file on any mismatch (`Revoked`).
//!
//! ```text
//!   trust(&dir, &id, version)      openat(dir, "trust", CREATE|EXCL|NOFOLLOW, 0600) ==> JSON record
//!   revalidate(&dir, &id)          openat(dir, "trust", RDONLY|NOFOLLOW) ==> compare dev/ino,
//!                                  btime when both have it, sorted roots; mismatch: unlinkat, Revoked
//!   untrust(&dir)                  unlinkat(dir, "trust")
//! ```

use kaban_codegraph::identity::{DirHandle, FsIdentity, RepoIdentity};
use rustix::fs::{AtFlags, Mode, OFlags};
use rustix::io::Errno;
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use thiserror::Error;

/// The file name inside the repository directory.
pub const TRUST_FILE: &str = "trust";

/// The fixed text `trust` prints, one sentence per line (G1-S6).
pub const TRUST_TEXT: &[&str] = &[
    "indexers may run build code from this repository as your user.",
    "Under the namespaces or container tier they see only this checkout and the pinned toolchains read-only, one scratch directory, and no network.",
    "Under the none tier they see your files and network as you do.",
    "The tier is chosen at index time, recorded in every snapshot and shown by kaban-index doctor and status; set index.isolation = required to refuse indexing without isolation.",
    "This mark never waives environment hygiene, quotas, output gating or identity checks.",
];

/// The fixed text of a revocation (G1-S6).
pub const REVOKED_TEXT: &str =
    "repository identity changed; trust mark revoked; run kaban-index trust again";

/// Why a trust operation did not succeed. Every text is fixed.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum TrustError {
    /// `trust` on a repository that already carries a mark.
    #[error("repository already trusted")]
    AlreadyTrusted,
    /// No mark: `trust_refused`.
    #[error("repository not trusted; run kaban-index trust first")]
    NotTrusted,
    /// The identity changed: the mark was deleted (`trust_revoked`).
    #[error("{REVOKED_TEXT}")]
    Revoked,
    /// The record did not parse as the one JSON shape; treated as revoked (deleted).
    #[error("trust record malformed; trust mark revoked")]
    Malformed,
    /// A filesystem failure, by class.
    #[error("trust record filesystem failure ({0})")]
    Io(&'static str),
}

impl TrustError {
    /// The exit code (§7): every trust refusal is 4.
    #[must_use]
    pub fn exit_code(&self) -> u8 {
        4
    }

    /// The fixed class string for the JSON report.
    #[must_use]
    pub fn class(&self) -> &'static str {
        match self {
            Self::AlreadyTrusted => "already_trusted",
            Self::NotTrusted => "trust_refused",
            Self::Revoked | Self::Malformed => "trust_revoked",
            Self::Io(_) => "io",
        }
    }
}

impl From<Errno> for TrustError {
    fn from(e: Errno) -> Self {
        match e {
            Errno::EXIST => Self::AlreadyTrusted,
            Errno::NOENT => Self::NotTrusted,
            Errno::LOOP => Self::Io("symlink"),
            Errno::ACCESS | Errno::PERM => Self::Io("permission_denied"),
            _ => Self::Io("other"),
        }
    }
}

impl From<std::io::Error> for TrustError {
    fn from(_: std::io::Error) -> Self {
        Self::Io("other")
    }
}

/// The record (RS2-2): `{v, common_dir: {dev, ino, btime | null}, root_commits, kaban_index_version}`,
/// no other field and no path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrustRecord {
    /// Record version, 1.
    pub v: u8,
    /// The common directory object.
    pub common_dir: CommonDir,
    /// Sorted root commits (sha40).
    pub root_commits: Vec<String>,
    /// The `kaban-index` version that wrote the mark: recorded, never compared.
    pub kaban_index_version: String,
}

/// The `common_dir` member of the record: the three numbers of [`FsIdentity`], nothing else.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommonDir {
    /// `st_dev`.
    pub dev: u64,
    /// `st_ino`.
    pub ino: u64,
    /// Birth time in nanoseconds, `null` when the filesystem has none.
    pub btime: Option<i128>,
}

impl From<FsIdentity> for CommonDir {
    fn from(fs: FsIdentity) -> Self {
        Self {
            dev: fs.dev,
            ino: fs.ino,
            btime: fs.btime,
        }
    }
}

impl From<CommonDir> for FsIdentity {
    fn from(c: CommonDir) -> Self {
        Self {
            dev: c.dev,
            ino: c.ino,
            btime: c.btime,
        }
    }
}

/// Write the mark for `id` into the admitted directory: `O_CREAT|O_EXCL|O_NOFOLLOW`, 0600.
///
/// # Errors
/// [`TrustError::AlreadyTrusted`] when a mark exists, [`TrustError::Io`] otherwise.
pub fn trust(
    dir: &DirHandle,
    id: &RepoIdentity,
    kaban_index_version: &str,
) -> Result<(), TrustError> {
    let record = TrustRecord {
        v: 1,
        common_dir: id.fs.into(),
        root_commits: id.root_commits.clone(),
        kaban_index_version: kaban_index_version.to_owned(),
    };
    let text = serde_json::to_string(&record).map_err(|_| TrustError::Io("serialise"))?;
    let fd = rustix::fs::openat(
        dir,
        TRUST_FILE,
        OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::WRONLY | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR,
    )?;
    // exact 0600 whatever the umask
    rustix::fs::fchmod(&fd, Mode::RUSR | Mode::WUSR)?;
    let mut file = std::fs::File::from(fd);
    file.write_all(text.as_bytes())?;
    file.sync_all()?;
    Ok(())
}

/// Read the mark through the directory handle.
fn read_record(dir: &DirHandle) -> Result<TrustRecord, TrustError> {
    let fd = rustix::fs::openat(
        dir,
        TRUST_FILE,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    let st = rustix::fs::fstat(&fd)?;
    if !rustix::fs::FileType::from_raw_mode(st.st_mode).is_file() {
        return Err(TrustError::Malformed);
    }
    let mut text = String::new();
    std::fs::File::from(fd)
        .take(1 << 20)
        .read_to_string(&mut text)?;
    serde_json::from_str(&text).map_err(|_| TrustError::Malformed)
}

/// Remove the mark; absent is `NotTrusted`.
///
/// # Errors
/// [`TrustError::NotTrusted`], [`TrustError::Io`].
pub fn untrust(dir: &DirHandle) -> Result<(), TrustError> {
    rustix::fs::unlinkat(dir, TRUST_FILE, AtFlags::empty())?;
    Ok(())
}

/// Compare the live identity with the mark: `dev`, `ino`, the birth time when both sides have
/// one, the sorted root commits; `kaban_index_version` is recorded, never compared. Any
/// mismatch deletes the file and is `Revoked`; an unparsable record is deleted too.
///
/// # Errors
/// [`TrustError::NotTrusted`], [`TrustError::Revoked`], [`TrustError::Malformed`],
/// [`TrustError::Io`].
pub fn revalidate(dir: &DirHandle, id: &RepoIdentity) -> Result<(), TrustError> {
    let record = match read_record(dir) {
        Ok(record) => record,
        Err(TrustError::Malformed) => {
            let _ = untrust(dir);
            return Err(TrustError::Malformed);
        }
        Err(e) => return Err(e),
    };
    let same = record.v == 1
        && FsIdentity::from(record.common_dir).matches(&id.fs)
        && record.root_commits == id.root_commits;
    if same {
        Ok(())
    } else {
        untrust(dir)?;
        Err(TrustError::Revoked)
    }
}

/// Whether a mark file exists (no comparison): for `status` and `doctor` before resolving.
#[must_use]
pub fn is_marked(dir: &DirHandle) -> bool {
    rustix::fs::statat(dir, TRUST_FILE, AtFlags::SYMLINK_NOFOLLOW).is_ok()
}
