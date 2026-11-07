//! `kaban-index gc` (stage1-spec G1-F7, §7 left-over cleanup, RS-3; stage1-plan CG4.6): keep
//! the snapshot of `HEAD` plus the `N` most recent by commit date and collect the rest. The
//! commit dates come from one `git log --format=%H --no-walk --ignore-missing <names>` run by
//! the caller (reverse chronological, unknown names dropped); commits the repository does not
//! know sort last and are collected first. `--prune-corrupt` adds the snapshots whose fresh open
//! fails (`PRAGMA quick_check`, unreadable `meta`); `--stale-policy` adds those whose
//! `policy_version` is not the current policy's hash (hashes have no order: "older" is
//! "different"). Every `gc` also sweeps the left-overs of killed runs (`<commit>.cg.db.tmp-<pid>`
//! and `run-<pid>` whose pid is dead), as §7 asks of the next `index`, `gc` or `purge-derived`.
//!
//! ```text
//!   final_commits(dir) + git log --no-walk --ignore-missing ==> plan(snapshots, head, order, keep)
//!       ==> Plan { keep: [c..], collect: [(c, beyond_keep | unknown_commit)] }
//!   corrupt(dir, ..) / stale(dir, .., policy_version) ==> more (c, reason)
//!   apply(&dir, &plan.collect) ==> unlink <c>.cg.db, <c>.cg.db.unsynced, <c>.<lang>.<a|b>.log
//! ```
//!
//! The planner is pure and unit-tested here; the deletions go through the admitted handle.

use std::collections::BTreeSet;

use kaban_codegraph::{CodeGraphReader, SnapshotError, SqliteSnapshot};
use rustix::fs::{AtFlags, Dir, Mode, OFlags};
use serde::Serialize;
use std::os::fd::AsFd;
use std::os::unix::ffi::OsStrExt;
use thiserror::Error;

use crate::detect::Lang;
use crate::identity_cli::AdmittedDir;

/// Why a snapshot is collected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    /// Older than the `N` most recent (and not HEAD's).
    BeyondKeep,
    /// The repository does not know the commit (sorted last, collected first).
    UnknownCommit,
    /// `--prune-corrupt`: the fresh open failed.
    Corrupt,
    /// `--stale-policy`: `policy_version` differs from the current policy's.
    StalePolicy,
}

impl Reason {
    /// The fixed text.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::BeyondKeep => "beyond_keep",
            Self::UnknownCommit => "unknown_commit",
            Self::Corrupt => "corrupt",
            Self::StalePolicy => "stale_policy",
        }
    }
}

/// What `gc` will do.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Plan {
    /// The snapshots kept, most recent first (HEAD's first when present).
    pub keep: Vec<String>,
    /// The snapshots collected with the reason, in collection order.
    pub collect: Vec<(String, Reason)>,
}

/// Plan over `snapshots` (the commits with a final name): `head` is kept when it has a
/// snapshot, then the first `keep` of the remaining known commits in `order` (the git output,
/// most recent first); everything else is collected, unknown commits first.
#[must_use]
pub fn plan(snapshots: &[String], head: Option<&str>, order: &[String], keep: usize) -> Plan {
    let present: BTreeSet<&str> = snapshots.iter().map(String::as_str).collect();
    let mut plan = Plan::default();
    let mut decided: BTreeSet<&str> = BTreeSet::new();
    if let Some(h) = head.filter(|h| present.contains(h)) {
        plan.keep.push(h.to_owned());
        decided.insert(h);
    }
    let mut kept = 0;
    let mut beyond = Vec::new();
    for c in order {
        let c = c.as_str();
        if !present.contains(c) || decided.contains(c) {
            continue;
        }
        decided.insert(c);
        if kept < keep {
            plan.keep.push(c.to_owned());
            kept += 1;
        } else {
            beyond.push(c.to_owned());
        }
    }
    for c in snapshots {
        if !decided.contains(c.as_str()) {
            plan.collect.push((c.clone(), Reason::UnknownCommit));
        }
    }
    plan.collect
        .extend(beyond.into_iter().map(|c| (c, Reason::BeyondKeep)));
    plan
}

/// Among `commits`, those whose fresh open fails as corrupt (`quick_check`, unreadable `meta`).
/// A refused file (symlink, mode) or a too-new schema is not corrupt and is left alone.
#[must_use]
pub fn corrupt(dir: &AdmittedDir, commits: &[String]) -> Vec<String> {
    commits
        .iter()
        .filter(|c| {
            let path = dir.path.join(format!("{c}.cg.db"));
            matches!(
                SqliteSnapshot::open_ro(&path)
                    .and_then(|s| s.meta().map_err(|_| SnapshotError::Corrupt)),
                Err(SnapshotError::Corrupt)
            )
        })
        .cloned()
        .collect()
}

/// Among `commits`, those whose `meta.policy_version` is readable and differs from
/// `policy_version`.
#[must_use]
pub fn stale(dir: &AdmittedDir, commits: &[String], policy_version: &str) -> Vec<String> {
    commits
        .iter()
        .filter(|c| {
            let path = dir.path.join(format!("{c}.cg.db"));
            SqliteSnapshot::open_ro(&path)
                .ok()
                .and_then(|s| s.meta().ok())
                .and_then(|m| m.get("policy_version").cloned())
                .is_some_and(|v| v != policy_version)
        })
        .cloned()
        .collect()
}

