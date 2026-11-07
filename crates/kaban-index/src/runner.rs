//! The indexer runner (stage1-spec G1-S5.3 clean environment, S5.4 process control, S5.6
//! quotas, S5.7 output gating; stage1-plan CG4.3, CG4.4): one indexer as a one-shot child of
//! `kaban_core::process::run` on the selected tier, with the S5.3 environment, the scratch as
//! the only writable place, the quota and RSS monitors, the deadline and output caps, and the
//! captured streams scanned through the policy before a single byte reaches
//! `<repo-id>/<commit40>.<lang>.<a|b>.log` (0600, one per run, opened through the admitted
//! directory handle).
//! The `none` tier uses the admitted cwd (Darwin: stdin handoff to a revalidated self-helper,
//! fchdir and exec with null stdin). The `namespaces` tier runs the trusted `unshare`
//! handle, which starts `kaban-index ns-helper` inside the new namespaces (`isolate::namespaces`);
//! the `container` tier is deferred past 3.0.0 (OWN-68 SC-11) and refused with a fixed class.
//!
//! ```text
//!   run_indexer(IndexerSpec { isolation: IsolationRun, .. })
//!     None       ==> process::run { Exec::Path(indexer), cwd = checkout, env_for(Host) }
//!     Namespaces ==> process::run { Exec::Handle(unshare), args = flags + helper + view + indexer,
//!                                   env_for(Namespaces) }  helper exit 8 + fixed text = exit 8
//!     Container  ==> Err(TierNotImplemented)               (deferred, OWN-68)
//!     monitors: QuotaMonitor(scratch roots, caps), RssMonitor (the indexer process)
//!     gate(stdout), gate(stderr) ==> openat(dir, <commit>.<lang>.<a|b>.log, EXCL|NOFOLLOW, 0600)
//!     Outcome ==> RunReport { class: precedence self-check > kill > descendants > silent > verdict > exit }
//! ```
//!
//! The report's `class` is a fixed string; nothing from the child is in it. The raw files
//! under the scratch are deleted by the caller's `Scratch` drop whatever the outcome. This
//! file is the crate's one `Command` site (CG2.4 lint `no_spawn_outside_runner`): the helper's
//! final `exec` into the indexer ([`exec_indexer`]) lives here for that reason.

use std::ffi::OsString;
use std::os::fd::AsFd;
#[cfg(target_os = "linux")]
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::atomic::Ordering;
use std::time::Duration;

use kaban_core::process::{
    EnvAllowlist, Exec, InheritedFds, KillReason, MonitorVerdict, Outcome, ProcessError, Spec,
    TrustedInternal,
};
use kaban_policy::Policy;
use rustix::fs::{Mode, OFlags};
use serde::Serialize;
use thiserror::Error;

use crate::detect::Lang;
use crate::identity_cli::AdmittedDir;
use crate::isolate::namespaces::{
    self, HELPER_EXIT, NsInodes, Overlay, SELFCHECK_FAILED_TEXT, UNSHARE_FLAGS, View,
};
use crate::isolate::{PathView, ResolvedWrapper, Scratch, Tier, WrapperError};
use crate::language::rust;
use crate::quota::{QUOTA_EXCEEDED, QUOTA_UNOBSERVABLE, QuotaMonitor};
use crate::tools::{self, AdmittedTool, ToolIdentity};
use crate::writer::Commit;
use kaban_codegraph::identity::{self as identity, DirHandle};

#[cfg(target_os = "linux")]
use crate::quota::RssMonitor;

pub use crate::quota::QuotaCaps;

/// The default output cap per stream (S5.4: 10 MiB).
pub const OUTPUT_CAP: usize = 10 * 1024 * 1024;
/// The teardown budget after a kill or an exit (reap, readers, the monitors' final samples).
pub const JOIN_BOUND: Duration = Duration::from_secs(2);
/// The characters of one scan window over a captured line (S5.7 overlapping windows).
const GATE_WINDOW: usize = 4096;

