//! Monitors and kill reasons.

use std::time::Duration;

/// One of the child's output streams.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    /// Standard output.
    Stdout,
    /// Standard error.
    Stderr,
}

/// Why the runner killed the process group (`SIGKILL`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KillReason {
    /// The deadline passed (or the child exited but a pipe holder outlived the deadline).
    Deadline,
    /// A stream produced more bytes than its cap; the buffer is truncated at the cap.
    OutputCap {
        /// Which stream overflowed.
        stream: Stream,
    },
    /// A [`Monitor`] asked for the kill with this fixed class (`"quota"`, ...).
    Monitor {
        /// The monitor's fixed reason class; never caller or child data.
        class: &'static str,
    },
}

/// What one monitor concluded by the end of the run (CG4.3, U1 residual 4 "missing monitor
/// verdicts fail closed"): the runner gives every monitor one final `sample` at stop, bounded
/// by `join_bound`, so a run shorter than the monitor's interval is still judged once. A
/// monitor that produced no verdict within the bound is `Silent`; the caller treats `Silent`
/// as a failure, never as `ok`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MonitorVerdict {
    /// Every sample passed, the final one included.
    Ok,
    /// A sample asked for the kill (during the run, or at the final sample).
    Failed(KillReason),
    /// No verdict within `join_bound` (a sample still in flight, a worker that ended
    /// abnormally): fail closed.
    Silent,
}

/// A periodic check over a running child (file quotas, RSS, ...). `sample` is called every
/// `interval` on a worker thread the runner owns (never on the supervising thread, so a slow
/// sample cannot delay the deadline); an `Err` kills the group with that reason, also after the
/// leader exited while descendants run. At the end of the run every monitor that has not
/// failed is sampled once more and its [`MonitorVerdict`] reported. Implemented by
/// `kaban-index` (CG4.3), so the trait is open, not sealed.
pub trait Monitor: Send {
    /// One sample; `Err(reason)` kills the group.
    ///
    /// # Errors
    /// The [`KillReason`] the runner records as `Outcome::killed`.
    fn sample(&mut self) -> Result<(), KillReason>;
    /// Time between two samples.
    fn interval(&self) -> Duration;
}
