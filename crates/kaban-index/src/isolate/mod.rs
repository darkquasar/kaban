//! Isolation tiers (stage1-spec G1-S5.1 selection, S5.2 wrapper handles, S5.3 clean
//! environment, S5.5 caches, S5.6 scratch; stage1-plan CG4.3): the tier set, probe results,
//! the selection matrix over the `index.isolation` policy, the per-run scratch directory with
//! its RAII deletion, how child-visible paths are spelled, and the S5.3 environment table per
//! language. The `namespaces` helper lives in `namespaces` (CG4.4); the `container` tier is
//! deferred past 3.0.0 (OWN-68 SC-11): `Tier::Container`, its probe and the config keys stay
//! reserved, the probe reports it unavailable and the runner never starts it.
//!
//! ```text
//!   probe::probe_all(&Index, helper) ==> [Probe { tier, available: Ok(Detail) | Err(ReasonClass) }]
//!   select(policy, &probes) ==> Selection { tier, detail, warning: Option<isolation_none> }
//!                          |  required and no isolating tier ==> SelectError (exit 8)
//!   Scratch::create(&AdmittedDir) ==> <repo-id>/run-<pid>/{home,tmp,out,cache,cargo-home} 0700
//!   runner::env_for(lang, tool_dirs, &scratch, view, go, linker) ==> EnvAllowlist (S5.3)
//! ```
//!
//! Layout (`proj-mod-by-feature`): `probe` runs the platform probes, `wrapper` resolves the
//! trusted wrapper handle from the fixed candidate list, `namespaces` holds the view, the
//! helper and its probe; this file holds the types shared by all of them and by `runner`.

pub mod namespaces;
pub mod probe;
pub mod wrapper;

use std::ffi::{OsStr, OsString};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use kaban_core::config::Isolation;
use rustix::fs::{AtFlags, Dir, FileType, Mode, OFlags};
use rustix::io::Errno;
use serde::Serialize;
use thiserror::Error;

use crate::identity_cli::AdmittedDir;

pub use kaban_codegraph::IsolationTier as Tier;
pub use probe::probe_all;
pub use wrapper::{ResolvedWrapper, WrapperError, resolve_wrapper};

// the S5.3 table lives in `runner.rs`: the CG2.4 lint `no_trusted_internal_outside_runners`
// keeps the trusted-internal constants inside the runners (SO-8); it is reachable here too
pub use crate::runner::{GO_INTERNAL, TS_INTERNAL, env_for};

/// The policy of one run (`index.isolation`, `--isolation`): the config enum.
pub type IsolationPolicy = Isolation;

/// The kaban-built identity of an available tier (`meta.isolation_detail`): the absolute
/// wrapper path and the kernel release, or the runtime version; never caller or child data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Detail(pub String);

/// Why a tier is unavailable: the closed set `doctor` prints (fixed strings).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasonClass {
    /// No candidate of the fixed list resolved to an acceptable handle (S5.2).
    WrapperNotFound,
    /// A candidate was found but refused by the handle rule (`wrapper_not_elf`,
    /// `wrapper_writable`, `wrapper_owner`, `wrapper_not_regular`).
    WrapperRefused,
    /// Handle execution does not exist on this target (RV-9): no wrapper tier in stage 1.
    HandleExecUnsupported,
    /// `unshare --user --map-root-user --net --mount --pid --fork` failed, or the probe helper
    /// could not be started.
    UnshareFailed,
    /// The kernel disables unprivileged user namespaces (`unprivileged_userns_clone = 0` or
    /// `max_user_namespaces = 0`; CG4.4 negative path).
    UsernsDisabled,
    /// The probe helper ran but its in-run self-check failed (exit 8): an anomaly, reported as
    /// unavailable so `preferred` falls to the next tier and `required` refuses.
    SelfcheckFailed,
    /// The container tier is deferred past 3.0.0 (OWN-68 SC-11): never attempted.
    NotInThisRelease,
    /// `index.container_image` is not configured (S5.1).
    ContainerImageNotConfigured,
    /// The container probe is not attempted in this stage (CG4.5 adds the daemon probe).
    ContainerNotAttempted,
    /// The policy was `off`: nothing probed.
    NotProbed,
}

impl ReasonClass {
    /// The fixed text.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::WrapperNotFound => "wrapper_not_found",
            Self::WrapperRefused => "wrapper_refused",
            Self::HandleExecUnsupported => "handle_exec_unsupported",
            Self::UnshareFailed => "unshare_failed",
            Self::UsernsDisabled => "userns_disabled",
            Self::SelfcheckFailed => "selfcheck_failed",
            Self::NotInThisRelease => "not_in_this_release",
            Self::ContainerImageNotConfigured => "container_image_not_configured",
            Self::ContainerNotAttempted => "container_not_attempted",
            Self::NotProbed => "not_probed",
        }
    }
}

