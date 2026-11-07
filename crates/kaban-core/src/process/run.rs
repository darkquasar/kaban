//! The runner: descriptor admission, spawn, one supervision loop (deadline, caps, monitors) until
//! the child status and every reader are complete, group kill on every reason, bounded teardown.

use std::ffi::OsString;
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use super::exec::{InheritedFds, admit_inherited_fds, command_for};
use super::{EnvAllowlist, Exec, KillReason, Monitor, MonitorVerdict, ProcessError, Stream};

/// Everything one run needs. `stdout_cap`/`stderr_cap` of 0 discards the stream (`/dev/null`,
/// no reader thread).
pub struct Spec<'a> {
    /// The executable.
    pub exec: Exec,
    /// `argv[1..]`.
    pub args: Vec<OsString>,
    /// The child's working directory (`.` keeps the caller's).
    pub cwd: &'a Path,
    /// The child's whole environment.
    pub env: EnvAllowlist,
    /// Hard deadline from spawn to the collected output.
    pub deadline: Duration,
    /// Bytes kept of stdout; more kills the group; 0 discards the stream.
    pub stdout_cap: usize,
    /// Bytes kept of stderr; more kills the group; 0 discards the stream.
    pub stderr_cap: usize,
    /// Periodic checks over the running child, each sampled on its own worker thread.
    pub monitors: Vec<Box<dyn Monitor>>,
    /// The teardown budget once the run is over (after a kill, or after the child exited and
    /// its output is complete): the reap, the reader threads and the monitor workers share it;
    /// whatever is still running when it is spent is traced and abandoned, never waited for.
    pub join_bound: Duration,
    /// What to do about descriptors this process holds without `FD_CLOEXEC` (they would pass to
    /// the child); [`InheritedFds::Refuse`] by default.
    pub inherited_fds: InheritedFds,
}

/// What one run produced.
#[derive(Debug)]
pub struct Outcome {
    /// The child's exit status, including the signal status after a kill; `None` when the
    /// reap did not complete within `join_bound`.
    pub status: Option<ExitStatus>,
    /// Stdout bytes (at most `stdout_cap`; the wrapper decides about UTF-8).
    pub stdout: Vec<u8>,
    /// Stderr bytes (at most `stderr_cap`).
    pub stderr: Vec<u8>,
    /// Why the group was killed, if it was: set only when the group kill was issued.
    pub killed: Option<KillReason>,
    /// Set when the group kill was issued but `kill_process_group` failed and the child-only
    /// fallback ran, as a fixed class: `group_not_found` (the group had no member left, the
    /// leader already reaped), `permission_denied`, `other`. `None` when no kill was issued or
    /// the group kill succeeded. A kill that was issued is not proof that every descendant was
    /// reached (`GM2a` residual).
    pub kill_error: Option<&'static str>,
    /// Set when a group kill was issued and the process group still had a member when
    /// `join_bound` expired (CG4.3, U1 residual 4: a group kill is not a proof of descendant
    /// death; the runner polls the group until it is empty and reports the expiry as a counted,
    /// fixed-class fact). `false` when no kill was issued or the group emptied in time.
    pub descendants_survived: bool,
    /// One verdict per monitor of the `Spec`, in order (CG4.3, U1 residual 4): the kill it
    /// asked for, the result of its final sample at stop, or `Silent` when it produced neither
    /// within `join_bound`.
    pub monitor_verdicts: Vec<MonitorVerdict>,
    /// Wall time from spawn to return.
    pub wall: Duration,
}

/// Poll period of the supervision loop.
const POLL: Duration = Duration::from_millis(1);

/// Run `spec` to completion or to a kill: descriptor admission, own process group, stdin null,
/// one supervision loop until the child status and every enabled reader are complete, SIGKILL
/// of the whole group on the deadline, an output cap or a monitor (also after the leader
/// exited), bounded teardown.
///
/// # Errors
/// [`ProcessError::Env`] before the spawn (handle checks, `inherited_fd` under
/// [`InheritedFds::Refuse`]); [`ProcessError::Spawn`] when the spawn or a `try_wait` poll fails
/// (the group is killed first).
pub fn run(spec: Spec<'_>) -> Result<Outcome, ProcessError> {
    run_with_stdin(spec, Stdio::null())
}

