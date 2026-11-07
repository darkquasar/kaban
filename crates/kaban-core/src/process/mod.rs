//! One-shot child processes under kaban's process control (stage1-spec G1-S5.3 clean
//! environment, G1-S5.4 process control; stage1-plan CG1.1): an own process group, stdin null, a
//! clean allowlisted environment, reader threads with byte caps, pluggable monitors sampled on
//! worker threads, a hard deadline that SIGKILLs the whole group and a bounded teardown.
//! `GitRunner` in `kaban-api` and the indexer runner of `kaban-index` are the only callers that
//! spawn (lint `no_spawn_outside_runner`); the one other `Command::new` in the workspace is the
//! M3-22 bench corpus builder (`kaban/src/bench/corpus.rs`), allowlisted by CG2.4's lint.
//!
//! ```text
//!   run(Spec { exec, args, cwd, env, deadline, caps, monitors, join_bound, inherited_fds })
//!     admit: every fd > 2 of this process has O_CLOEXEC, else Refuse -> InheritedFd { fd }
//!            or Tolerate -> one info trace with the count
//!     spawn: env_clear() + allowlist, process_group(0), stdin null, stdout/stderr piped or null
//!       |                       reader thread per stream: pipe -> buf (cap) -> channel
//!       |                       worker thread per monitor: sleep(interval), sample() -> channel
//!       | one loop, 1 ms: try_wait(); poll the readers; then in order: a cap flag, the deadline,
//!       |                 a monitor verdict -> kill; else exit status + every reader complete
//!       +-- complete --------------> Outcome { killed: None, .. }
//!       +-- deadline / cap / monitor (before or after the leader exited)
//!                                --> SIGKILL the group, then within join_bound: reap, poll the
//!                                    group until empty (else descendants_survived), readers,
//!                                    workers; the rest is traced and abandoned
//!                                    Outcome { killed: Some(reason), .. }
//!       every path: each monitor's final sample at stop -> Outcome.monitor_verdicts
//!                   (Ok | Failed(reason) | Silent when nothing arrived within join_bound)
//! ```
//!
//! The group kill is what makes the teardown bounded: a child that forked a grandchild holding
//! the pipe would otherwise keep a reader alive until that grandchild exits (R-C3.2-4). A reader
//! still alive after `join_bound` means a process outside the killed group holds the pipe; it is
//! traced and abandoned, never waited for. Every kill reason, including one found after the
//! leader exited (a descendant overflowing a cap, a monitor verdict), goes through the group
//! kill, and `Outcome::killed` is set only when it was issued; a group kill that fails (the
//! group already empty, a permission refusal) falls back to the child alone and is named by
//! `Outcome::kill_error` (`GM2a`). After the kill the group is polled until it has no member or
//! `join_bound` expires; the expiry is `Outcome::descendants_survived` (CG4.3, U1 residual 4: a
//! kill issued is not a proof of descendant death). A monitor's `sample` runs on its own worker
//! thread, so a slow sample never delays the deadline (the supervisor only reads the verdict
//! channel); at stop every monitor that has not failed takes one final sample and reports a
//! [`MonitorVerdict`]; a sample still in flight at the end of the run completes on its detached
//! worker and the monitor is judged `Silent` (fail closed for the caller). A kill verdict that
//! arrives after the supervision loop broke is never `Outcome::killed`.
//!
//! Handle execution (OD-G1-4, RV-9): [`Exec::Handle`] is an `O_PATH|O_NOFOLLOW|O_CLOEXEC`
//! descriptor of an ELF regular file. Immediately before the spawn the parent `fstat`s the handle
//! (regular file, owned by root or the invoking user, writable by neither group nor other) and
//! reads the first four bytes through a second `open("/proc/self/fd/<n>")` of the same handle,
//! requiring `\x7fELF`; the child then `execve`s `/proc/self/fd/<n>`, which the kernel resolves
//! while the descriptor is still open in the child (close-on-exec runs only once the new image is
//! committed), so the checked bytes are the executed bytes even if the path was replaced on disk
//! in between. A `#!` script is refused (`wrapper_not_elf`): its interpreter would reopen
//! `/proc/self/fd/<n>` after the CLOEXEC close and fail. On targets other than Linux
//! `Exec::Handle` is `handle_exec_unsupported` (no wrapper tier exists there in stage 1).
//!
//! Descriptors (CG1.1 slice 5c, RG1-2): every descriptor Rust or rustix opens is CLOEXEC, so the
//! child inherits exactly stdin, stdout and stderr from this crate's own descriptors. A
//! descriptor this process inherited without CLOEXEC (an MCP host launching `kaban serve`, a
//! `flock(1)` launcher) would pass to the child and cannot be closed or re-flagged here without
//! `unsafe`; `run` therefore reads Linux `/proc/self/fdinfo` or Darwin `/dev/fd` with
//! F_GETFD on each original raw descriptor before the spawn and, under
//! [`InheritedFds::Refuse`] (the default), refuses with `InheritedFd { fd }` (the lowest such
//! descriptor named in the message, G7); under
//! [`InheritedFds::Tolerate`] (`GitRunner`) it traces once at `info` with the count only and
//! spawns (a hook's stderr stays empty on a clean run under `RUST_LOG=warn`).
//!
//! Layout (proj-mod-rs-dir, proj-mod-by-feature; OWN-61): `env` holds the allowlist, `exec` the
//! executable, the handle checks and the descriptor admission, `monitor` the monitor trait and
//! kill reasons, `observe` conservative process liveness, `run` the runner with its reader threads and monitor workers. The public surface
//! is re-exported here and is the plan's plus [`InheritedFds`].

mod env;
mod exec;
mod monitor;
mod observe;
mod run;

use thiserror::Error;

pub use env::{EnvAllowlist, GIT_INTERNAL, TrustedInternal};
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use exec::inheritable_fds;
pub use exec::{Exec, InheritedFds};
pub use monitor::{KillReason, Monitor, MonitorVerdict, Stream};
pub use observe::{Liveness, process_liveness};
pub use run::{Outcome, Spec, run, run_with_directory_handoff};

/// Why no child ran, or why it could not be supervised. Messages name the class only.
#[derive(Debug, Error)]
pub enum ProcessError {
    /// The child could not be started or polled.
    #[error("the child could not be started")]
    Spawn(#[source] std::io::Error),
    /// Refused before any spawn: an environment key (`env_key_refused`) or a handle that is not
    /// an acceptable wrapper (`wrapper_not_elf`, `wrapper_not_regular`, `wrapper_owner`,
    /// `wrapper_writable`, `wrapper_changed`, `handle_exec_unsupported`).
    #[error("refused before spawn: {0}")]
    Env(&'static str),
    /// Refused before any spawn under [`InheritedFds::Refuse`]: this process holds descriptor
    /// `fd` without `FD_CLOEXEC` (inherited from an ancestor, a launcher's `flock(1)` for one)
    /// and it would pass to the child; `None` when the number cannot be read on this target.
    /// The message names it for what it is and says the action (G7).
    #[error("inherited an open file descriptor{}; close it in the caller", fd.map_or_else(String::new, |n| format!(" (fd {n})")))]
    InheritedFd {
        /// The lowest such descriptor.
        fd: Option<u32>,
    },
}