/// One tier's probe result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Probe {
    /// The tier probed.
    #[serde(serialize_with = "tier_name")]
    pub tier: Tier,
    /// Available with its detail, or the fixed reason.
    pub available: Result<Detail, ReasonClass>,
}

/// `meta.isolation` spelling of a tier for the JSON report.
#[allow(clippy::trivially_copy_pass_by_ref)] // serde's `serialize_with` signature
fn tier_name<S: serde::Serializer>(tier: &Tier, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(tier.as_str())
}

/// What `select` chose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    /// The tier that will run.
    pub tier: Tier,
    /// Its detail (`meta.isolation_detail`); `none` on the `none` tier.
    pub detail: Detail,
    /// `isolation_none` when `preferred` fell back to `none` (one fixed stderr line).
    pub warning: Option<&'static str>,
}

/// The fixed warning class of the fallback.
pub const ISOLATION_NONE: &str = "isolation_none";
/// The fixed stderr line of the fallback.
pub const ISOLATION_NONE_TEXT: &str =
    "isolation: no isolating tier is available; running the indexer without isolation (none)";
/// The fixed text of the `required` refusal (exit 8).
pub const REQUIRED_REFUSED_TEXT: &str =
    "isolation required but no isolating tier is available; indexing refused";

/// Why no tier could be selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum SelectError {
    /// `required` and neither `namespaces` nor `container` probed available (exit 8).
    #[error("{REQUIRED_REFUSED_TEXT}")]
    RequiredUnavailable,
}

impl SelectError {
    /// Exit code 8 (§7).
    #[must_use]
    pub const fn exit_code(self) -> u8 {
        8
    }
}

/// The selection matrix of G1-S5.1: `preferred` takes the strongest available tier in the
/// order `namespaces`, `container`, else `none` with the warning; `required` refuses without
/// an isolating tier; `off` is `none` without consulting the probes.
///
/// # Errors
/// [`SelectError::RequiredUnavailable`].
pub fn select(policy: IsolationPolicy, probes: &[Probe]) -> Result<Selection, SelectError> {
    let none = |warning| Selection {
        tier: Tier::None,
        detail: Detail("none".to_owned()),
        warning,
    };
    if policy == Isolation::Off {
        return Ok(none(None));
    }
    let strongest = [Tier::Namespaces, Tier::Container]
        .into_iter()
        .find_map(|tier| {
            probes
                .iter()
                .find(|p| p.tier == tier)
                .and_then(|p| p.available.clone().ok())
                .map(|detail| Selection {
                    tier,
                    detail,
                    warning: None,
                })
        });
    match (strongest, policy) {
        (Some(selection), _) => Ok(selection),
        (None, Isolation::Required) => Err(SelectError::RequiredUnavailable),
        (None, Isolation::Preferred | Isolation::Off) => Ok(none(Some(ISOLATION_NONE))),
    }
}

/// How child-visible paths are spelled (RV-8): host paths; the namespaces view (host paths
/// for the checkout and tools, `/scratch` and `/caches/<name>` for the scratch and the shared
/// caches); or `/work`, `/scratch` and `/caches/<name>` inside the container.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathView {
    /// The host's paths (the `none` tier).
    Host,
    /// The `namespaces` view: tools and checkout at their host paths, the scratch at
    /// `/scratch`, shared caches at `/caches/<name>` (CG4.4).
    Namespaces,
    /// The container's fixed mount points (reserved, OWN-68).
    Container,
}

/// Why the scratch could not be created.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ScratchError {
    /// A directory could not be created or its mode set; the io class only.
    #[error("scratch: {0}")]
    Io(&'static str),
}

/// Why a child's output file was refused (U2 RU2-6): the output directory is reached through
/// the handle taken at creation, the file through that directory handle with `O_NOFOLLOW`,
/// and only a regular file within the per-file quota is read. Fixed reasons, no name in the
/// text.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum OutputError {
    /// `<scratch>/out` is no longer the directory the handle was taken on.
    #[error("output directory replaced")]
    DirReplaced,
    /// No entry of that name.
    #[error("output file missing")]
    Missing,
    /// The entry is a symbolic link (never followed: a link to a device, to a file outside the
    /// view or to anything else is refused).
    #[error("output file is a symlink")]
    Symlink,
    /// The entry is not a regular file (a FIFO would block the parent, a device has no end).
    #[error("output file is not a regular file")]
    NotRegular,
    /// The file is larger than the configured per-file quota.
    #[error("output file exceeds the file quota")]
    TooLarge,
    /// The open or the read failed; the errno class.
    #[error("output read failed")]
    Io(#[source] Errno),
}

