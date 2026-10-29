//! The run monitors (stage1-spec G1-S5.6 quotas, G1-N6 indexer peak RSS; stage1-plan CG4.3):
//! [`QuotaMonitor`] walks every child-writable root every 250 ms and enforces the per-file and
//! total caps over both the apparent size and the allocated blocks (a sparse file counts by
//! whichever is larger: `st_size` catches a hole-punched giant, `st_blocks` a dense one);
//! [`RssMonitor`] reads `VmHWM` of the direct child from `/proc/<pid>/status` every 50 ms and
//! keeps the peak behind a shared cell the runner reads after the run.
//!
//! Both implement `kaban_core::process::Monitor`: a sample runs on the runner's worker thread,
//! an `Err` kills the group with the fixed class (`quota_exceeded`, or `quota_unobservable` when
//! the walk itself failed, U2 RU2-7), and the runner takes one final sample at stop (U1
//! residual 4), so a run shorter than the interval is still walked. A directory the child made
//! unlistable (mode 000: the child runs as the invoking user, so every entry is the user's own)
//! has the owner's permission restored and is walked; an entry that vanished between the
//! listing and its `stat` is the one tolerated error.

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use kaban_core::process::{KillReason, Monitor};
use rustix::io::Errno;

/// The sampler period of S5.6.
pub const QUOTA_INTERVAL: Duration = Duration::from_millis(250);
/// The RSS sampler period: finer than the quota walk (one `/proc` read) because the final
/// sample at stop comes after the reap, when `/proc/<pid>` is gone, so a short-lived indexer
/// is only observed by a periodic sample (CG4.3 deviation from "at the quota interval").
pub const RSS_INTERVAL: Duration = Duration::from_millis(50);
/// The fixed kill class of a quota excess (exit 6).
pub const QUOTA_EXCEEDED: &str = "quota_exceeded";
/// The fixed kill class of a scratch that could not be measured (exit 6, fatal: U2 RU2-7).
pub const QUOTA_UNOBSERVABLE: &str = "quota_unobservable";

/// The scratch quotas of one run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuotaCaps {
    /// Per file (`index.quota.file_bytes`, default 1 GiB).
    pub file_bytes: u64,
    /// Over every child-writable root together (`index.quota.run_dir_bytes`, default 4 GiB;
    /// on the container tier the reserved tmpfs bound is subtracted by the caller).
    pub total_bytes: u64,
}

/// The scratch walker: every child-writable path, the caches included (RS3-5).
#[derive(Debug)]
pub struct QuotaMonitor {
    roots: Vec<PathBuf>,
    caps: QuotaCaps,
    /// The largest total observed (bytes), for the report.
    observed_total: Arc<AtomicU64>,
}

impl QuotaMonitor {
    /// A monitor over `roots` under `caps`.
    #[must_use]
    pub fn new(roots: Vec<PathBuf>, caps: QuotaCaps) -> Self {
        Self {
            roots,
            caps,
            observed_total: Arc::new(AtomicU64::new(0)),
        }
    }

    /// A handle on the largest total the monitor observed (read after the run).
    #[must_use]
    pub fn observed_total(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.observed_total)
    }

    /// One walk over every root: the per-file maximum and the total, both as
    /// `max(st_size, st_blocks * 512)` over every non-directory entry (regular files, FIFOs,
    /// sockets and device nodes alike); symlinks are not followed, directories contribute
    /// nothing. An entry that disappeared between the listing and its `stat` is skipped; a
    /// directory the owner cannot list gets `u+rwx` and is retried once; every other error is
    /// the walk's error (`quota_unobservable` for the monitor).
    ///
    /// # Errors
    /// The `errno` of the first unrecoverable `read_dir` or entry error.
    pub fn measure(&self) -> Result<Usage, Errno> {
        let mut usage = Usage::default();
        for root in &self.roots {
            walk(root, &mut usage)?;
        }
        Ok(usage)
    }
}

/// What one walk measured.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    /// The largest single file in bytes.
    pub largest_file: u64,
    /// The sum over every file in bytes.
    pub total: u64,
}

fn errno(e: &std::io::Error) -> Errno {
    Errno::from_io_error(e).unwrap_or(Errno::IO)
}

fn walk(dir: &Path, usage: &mut Usage) -> Result<(), Errno> {
    use std::io::ErrorKind;
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(()),
        Err(e) if e.kind() == ErrorKind::PermissionDenied => {
            // the child's own mode-000 directory: the owner's bits are restored (the owner is
            // the invoking user: a child cannot create what the user cannot own)
            recover_owner_access(dir)?;
            std::fs::read_dir(dir).map_err(|e| errno(&e))?
        }
        Err(e) => return Err(errno(&e)),
    };
    for entry in entries {
        let entry = entry.map_err(|e| errno(&e))?;
        let meta = match entry.metadata() {
            Ok(meta) => meta,
            Err(e) if e.kind() == ErrorKind::NotFound => continue,
            Err(e) => return Err(errno(&e)),
        };
        if meta.file_type().is_dir() {
            walk(&entry.path(), usage)?;
        } else {
            let bytes = meta.len().max(meta.blocks().saturating_mul(512));
            usage.largest_file = usage.largest_file.max(bytes);
            usage.total = usage.total.saturating_add(bytes);
        }
    }
    Ok(())
}