/// Internal helper handoff: pass a retained directory as stdin to an admitted helper.
/// The helper must validate its identity, fchdir, and reset stdin to null before exec.
/// Ordinary callers retain [`run`]'s null stdin and unchanged [`Spec`].
///
/// # Errors
/// A non-directory, descriptor clone error, or the same errors as [`run`].
pub fn run_with_directory_handoff(
    spec: Spec<'_>,
    directory: std::os::fd::BorrowedFd<'_>,
) -> Result<Outcome, ProcessError> {
    let stat = rustix::fs::fstat(directory).map_err(|e| ProcessError::Spawn(e.into()))?;
    if !rustix::fs::FileType::from_raw_mode(stat.st_mode).is_dir() {
        return Err(ProcessError::Env("cwd_not_directory"));
    }
    let owned = directory
        .try_clone_to_owned()
        .map_err(ProcessError::Spawn)?;
    run_with_stdin(spec, Stdio::from(owned))
}

fn run_with_stdin(spec: Spec<'_>, stdin: Stdio) -> Result<Outcome, ProcessError> {
    let Spec {
        exec,
        args,
        cwd,
        env,
        deadline,
        stdout_cap,
        stderr_cap,
        monitors,
        join_bound,
        inherited_fds,
    } = spec;
    let mut command = command_for(&exec)?;
    admit_inherited_fds(inherited_fds)?;
    command
        .args(&args)
        .current_dir(cwd)
        .env_clear()
        .envs(env.iter())
        .stdin(stdin)
        .stdout(stdio_for(stdout_cap))
        .stderr(stdio_for(stderr_cap))
        .process_group(0);
    let started = Instant::now();
    let mut child = command.spawn().map_err(ProcessError::Spawn)?;
    // the handle did its job at execve; close it in the parent now
    drop(command);
    drop(exec);
    let mut out = child
        .stdout
        .take()
        .map(|pipe| Reader::start(pipe, stdout_cap, Stream::Stdout));
    let mut err = child
        .stderr
        .take()
        .map(|pipe| Reader::start(pipe, stderr_cap, Stream::Stderr));
    let mut workers: Vec<MonitorWorker> = monitors.into_iter().map(MonitorWorker::start).collect();
    let end = started + deadline;

    let mut status: Option<ExitStatus> = None;
    let killed = loop {
        if status.is_none() {
            match child.try_wait() {
                Ok(Some(st)) => status = Some(st),
                Ok(None) => {}
                Err(e) => {
                    let until = Instant::now() + join_bound;
                    let _ = kill_group(&mut child, until);
                    abandon(out.take(), until);
                    abandon(err.take(), until);
                    let _ = stop_workers(workers, until);
                    return Err(ProcessError::Spawn(e));
                }
            }
        }
        if let Some(reader) = out.as_mut() {
            reader.poll();
        }
        if let Some(reader) = err.as_mut() {
            reader.poll();
        }
        // the reasons are checked before completion: a reader that finished by overflowing
        // still kills the group (the writer, or a descendant, may be alive)
        let reason = overflowed(out.as_ref())
            .or_else(|| overflowed(err.as_ref()))
            .or_else(|| (Instant::now() >= end).then_some(KillReason::Deadline))
            .or_else(|| workers.iter_mut().find_map(MonitorWorker::verdict));
        if reason.is_some() {
            break reason;
        }
        if status.is_some() && finished(out.as_ref()) && finished(err.as_ref()) {
            break None;
        }
        std::thread::sleep(POLL);
    };

    let until = Instant::now() + join_bound;
    let mut kill_error = None;
    let mut descendants_survived = false;
    if killed.is_some() {
        let kill = kill_group(&mut child, until);
        status = status.or(kill.reaped);
        kill_error = kill.error;
        descendants_survived = kill.descendants_survived;
    }
    let stdout = collect(out.take(), until);
    let stderr = collect(err.take(), until);
    let monitor_verdicts = stop_workers(workers, until);
    Ok(Outcome {
        status,
        stdout,
        stderr,
        killed,
        kill_error,
        descendants_survived,
        monitor_verdicts,
        wall: started.elapsed(),
    })
}

fn stdio_for(cap: usize) -> Stdio {
    if cap == 0 {
        Stdio::null()
    } else {
        Stdio::piped()
    }
}

/// A reader thread over one pipe: bytes up to `cap` are kept, one more sets `overflow` and
/// stops the read (the group is killed by the supervisor). The thread is detached on purpose:
/// a pipe held by a process outside the group must not block `run` (scoped threads would).
struct Reader {
    rx: mpsc::Receiver<Vec<u8>>,
    overflow: Arc<AtomicBool>,
    stream: Stream,
    /// The bytes once the thread delivered them (the pipe closed or the cap was passed).
    bytes: Option<Vec<u8>>,
}