/// Everything one indexer run needs.
pub struct IndexerSpec<'a> {
    /// The language (selects the S5.3 block and the log name).
    pub lang: Lang,
    /// Which run of the language this is: the log is `<commit40>.<lang>.<a|b>.log`, so Go's
    /// second run never replaces the first run's log.
    pub run: RunSlot,
    /// The resolved indexer (absolute, `fstat`-checked at resolution; executed by path: the
    /// handle rule is the wrappers' contract, indexers are the pinned toolchain's files).
    pub indexer: &'a Path,
    /// `argv[1..]`, kaban-built.
    pub args: Vec<OsString>,
    /// The checkout: the child's working directory.
    pub repo_root: &'a Path,
    /// The pinned tool path: the directories of the indexer and the language toolchain.
    pub tool_dirs: Vec<PathBuf>,
    /// The run's scratch (the only writable place).
    pub scratch: &'a Scratch,
    /// The hard deadline (`index.deadline_secs`).
    pub deadline: Duration,
    /// Bytes kept per stream; more kills the group (`output_cap`).
    pub output_cap: usize,
    /// The scratch quotas.
    pub quota: QuotaCaps,
    /// The admitted `<repo-id>` directory: the log is created through its handle.
    pub data_dir: &'a AdmittedDir,
    /// The indexed commit (the log name).
    pub commit: Commit,
    /// The gate over the captured streams.
    pub policy: &'a Policy,
    /// Descriptor admission (`Refuse` for indexers; a test launched under `flock(1)` passes
    /// `Tolerate` explicitly).
    pub inherited_fds: InheritedFds,
    /// Go: the driver's admitted inputs (the module cache, the vendor mode); the default for
    /// the other languages.
    pub go: GoRun<'a>,
    /// Rust: the admitted system C compiler cargo links with, by its canonical path
    /// (`CARGO_TARGET_<TRIPLE>_LINKER`); the driver's value, never a constant. `None` for the
    /// other languages.
    pub linker: Option<&'a Path>,
    /// The tier this run executes on, with what that tier needs (`type-enum-states`: the
    /// namespaces inputs exist exactly when the tier is `namespaces`).
    pub isolation: IsolationRun<'a>,
    /// The identity the indexer was admitted with (U2 RU2-3): re-checked immediately before
    /// the spawn (`RunError::IndexerChanged`) and by the helper before its `exec`; `None` for a
    /// caller that did not admit (library tests).
    pub indexer_identity: Option<ToolIdentity>,
    /// The toolchain's read-only resources (the admitted root and its symlinked entries, U2
    /// RU2-2): bound inside the namespaces view, never on `PATH`.
    pub resource_dirs: Vec<PathBuf>,
    /// The checkout handle admitted at the start (U2 RU2-5): `repo_root` must still lead to
    /// this object at the spawn (`RunError::CheckoutReplaced`); on the `none` tier the child's
    /// working directory is the object itself (Linux procfs or native directory handoff); on the namespaces tier
    /// the helper re-checks the `(dev, ino)` of its bind source before mounting.
    pub checkout: Option<&'a DirHandle>,
}

/// The selected tier with its run inputs.
#[derive(Debug)]
#[non_exhaustive]
pub enum IsolationRun<'a> {
    /// The native child (S5.1 `none`).
    None,
    /// The `namespaces` tier (CG4.4).
    Namespaces(NsRun<'a>),
    /// Reserved: deferred past 3.0.0 (OWN-68 SC-11); `run_indexer` refuses it.
    Container,
}

impl IsolationRun<'_> {
    /// The `meta.isolation` tier.
    #[must_use]
    pub const fn tier(&self) -> Tier {
        match self {
            Self::None => Tier::None,
            Self::Namespaces(_) => Tier::Namespaces,
            Self::Container => Tier::Container,
        }
    }
}

/// What a `namespaces` run needs beyond the [`IndexerSpec`]: the verified `unshare` handle,
/// the helper binary, the overlay mechanism the probe found, the linked worktree's common
/// directory (read-only in the view when it lies outside the checkout) and the user caches to
/// share read-only.
#[derive(Debug)]
pub struct NsRun<'a> {
    /// The `unshare` handle and its candidate path (S5.2).
    pub wrapper: ResolvedWrapper,
    /// The `kaban-index` binary that runs `ns-helper` (the canonical `/proc/self/exe`).
    pub helper: PathBuf,
    /// `overlay=overlayfs | none`, as the probe recorded.
    pub overlay: Overlay,
    /// The common directory of a linked worktree, if any.
    pub common_dir: Option<&'a Path>,
}

impl<'a> NsRun<'a> {
    /// Resolve the `unshare` handle and bind `helper`; `overlay` defaults to `none` until the
    /// probe's finding is passed (`with_overlay`).
    ///
    /// # Errors
    /// [`WrapperError`] when no acceptable `unshare` candidate exists.
    pub fn prepare(helper: &Path, common_dir: Option<&'a Path>) -> Result<Self, WrapperError> {
        Ok(Self {
            wrapper: namespaces::resolve_unshare()?,
            helper: helper.to_path_buf(),
            overlay: Overlay::None,
            common_dir,
        })
    }

    /// Record the probe's overlay finding.
    #[must_use]
    pub fn with_overlay(mut self, overlay: Overlay) -> Self {
        self.overlay = overlay;
        self
    }
}

/// The Go inputs of one run as the environment spells them (G1): the admitted module cache is
/// `GOMODCACHE` on both tiers (its host path; an isolating view binds it read-only there), a
/// dedicated empty directory under the scratch when the caller has none; `vendored` selects
/// `GOFLAGS=-mod=vendor`; `goroot` the module-cache toolchain's root and `toolchain` its
/// admitted `bin/go`, rechecked immediately before every spawn (G2, gofix1-3). The driver's
/// values, never constants.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GoRun<'a> {
    /// The caller's admitted module cache, by its canonical host path.
    pub modcache: Option<&'a Path>,
    /// The repository has `vendor/modules.txt`.
    pub vendored: bool,
    /// The root of the module-cache toolchain admitted for the run (G2): the child's
    /// `GOROOT` (its `bin` is first in `tool_dirs`); `None` when the installed `go` runs.
    pub goroot: Option<&'a Path>,
    /// The module-cache toolchain's admitted `bin/go`: the object at that path must still be
    /// the admitted one at every spawn, run B included ([`RunError::ToolchainChanged`]).
    pub toolchain: Option<&'a AdmittedTool>,
}

