//! The only process spawner in `kaban-api` (M3-10, now a wrapper): a bounded, hygienic `git`
//! runner over `kaban_core::process` (stage1-plan CG1.1). `rev-parse HEAD` stamps
//! `accepted_commit` at acceptance (C3.2, M3-23); the currency queries of C3.3 (`log
//! --name-status -M`, `ls-files`) and the sync queries run through the same entry. Local git
//! only, never the network (core-spec S8). This crate no longer constructs a `Command` of its own.
//!
//! ```text
//!   GitRunner::run(root, args)
//!     Spec { Exec::Path(program), [--no-pager -C root -c core.fsmonitor=false args..],
//!            env: internal_git() + LC_ALL=C + carried PATH, HOME, stdout cap 1 MiB, stderr off,
//!            inherited_fds: Tolerate }
//!       => kaban_core::process::run       (own group, stdin null, 1 ms poll, group SIGKILL)
//!       <= Outcome { killed, status, stdout }  ->  GitError::{Timeout, Output, Status} or String
//! ```
//!
//! The group kill (R-C3.2-4) is what makes the join bounded: a wrapper that forked a grandchild
//! holding the pipe would otherwise keep the reader alive until that grandchild exits.
//!
//! Inherited descriptors (RG1-2): an MCP host that launches `kaban serve` may leave descriptors
//! without `FD_CLOEXEC` open in the server, and a `git` that cannot run would stop every
//! currency query, so this wrapper passes [`InheritedFds::Tolerate`]: the runner spawns and
//! traces once per invocation at `info` with the count only (the hooks' stderr stays empty on
//! a clean run under `RUST_LOG=warn`). A documented narrowing of CG1.1 slice 5c for
//! `git` alone; the indexer runners of CG4.3 use the default `Refuse`.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use kaban_core::process::{EnvAllowlist, Exec, InheritedFds, KillReason, ProcessError, Spec};
use thiserror::Error;

/// Output the runner refuses to read past (a pathological repository, not a kaban failure).
const MAX_OUTPUT: usize = 1024 * 1024;

/// The runner's teardown budget after the group was killed (reap, reader thread).
const JOIN_BOUND: Duration = Duration::from_millis(25);

/// Why a git invocation produced nothing usable. Messages name the failure class only.
#[derive(Debug, Error)]
pub enum GitError {
    /// `git` could not be started.
    #[error("git could not be started")]
    Spawn(#[source] std::io::Error),
    /// The deadline passed; the process group was killed.
    #[error("git timed out after {0:?}")]
    Timeout(Duration),
    /// Non-zero exit status.
    #[error("git exited with status {0}")]
    Status(i32),
    /// The output was not UTF-8 or exceeded the cap.
    #[error("git output unusable")]
    Output,
}

/// A bounded `git` invocation: `program` (configurable so tests can point at a wrapper script)
/// run with `-C <root>`, a clean environment and `timeout`, in its own process group. Stdout is
/// read on a thread up to 1 MiB; the process is polled until the deadline, then the whole
/// group is killed and the reader joined within a bound (R-C3.2-4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitRunner {
    /// The executable (`git` on `PATH` by default).
    pub program: PathBuf,
    /// Hard deadline for one invocation.
    pub timeout: Duration,
}

impl Default for GitRunner {
    /// `git` with the 200 ms deadline (core-spec F14, core-plan C3.2).
    fn default() -> Self {
        Self {
            program: PathBuf::from("git"),
            timeout: Duration::from_millis(200),
        }
    }
}