/// Add the owner's `rwx` to `dir` (listed as a directory by its parent, never a symlink); only
/// the owner's bits change, nothing is taken from anyone.
fn recover_owner_access(dir: &Path) -> Result<(), Errno> {
    use std::os::unix::fs::PermissionsExt;
    let meta = std::fs::symlink_metadata(dir).map_err(|e| errno(&e))?;
    if !meta.file_type().is_dir() {
        return Err(Errno::NOTDIR);
    }
    let mode = meta.permissions().mode() & 0o7777;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(mode | 0o700))
        .map_err(|e| errno(&e))
}

impl Monitor for QuotaMonitor {
    fn sample(&mut self) -> Result<(), KillReason> {
        let usage = self.measure().map_err(|_| KillReason::Monitor {
            class: QUOTA_UNOBSERVABLE,
        })?;
        // the cell carries a report figure, not a synchronisation: Relaxed suffices
        self.observed_total
            .fetch_max(usage.total, Ordering::Relaxed);
        if usage.largest_file > self.caps.file_bytes || usage.total > self.caps.total_bytes {
            return Err(KillReason::Monitor {
                class: QUOTA_EXCEEDED,
            });
        }
        Ok(())
    }

    fn interval(&self) -> Duration {
        QUOTA_INTERVAL
    }
}

/// `VmHWM` of the indexer process, sampled every [`RSS_INTERVAL`]; the peak survives the
/// child (a sample after the exit finds no `/proc/<pid>` and keeps the last peak). The runner
/// spawns inside `kaban_core::process::run`, so the child's pid is not known when the monitor
/// is built: the monitor binds at its first sample to the one live process whose parent is
/// this process and whose process group is its own pid (the group leader the runner just
/// started; runs are sequential, so exactly one exists), then follows `hops` single-child
/// links down (the `namespaces` tier runs `unshare --fork`, whose one child is the helper
/// that `exec`s into the indexer: one hop; CG4.4). The container tier has no process to read
/// (`n/a`).
#[derive(Debug)]
pub struct RssMonitor {
    pid: Option<u32>,
    hops: u8,
    peak: Arc<AtomicU64>,
}

impl Default for RssMonitor {
    fn default() -> Self {
        Self::new()
    }
}

impl RssMonitor {
    /// A monitor that binds to the runner's direct child at its first sample.
    #[must_use]
    pub fn new() -> Self {
        Self::through(0)
    }

    /// A monitor that binds `hops` single-child links below the runner's direct child.
    #[must_use]
    pub fn through(hops: u8) -> Self {
        Self {
            pid: None,
            hops,
            peak: Arc::new(AtomicU64::new(0)),
        }
    }

    /// A handle on the peak in bytes (0 until the first successful sample).
    #[must_use]
    pub fn peak(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.peak)
    }
}

/// `VmHWM` in bytes from a `/proc/<pid>/status` text.
#[must_use]
pub fn vm_hwm_bytes(status: &str) -> Option<u64> {
    status
        .lines()
        .find_map(|l| l.strip_prefix("VmHWM:"))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|kb| kb.parse::<u64>().ok())
        .map(|kb| kb * 1024)
}

/// The pid of the one live process whose parent is this process and which leads its own
/// process group (`/proc/<pid>/stat` fields 4 and 5 after the parenthesised `comm`).
fn group_leader_child() -> Option<u32> {
    child_of(std::process::id(), true)
}

/// The one live child of `parent` (`leader`: that also leads its own process group); `None`
/// when there is none or more than one (ambiguous: bind at a later sample).
fn child_of(parent_pid: u32, leader: bool) -> Option<u32> {
    let mut found = None;
    for entry in std::fs::read_dir("/proc").ok()?.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        let Some(after) = stat.rsplit(')').next() else {
            continue;
        };
        let mut fields = after.split_whitespace();
        let (Some(parent), Some(group)) = (fields.nth(1), fields.next()) else {
            continue;
        };
        if parent.parse::<u32>().ok() == Some(parent_pid)
            && (!leader || group.parse::<u32>().ok() == Some(pid))
        {
            if found.is_some() {
                return None; // ambiguous: bind at a later sample
            }
            found = Some(pid);
        }
    }
    found
}

impl Monitor for RssMonitor {
    fn sample(&mut self) -> Result<(), KillReason> {
        if self.pid.is_none() {
            let mut pid = group_leader_child();
            for _ in 0..self.hops {
                pid = pid.and_then(|p| child_of(p, false));
            }
            self.pid = pid;
        }
        let Some(pid) = self.pid else {
            return Ok(());
        };
        if let Some(bytes) = std::fs::read_to_string(format!("/proc/{pid}/status"))
            .ok()
            .as_deref()
            .and_then(vm_hwm_bytes)
        {
            self.peak.fetch_max(bytes, Ordering::Relaxed);
        }
        Ok(())
    }

    fn interval(&self) -> Duration {
        RSS_INTERVAL
    }
}