/// Which run of a language a log belongs to: `a` is the first (the only one for TypeScript),
/// `b` is Go's `--skip-tests` run (G1-F3). The log name is `<commit40>.<lang>.<a|b>.log`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunSlot {
    /// The first run.
    A,
    /// Go's second run (`--skip-tests`).
    B,
}

impl RunSlot {
    /// The letter in the log name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::A => "a",
            Self::B => "b",
        }
    }
}

/// The fixed outcome class of a run that did not end `ok`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunClass {
    /// The indexer exited non-zero.
    IndexerFailed,
    /// The deadline killed the group.
    IndexerTimeout,
    /// A stream passed its cap (exit 6).
    OutputCap,
    /// The scratch quota was exceeded (exit 6).
    QuotaExceeded,
    /// The scratch could not be measured (a walk error other than a disappearance race, U2
    /// RU2-7): the run is killed and the class is fatal (exit 6).
    QuotaUnobservable,
    /// Another monitor asked for the kill, or failed at its final sample.
    MonitorFailed,
    /// A monitor produced no verdict by the join (U1 residual 4): never `ok`.
    MonitorSilent,
    /// The process group still had a member when the join expired (U1 residual 4).
    DescendantsSurvived,
    /// The `ns-helper` self-check failed or the view could not be built (exit 8, S5.2): an
    /// anomaly after a passed probe, refused under every policy.
    IsolationSelfcheckFailed,
}

impl RunClass {
    /// The fixed text (`failed:` prefixed for the monitor classes, as the plan spells them).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::IndexerFailed => "indexer_failed",
            Self::IndexerTimeout => "indexer_timeout",
            Self::OutputCap => "output_cap",
            Self::QuotaExceeded => "quota_exceeded",
            Self::QuotaUnobservable => "quota_unobservable",
            Self::MonitorFailed => "failed:monitor",
            Self::MonitorSilent => "failed:monitor_silent",
            Self::DescendantsSurvived => "descendants_survived",
            Self::IsolationSelfcheckFailed => "isolation_selfcheck_failed",
        }
    }

    /// The exit code of §7: `output_cap`, `quota_exceeded` and `indexer_failed` are 6,
    /// `indexer_timeout` 7, `isolation_selfcheck_failed` 8, the monitor and group classes 6
    /// (the indexer did not end `ok`).
    #[must_use]
    pub const fn exit_code(self) -> u8 {
        match self {
            Self::IndexerTimeout => 7,
            Self::IsolationSelfcheckFailed => 8,
            Self::OutputCap
            | Self::QuotaExceeded
            | Self::QuotaUnobservable
            | Self::IndexerFailed
            | Self::MonitorFailed
            | Self::MonitorSilent
            | Self::DescendantsSurvived => 6,
        }
    }

    /// A class that ends the whole `index` run whatever `--partial` says (U2 RU2-9, G1-S5.2
    /// "exit 8 under every policy", §7): an isolation refusal is an anomaly of the boundary,
    /// not a failure of one language's indexer, and so is a quota that could not be observed
    /// (`quota_unobservable`, RU2-7) or a monitor that failed or stayed silent. The
    /// per-language classes (`indexer_failed`, `indexer_timeout`, `output_cap`,
    /// `quota_exceeded`) may be skipped under `--partial`.
    #[must_use]
    pub const fn is_fatal(self) -> bool {
        matches!(
            self,
            Self::IsolationSelfcheckFailed
                | Self::QuotaUnobservable
                | Self::MonitorFailed
                | Self::MonitorSilent
        )
    }
}

/// What one run produced.
#[derive(Debug)]
pub struct RunReport {
    /// The tier that ran.
    pub tier: Tier,
    /// The child's exit status (`None` when the reap did not complete within the bound).
    pub status: Option<ExitStatus>,
    /// The fixed failure class; `None` means `ok`.
    pub class: Option<RunClass>,
    /// Why the group was killed, if it was.
    pub killed: Option<KillReason>,
    /// `<repo-id>/<commit40>.<lang>.<a|b>.log`, gated, 0600.
    pub log_path: PathBuf,
    /// Linux `VmHWM` in bytes; `None` on Darwin (unavailable) or when never sampled.
    /// Report-only: file and scratch quotas remain enforced on every native run.
    pub peak_rss_bytes: Option<u64>,
    /// The largest scratch total the quota monitor observed, in bytes.
    pub observed_scratch_bytes: u64,
    /// One verdict per installed monitor (quota, then Linux report-only RSS).
    pub monitor_verdicts: Vec<MonitorVerdict>,
    /// The group still had a member when the join expired.
    pub descendants_survived: bool,
    /// Fixed warning classes.
    pub warnings: Vec<&'static str>,
    /// Wall time of the child.
    pub wall: Duration,
    /// The effective cgo setting of a Go run (U2 RU2-12; see [`cgo_setting`]); `None` for
    /// other languages.
    pub cgo_enabled: Option<&'static str>,
}

