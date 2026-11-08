//! The `namespaces` tier (stage1-spec G1-S5.1 `namespaces`, S5.2 helper and in-run self-check,
//! S5.5 shared read-only caches; stage1-plan CG4.4; OD-G1-5). Two halves live here because
//! they share one vocabulary: the parent builds the [`View`] (the mount list, kaban-built) and
//! the helper arguments; `kaban-index ns-helper` ([`run_helper`]) runs inside the namespaces
//! that one trusted `unshare` invocation opened, builds the view, verifies it from the inside
//! and `exec`s the indexer without leaving them.
//!
//! ```text
//!   parent: run_indexer(Namespaces)  ==> unshare(handle) --user --map-root-user --net --mount --pid --fork
//!                                        <helper> ns-helper --parent-ns u,n,m --checkout C --scratch S
//!                                        [--ro P]* [--cache name=lower]* --overlay overlayfs|none
//!                                        -- <indexer> <args>
//!   helper: make-rprivate /  ==> tmpfs at S/.newroot ==> bind ro each P (remount BIND|RDONLY
//!           + the source's locked flags) ==> S at /scratch rw ==> /dev/{null,zero,urandom}
//!           by bind ==> caches at /caches/<name> (overlayfs upper/work in S, else ro bind)
//!           ==> /proc (needs the fresh pid namespace on this kernel) ==> root remount ro
//!           ==> pivot_root(".", ".") + umount2(".", MNT_DETACH)
//!           ==> self-check: ns inodes differ, /proc/net/dev = {lo}, checkout EROFS,
//!               /scratch writable, mountinfo == View::mount_points()
//!           ==> probe: print "overlay=<x>", exit 0
//!           ==> run: chdir(C), drop privileges (nested userns closed, securebits locked,
//!               ambient cleared, bounding set emptied, no_new_privs, capset empty; read back),
//!               exec(indexer)
//!           mismatch or setup failure: stderr SELFCHECK_FAILED_TEXT, exit 8 (every policy)
//! ```
//!
//! Every mount call is the safe `rustix::mount` / `rustix::process::pivot_root` API, and the
//! privilege drop before the `exec` is the safe `rustix::thread` prctl and capset API
//! (`unsafe-minimize-scope`: no `unsafe` is needed, confirmed against rustix 1.1.5; the
//! util-linux `--root`/`--mount-proc` fallback of the plan is therefore unused). Overlayfs
//! inside the user namespace is tried once by the probe and recorded in `isolation_detail`
//! as `overlay=overlayfs` or `overlay=none` (the OD-G1-5 fallback to a plain read-only bind).
//! The indexer runs as uid 0 of the user namespace with no capability at all (U2 RU2-1):
//! the read-only binds and the mount list the self-check verified cannot be changed by it.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[cfg(target_os = "linux")]
use kaban_core::process::{EnvAllowlist, InheritedFds, Spec};
use rustix::io::Errno;
use serde::Serialize;
use thiserror::Error;

use super::wrapper::{ResolvedWrapper, UNSHARE_CANDIDATES, resolve_wrapper};
use super::{Detail, Probe, ReasonClass, Tier, WrapperError};

#[cfg(target_os = "linux")]
mod linux;