impl Reader {
    fn start(mut pipe: impl Read + Send + 'static, cap: usize, stream: Stream) -> Self {
        let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(1);
        let overflow = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&overflow);
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let mut chunk = [0u8; 8192];
            loop {
                match pipe.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => {
                        buf.extend_from_slice(&chunk[..n]);
                        if buf.len() > cap {
                            buf.truncate(cap);
                            // the flag carries no payload (the channel does): Relaxed suffices
                            flag.store(true, Ordering::Relaxed);
                            break;
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(_) => break,
                }
            }
            let _ = tx.send(buf);
        });
        Self {
            rx,
            overflow,
            stream,
            bytes: None,
        }
    }

    /// Take the bytes if the thread delivered them; never blocks.
    fn poll(&mut self) {
        if self.bytes.is_none()
            && let Ok(bytes) = self.rx.try_recv()
        {
            self.bytes = Some(bytes);
        }
    }
}

fn overflowed(reader: Option<&Reader>) -> Option<KillReason> {
    let reader = reader?;
    reader
        .overflow
        .load(Ordering::Relaxed)
        .then_some(KillReason::OutputCap {
            stream: reader.stream,
        })
}

/// Whether the stream is complete (no reader, or its bytes were delivered).
fn finished(reader: Option<&Reader>) -> bool {
    reader.is_none_or(|r| r.bytes.is_some())
}

/// The reader's bytes: already delivered, or delivered before `until`; else nothing (the
/// thread is abandoned with a trace: a process outside the killed group holds the pipe).
fn collect(reader: Option<Reader>, until: Instant) -> Vec<u8> {
    let Some(reader) = reader else {
        return Vec::new();
    };
    if let Some(bytes) = reader.bytes {
        return bytes;
    }
    if let Ok(bytes) = recv_until(&reader.rx, Some(until)) {
        bytes
    } else {
        tracing::warn!(stream = ?reader.stream, "pipe still held after the group kill; reader thread abandoned");
        Vec::new()
    }
}

/// On an error path: wait for the reader until `until`, keep nothing.
fn abandon(reader: Option<Reader>, until: Instant) {
    let _ = collect(reader, until);
}

/// Receive within the existing absolute budget. Darwin channel timeout waits can wake much
/// later than requested (hosted measurements: a roughly 15 ms remaining wait took 135 ms).
/// Use the supervisor's short sleep/poll cadence there; a slow reader or monitor must not
/// extend teardown. Scheduling can still delay a wakeup; this is not a real-time guarantee.
fn recv_until<T>(
    rx: &mpsc::Receiver<T>,
    until: Option<Instant>,
) -> Result<T, mpsc::RecvTimeoutError> {
    // Preserve recv_timeout's unbounded wait for an interval beyond Instant's range.
    let Some(until) = until else {
        return rx.recv().map_err(|_| mpsc::RecvTimeoutError::Disconnected);
    };
    #[cfg(not(target_os = "macos"))]
    {
        rx.recv_timeout(remaining(until))
    }
    #[cfg(target_os = "macos")]
    loop {
        match rx.try_recv() {
            Ok(value) => return Ok(value),
            Err(mpsc::TryRecvError::Disconnected) => {
                return Err(mpsc::RecvTimeoutError::Disconnected);
            }
            Err(mpsc::TryRecvError::Empty) => {}
        }
        let left = remaining(until);
        if left.is_zero() {
            return Err(mpsc::RecvTimeoutError::Timeout);
        }
        std::thread::sleep(POLL.min(left));
    }
}

fn remaining(until: Instant) -> Duration {
    until.saturating_duration_since(Instant::now())
}

/// What the group kill produced.
struct GroupKill {
    /// The child's status when the reap completed within the budget.
    reaped: Option<ExitStatus>,
    /// The class of a failed `kill_process_group` (the child-only fallback ran), see
    /// [`Outcome::kill_error`].
    error: Option<&'static str>,
    /// The group still had a member at `until` (see [`Outcome::descendants_survived`]).
    descendants_survived: bool,
}

/// SIGKILL the child's whole process group (the child is its leader; the group outlives a
/// reaped leader while any member runs) and reap the child within `until`. A reap that does
/// not complete in time is abandoned with a trace, never waited for. A failed group kill falls
/// back to the child alone and is reported by class. After the reap the group is polled until
/// it has no member (`ESRCH`) or `until` passes: the expiry is `descendants_survived` (U1
/// residual 4), a counted fact, never a wait beyond the bound.
fn kill_group(child: &mut Child, until: Instant) -> GroupKill {
    let pid = rustix::process::Pid::from_child(child);
    let error = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL)
        .err()
        .map(|e| {
            let class = kill_error_class(e);
            tracing::warn!(error = %e, class, "process group kill failed; killing the child only");
            let _ = child.kill();
            class
        });
    let reaped = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < until => std::thread::sleep(POLL),
            Ok(None) => {
                tracing::warn!(
                    "child not reaped within join_bound after the group kill; abandoned"
                );
                break None;
            }
            Err(e) => {
                tracing::debug!(error = %e, "reap after the group kill failed");
                break None;
            }
        }
    };
    let descendants_survived = loop {
        // a zombie is still a member: an unreaped leader keeps the group non-empty, which
        // counts as survived (fail closed) rather than as proof of death
        match rustix::process::test_kill_process_group(pid) {
            Err(rustix::io::Errno::SRCH) => break false,
            _ if Instant::now() < until => std::thread::sleep(POLL),
            _ => {
                tracing::warn!("process group not empty within join_bound after the kill");
                break true;
            }
        }
    };
    GroupKill {
        reaped,
        error,
        descendants_survived,
    }
}

