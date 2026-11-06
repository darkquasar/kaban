//! Verified tools (U2 RU2-2, RU2-3; stage1-spec G1-S5.3 "the resolved indexer binary, absolute,
//! verified at run start", S5.1 view). An executable is admitted from the object, not the
//! name: canonicalised once (an installer's symlink is resolved at admission), opened
//! with no-follow host flags (metadata-only on Linux, read-only/nonblocking on Darwin),
//! `fstat`-checked (a regular file owned by the invoking user or root,
//! writable by neither group nor other) and refused under the checkout or the kaban data
//! directory; it carries its identity (device, inode, size, change time) to the spawn, where
//! [`recheck`] compares it again. The language toolchain found on `PATH` contributes its
//! executable's directory to the pinned `PATH` and, separately, a narrowly admitted root with
//! the targets of its symlinked entries as read-only resources of the namespaces view: never
//! the first `PATH` entry's grandparent as such, never `$HOME` or one of its ancestors, never
//! the checkout or the data directory, never `/`.
//!
//! ```text
//!   admit_executable(path, forbidden) ==> canonicalize ==> no-follow open ==> fstat
//!       regular, uid in {euid, 0}, mode & 0o022 == 0, not under a forbidden root ==> AdmittedTool
//!   recheck(&tool) at the spawn ==> the same canonical path and ToolIdentity, else Changed
//!   toolchain(lang, PATH, &RootRule) ==> Toolchain { bin_dir (PATH), root? + resources (binds) }
//! ```

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use rustix::fs::{FileType, Mode, OFlags, Stat};
use thiserror::Error;

use crate::detect::Lang;

/// The warning class of a toolchain whose root was not admitted (its `bin` directory still is).
pub const TOOLCHAIN_ROOT_REFUSED: &str = "toolchain_root_refused";

/// The identity of an admitted executable object: what [`recheck`] compares at the spawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolIdentity {
    /// `st_dev`.
    pub dev: u64,
    /// `st_ino`.
    pub ino: u64,
    /// `st_size`.
    pub size: u64,
    /// `st_ctime` (seconds): any content or metadata change moves it and it cannot be set.
    pub ctime_sec: i64,
    /// `st_ctime_nsec`.
    pub ctime_nsec: u32,
}

/// An executable that passed [`admit_executable`]: the canonical path and the identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdmittedTool {
    /// The canonical absolute path.
    pub path: PathBuf,
    /// The identity at admission.
    pub identity: ToolIdentity,
}

/// Why an executable was not admitted, or no longer is. Classes only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum ToolError {
    /// No such object (or it could not be opened).
    #[error("tool not found")]
    NotFound,
    /// Not a regular file, owned by neither the invoking user nor root, or writable by group or
    /// other (`indexer_untrusted_mode`).
    #[error("indexer_untrusted_mode")]
    UntrustedMode,
    /// Under the checkout, the kaban data directory or another forbidden root
    /// (`indexer_untrusted_location`).
    #[error("indexer_untrusted_location")]
    UntrustedLocation,
    /// The object at the admitted path is not the admitted object any more (`indexer_changed`).
    #[error("indexer_changed")]
    Changed,
}

#[cfg(target_os = "linux")]
const TOOL_OPEN_FLAGS: OFlags = OFlags::PATH.union(OFlags::NOFOLLOW).union(OFlags::CLOEXEC);

#[cfg(target_os = "macos")]
const TOOL_OPEN_FLAGS: OFlags = OFlags::RDONLY
    // Check type before consuming bytes; a FIFO must not wait for a writer at open.
    .union(OFlags::NONBLOCK)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

fn stat_tool(path: &Path) -> Result<(PathBuf, Stat), ToolError> {
    let canonical = std::fs::canonicalize(path).map_err(|_| ToolError::NotFound)?;
    let fd = rustix::fs::open(&canonical, TOOL_OPEN_FLAGS, Mode::empty())
        .map_err(|_| ToolError::NotFound)?;
    let st = rustix::fs::fstat(&fd).map_err(|_| ToolError::NotFound)?;
    Ok((canonical, st))
}