impl OutputError {
    /// The fixed `reason` of the report.
    #[must_use]
    pub const fn reason(&self) -> &'static str {
        match self {
            Self::DirReplaced => "output_dir_replaced",
            Self::Missing => "output_missing",
            Self::Symlink => "output_symlink",
            Self::NotRegular => "output_not_regular",
            Self::TooLarge => "output_too_large",
            Self::Io(_) => "output_read_failed",
        }
    }
}

/// `O_PATH|O_DIRECTORY|O_NOFOLLOW|O_CLOEXEC`: a directory handle that follows no link.
#[cfg(target_os = "linux")]
const DIR_HANDLE: OFlags = OFlags::PATH
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

// Darwin has no O_PATH. Retain the opened directory object, conservatively requiring
// read permission as in codegraph's directory admission.
#[cfg(target_os = "macos")]
const DIR_HANDLE: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

/// The per-run scratch directory `<repo-id>/run-<pid>/` (0700) with `home`, `tmp`, `out`,
/// `cache` and `cargo-home` (S5.3, S5.6). The only child-writable paths of a run live here.
/// Everything is deleted on every path (success, failure, timeout, quota, panic): there is no
/// option to keep raw files (RS2-7); [`Scratch::remove`] is the explicit, checked deletion the
/// pipeline runs before it reports success (U2 RU2-7: a child's mode-000 directory gets the
/// owner's permission back and is removed too), `Drop` the best-effort one. The run directory
/// and `out` are also held as handles opened through the admitted directory at creation, so
/// the child's output is read from the directory kaban created, never from a name the child
/// may have replaced (U2 RU2-6).
#[derive(Debug)]
pub struct Scratch {
    root: PathBuf,
    root_handle: OwnedFd,
    out_handle: OwnedFd,
    parent_handle: OwnedFd,
    removed: bool,
}

/// The name prefix of a run's scratch directory.
const RUN_PREFIX: &str = "run-";

impl Scratch {
    /// Create `<dir>/run-<pid>/` and its subdirectories, each 0700, after removing the
    /// left-over scratch directories of runs whose pid is no longer alive (spec §7: left-over
    /// temp and scratch directories are removed by the next run). An existing directory of
    /// this pid is removed first (a previous run of the same process).
    ///
    /// # Errors
    /// [`ScratchError::Io`].
    pub fn create(dir: &AdmittedDir) -> Result<Self, ScratchError> {
        use std::os::unix::fs::DirBuilderExt;
        sweep_leftovers(&dir.path);
        let root = dir.path.join(format!("{RUN_PREFIX}{}", std::process::id()));
        if root.exists() {
            std::fs::remove_dir_all(&root).map_err(|_| ScratchError::Io("remove_leftover"))?;
        }
        let mut builder = std::fs::DirBuilder::new();
        builder.mode(0o700);
        builder
            .create(&root)
            .map_err(|_| ScratchError::Io("create_root"))?;
        for sub in ["home", "tmp", "out", "cache", "cargo-home"] {
            builder
                .create(root.join(sub))
                .map_err(|_| ScratchError::Io("create_subdir"))?;
        }
        let name = format!("{RUN_PREFIX}{}", std::process::id());
        let root_handle = rustix::fs::openat(&dir.handle, name, DIR_HANDLE, Mode::empty())
            .map_err(|_| ScratchError::Io("open_root_handle"))?;
        let out_handle = rustix::fs::openat(&root_handle, "out", DIR_HANDLE, Mode::empty())
            .map_err(|_| ScratchError::Io("open_out_handle"))?;
        let parent_handle = rustix::fs::openat(&root_handle, "..", DIR_HANDLE, Mode::empty())
            .map_err(|_| ScratchError::Io("open_parent_handle"))?;
        Ok(Self {
            root,
            root_handle,
            out_handle,
            parent_handle,
            removed: false,
        })
    }

    /// Remove the run directory and everything in it, through handles: a directory the child
    /// left without the owner's permission bits gets `u+rwx` first (the child ran as the
    /// invoking user, so it is the user's own), no symlink is followed, every entry is
    /// unlinked relative to its parent. Called after the child terminated and before the run
    /// reports success (U2 RU2-7).
    ///
    /// # Errors
    /// [`ScratchError::Io`]`("remove")` when any entry could not be removed.
    pub fn remove(mut self) -> Result<(), ScratchError> {
        self.removed = true;
        let name = self
            .root
            .file_name()
            .map(OsStr::to_owned)
            .ok_or(ScratchError::Io("remove"))?;
        remove_tree(self.parent_handle.as_fd(), &name).map_err(|_| ScratchError::Io("remove"))
    }