/// The fixed class of a failed `kill_process_group` ([`Outcome::kill_error`]).
fn kill_error_class(e: rustix::io::Errno) -> &'static str {
    match e {
        rustix::io::Errno::SRCH => "group_not_found",
        rustix::io::Errno::PERM => "permission_denied",
        _ => "other",
    }
}

/// What a monitor worker reports to the supervisor: exactly one terminal message.
enum Report {
    /// A periodic sample asked for the kill (the loop ends).
    Kill(KillReason),
    /// The worker was stopped and ran its final sample.
    Final(Result<(), KillReason>),
}

/// One monitor sampled on its own thread at its interval (RG1-3): the supervisor only ever
/// `try_recv`s the verdict, so a sample that blocks (a slow quota walk) never delays the
/// deadline or a cap kill. `stop` is dropped by the supervisor to end the loop; the worker
/// then takes one final sample (U1 residual 4: a run shorter than the interval is still judged
/// once) and reports it. A sample in flight completes on the worker, which is abandoned with a
/// trace and a `Silent` verdict if it has not reported within the teardown budget. A kill
/// verdict that arrives only after the supervision loop broke never becomes `Outcome::killed`
/// (the late-sample drop); it is reported as the monitor's verdict.
struct MonitorWorker {
    rx: mpsc::Receiver<Report>,
    /// Held to keep the worker sampling; dropped to stop it.
    stop: mpsc::SyncSender<()>,
    /// The kill the supervisor already took from this worker.
    killed: Option<KillReason>,
}

impl MonitorWorker {
    fn start(mut monitor: Box<dyn Monitor>) -> Self {
        let (tx, rx) = mpsc::sync_channel::<Report>(1);
        let (stop_tx, stop_rx) = mpsc::sync_channel::<()>(0);
        std::thread::spawn(move || {
            // an interval elapses (Timeout) between samples; the supervisor's drop of `stop`
            // ends the loop and asks for the final sample
            while let Err(mpsc::RecvTimeoutError::Timeout) =
                recv_until(&stop_rx, Instant::now().checked_add(monitor.interval()))
            {
                if let Err(reason) = monitor.sample() {
                    let _ = tx.send(Report::Kill(reason));
                    return;
                }
            }
            let _ = tx.send(Report::Final(monitor.sample()));
        });
        Self {
            rx,
            stop: stop_tx,
            killed: None,
        }
    }

    /// The kill verdict, if the worker delivered one; never blocks (the supervisor kills on the
    /// first verdict, so none is kept).
    fn verdict(&mut self) -> Option<KillReason> {
        match self.rx.try_recv() {
            Ok(Report::Kill(reason)) => {
                self.killed = Some(reason);
                Some(reason)
            }
            Ok(Report::Final(_)) | Err(_) => None,
        }
    }

    /// Stop the worker, wait for its verdict until `until`; a worker still inside a sample is
    /// abandoned with a trace and judged `Silent`.
    fn stop(self, until: Instant) -> MonitorVerdict {
        let Self { rx, stop, killed } = self;
        drop(stop);
        if let Some(reason) = killed {
            return MonitorVerdict::Failed(reason);
        }
        match recv_until(&rx, Some(until)) {
            Ok(Report::Final(Ok(()))) => MonitorVerdict::Ok,
            Ok(Report::Final(Err(reason)) | Report::Kill(reason)) => MonitorVerdict::Failed(reason),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                tracing::warn!("monitor worker ended without a verdict; judged silent");
                MonitorVerdict::Silent
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                tracing::warn!(
                    "monitor sample still running at the end of the run; worker abandoned"
                );
                MonitorVerdict::Silent
            }
        }
    }
}

fn stop_workers(workers: Vec<MonitorWorker>, until: Instant) -> Vec<MonitorVerdict> {
    workers.into_iter().map(|w| w.stop(until)).collect()
}