fn identity_of(st: &Stat) -> ToolIdentity {
    #[cfg(target_os = "macos")]
    let device = u64::from(u32::from_ne_bytes(st.st_dev.to_ne_bytes()));
    #[cfg(not(target_os = "macos"))]
    let device = st.st_dev;
    ToolIdentity {
        dev: device,
        ino: st.st_ino,
        size: u64::try_from(st.st_size).unwrap_or(0),
        ctime_sec: st.st_ctime,
        ctime_nsec: u32::try_from(st.st_ctime_nsec).unwrap_or(0),
    }
}

/// Admit the executable at `path` (U2 RU2-3): canonical, a regular file owned by the invoking
/// user or root, writable by neither group nor other, and not under any of `forbidden` (the
/// checkout, the kaban data directory).
///
/// # Errors
/// [`ToolError::NotFound`], [`ToolError::UntrustedMode`], [`ToolError::UntrustedLocation`].
pub fn admit_executable(path: &Path, forbidden: &[&Path]) -> Result<AdmittedTool, ToolError> {
    let (canonical, st) = stat_tool(path)?;
    if !FileType::from_raw_mode(st.st_mode).is_file() {
        return Err(ToolError::UntrustedMode);
    }
    let euid = rustix::process::geteuid().as_raw();
    if st.st_uid != 0 && st.st_uid != euid {
        return Err(ToolError::UntrustedMode);
    }
    if st.st_mode & 0o022 != 0 {
        return Err(ToolError::UntrustedMode);
    }
    if forbidden.iter().any(|root| canonical.starts_with(root)) {
        return Err(ToolError::UntrustedLocation);
    }
    Ok(AdmittedTool {
        path: canonical,
        identity: identity_of(&st),
    })
}

/// Re-check an admitted executable immediately before a spawn (U2 RU2-3): the path must still
/// resolve to the same canonical path and the object must carry the same identity.
///
/// # Errors
/// [`ToolError::Changed`] (a replaced, rewritten or re-linked object); [`ToolError::NotFound`].
pub fn recheck(tool: &AdmittedTool) -> Result<(), ToolError> {
    let (canonical, st) = stat_tool(&tool.path)?;
    if canonical != tool.path || identity_of(&st) != tool.identity {
        return Err(ToolError::Changed);
    }
    Ok(())
}

/// Where a toolchain root may never be (U2 RU2-2).
#[derive(Debug, Clone, Copy)]
pub struct RootRule<'a> {
    /// The canonical checkout.
    pub checkout: &'a Path,
    /// The caller's canonical home directory, when known (the root may be neither it nor one of
    /// its ancestors).
    pub home: Option<&'a Path>,
    /// The kaban data directory.
    pub data_dir: &'a Path,
}

impl RootRule<'_> {
    /// Whether `dir` may be bound into the view as a toolchain resource.
    #[must_use]
    pub fn allows(&self, dir: &Path) -> bool {
        if dir == Path::new("/") || dir.starts_with(self.checkout) || dir.starts_with(self.data_dir)
        {
            return false;
        }
        if let Some(home) = self.home
            && home.starts_with(dir)
        {
            return false;
        }
        true
    }
}

/// The language toolchain of a run (U2 RU2-2): what goes on the pinned `PATH` and what is
/// bound read-only as a resource.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Toolchain {
    /// The directory of the admitted toolchain executable (`go`, `node`): on the pinned
    /// `PATH`, bound read-only.
    pub bin_dir: PathBuf,
    /// The admitted root (the canonical grandparent with the toolchain layout, outside every
    /// forbidden place): bound read-only, never on `PATH`.
    pub root: Option<PathBuf>,
    /// The canonical targets of the root's symlinked entries and their children's (Debian's
    /// `GOROOT/src -> /usr/share/go-<v>/src`), each under the same rule.
    pub resources: Vec<PathBuf>,
    /// The canonical grandparent did not qualify as a root (the warning
    /// [`TOOLCHAIN_ROOT_REFUSED`]).
    pub root_refused: bool,
}