/// Why a collection did not complete.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum GcError {
    /// A snapshot could not be unlinked through the handle; the errno class only.
    #[error("gc: unlink failed ({0})")]
    Unlink(&'static str),
}

/// Delete the snapshot, the `.unsynced` marker and every run log of each collected commit
/// (`<commit>.<lang>.a.log`, `<commit>.<lang>.b.log`, and the single `<commit>.<lang>.log` an
/// earlier release wrote) through the admitted handle. Returns the commits whose snapshot was
/// removed.
///
/// # Errors
/// [`GcError::Unlink`] on the first snapshot that could not be unlinked (absence is not an
/// error: a concurrent `gc` may have taken it).
pub fn apply(dir: &AdmittedDir, collect: &[(String, Reason)]) -> Result<Vec<String>, GcError> {
    let mut removed = Vec::new();
    for (commit, _) in collect {
        match rustix::fs::unlinkat(&dir.handle, format!("{commit}.cg.db"), AtFlags::empty()) {
            Ok(()) => removed.push(commit.clone()),
            Err(rustix::io::Errno::NOENT) => {}
            Err(rustix::io::Errno::ACCESS | rustix::io::Errno::PERM) => {
                return Err(GcError::Unlink("permission_denied"));
            }
            Err(_) => return Err(GcError::Unlink("other")),
        }
        let _ = rustix::fs::unlinkat(
            &dir.handle,
            format!("{commit}.cg.db.unsynced"),
            AtFlags::empty(),
        );
        for lang in Lang::ALL {
            for name in [
                format!("{commit}.{}.a.log", lang.name()),
                format!("{commit}.{}.b.log", lang.name()),
                format!("{commit}.{}.log", lang.name()),
            ] {
                let _ = rustix::fs::unlinkat(&dir.handle, name, AtFlags::empty());
            }
        }
    }
    Ok(removed)
}

/// Remove the left-overs of killed runs (§7): `*.cg.db.tmp-<pid>` files and `run-<pid>`
/// directories whose pid is observed dead. Never recover another run's permissions:
/// its producer group may still be alive. Inaccessible entries remain for later cleanup.
/// Returns the number removed.
#[must_use]
pub fn sweep_leftovers(dir: &AdmittedDir) -> usize {
    // Linux's admitted handle is O_PATH. Explicitly obtain a readable stream relative
    // to that retained inode; Dir::read_from would retain O_PATH and fail iteration.
    let Ok(readable) = rustix::fs::openat(
        &dir.handle,
        ".",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) else {
        return 0;
    };
    let Ok(rd) = Dir::new(readable) else {
        return 0;
    };
    let mut removed = 0;
    for entry in rd.flatten() {
        let raw_name = std::ffi::OsStr::from_bytes(entry.file_name().to_bytes());
        let Some(name) = raw_name.to_str() else {
            continue;
        };
        let pid = name
            .strip_prefix("run-")
            .or_else(|| name.rsplit_once(".cg.db.tmp-").map(|(_, p)| p))
            .and_then(|p| p.parse::<u32>().ok());
        let Some(pid) = pid else {
            continue;
        };
        if pid == std::process::id()
            || kaban_core::process::process_liveness(pid) != kaban_core::process::Liveness::Dead
        {
            continue;
        }
        let ok = crate::isolate::remove_leftover_tree(dir.handle.as_fd(), raw_name).is_ok();
        if ok {
            removed += 1;
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(n: u8) -> String {
        format!("{n:02x}").repeat(20)
    }

    // the order is git's (most recent first); HEAD is kept outside the count; unknown commits
    // are collected first, then the ones beyond `keep`
    #[test]
    fn head_plus_n_most_recent_are_kept_unknown_collected_first() {
        let snaps = vec![c(1), c(2), c(3), c(4), c(9)];
        let order = vec![c(4), c(3), c(2), c(1)];
        let p = plan(&snaps, Some(&c(1)), &order, 2);
        assert_eq!(p.keep, vec![c(1), c(4), c(3)]);
        assert_eq!(
            p.collect,
            vec![(c(9), Reason::UnknownCommit), (c(2), Reason::BeyondKeep)]
        );
    }

    #[test]
    fn head_without_a_snapshot_does_not_count_and_keep_zero_keeps_only_head() {
        let snaps = vec![c(1), c(2)];
        let order = vec![c(2), c(1)];
        let p = plan(&snaps, Some(&c(7)), &order, 1);
        assert_eq!(p.keep, vec![c(2)]);
        assert_eq!(p.collect, vec![(c(1), Reason::BeyondKeep)]);
        let p = plan(&snaps, Some(&c(2)), &order, 0);
        assert_eq!(p.keep, vec![c(2)]);
        assert_eq!(p.collect, vec![(c(1), Reason::BeyondKeep)]);
    }

    #[test]
    fn nothing_to_collect_within_keep() {
        let snaps = vec![c(1), c(2)];
        let order = vec![c(2), c(1)];
        let p = plan(&snaps, None, &order, 5);
        assert_eq!(p.keep, vec![c(2), c(1)]);
        assert_eq!(p.collect, Vec::<(String, Reason)>::new());
    }
}