/// The fixed text of an in-run self-check mismatch (exit 8 under every policy, S5.2).
pub const SELFCHECK_FAILED_TEXT: &str = "isolation self-check failed; indexing refused";
/// The helper's exit code on a self-check mismatch or a view it could not build.
pub const HELPER_EXIT: u8 = 8;
/// The `unshare` flags (S5.1) plus `--pid --fork`: a fresh `/proc` can only be mounted for a
/// pid namespace the user namespace owns (observed on this kernel: `EPERM` otherwise; the
/// CG4.4 report records it). `--fork` keeps `unshare` as the waiting parent; the helper is
/// pid 1 of the new pid namespace and `exec`s into the indexer under that pid.
pub const UNSHARE_FLAGS: [&str; 6] = [
    "--user",
    "--map-root-user",
    "--net",
    "--mount",
    "--pid",
    "--fork",
];
/// The system paths a dynamically linked tool needs, bound read-only when present, plus the
/// system headers a build script compiling C needs (`/usr/include`, the multiarch libc headers
/// under it; the compiler's own headers are under `/usr/lib`; DF-2).
pub const SYSTEM_RO: [&str; 5] = [
    "/lib",
    "/lib64",
    "/usr/lib",
    "/usr/include",
    "/etc/ld.so.cache",
];
/// Where the scratch is bound inside the view.
pub const SCRATCH_MOUNT: &str = "/scratch";
/// Where the shared read-only caches are mounted inside the view (`/caches/<name>`).
pub const CACHES_MOUNT: &str = "/caches";
/// The name of the Go module cache mount (`GOMODCACHE` = `/caches/gomod`).
pub const GOMOD_CACHE: &str = "gomod";
/// Deadline of the availability probe.
pub const PROBE_DEADLINE: Duration = Duration::from_secs(10);
/// The device paths included in the shared view and its mount-list self-check.
const DEV_NODES: [&str; 3] = ["null", "zero", "urandom"];

/// How the shared caches are exposed (OD-G1-5): a private writable overlay, or the plain
/// read-only bind when overlayfs is unavailable inside the user namespace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Overlay {
    /// `overlay` mounted with `upperdir`/`workdir` under `/scratch/cache/<name>`.
    Overlayfs,
    /// A plain read-only bind of the user's cache (writes fail).
    None,
}

impl Overlay {
    /// The `isolation_detail` spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Overlayfs => "overlayfs",
            Self::None => "none",
        }
    }

    /// The inverse of [`Overlay::as_str`].
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "overlayfs" => Some(Self::Overlayfs),
            "none" => Some(Self::None),
            _ => None,
        }
    }

    /// The finding recorded in a probe's detail (`... overlay=<x>`), the way the `index`
    /// pipeline recovers it from the selected tier's [`Detail`] for the run.
    #[must_use]
    pub fn from_detail(detail: &str) -> Option<Self> {
        detail
            .split_whitespace()
            .find_map(|word| word.strip_prefix("overlay="))
            .and_then(Self::parse)
    }
}

/// The inodes of `/proc/self/ns/{user,net,mnt}`: the parent's are passed to the helper, which
/// requires all three to differ from its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NsInodes {
    /// `user:[<n>]`.
    pub user: u64,
    /// `net:[<n>]`.
    pub net: u64,
    /// `mnt:[<n>]`.
    pub mnt: u64,
}

impl NsInodes {
    /// Read this process's namespace inodes.
    ///
    /// # Errors
    /// [`NsError::Inodes`] when `/proc/self/ns/*` cannot be read.
    pub fn current() -> Result<Self, NsError> {
        Ok(Self {
            user: ns_inode("user")?,
            net: ns_inode("net")?,
            mnt: ns_inode("mnt")?,
        })
    }

    /// The helper argument `user,net,mnt`.
    #[must_use]
    pub fn to_arg(self) -> String {
        format!("{},{},{}", self.user, self.net, self.mnt)
    }

    /// The inverse of [`NsInodes::to_arg`].
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        let mut parts = s.split(',').map(|p| p.parse::<u64>().ok());
        let (Some(user), Some(net), Some(mnt), None) =
            (parts.next()?, parts.next()?, parts.next()?, parts.next())
        else {
            return None;
        };
        Some(Self { user, net, mnt })
    }
}

#[cfg(target_os = "linux")]
fn ns_inode(kind: &'static str) -> Result<u64, NsError> {
    let link = std::fs::read_link(format!("/proc/self/ns/{kind}"))
        .map_err(|source| NsError::Inodes { kind, source })?;
    link.to_str()
        .and_then(|s| s.strip_prefix(kind)?.strip_prefix(":[")?.strip_suffix(']'))
        .and_then(|n| n.parse().ok())
        .ok_or(NsError::Inodes {
            kind,
            source: std::io::Error::other("unexpected link form"),
        })
}