impl RunReport {
    /// How the run died, for the two classes a developer has to diagnose from the child's side
    /// (`indexer_failed`, `indexer_timeout`); `None` for `ok` and for the monitor and group
    /// classes, whose cause is the class itself.
    #[must_use]
    pub fn failure(&self) -> Option<RunFailure> {
        use std::os::unix::process::ExitStatusExt;
        matches!(
            self.class,
            Some(RunClass::IndexerFailed | RunClass::IndexerTimeout)
        )
        .then(|| RunFailure {
            signal: self.status.and_then(|s| s.signal()),
            exit_code: self.status.and_then(|s| s.code()),
            peak_rss_bytes: self.peak_rss_bytes,
            observed_scratch_bytes: self.observed_scratch_bytes,
            quota_breached: self.monitor_verdicts.iter().any(|v| {
                matches!(v, MonitorVerdict::Failed(KillReason::Monitor { class }) if *class == QUOTA_EXCEEDED)
            }),
            wall_ms: self.wall.as_millis(),
        })
    }
}

/// How a run that ended `indexer_failed` or `indexer_timeout` died (backlog 22): what tells the
/// host's OOM killer (signal 9, no quota breach) from a crash (an exit code) and from kaban's
/// own kills. The JSON report carries it per language (`failure_per_indexer`), the stderr line
/// spells it out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct RunFailure {
    /// The signal that ended the direct child; `None` when it exited.
    pub signal: Option<i32>,
    /// The exit code; `None` when a signal ended the child or the reap did not complete.
    pub exit_code: Option<i32>,
    /// `VmHWM` of the direct child in bytes (`None` when never sampled).
    pub peak_rss_bytes: Option<u64>,
    /// The largest scratch total the quota monitor observed, in bytes.
    pub observed_scratch_bytes: u64,
    /// The quota monitor asked for the kill or failed at its final sample. A breach classifies
    /// the run `quota_exceeded` before these two classes, so this is `false` for them today:
    /// stated, so the report says "no quota breach" instead of leaving it implied.
    pub quota_breached: bool,
    /// Wall time of the child in milliseconds.
    pub wall_ms: u128,
}