/// Whether `s` is a commit id that may reach argv (M3-6): 7 to 64 lowercase hex digits.
pub(crate) fn is_commit_hex(s: &str) -> bool {
    (7..=64).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

impl GitRunner {
    /// `git rev-parse HEAD` in `root`: the lowercase hex commit id, or `None` when the
    /// repository has no HEAD, git is absent, slow or fails (acceptance proceeds without a
    /// commit stamp; the failure class is traced).
    pub fn head(&self, root: &Path) -> Option<String> {
        match self.run(root, &["rev-parse", "HEAD"]) {
            Ok(out) => {
                let sha = out.trim();
                is_commit_hex(sha).then(|| sha.to_owned())
            }
            Err(e) => {
                tracing::debug!(error = %e, "accepted_commit not stamped");
                None
            }
        }
    }

    /// Run `git <args>` in `root` and return its stdout.
    ///
    /// # Errors
    /// [`GitError`] by failure class; stderr is discarded. The runner's `killed == Deadline` is
    /// [`GitError::Timeout`], an output cap or non-UTF-8 output is [`GitError::Output`].
    pub fn run(&self, root: &Path, args: &[&str]) -> Result<String, GitError> {
        let mut git_args: Vec<OsString> = vec![
            OsString::from("--no-pager"),
            OsString::from("-C"),
            root.as_os_str().to_owned(),
            OsString::from("-c"),
            OsString::from("core.fsmonitor=false"),
        ];
        git_args.extend(args.iter().map(OsString::from));
        let outcome = kaban_core::process::run(Spec {
            exec: Exec::Path(self.program.clone()),
            args: git_args,
            // `-C <root>` selects the repository; the working directory stays the caller's so
            // a relative root and the error class of a missing root are as before
            cwd: Path::new("."),
            env: git_env().map_err(spawn_error)?,
            deadline: self.timeout,
            stdout_cap: MAX_OUTPUT,
            stderr_cap: 0,
            monitors: Vec::new(),
            join_bound: JOIN_BOUND,
            // see the module documentation: the host may leak descriptors into `kaban serve`
            inherited_fds: InheritedFds::Tolerate,
        })
        .map_err(spawn_error)?;
        match outcome.killed {
            Some(KillReason::Deadline) => return Err(GitError::Timeout(self.timeout)),
            Some(KillReason::OutputCap { .. } | KillReason::Monitor { .. }) => {
                return Err(GitError::Output);
            }
            None => {}
        }
        let status = outcome.status.ok_or(GitError::Output)?;
        if !status.success() {
            return Err(GitError::Status(status.code().unwrap_or(-1)));
        }
        String::from_utf8(outcome.stdout).map_err(|_| GitError::Output)
    }
}

/// A refusal before the spawn or a failed spawn: both mean no usable git ran.
fn spawn_error(e: ProcessError) -> GitError {
    match e {
        ProcessError::Spawn(io) => GitError::Spawn(io),
        ProcessError::Env(class) => GitError::Spawn(std::io::Error::other(class)),
        e @ ProcessError::InheritedFd { .. } => {
            GitError::Spawn(std::io::Error::other(e.to_string()))
        }
    }
}

/// The child's whole environment (G1-S5.3, RV-1): the two constant Git keys of
/// [`EnvAllowlist::internal_git`], `LC_ALL=C`, and `PATH` and `HOME` carried from the caller
/// because git needs them. Every caller `GIT_*` variable (`GIT_DIR`, `GIT_WORK_TREE`,
/// `GIT_INDEX_FILE`, `GIT_SSH_COMMAND`, ...) is gone with the `env_clear()`.
fn git_env() -> Result<EnvAllowlist, ProcessError> {
    let mut env = EnvAllowlist::internal_git();
    env.insert("LC_ALL", "C")?;
    env.carry("PATH")?;
    env.carry("HOME")?;
    Ok(env)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)] // test code: failures are the assertion

    use super::*;

    /// RV-1, presence direction: the child of `GitRunner` sees the two internal Git keys,
    /// `LC_ALL=C`, the carried `PATH` and `HOME`, and nothing else (a caller `GIT_*` variable in
    /// this process never reaches it).
    /// A scratch directory removed on drop (test-fixture-raii).
    struct Rm(PathBuf);

    impl Drop for Rm {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn env_allowlist_internal_git_keys_present() {
        let dir = std::env::temp_dir().join(format!(
            "kaban-gitenv-{}-{}",
            std::process::id(),
            kaban_core::new_id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let _rm = Rm(dir.clone());
        let wrapper = dir.join("env-git.sh");
        std::fs::write(&wrapper, "#!/bin/sh\nexec env\n").unwrap();
        std::fs::set_permissions(
            &wrapper,
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        )
        .unwrap();
        let git = GitRunner {
            program: wrapper,
            timeout: Duration::from_secs(5),
        };
        let out = git.run(&dir, &["status"]).unwrap();
        // the `#!/bin/sh` wrapper exports its own PWD; everything else came from the runner
        let mut seen: Vec<&str> = out.lines().filter(|l| !l.starts_with("PWD=")).collect();
        seen.sort_unstable();
        let path = std::env::var("PATH").unwrap();
        let home = std::env::var("HOME").unwrap();
        let mut expected = vec![
            "GIT_OPTIONAL_LOCKS=0".to_owned(),
            "GIT_TERMINAL_PROMPT=0".to_owned(),
            format!("HOME={home}"),
            "LC_ALL=C".to_owned(),
            format!("PATH={path}"),
        ];
        expected.sort_unstable();
        assert_eq!(seen, expected);
    }
}