    /// Read the child's output file `name` (one component) from `<scratch>/out` (U2 RU2-6):
    /// `out` must still be the directory whose handle was taken at creation, the entry is
    /// opened through that handle with `O_NOFOLLOW|O_NONBLOCK` (a symlink is refused, a FIFO
    /// cannot block the open), must be a regular file, and is read up to `cap` bytes (the
    /// per-file quota); more is refused.
    ///
    /// # Errors
    /// [`OutputError`]: every refusal is a fixed reason for the report.
    pub fn read_output(&self, name: &str, cap: u64) -> Result<Vec<u8>, OutputError> {
        use std::io::Read;
        self.check_out_dir()?;
        let fd = rustix::fs::openat(
            &self.out_handle,
            name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::NOCTTY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|e| match e {
            Errno::LOOP => OutputError::Symlink,
            Errno::NOENT => OutputError::Missing,
            other => OutputError::Io(other),
        })?;
        let st = rustix::fs::fstat(&fd).map_err(OutputError::Io)?;
        if !FileType::from_raw_mode(st.st_mode).is_file() {
            return Err(OutputError::NotRegular);
        }
        let size = u64::try_from(st.st_size).unwrap_or(u64::MAX);
        if size > cap {
            return Err(OutputError::TooLarge);
        }
        let mut bytes = Vec::with_capacity(usize::try_from(size).unwrap_or(0));
        std::fs::File::from(fd)
            .take(cap.saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(|e| OutputError::Io(Errno::from_io_error(&e).unwrap_or(Errno::IO)))?;
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > cap {
            return Err(OutputError::TooLarge);
        }
        Ok(bytes)
    }

    /// Remove the entry `name` (one component) from `<scratch>/out` through the handle taken
    /// at creation, so that what a previous invocation left (the coverage sidecar of a
    /// producer that then failed, r1-3) is not read as the next invocation's output. No entry
    /// of that name is already the wanted state; a symlink is unlinked, never followed; a
    /// directory of that name (a producer's, never kaban's) goes with everything in it.
    ///
    /// # Errors
    /// [`OutputError::DirReplaced`] when `out` is no longer the directory of the handle,
    /// [`OutputError::Io`] when the entry exists and could not be removed.
    pub fn remove_output(&self, name: &str) -> Result<(), OutputError> {
        self.check_out_dir()?;
        remove_tree(self.out_handle.as_fd(), OsStr::new(name)).map_err(OutputError::Io)
    }

    /// `out` must still be the directory whose handle was taken at creation (U2 RU2-6).
    fn check_out_dir(&self) -> Result<(), OutputError> {
        let now = rustix::fs::statat(&self.root_handle, "out", AtFlags::SYMLINK_NOFOLLOW)
            .map_err(|_| OutputError::DirReplaced)?;
        let mine = rustix::fs::fstat(&self.out_handle).map_err(OutputError::Io)?;
        if (now.st_dev, now.st_ino) != (mine.st_dev, mine.st_ino)
            || !FileType::from_raw_mode(now.st_mode).is_dir()
        {
            return Err(OutputError::DirReplaced);
        }
        Ok(())
    }

    /// The run directory.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `<scratch>/home` (`HOME`).
    #[must_use]
    pub fn home(&self) -> PathBuf {
        self.root.join("home")
    }

    /// `<scratch>/tmp` (`TMPDIR`).
    #[must_use]
    pub fn tmp(&self) -> PathBuf {
        self.root.join("tmp")
    }

    /// `<scratch>/out` (where `index.scip` is written).
    #[must_use]
    pub fn out(&self) -> PathBuf {
        self.root.join("out")
    }

    /// `<scratch>/cache` (the dedicated build and dependency caches).
    #[must_use]
    pub fn cache(&self) -> PathBuf {
        self.root.join("cache")
    }

    /// `<scratch>/cargo-home` (`CARGO_HOME`, stage 2 Rust).
    #[must_use]
    pub fn cargo_home(&self) -> PathBuf {
        self.root.join("cargo-home")
    }

    /// Write the internal Rust invocation and, when `registry` is given, the `cargo-home/registry`
    /// link to it (the admitted host registry, bound read-only at that path inside an isolating
    /// view) through the owned scratch handles, refusing any file a previous child planted.
    ///
    /// # Errors
    /// [`ScratchError::Io`] if a path was replaced or already exists.
    pub fn write_rust_invocation(
        &self,
        bytes: &[u8],
        registry: Option<&Path>,
    ) -> Result<(), ScratchError> {
        write_new_at(&self.root_handle, "rust-prepared.json", bytes)?;
        if let Some(target) = registry {
            let cargo =
                rustix::fs::openat(&self.root_handle, "cargo-home", DIR_HANDLE, Mode::empty())
                    .map_err(|_| ScratchError::Io("cargo_home_replaced"))?;
            rustix::fs::symlinkat(target, &cargo, "registry")
                .map_err(|_| ScratchError::Io("cargo_registry_replaced"))?;
        }
        Ok(())
    }

    pub(crate) fn prepare_rust_environment(&self) -> Result<(), ScratchError> {
        let cargo = rustix::fs::openat(&self.root_handle, "cargo-home", DIR_HANDLE, Mode::empty())
            .map_err(|_| ScratchError::Io("cargo_home_replaced"))?;
        write_new_at(&cargo, "config.toml", b"[net]\noffline = true\n")?;
        rustix::fs::mkdirat(
            &self.root_handle,
            "target",
            Mode::RUSR | Mode::WUSR | Mode::XUSR,
        )
        .map_err(|_| ScratchError::Io("cargo_target_replaced"))?;
        Ok(())
    }

    /// Every child-writable path the quota monitor walks (RS3-5): the whole run directory.
    #[must_use]
    pub fn writable_roots(&self) -> Vec<PathBuf> {
        vec![self.root.clone()]
    }
}

fn write_new_at(dir: &OwnedFd, name: &str, bytes: &[u8]) -> Result<(), ScratchError> {
    use std::io::Write;
    let fd = rustix::fs::openat(
        dir,
        name,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR,
    )
    .map_err(|_| ScratchError::Io("rust_prepared_file"))?;
    std::fs::File::from(fd)
        .write_all(bytes)
        .map_err(|_| ScratchError::Io("rust_prepared_write"))
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if self.removed {
            return;
        }
        let Some(name) = self.root.file_name().map(OsStr::to_owned) else {
            return;
        };
        if let Err(e) = remove_tree(self.parent_handle.as_fd(), &name) {
            tracing::warn!(errno = ?e, "scratch directory not fully removed");
        }
    }
}