/// Why no indexer ran, or why its run could not be recorded.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum RunError {
    /// The tier's runner is not in this release (`container`: deferred, OWN-68 SC-11).
    #[error("tier not implemented: {}", .0.as_str())]
    TierNotImplemented(Tier),
    /// The parent's namespace inodes could not be read (`namespaces` tier).
    #[error(transparent)]
    Namespaces(#[from] namespaces::NsError),
    /// The runner refused before the spawn or the spawn failed.
    #[error(transparent)]
    Process(#[from] ProcessError),
    /// The gated log could not be created through the admitted directory; the errno class.
    #[error("log: {0}")]
    Log(#[source] rustix::io::Errno),
    /// The indexer at the admitted path is not the admitted object any more (U2 RU2-3).
    #[error("indexer changed since admission")]
    IndexerChanged,
    /// The checkout path no longer leads to the admitted checkout object (U2 RU2-5).
    #[error("checkout replaced since admission")]
    CheckoutReplaced,
    /// The module-cache `go` at its admitted path is not the admitted object any more
    /// (gofix1-3: checked at every spawn, the path `GOROOT` and the pinned `PATH` execute).
    #[error("go toolchain changed since admission")]
    ToolchainChanged,
    /// The clean Rust scratch could not be prepared.
    #[error("Rust scratch preparation failed")]
    RustScratch,
    /// Native cwd helper admission or handoff failed; no partial success is permitted.
    #[error("cwd helper refused")]
    CwdHandoff,
}

/// Run one indexer per `spec` on its tier (see the module documentation).
///
/// # Errors
/// [`RunError`].
pub fn run_indexer(spec: IndexerSpec<'_>) -> Result<RunReport, RunError> {
    let tier = spec.isolation.tier();
    if spec.lang == Lang::Rust {
        spec.scratch
            .prepare_rust_environment()
            .map_err(|_| RunError::RustScratch)?;
    }
    // the admitted indexer object is still the object at its path (U2 RU2-3)
    if let Some(identity) = spec.indexer_identity {
        tools::recheck(&AdmittedTool {
            path: spec.indexer.to_path_buf(),
            identity,
        })
        .map_err(|_| RunError::IndexerChanged)?;
    }
    // the module-cache go the run executes is still the admitted object (gofix1-3)
    if let Some(go) = spec.go.toolchain {
        tools::recheck(go).map_err(|_| RunError::ToolchainChanged)?;
    }
    // the checkout path still leads to the admitted checkout object (U2 RU2-5)
    let checkout_id = match spec.checkout {
        Some(handle) => {
            let expected = handle.dev_ino().map_err(|_| RunError::CheckoutReplaced)?;
            let live = identity::walk(spec.repo_root)
                .and_then(|h| h.dev_ino())
                .map_err(|_| RunError::CheckoutReplaced)?;
            if live != expected {
                return Err(RunError::CheckoutReplaced);
            }
            Some(expected)
        }
        None => None,
    };
    // the native child's working directory is the admitted object itself: the descriptor is
    // open across the child's chdir (close-on-exec runs at the exec); the helper is given the
    // expected identity instead and binds only a matching source
    let cwd: PathBuf = match (spec.checkout, &spec.isolation) {
        (Some(handle), IsolationRun::None) => {
            #[cfg(target_os = "linux")]
            {
                PathBuf::from(format!("/proc/self/fd/{}", handle.as_fd().as_raw_fd()))
            }
            #[cfg(not(target_os = "linux"))]
            {
                let _ = handle;
                PathBuf::from("/")
            }
        }
        _ => spec.repo_root.to_path_buf(),
    };
    let (exec, args, view, rss_hops) = match spec.isolation {
        IsolationRun::Container => return Err(RunError::TierNotImplemented(tier)),
        IsolationRun::None => (
            Exec::Path(spec.indexer.to_path_buf()),
            spec.args,
            PathView::Host,
            0,
        ),
        IsolationRun::Namespaces(ns) => {
            // the executable directories (the pinned PATH) and the toolchain resources are
            // both bound read-only; only the former are on PATH (U2 RU2-2); no cache mounts
            // since G1 (the module cache and the registry are resources)
            let mut ro: Vec<PathBuf> = spec.tool_dirs.clone();
            ro.extend(spec.resource_dirs.iter().cloned());
            let view = View::new(
                spec.repo_root,
                ns.common_dir,
                spec.indexer,
                &ro,
                spec.scratch.root(),
                Vec::new(),
            );
            let parent = NsInodes::current()?;
            let mut args: Vec<OsString> = UNSHARE_FLAGS.iter().map(OsString::from).collect();
            args.push(ns.helper.as_os_str().to_owned());
            args.push("ns-helper".into());
            args.extend(view.helper_args(parent, ns.overlay));
            if let Some((dev, ino)) = checkout_id {
                args.push("--expect-checkout".into());
                args.push(format!("{dev},{ino}").into());
            }
            if let Some(id) = spec.indexer_identity {
                args.push("--expect-indexer".into());
                args.push(format!("{},{}", id.dev, id.ino).into());
            }
            args.push("--".into());
            args.push(spec.indexer.as_os_str().to_owned());
            args.extend(spec.args);
            (ns.wrapper.exec, args, PathView::Namespaces, 1)
        }
    };
    #[cfg(target_os = "macos")]
    let native_handoff = tier == Tier::None && spec.checkout.is_some();
    #[cfg(target_os = "macos")]
    let (exec, args) = if native_handoff {
        let expected = checkout_id.ok_or(RunError::CwdHandoff)?;
        let indexer = match spec.indexer_identity {
            Some(identity) => AdmittedTool {
                path: spec.indexer.to_path_buf(),
                identity,
            },
            None => {
                tools::admit_executable(spec.indexer, &[]).map_err(|_| RunError::IndexerChanged)?
            }
        };
        let helper = tools::admit_executable(
            &std::env::current_exe().map_err(|_| RunError::CwdHandoff)?,
            &[spec.repo_root, &spec.data_dir.path],
        )
        .map_err(|_| RunError::CwdHandoff)?;
        let id = indexer.identity;
        let mut helper_args: Vec<OsString> = vec![
            "cwd-helper".into(),
            "--directory".into(),
            format!("{},{}", expected.0, expected.1).into(),
            "--identity".into(),
            format!(
                "{},{},{},{},{}",
                id.dev, id.ino, id.size, id.ctime_sec, id.ctime_nsec
            )
            .into(),
            "--".into(),
            indexer.path.into_os_string(),
        ];
        helper_args.extend(args);
        tools::recheck(&helper).map_err(|_| RunError::CwdHandoff)?;
        (Exec::Path(helper.path), helper_args)
    } else {
        (exec, args)
    };
    let env = env_for(
        spec.lang,
        &spec.tool_dirs,
        spec.scratch,
        view,
        spec.go,
        spec.linker,
    )?;
    let quota = QuotaMonitor::new(spec.scratch.writable_roots(), spec.quota);
    let observed = quota.observed_total();
    #[allow(unused_mut)] // Linux adds its report-only RSS observer.
    let mut monitors: Vec<Box<dyn kaban_core::process::Monitor>> = vec![Box::new(quota)];
    #[cfg(target_os = "linux")]
    let peak = {
        let rss = RssMonitor::through(rss_hops);
        let peak = rss.peak();
        monitors.push(Box::new(rss));
        peak
    };
    #[cfg(not(target_os = "linux"))]
    let _ = rss_hops; // Linux VmHWM is report-only; no native RSS observer is installed.
    let process_spec = Spec {
        exec,
        args,
        cwd: &cwd,
        env,
        deadline: spec.deadline,
        stdout_cap: spec.output_cap,
        stderr_cap: spec.output_cap,
        monitors,
        join_bound: JOIN_BOUND,
        inherited_fds: spec.inherited_fds,
    };
    #[cfg(target_os = "macos")]
    let outcome = if native_handoff {
        let directory = spec.checkout.ok_or(RunError::CwdHandoff)?;
        kaban_core::process::run_with_directory_handoff(process_spec, directory.as_fd())?
    } else {
        kaban_core::process::run(process_spec)?
    };
    #[cfg(not(target_os = "macos"))]
    let outcome = kaban_core::process::run(process_spec)?;
    #[cfg(target_os = "macos")]
    if native_handoff
        && outcome.killed.is_none()
        && outcome.status.and_then(|s| s.code()) == Some(i32::from(CWD_HELPER_EXIT))
        && String::from_utf8_lossy(&outcome.stderr)
            .lines()
            .any(|l| l == CWD_HELPER_REFUSED)
    {
        return Err(RunError::CwdHandoff);
    }
    // S5.7: scanned before any byte is written; the log is the only artifact of the run
    let log_name = format!(
        "{}.{}.{}.log",
        spec.commit,
        spec.lang.name(),
        spec.run.as_str()
    );
    let log_path = spec.data_dir.path.join(&log_name);
    let mut text = String::from("--- stdout ---\n");
    text.push_str(&gate_text(spec.policy, &outcome.stdout));
    text.push_str("--- stderr ---\n");
    text.push_str(&gate_text(spec.policy, &outcome.stderr));
    write_log(spec.data_dir, &log_name, text.as_bytes())?;
    let class = if tier == Tier::Namespaces && helper_refused(&outcome) {
        Some(RunClass::IsolationSelfcheckFailed)
    } else {
        classify_outcome(&outcome)
    };
    #[cfg(target_os = "linux")]
    let peak_rss_bytes = {
        let bytes = peak.load(Ordering::Relaxed);
        (bytes > 0).then_some(bytes)
    };
    #[cfg(not(target_os = "linux"))]
    let peak_rss_bytes = None; // explicitly unavailable, never a zero-success observation
    let warnings = Vec::new();
    Ok(RunReport {
        tier,
        status: outcome.status,
        class,
        killed: outcome.killed,
        log_path,
        peak_rss_bytes,
        observed_scratch_bytes: observed.load(Ordering::Relaxed),
        monitor_verdicts: outcome.monitor_verdicts,
        descendants_survived: outcome.descendants_survived,
        warnings,
        wall: outcome.wall,
        cgo_enabled: (spec.lang == Lang::Go).then(|| cgo_setting(view)),
    })
}

/// The helper refused before the indexer started: exit [`HELPER_EXIT`] and the fixed text on
/// stderr (an indexer forging both only turns its own failure into a refusal: harmless).
fn helper_refused(outcome: &Outcome) -> bool {
    outcome.killed.is_none()
        && outcome.status.and_then(|s| s.code()) == Some(i32::from(HELPER_EXIT))
        && String::from_utf8_lossy(&outcome.stderr)
            .lines()
            .any(|l| l == SELFCHECK_FAILED_TEXT)
}

/// The helper's final step: replace this process with `indexer` (`argv[1..]` = `args`), the
/// current environment (the clean S5.3 set `unshare` passed through) and the current working
/// directory (the checkout inside the view). Returns only the error.
#[must_use]
pub fn exec_indexer(indexer: &Path, args: &[OsString]) -> std::io::Error {
    use std::os::unix::process::CommandExt;
    std::process::Command::new(indexer).args(args).exec()
}

/// The fixed class of a runner outcome (U1 residual 4 fail-closed rules), in precedence: the
/// kill reason, then a surviving group (`descendants_survived`), then a silent monitor
/// (`failed:monitor_silent`, never `ok`), then a monitor that failed at its final sample (its
/// class), then the exit status; `None` is `ok`.
#[must_use]
pub fn classify_outcome(outcome: &Outcome) -> Option<RunClass> {
    if let Some(reason) = outcome.killed {
        return Some(kill_class(reason));
    }
    if outcome.descendants_survived {
        return Some(RunClass::DescendantsSurvived);
    }
    if outcome.monitor_verdicts.contains(&MonitorVerdict::Silent) {
        return Some(RunClass::MonitorSilent);
    }
    if let Some(MonitorVerdict::Failed(reason)) = outcome
        .monitor_verdicts
        .iter()
        .find(|v| matches!(v, MonitorVerdict::Failed(_)))
    {
        return Some(kill_class(*reason));
    }
    match outcome.status {
        Some(st) if st.success() => None,
        _ => Some(RunClass::IndexerFailed),
    }
}

fn kill_class(reason: KillReason) -> RunClass {
    match reason {
        KillReason::Deadline => RunClass::IndexerTimeout,
        KillReason::OutputCap { .. } => RunClass::OutputCap,
        KillReason::Monitor { class } if class == QUOTA_EXCEEDED => RunClass::QuotaExceeded,
        KillReason::Monitor { class } if class == QUOTA_UNOBSERVABLE => RunClass::QuotaUnobservable,
        KillReason::Monitor { .. } => RunClass::MonitorFailed,
    }
}

/// The captured bytes line by line through the policy (S5.7): a reject hit replaces the line
/// with `[removed: <rule id>]`, a redact hit masks it, long lines are scanned in overlapping
/// windows. Invalid UTF-8 is replaced before the scan (the gate sees what the log will hold).
#[must_use]
pub fn gate_text(policy: &Policy, bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut out = String::with_capacity(text.len());
    for line in text.lines() {
        out.push_str(&gate_line(policy, line));
        out.push('\n');
    }
    out
}

/// One line through the policy (S5.7, U2 RU2-8: the one gate every printed string passes): a
/// reject hit is `[removed: <rule id>]`, a redact hit the masked line, otherwise the line
/// itself; long lines are scanned in overlapping windows.
#[must_use]
pub fn gate_line(policy: &Policy, line: &str) -> String {
    let verdict = policy.scan_windows(line, GATE_WINDOW);
    if verdict.rejected {
        let rule = verdict
            .reject_rules
            .first()
            .map_or("policy", String::as_str);
        format!("[removed: {rule}]")
    } else if verdict.redacted.is_empty() {
        line.to_owned()
    } else {
        verdict.redacted
    }
}

/// Create `<dir>/<name>` through the admitted handle (`O_CREAT|O_EXCL|O_NOFOLLOW`, 0600; an
/// earlier log of the same commit and language is replaced) and write `bytes`.
fn write_log(dir: &AdmittedDir, name: &str, bytes: &[u8]) -> Result<(), RunError> {
    use std::io::Write;
    let _ = rustix::fs::unlinkat(&dir.handle, name, rustix::fs::AtFlags::empty());
    let fd = rustix::fs::openat(
        &dir.handle,
        name,
        OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::WRONLY | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR,
    )
    .map_err(RunError::Log)?;
    let mut file = std::fs::File::from(fd);
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|e| {
            RunError::Log(rustix::io::Errno::from_io_error(&e).unwrap_or(rustix::io::Errno::IO))
        })
}

/// The offline toolchain constants of the Go indexer (SO-8): `GOPROXY` is a `*PROXY*` name and
/// can only enter through a runner's trusted-internal list; the values are constants.
pub const GO_INTERNAL: TrustedInternal = TrustedInternal(&[
    ("GOPROXY", "off"),
    ("GOSUMDB", "off"),
    ("GOTOOLCHAIN", "local"),
]);

/// The offline constants of the TypeScript indexer (S5.3).
pub const TS_INTERNAL: TrustedInternal = TrustedInternal(&[("npm_config_offline", "true")]);

/// Fixed offline Rust build settings; never inherited from the caller.
pub const RUST_INTERNAL: TrustedInternal = TrustedInternal(&[
    ("CARGO_NET_OFFLINE", "true"),
    ("CARGO_BUILD_JOBS", "2"),
    ("CARGO_INCREMENTAL", "0"),
    ("CARGO_PROFILE_DEV_DEBUG", "0"),
    ("CARGO_PROFILE_TEST_DEBUG", "0"),
]);

/// The constants of the pinned git's metadata queries (U2 RU2-4; `identity_cli::git`): no
/// system and no global configuration (the repository's own configuration is still read, with
/// every execution-capable key overridden on the command line), no optional locks, no terminal
/// prompt. `GIT_*` names can only enter through a runner's trusted-internal list (RV-1).
pub const GIT_QUERY_INTERNAL: TrustedInternal = TrustedInternal(&[
    ("GIT_CONFIG_NOSYSTEM", "1"),
    ("GIT_CONFIG_GLOBAL", "/dev/null"),
    ("GIT_OPTIONAL_LOCKS", "0"),
    ("GIT_TERMINAL_PROMPT", "0"),
]);

/// The whole environment of one metadata query through the pinned git: [`GIT_QUERY_INTERNAL`]
/// and `LC_ALL=C`; neither the caller's `PATH` nor `HOME` (git finds no user configuration and
/// no helper through them).
///
/// # Errors
/// [`ProcessError::Env`] from the allowlist (unreachable with these constant names).
pub fn git_query_env() -> Result<EnvAllowlist, ProcessError> {
    let mut env = EnvAllowlist::with_trusted(GIT_QUERY_INTERNAL);
    env.insert("LC_ALL", "C")?;
    Ok(env)
}

/// The S5.3 environment table for `lang`: `PATH` = `tool_dirs` joined (the pinned tool path,
/// never the caller's), `HOME`, `TMPDIR`, `LANG=C.UTF-8`, `LC_ALL=C`, `CARGO_HOME`, then the
/// language block: Go `GOCACHE` (scratch build cache), `GOMODCACHE` (the admitted module cache
/// of `go` by its host path on both tiers, G1: an isolating view binds it read-only there; a
/// dedicated empty directory under the scratch when the caller has none), `GOROOT` only when a
/// module-cache toolchain was admitted for the run (G2), `GOFLAGS=-mod=readonly` (`-mod=vendor` when `go.vendored`), the offline constants, `CGO_ENABLED` as the caller's, else `0` under the
/// namespaces and container views (U2 RU2-12: the view has no C toolchain, autodetection would
/// turn cgo on and change build selection); TypeScript `npm_config_offline=true`,
/// `npm_config_cache` (scratch); `NODE_OPTIONS` is never set; Rust `CARGO_TARGET_DIR`
/// (scratch) and `CARGO_TARGET_<TRIPLE>_LINKER` = `linker`, the system compiler the driver
/// admitted (the view holds no `cc` on `PATH` beyond `/usr/bin`; cargo is told the canonical
/// path on both tiers); the caller's Cargo registry is a resource of the view and a link of
/// the scratch Cargo home, not a cache policy item (DF-2).
///
/// # Errors
/// [`ProcessError::Env`] from the allowlist (unreachable with these constant names).
pub fn env_for(
    lang: Lang,
    tool_dirs: &[PathBuf],
    scratch: &Scratch,
    view: PathView,
    go: GoRun<'_>,
    linker: Option<&Path>,
) -> Result<EnvAllowlist, ProcessError> {
    let scratch_root = match view {
        PathView::Host => scratch.root().to_path_buf(),
        PathView::Namespaces | PathView::Container => PathBuf::from(namespaces::SCRATCH_MOUNT),
    };
    let mut env = match lang {
        Lang::Go => EnvAllowlist::with_trusted(GO_INTERNAL),
        Lang::TypeScript => EnvAllowlist::with_trusted(TS_INTERNAL),
        Lang::Rust => EnvAllowlist::with_trusted(RUST_INTERNAL),
    };
    let path: OsString = match view {
        PathView::Host | PathView::Namespaces => {
            std::env::join_paths(tool_dirs).unwrap_or_default()
        }
        PathView::Container => OsString::from("/usr/local/bin:/usr/bin:/bin"),
    };
    env.insert("PATH", path)?;
    env.insert("HOME", scratch_root.join("home"))?;
    env.insert("TMPDIR", scratch_root.join("tmp"))?;
    env.insert("LANG", "C.UTF-8")?;
    env.insert("LC_ALL", "C")?;
    env.insert("CARGO_HOME", scratch_root.join("cargo-home"))?;
    match lang {
        Lang::Go => {
            env.insert("GOCACHE", scratch_root.join("cache").join("go-build"))?;
            let gomodcache = go.modcache.map_or_else(
                || scratch_root.join("cache").join("gomod"),
                Path::to_path_buf,
            );
            env.insert("GOMODCACHE", gomodcache)?;
            if let Some(root) = go.goroot {
                env.insert("GOROOT", root)?;
            }
            env.insert(
                "GOFLAGS",
                if go.vendored {
                    "-mod=vendor"
                } else {
                    "-mod=readonly"
                },
            )?;
            match (view, std::env::var_os("CGO_ENABLED")) {
                (PathView::Namespaces | PathView::Container, None) => {
                    env.insert("CGO_ENABLED", "0")?;
                }
                _ => env.carry("CGO_ENABLED")?,
            }
        }
        Lang::TypeScript => {
            env.insert("npm_config_cache", scratch_root.join("cache").join("npm"))?;
        }
        Lang::Rust => {
            env.insert("CARGO_TARGET_DIR", scratch_root.join("target"))?;
            if let Some(cc) = linker {
                env.insert(&rust::linker_variable(), cc)?;
            }
        }
    }
    Ok(env)
}

/// The effective cgo setting of a Go run as a fixed word (U2 RU2-12; never the caller's text):
/// `0` or `1` when the caller set exactly that, `0` for unset under the namespaces and container
/// views, `autodetect` for unset on the `none` tier, `caller` for any other caller value.
#[must_use]
pub fn cgo_setting(view: PathView) -> &'static str {
    match (std::env::var_os("CGO_ENABLED"), view) {
        (Some(v), _) if v == "0" => "0",
        (Some(v), _) if v == "1" => "1",
        (Some(_), _) => "caller",
        (None, PathView::Namespaces | PathView::Container) => "0",
        (None, PathView::Host) => "autodetect",
    }
}

