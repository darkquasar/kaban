//! Named crash points of compaction, reclamation and recovery (spec 11 T6 Step 4): the crash
//! campaign (`catalog_recover.rs`, `bench crash`) runs a child process with [`ARM`] naming one
//! point, and the child kills itself with `SIGKILL` when it gets there: no unwinding, no
//! destructor, no flush, as a power cut or an OOM kill would leave it. Without the
//! `failpoints` feature every point compiles to nothing.
//!
//! The points: `publish.after_append`, `publish.before_commit`, `seal.mid`, `fold.mid`,
//! `reclaim.after_floor`, `reclaim.mid_punch`, `recover.mid`.

/// The environment variable that arms one point by name (read once per process).
#[cfg_attr(
    not(feature = "failpoints"),
    expect(dead_code, reason = "only a failpoints build reads it")
)]
pub(crate) const ARM: &str = "KABAN_FAILPOINT";

/// Kill this process with `SIGKILL` if [`ARM`] names `name`; otherwise nothing.
#[cfg(feature = "failpoints")]
pub(crate) fn point(name: &'static str) {
    static ARMED: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    // A separate, bounded rendezvous lets cross-process tests inspect lock ownership
    // without sleeps or relying on how quickly an opener reaches this point.
    if name.starts_with("replacement.") {
        replacement_rendezvous(name);
    }
    let armed = ARMED.get_or_init(|| std::env::var(ARM).ok());
    if armed.as_deref() == Some(name) {
        let _ =
            rustix::process::kill_process(rustix::process::getpid(), rustix::process::Signal::KILL);
        // the signal is delivered before the call returns; never go on past an armed point
        std::process::abort();
    }
}

#[cfg(feature = "failpoints")]
fn replacement_rendezvous(name: &str) {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    let Ok(path) = std::env::var("KABAN_TEST_REPLACEMENT_SOCKET") else {
        return;
    };
    let exchange = || -> std::io::Result<()> {
        let mut socket = UnixStream::connect(path)?;
        socket.set_read_timeout(Some(Duration::from_secs(30)))?;
        socket.set_write_timeout(Some(Duration::from_secs(30)))?;
        writeln!(socket, "{name}")?;
        let mut release = [0];
        socket.read_exact(&mut release)
    };
    if exchange().is_err() {
        std::process::abort();
    }
}

/// Crash points compile to nothing without the `failpoints` feature.
#[cfg(not(feature = "failpoints"))]
#[inline]
pub(crate) fn point(name: &'static str) {
    let _ = name;
}