#[cfg(not(target_os = "linux"))]
fn ns_inode(kind: &'static str) -> Result<u64, NsError> {
    Err(NsError::Inodes {
        kind,
        source: std::io::Error::from(std::io::ErrorKind::Unsupported),
    })
}

/// One shared read-only cache (S5.5): mounted at `/caches/<name>` from the user's `lower`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheMount {
    /// The mount name (`gomod`).
    pub name: String,
    /// The user's cache directory on the host.
    pub lower: PathBuf,
}

/// The mount view of one run: kaban-built from the resolved identity, the pinned tool
/// directories and the scratch. Nothing user-controlled reaches the helper's argv unscanned:
/// every path is a canonical path kaban resolved itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct View {
    /// The checkout (read-only; the indexer's working directory).
    pub checkout: PathBuf,
    /// The host scratch directory (`/scratch` inside, the one writable place).
    pub scratch: PathBuf,
    /// Every read-only bind, host path == view path (the checkout and common directory,
    /// the tool directories, the system library paths), sorted, de-duplicated, no descendant
    /// of another entry.
    pub ro: Vec<PathBuf>,
    /// The shared caches.
    pub caches: Vec<CacheMount>,
}

impl View {
    /// Build the view of a run: `checkout`, the linked worktree's `common_dir` when it lies
    /// outside the checkout, the directory of the `indexer`, every `tool_dir`, the system
    /// paths that exist, and the `caches`.
    #[must_use]
    pub fn new(
        checkout: &Path,
        common_dir: Option<&Path>,
        indexer: &Path,
        tool_dirs: &[PathBuf],
        scratch: &Path,
        caches: Vec<CacheMount>,
    ) -> Self {
        let mut ro: Vec<PathBuf> = vec![checkout.to_path_buf()];
        ro.extend(common_dir.map(Path::to_path_buf));
        ro.extend(indexer.parent().map(Path::to_path_buf));
        ro.extend(tool_dirs.iter().cloned());
        ro.extend(SYSTEM_RO.iter().map(PathBuf::from).filter(|p| p.exists()));
        Self {
            checkout: checkout.to_path_buf(),
            scratch: scratch.to_path_buf(),
            ro: normalise(ro),
            caches,
        }
    }

    /// The mount points the self-check expects in `/proc/self/mountinfo`, exactly.
    #[must_use]
    pub fn mount_points(&self) -> BTreeSet<PathBuf> {
        let mut set: BTreeSet<PathBuf> = ["/", "/proc", SCRATCH_MOUNT]
            .iter()
            .map(PathBuf::from)
            .collect();
        set.extend(DEV_NODES.iter().map(|n| Path::new("/dev").join(n)));
        set.extend(self.ro.iter().cloned());
        set.extend(
            self.caches
                .iter()
                .map(|c| Path::new(CACHES_MOUNT).join(&c.name)),
        );
        set
    }

    /// The helper's arguments after `ns-helper` (before the `-- <indexer> <args>` tail).
    #[must_use]
    pub fn helper_args(&self, parent: NsInodes, overlay: Overlay) -> Vec<OsString> {
        let mut args: Vec<OsString> = vec![
            "--parent-ns".into(),
            parent.to_arg().into(),
            "--checkout".into(),
            self.checkout.clone().into(),
            "--scratch".into(),
            self.scratch.clone().into(),
            "--overlay".into(),
            overlay.as_str().into(),
        ];
        for p in &self.ro {
            args.push("--ro".into());
            args.push(p.clone().into());
        }
        for c in &self.caches {
            args.push("--cache".into());
            let mut v = OsString::from(&c.name);
            v.push("=");
            v.push(&c.lower);
            args.push(v);
        }
        args
    }
}

/// Sort, de-duplicate and drop every path that lies under another listed path (the parent
/// bind already carries it; a nested bind on a read-only parent could not be created).
fn normalise(mut paths: Vec<PathBuf>) -> Vec<PathBuf> {
    paths.sort();
    paths.dedup();
    let mut out: Vec<PathBuf> = Vec::new();
    for p in paths {
        if out.iter().any(|kept| p.starts_with(kept)) {
            continue;
        }
        out.push(p);
    }
    out
}

