//! The mode-B projection loop (spec 05 SS-21): a serving process that won the `text-index`
//! lease holds it and the writer lock, and wakes on `PRAGMA data_version` of a stable
//! observation connection every [`POLL_TICK`] instead of re-reading the log: the version
//! moves when any other connection commits, so a write by another process (or by this one)
//! is consumed within a tick and coalesced with whatever else landed into one commit per page.
//! No daemon, no IPC: the loop lives inside the serving process and ends with it. The process's
//! own writers do not wait for the tick (lane T8b): they signal the loop through [`Signals`],
//! which also carries the published position back to whoever waits on it.
//!
//! ```text
//!   open: lease (election) => writer lock (the fence)          [Halt when held elsewhere]
//!   loop: publications to the head ; signals.published(seq) after each  [and once at the head]
//!         seen = data_version ; one page says "quiet"?     [the ack's own commit moved the version]
//!         wait: signals.wait(100 ms) => Publish: run again [a local commit's window closed, a barrier]
//!               renew ; data_version != seen => run again ; stop => return
//!   LeaseLost (a successor acquired it while this process was stopped) => writer dropped, return
//! ```
//!
//! The quiet check after a run exists because the run's own acknowledgement is a commit on the
//! store's connection, which moves the observation connection's version too; reading the
//! version after the check and before the idle wait closes the window a foreign write could
//! fall into between the run's last page and the first tick.

use super::{Progress, Projector, Run, Stop};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// How often the holder reads `PRAGMA data_version` and renews (SS-21: 100 ms).
pub const POLL_TICK: Duration = Duration::from_millis(100);

/// The observation: `PRAGMA data_version` on one connection kept for the loop's lifetime (the
/// pragma is per connection: it moves when another connection committed since this one last
/// read).
pub type DataVersion<'a> = &'a dyn Fn() -> kaban_core::Result<u64>;

/// What an idle wait ended with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wake {
    /// The tick passed with nothing local to publish: renew, then look at the data version.
    Tick,
    /// Local work asks for publication now (a commit's window closed, a barrier is waiting).
    Publish,
}

/// The holder's own process talking to its loop: its writers' commits and barriers wake it,
/// and every run reports the position it published.
pub trait Signals: Sync {
    /// Block until local work asks for publication or `tick` passes.
    fn wait(&self, tick: Duration) -> Wake;
    /// Lease and lock are held from here: this loop is the publisher until it returns.
    fn acquired(&self) {}
    /// The payload's sequence after every commit and after a run that reached the head.
    fn published(&self, seq: u64);
    /// The highest position a barrier or a read of this process waits for now (0: none).
    fn wanted(&self) -> u64 {
        0
    }
}

/// The loop with no local signals: ticks only (the cross-process discovery alone).
struct TickOnly;

impl Signals for TickOnly {
    fn wait(&self, tick: Duration) -> Wake {
        std::thread::sleep(tick);
        Wake::Tick
    }
    fn published(&self, _seq: u64) {}
}

impl Projector<'_> {
    /// Hold lease and lock and project every committed change as it lands, waking on
    /// `observe` (see the module doc), until `stop` is set or the lease is lost; a payload
    /// that cannot be resumed from is rebuilt here (this process is the elected projector).
    ///
    /// # Errors
    /// A store or index failure (the observation included); a held lease or lock at the start
    /// and a lost lease are a [`Halt`](super::Halt).
    pub fn poll(
        &self,
        observe: DataVersion<'_>,
        stop: &AtomicBool,
    ) -> kaban_core::Result<Progress> {
        self.poll_with(observe, stop, &TickOnly)
    }

    /// [`Self::poll`] woken by `signals` as well (the module doc).
    ///
    /// # Errors
    /// As [`Self::poll`].
    pub fn poll_with(
        &self,
        observe: DataVersion<'_>,
        stop: &AtomicBool,
        signals: &dyn Signals,
    ) -> kaban_core::Result<Progress> {
        let mut session = match self.open()? {
            Ok(session) => session,
            Err(halt) => return self.progress(Some(halt), 0),
        };
        signals.acquired();
        let run = Run {
            target: None,
            deadline: None,
            may_rebuild: true,
            signals: Some(signals),
        };
        let halt = 'held: loop {
            if let Some(halt) = session.run(&run)? {
                break Some(halt);
            }
            signals.published(self.index.payload()?.map_or(0, |p| p.seq));
            let seen = observe()?;
            if self.pending()? {
                continue;
            }
            loop {
                if stop.load(Ordering::Acquire) {
                    break 'held None;
                }
                let wake = signals.wait(POLL_TICK);
                match session.renew() {
                    Ok(()) => {}
                    Err(Stop::Halt(halt)) => break 'held Some(halt),
                    Err(Stop::Failed(e)) => return Err(e),
                }
                if wake == Wake::Publish || observe()? != seen {
                    break;
                }
            }
        };
        let commits = session.commits;
        drop(session);
        self.progress(halt, commits)
    }

    /// Whether the log holds a row past the payload (one row read; the quiet check).
    fn pending(&self) -> kaban_core::Result<bool> {
        let after = self.index.payload()?.map_or(0, |p| p.seq);
        Ok(!self.index.rows().replay(after, 1)?.changes.is_empty())
    }
}