impl Toolchain {
    /// Every directory bound read-only as a resource (the root and its symlink targets).
    #[must_use]
    pub fn resource_dirs(&self) -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = self.root.iter().cloned().collect();
        out.extend(self.resources.iter().cloned());
        out
    }
}

/// The executable name of a language's toolchain.
#[must_use]
pub const fn toolchain_exe(lang: Lang) -> &'static str {
    match lang {
        Lang::Go => "go",
        Lang::TypeScript => "node",
        Lang::Rust => "rustc",
    }
}

/// Whether `root` has the layout of `lang`'s toolchain root.
fn has_layout(lang: Lang, root: &Path) -> bool {
    match lang {
        Lang::Go => root.join("VERSION").is_file() && root.join("src").is_dir(),
        Lang::TypeScript => root.join("lib").join("node_modules").is_dir(),
        Lang::Rust => {
            root.join("lib/rustlib/src/rust/library/core/src/lib.rs")
                .is_file()
                && root
                    .join("lib/rustlib/src/rust/library/std/src/lib.rs")
                    .is_file()
                && root.join("bin/cargo").is_file()
                && root.join("libexec/rust-analyzer-proc-macro-srv").is_file()
        }
    }
}

/// Find `lang`'s toolchain on the absolute entries of `path_env`, admit its executable (owner,
/// mode, not under the checkout or the data directory) and derive the narrow root and its
/// resources under `rule`. `Ok(None)` when no entry holds the executable.
///
/// # Errors
/// [`ToolError::UntrustedLocation`] for an executable under the checkout or the data directory,
/// [`ToolError::UntrustedMode`] for one that fails the owner or mode rule: the run is refused
/// (`toolchain_refused`), the entry is not skipped over.
pub fn toolchain(
    lang: Lang,
    path_env: Option<&OsString>,
    rule: &RootRule<'_>,
) -> Result<Option<Toolchain>, ToolError> {
    let exe = toolchain_exe(lang);
    let Some(entry) = path_env.and_then(|p| {
        std::env::split_paths(p)
            .filter(|p| p.is_absolute())
            .find(|p| p.join(exe).is_file())
    }) else {
        return Ok(None);
    };
    let admitted = admit_executable(&entry.join(exe), &[rule.checkout, rule.data_dir])?;
    let Some(bin_dir) = admitted.path.parent() else {
        return Err(ToolError::UntrustedLocation);
    };
    let candidate = bin_dir.parent();
    let root = candidate.filter(|root| has_layout(lang, root) && rule.allows(root));
    let resources = root.map_or_else(Vec::new, |root| symlink_targets(root, rule));
    Ok(Some(Toolchain {
        bin_dir: bin_dir.to_path_buf(),
        root: root.map(Path::to_path_buf),
        resources,
        root_refused: root.is_none(),
    }))
}

/// The canonical directory targets of the symlinked entries of `root` and of its plain
/// subdirectories (one level deeper), each allowed by `rule`; sorted, de-duplicated.
fn symlink_targets(root: &Path, rule: &RootRule<'_>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let scan = |dir: &Path, out: &mut Vec<PathBuf>| -> Vec<PathBuf> {
        let mut plain = Vec::new();
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let Ok(kind) = entry.file_type() else {
                    continue;
                };
                if kind.is_symlink() {
                    if let Ok(target) = std::fs::canonicalize(entry.path())
                        && target.is_dir()
                        && rule.allows(&target)
                        && !target.starts_with(root)
                    {
                        out.push(target);
                    }
                } else if kind.is_dir() {
                    plain.push(entry.path());
                }
            }
        }
        plain
    };
    for child in scan(root, &mut out) {
        scan(&child, &mut out);
    }
    out.sort();
    out.dedup();
    out
}
