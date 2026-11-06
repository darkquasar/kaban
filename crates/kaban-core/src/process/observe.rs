//! Conservative process observation, shared by run cleanup and its native tests.

/// Whether a positive PID is known to exist. Unknown never authorizes cleanup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Liveness {
    /// The process exists (possibly a zombie).
    Alive,
    /// The kernel reports no such process.
    Dead,
    /// Invalid PID, permission refusal or an unavailable observation.
    Unknown,
}

/// Observe a PID without procfs or changing the process. Only ESRCH proves death.
#[must_use]
pub fn process_liveness(raw: u32) -> Liveness {
    let Some(pid) = i32::try_from(raw)
        .ok()
        .and_then(rustix::process::Pid::from_raw)
    else {
        return Liveness::Unknown;
    };
    match rustix::process::test_kill_process(pid) {
        Ok(()) => Liveness::Alive,
        Err(rustix::io::Errno::SRCH) => Liveness::Dead,
        Err(_) => Liveness::Unknown,
    }
}
