//! One content-free panic hook for the `kaban` process (DI2.2a, review RD3-2; docs-ingest-spec
//! S1, D8). Rust runs the panic hook before the unwinding reaches Tokio, so a parser panic
//! whose message quotes document bytes would print them through the default hook even though
//! `spawn_blocking` turns the unwinding into the `JoinError` the ingestor records as the fixed
//! `extractor panicked`. [`install`] replaces the default hook once, process-wide, with a hook
//! that prints [`render`]'s line: the thread name and the panic location, never the payload.
//! Unwinding is unchanged (no `panic = "abort"`, no `catch_unwind`), so task isolation keeps
//! working; the `hook` subcommand still installs its own line on top of this one.

use std::io::Write;
use std::panic::PanicHookInfo;
use std::sync::Once;

static INSTALLED: Once = Once::new();

/// Install the content-free hook once for the whole process (later calls do nothing). Call it
/// first thing in `fn main`, before any thread or runtime starts.
pub fn install() {
    INSTALLED.call_once(|| {
        std::panic::set_hook(Box::new(|info| {
            // a failed write to stderr is ignored: a panic inside the hook would abort
            let mut err = std::io::stderr().lock();
            let _ = writeln!(err, "{}", render(info));
        }));
    });
}

/// The one line the hook prints: `kaban: internal error in thread <name> at <file>:<line>`,
/// from the current thread's name and the panic location only; `info.payload()` and the panic
/// message are never read.
#[must_use]
pub fn render(info: &PanicHookInfo<'_>) -> String {
    let thread = std::thread::current();
    let name = thread.name().unwrap_or("<unnamed>");
    match info.location() {
        Some(at) => format!(
            "kaban: internal error in thread {name} at {}:{}",
            at.file(),
            at.line()
        ),
        None => format!("kaban: internal error in thread {name}"),
    }
}