/// Reserved native-helper failure status (not a producer status).
pub const CWD_HELPER_EXIT: u8 = 125;
/// Fixed native-helper marker, captured and gated by the runner.
pub const CWD_HELPER_REFUSED: &str = "cwd_helper_refused";

/// Validate the inherited directory, enter it in this child only, revalidate the complete
/// producer identity, then replace this helper with that producer and null stdin.
/// Returns only on refusal; diagnostics never include caller input.
///
/// # Errors
/// A fixed helper refusal. Successful exec never returns.
pub fn exec_cwd_helper(
    directory: &str,
    identity: &str,
    exec: &[OsString],
) -> Result<(), &'static str> {
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::process::CommandExt;
    let parse = || -> Option<((u64, u64), ToolIdentity)> {
        let (dev, ino) = directory.split_once(',')?;
        let fields: Vec<_> = identity.split(',').collect();
        if fields.len() != 5 {
            return None;
        }
        Some((
            (dev.parse().ok()?, ino.parse().ok()?),
            ToolIdentity {
                dev: fields[0].parse().ok()?,
                ino: fields[1].parse().ok()?,
                size: fields[2].parse().ok()?,
                ctime_sec: fields[3].parse().ok()?,
                ctime_nsec: fields[4].parse().ok()?,
            },
        ))
    };
    let (expected, identity) = parse().ok_or(CWD_HELPER_REFUSED)?;
    let (executable, args) = exec.split_first().ok_or(CWD_HELPER_REFUSED)?;
    let input = std::io::stdin();
    let st = rustix::fs::fstat(&input).map_err(|_| CWD_HELPER_REFUSED)?;
    #[cfg(target_os = "macos")]
    let dev = u64::from(u32::from_ne_bytes(st.st_dev.to_ne_bytes()));
    #[cfg(not(target_os = "macos"))]
    let dev = st.st_dev;
    if !rustix::fs::FileType::from_raw_mode(st.st_mode).is_dir() || (dev, st.st_ino) != expected {
        return Err(CWD_HELPER_REFUSED);
    }
    rustix::process::fchdir(&input).map_err(|_| CWD_HELPER_REFUSED)?;
    let actual = std::fs::metadata(".").map_err(|_| CWD_HELPER_REFUSED)?;
    if (actual.dev(), actual.ino()) != expected {
        return Err(CWD_HELPER_REFUSED);
    }
    let tool = AdmittedTool {
        path: PathBuf::from(executable),
        identity,
    };
    tools::recheck(&tool).map_err(|_| CWD_HELPER_REFUSED)?;
    let _error = std::process::Command::new(&tool.path)
        .args(args)
        .stdin(std::process::Stdio::null())
        .exec();
    Err(CWD_HELPER_REFUSED)
}