/// Remove `name` under `parent` recursively through handles: `statat(NOFOLLOW)` decides (a
/// symlink is unlinked, never followed), a directory first gets the owner's `rwx` back, is
/// opened `O_DIRECTORY|O_NOFOLLOW`, emptied entry by entry, then removed. Absence is success.
fn remove_tree(parent: BorrowedFd<'_>, name: &OsStr) -> Result<(), Errno> {
    let st = match rustix::fs::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(st) => st,
        Err(Errno::NOENT) => return Ok(()),
        Err(e) => return Err(e),
    };
    if !FileType::from_raw_mode(st.st_mode).is_dir() {
        return match rustix::fs::unlinkat(parent, name, AtFlags::empty()) {
            Ok(()) | Err(Errno::NOENT) => Ok(()),
            Err(e) => Err(e),
        };
    }
    // the owner's bits, so a mode-000 directory can be listed (the entry is a directory, not
    // a link: the stat above did not follow one; the child that could race this is dead)
    let mode = Mode::from_raw_mode(st.st_mode & 0o7777) | Mode::RWXU;
    rustix::fs::chmodat(parent, name, mode, AtFlags::empty())?;
    let dir = rustix::fs::openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    let mut children: Vec<OsString> = Vec::new();
    for entry in Dir::read_from(&dir)? {
        let entry = entry?;
        let bytes = entry.file_name().to_bytes();
        if bytes == b"." || bytes == b".." {
            continue;
        }
        children.push(OsStr::from_bytes(bytes).to_owned());
    }
    for child in &children {
        remove_tree(dir.as_fd(), child)?;
    }
    drop(dir);
    match rustix::fs::unlinkat(parent, name, AtFlags::REMOVEDIR) {
        Ok(()) | Err(Errno::NOENT) => Ok(()),
        Err(e) => Err(e),
    }
}

/// Remove `run-<pid>` entries of `dir` whose pid is not a live process.
fn sweep_leftovers(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name
            .to_str()
            .and_then(|n| n.strip_prefix(RUN_PREFIX))
            .and_then(|p| p.parse::<u32>().ok())
        else {
            continue;
        };
        if pid != std::process::id() && !Path::new(&format!("/proc/{pid}")).exists() {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}