/// What the helper does after the self-check.
#[derive(Debug)]
pub enum HelperMode {
    /// Availability probe: detect overlayfs, print `overlay=<x>`, exit 0.
    Probe,
    /// A run: mount the caches as `overlay` says, then `exec` the indexer in the checkout.
    Run {
        /// The overlay mechanism the probe recorded.
        overlay: Overlay,
        /// The indexer's absolute path inside the view.
        indexer: PathBuf,
        /// `argv[1..]`.
        args: Vec<OsString>,
    },
}

/// The identities the helper verifies at its boundary (U2 RU2-3, RU2-5): the checkout's
/// `(dev, ino)` before it is bound, the indexer's immediately before the `exec`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Expect {
    /// The admitted checkout object.
    pub checkout: Option<(u64, u64)>,
    /// The admitted indexer object.
    pub indexer: Option<(u64, u64)>,
}

/// The helper's parsed command line.
#[derive(Debug)]
pub struct HelperOpts {
    /// The parent's namespace inodes.
    pub parent: NsInodes,
    /// The view to build.
    pub view: View,
    /// Probe or run.
    pub mode: HelperMode,
    /// The identities to verify at the boundary.
    pub expect: Expect,
}

/// Why the view could not be built or verified. Each variant names the operation and keeps
/// its `errno` (`err-source-chain`); no path from the view is in the text.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum NsError {
    /// `/proc/self/ns/<kind>` could not be read.
    #[error("namespace inode {kind}")]
    Inodes {
        /// `user`, `net` or `mnt`.
        kind: &'static str,
        /// The read error.
        #[source]
        source: std::io::Error,
    },
    /// A mount, directory or pivot step failed.
    #[error("setup {op}")]
    Setup {
        /// The step (`tmpfs`, `bind`, `remount_ro`, `pivot_root`, ...).
        op: &'static str,
        /// The `errno`.
        #[source]
        source: Errno,
    },
    /// One or more self-check items did not hold (every item is evaluated; the failing ones
    /// are listed in the spec's order, comma separated: `namespace_inodes,network_interfaces`
    /// is what a helper started without `--net` reports).
    #[error("self-check {}", .0.join(","))]
    SelfCheck(Vec<&'static str>),
    /// The `exec` of the indexer inside the view failed.
    #[error("exec")]
    Exec(#[source] std::io::Error),
    /// The helper could not be started (`unshare`), or ended without reaching the probe line.
    #[error("probe {0}")]
    Probe(&'static str),
}

impl NsError {
    /// The fixed class of the failure.
    #[must_use]
    pub const fn class(&self) -> &'static str {
        match self {
            Self::Inodes { .. } | Self::Setup { .. } | Self::Exec(_) => "isolation_setup_failed",
            Self::SelfCheck(_) => "isolation_selfcheck_failed",
            Self::Probe(_) => "unshare_failed",
        }
    }
}

/// Where the kernel reports whether unprivileged user namespaces are available; both files
/// may be absent (absence is "not restricted").
pub const USERNS_SYSCTLS: [&str; 2] = [
    "/proc/sys/kernel/unprivileged_userns_clone",
    "/proc/sys/user/max_user_namespaces",
];

/// `true` when the sysctl texts say user namespaces are disabled for unprivileged users:
/// `unprivileged_userns_clone` = `0` or `max_user_namespaces` = `0`; an absent file is not
/// a restriction.
#[must_use]
pub fn userns_disabled(
    unprivileged_userns_clone: Option<&str>,
    max_user_namespaces: Option<&str>,
) -> bool {
    let is_zero = |s: Option<&str>| s.is_some_and(|v| v.trim() == "0");
    is_zero(unprivileged_userns_clone) || is_zero(max_user_namespaces)
}

/// The availability probe (S5.2 `--probe-only`): the wrapper resolves from the fixed list,
/// the sysctls do not disable user namespaces, and `unshare <flags> <helper> ns-helper
/// --probe-only ...` through the handle builds a minimal view over a temporary scratch,
/// passes its self-check and prints the overlay finding. `helper` is the `kaban-index` binary
/// (the canonical `/proc/self/exe` read by the parent).
#[must_use]
pub fn probe_namespaces(helper: &Path) -> Probe {
    Probe {
        tier: Tier::Namespaces,
        available: probe_detail(helper),
    }
}

#[cfg(target_os = "linux")]
fn probe_detail(helper: &Path) -> Result<Detail, ReasonClass> {
    let wrapper = resolve_unshare().map_err(|e| match e {
        WrapperError::NotFound => ReasonClass::WrapperNotFound,
        WrapperError::Refused(_) | WrapperError::NotAbsolute => ReasonClass::WrapperRefused,
        WrapperError::HandleExecUnsupported => ReasonClass::HandleExecUnsupported,
    })?;
    let read = |p: &str| std::fs::read_to_string(p).ok();
    if userns_disabled(
        read(USERNS_SYSCTLS[0]).as_deref(),
        read(USERNS_SYSCTLS[1]).as_deref(),
    ) {
        return Err(ReasonClass::UsernsDisabled);
    }
    let overlay = run_probe(helper, wrapper.exec)?;
    Ok(Detail(format!(
        "unshare={} kernel={} overlay={}",
        wrapper.path.display(),
        kernel_release(),
        overlay.as_str()
    )))
}

#[cfg(not(target_os = "linux"))]
fn probe_detail(_helper: &Path) -> Result<Detail, ReasonClass> {
    Err(ReasonClass::HandleExecUnsupported)
}

/// Resolve the `unshare` handle from the fixed candidate list (S5.2).
///
/// # Errors
/// [`WrapperError`].
pub fn resolve_unshare() -> Result<ResolvedWrapper, WrapperError> {
    let candidates: Vec<PathBuf> = UNSHARE_CANDIDATES.iter().map(PathBuf::from).collect();
    let refs: Vec<&Path> = candidates.iter().map(PathBuf::as_path).collect();
    resolve_wrapper(&refs, None)
}

/// A temporary probe scratch under `/tmp`, removed on drop.
#[cfg(target_os = "linux")]
struct ProbeDir(PathBuf);

#[cfg(target_os = "linux")]
impl ProbeDir {
    fn create() -> Result<Self, ReasonClass> {
        use std::os::unix::fs::DirBuilderExt;
        let dir = std::env::temp_dir().join(format!(
            "kaban-ns-probe-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        let mut builder = std::fs::DirBuilder::new();
        builder.mode(0o700);
        builder
            .create(&dir)
            .map_err(|_| ReasonClass::UnshareFailed)?;
        let this = Self(dir);
        for sub in ["scratch", "ro"] {
            builder
                .create(this.0.join(sub))
                .map_err(|_| ReasonClass::UnshareFailed)?;
        }
        Ok(this)
    }
}

#[cfg(target_os = "linux")]
impl Drop for ProbeDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(target_os = "linux")]
fn run_probe(helper: &Path, exec: kaban_core::process::Exec) -> Result<Overlay, ReasonClass> {
    let dir = ProbeDir::create()?;
    let view = View {
        checkout: dir.0.join("ro"),
        scratch: dir.0.join("scratch"),
        ro: normalise(
            std::iter::once(dir.0.join("ro"))
                .chain(SYSTEM_RO.iter().map(PathBuf::from).filter(|p| p.exists()))
                .collect(),
        ),
        caches: Vec::new(),
    };
    let parent = NsInodes::current().map_err(|_| ReasonClass::UnshareFailed)?;
    let mut args: Vec<OsString> = UNSHARE_FLAGS.iter().map(OsString::from).collect();
    args.push(helper.as_os_str().to_owned());
    args.push("ns-helper".into());
    args.push("--probe-only".into());
    args.extend(view.helper_args(parent, Overlay::None));
    let outcome = kaban_core::process::run(Spec {
        exec,
        args,
        cwd: Path::new("/"),
        env: EnvAllowlist::new(),
        deadline: PROBE_DEADLINE,
        stdout_cap: 256,
        stderr_cap: 4096,
        monitors: Vec::new(),
        join_bound: Duration::from_secs(2),
        // a probe is not an indexer: a launcher's leaked descriptor must not stop it
        inherited_fds: InheritedFds::Tolerate,
    })
    .map_err(|_| ReasonClass::UnshareFailed)?;
    if outcome.killed.is_some() {
        return Err(ReasonClass::UnshareFailed);
    }
    match outcome.status.and_then(|s| s.code()) {
        Some(0) => {}
        Some(code) if code == i32::from(HELPER_EXIT) => return Err(ReasonClass::SelfcheckFailed),
        _ => return Err(ReasonClass::UnshareFailed),
    }
    let text = String::from_utf8_lossy(&outcome.stdout);
    text.trim()
        .strip_prefix("overlay=")
        .and_then(Overlay::parse)
        .ok_or(ReasonClass::SelfcheckFailed)
}

/// Run `tool <args>` inside a minimal namespaces view (U2 RU2-3, `doctor`'s probes): the
/// tool's own directory, `/usr/bin`, `/bin` and the system library paths read-only, an empty
/// read-only checkout, a temporary scratch; the production descriptor policy
/// (`InheritedFds::Refuse`), stdin null, 64 KiB per stream. The helper's self-check runs as for
/// an indexer; the tool is `exec`ed after the privilege drop.
/// `resources` must contain only admitted narrow toolchain directories; these are bound
/// read-only and are not added to the executable search path.
///
/// # Errors
/// [`ReasonClass`] as the availability probe: the wrapper, the helper or the spawn failed.
#[cfg(target_os = "linux")]
pub fn run_tool_in_view(
    helper: &Path,
    tool: &Path,
    args: &[&str],
    deadline: Duration,
    resources: &[PathBuf],
) -> Result<kaban_core::process::Outcome, ReasonClass> {
    let wrapper = resolve_unshare().map_err(|e| match e {
        WrapperError::NotFound => ReasonClass::WrapperNotFound,
        WrapperError::Refused(_) | WrapperError::NotAbsolute => ReasonClass::WrapperRefused,
        WrapperError::HandleExecUnsupported => ReasonClass::HandleExecUnsupported,
    })?;
    let dir = ProbeDir::create()?;
    for sub in ["home", "tmp"] {
        std::fs::create_dir_all(dir.0.join("scratch").join(sub))
            .map_err(|_| ReasonClass::UnshareFailed)?;
    }
    let tool_dir = tool
        .parent()
        .ok_or(ReasonClass::UnshareFailed)?
        .to_path_buf();
    let mut tool_dirs = vec![tool_dir];
    tool_dirs.extend(
        ["/usr/bin", "/bin"]
            .iter()
            .map(PathBuf::from)
            .filter(|p| p.is_dir()),
    );
    // Only caller-admitted narrow toolchain resources; never the checkout or HOME.
    let mut read_only = tool_dirs.clone();
    read_only.extend_from_slice(resources);
    let view = View::new(
        &dir.0.join("ro"),
        None,
        tool,
        &read_only,
        &dir.0.join("scratch"),
        Vec::new(),
    );
    let parent = NsInodes::current().map_err(|_| ReasonClass::UnshareFailed)?;
    let mut command: Vec<OsString> = UNSHARE_FLAGS.iter().map(OsString::from).collect();
    command.push(helper.as_os_str().to_owned());
    command.push("ns-helper".into());
    command.extend(view.helper_args(parent, Overlay::None));
    command.push("--".into());
    command.push(tool.as_os_str().to_owned());
    command.extend(args.iter().map(OsString::from));
    let mut env = EnvAllowlist::new();
    let path = std::env::join_paths(&tool_dirs).map_err(|_| ReasonClass::UnshareFailed)?;
    for (k, v) in [
        ("PATH", path),
        ("HOME", OsString::from(format!("{SCRATCH_MOUNT}/home"))),
        ("TMPDIR", OsString::from(format!("{SCRATCH_MOUNT}/tmp"))),
        ("LC_ALL", OsString::from("C")),
    ] {
        env.insert(k, v).map_err(|_| ReasonClass::UnshareFailed)?;
    }
    kaban_core::process::run(Spec {
        exec: wrapper.exec,
        args: command,
        cwd: Path::new("/"),
        env,
        deadline,
        stdout_cap: 64 * 1024,
        stderr_cap: 64 * 1024,
        monitors: Vec::new(),
        join_bound: Duration::from_secs(1),
        inherited_fds: InheritedFds::Refuse,
    })
    .map_err(|_| ReasonClass::UnshareFailed)
}

/// No namespace tool execution is available on this target.
///
/// # Errors
/// Always `handle_exec_unsupported`, before any tool or procfs access.
#[cfg(not(target_os = "linux"))]
pub fn run_tool_in_view(
    _helper: &Path,
    _tool: &Path,
    _args: &[&str],
    _deadline: Duration,
    _resources: &[PathBuf],
) -> Result<kaban_core::process::Outcome, ReasonClass> {
    Err(ReasonClass::HandleExecUnsupported)
}

/// The kernel release without a child: `/proc/sys/kernel/osrelease` (`unknown` when absent).
#[must_use]
pub fn kernel_release() -> String {
    std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .map_or_else(|_| "unknown".to_owned(), |s| s.trim().to_owned())
}

/// The canonical path of this binary (`/proc/self/exe`), passed to `unshare` as the helper.
///
/// # Errors
/// The `readlink` error.
#[cfg(target_os = "linux")]
pub fn self_exe() -> std::io::Result<PathBuf> {
    std::fs::read_link("/proc/self/exe")
}

/// Discover the native helper path; admission and revalidation are separate operations.
///
/// # Errors
/// The standard current-executable discovery error.
#[cfg(not(target_os = "linux"))]
pub fn self_exe() -> std::io::Result<PathBuf> {
    std::env::current_exe()
}

// ---------------------------------------------------------------------------------------
// the helper (inside the namespaces)
// ---------------------------------------------------------------------------------------

/// What the helper reports back on success: the probe's overlay finding; a run never returns
/// (the `exec` replaces the process).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProbeFinding {
    /// Whether overlayfs mounted inside the user namespace.
    pub overlay: Overlay,
}

impl fmt::Display for ProbeFinding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "overlay={}", self.overlay.as_str())
    }
}

