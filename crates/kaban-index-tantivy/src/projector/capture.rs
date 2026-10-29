//! The builder's session of a pack build (spec 05 SS-18): `pack build` on an active Tantivy
//! deployment takes the projector lease and the writer lock, drains projection to the head
//! (its own acknowledgement, while other canonical writers are excluded by the builder's
//! transaction), and captures the index directory at the sequence it snapshots. The session
//! holds both for its lifetime with the writer's merges frozen, so nothing publishes and no
//! merge rewrites `meta.json` or collects a segment while the directory is copied (T4 review
//! t4-4); the store crate's builder drives it through its component seam and decides busy and
//! quiescence.

use super::{Halt, Projector, Run, Session};
use std::path::Path;
use std::time::Instant;

/// Lease and lock held for a capture; released on drop.
pub struct Capture<'p, 'a> {
    session: Session<'p, 'a>,
}

impl<'a> Projector<'a> {
    /// Take lease and lock for a pack build, the writer's merges frozen for the session:
    /// `Err(halt)` when either is held elsewhere (busy for the builder) or the lease is lost
    /// at once.
    ///
    /// # Errors
    /// A store or index failure.
    pub fn capture(&self) -> kaban_core::Result<Result<Capture<'_, 'a>, Halt>> {
        Ok(self.open()?.map(|session| {
            session.writer.freeze_merges();
            Capture { session }
        }))
    }
}

impl Capture<'_, '_> {
    /// Publish every committed change through `target` (rebuilding when the payload cannot
    /// be resumed from), stopping at `deadline`, and return the payload's sequence, or the
    /// halt that stopped short (`Halt::Deadline` among them). Both bounds are the builder's
    /// (T4 review t4-8): a writer that keeps the log moving cannot keep the capture from
    /// returning.
    ///
    /// # Errors
    /// A store or index failure.
    pub fn drain(
        &mut self,
        target: u64,
        deadline: Instant,
    ) -> kaban_core::Result<Result<u64, Halt>> {
        let run = Run {
            target: Some(target),
            deadline: Some(deadline),
            may_rebuild: true,
            signals: None,
        };
        if let Some(halt) = self.session.run(&run)? {
            return Ok(Err(halt));
        }
        let index = self.session.projector.index;
        Ok(Ok(index.payload()?.map_or(0, |p| p.seq)))
    }

    /// The directory to capture and the payload of its last commit as JSON (`None` for an
    /// index in memory or never committed).
    ///
    /// # Errors
    /// The payload could not be read.
    pub fn captured(&self) -> kaban_core::Result<Option<(&Path, serde_json::Value)>> {
        let index = self.session.projector.index;
        let Some(dir) = index.directory() else {
            return Ok(None);
        };
        let Some(payload) = index.payload()? else {
            return Ok(None);
        };
        let json = payload.to_json()?;
        Ok(Some((
            dir,
            serde_json::from_str(&json).map_err(crate::Error::Payload)?,
        )))
    }
}