/// Build the view, verify it from the inside, then probe or `exec` (see the module
/// documentation). Must run inside the namespaces `unshare` opened; returns only in probe mode
/// or on failure.
///
/// # Errors
/// [`NsError`]: the caller prints [`SELFCHECK_FAILED_TEXT`] and exits [`HELPER_EXIT`].
#[cfg(target_os = "linux")]
pub use linux::run_helper;

/// Namespace setup is unavailable on this target; never return a successful view.
///
/// # Errors
/// Always an unsupported setup error.
#[cfg(not(target_os = "linux"))]
pub fn run_helper(_opts: &HelperOpts) -> Result<ProbeFinding, NsError> {
    Err(NsError::Setup {
        op: "namespaces_unsupported",
        source: Errno::NOSYS,
    })
}

/// The mount points (field 5, octal escapes decoded) of a `/proc/self/mountinfo` text.
#[must_use]
pub fn mount_points(mountinfo: &str) -> BTreeSet<PathBuf> {
    mountinfo
        .lines()
        .filter_map(|l| l.split(' ').nth(4))
        .map(|f| PathBuf::from(unescape_octal(f)))
        .collect()
}

fn unescape_octal(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\'
            && i + 4 <= bytes.len()
            && s.is_char_boundary(i + 4)
            && let Ok(v) = u8::from_str_radix(&s[i + 1..i + 4], 8)
        {
            out.push(v);
            i += 4;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}
