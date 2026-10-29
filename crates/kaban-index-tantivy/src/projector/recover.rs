//! The resume point (spec 05 SS-09, design-5): a writer resumes from the index's last commit
//! payload, never from the SQLite cursor, and only when that payload still joins the
//! surviving canonical history. Anything else (no payload, an incompatible one, a payload ahead
//! of the database, a history gap) is a rebuild from SQLite: an index whose watermark cannot
//! be trusted is never served or extended.

use crate::payload::Payload;
use kaban_core::text_index::Replay;

/// Where the writer resumes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Resume {
    /// Replay the change log after this sequence (the payload's).
    From(u64),
    /// Rebuild from the canonical rows; the reason, for the log.
    Rebuild(&'static str),
}

/// The resume point of `payload` against one consistent read of the log (`replay`, taken
/// after the payload's sequence, design amendment 6): the oldest surviving row must be at or
/// below the payload's successor, the payload must not claim more than the database has, and
/// its versions and lineage must be this build's and this database's.
pub(crate) fn resume_point(payload: Option<&Payload>, replay: &Replay) -> Resume {
    let Some(payload) = payload else {
        return Resume::Rebuild("no payload");
    };
    if !payload.compatible(&replay.at) {
        return Resume::Rebuild("incompatible payload");
    }
    if payload.seq > replay.at.seq {
        return Resume::Rebuild("payload ahead of the canonical history");
    }
    if replay.oldest > payload.seq.saturating_add(1) {
        return Resume::Rebuild("history pruned past the payload");
    }
    Resume::From(payload.seq)
}

#[cfg(test)]
mod tests {
    use super::{Resume, resume_point};
    use crate::payload::Payload;
    use kaban_core::text_index::{Canonical, IndexIdentity, Replay};

    fn at(seq: u64) -> Canonical {
        Canonical {
            seq,
            identity: IndexIdentity {
                instance_id: "db".into(),
                pack_lineage_id: "L".into(),
                projection_signature: "sig".into(),
                ident_terms_version: "2".into(),
            },
        }
    }

    fn replay(oldest: u64, head: u64) -> Replay {
        Replay {
            oldest,
            at: at(head),
            changes: vec![],
        }
    }

    /// The four rebuild reasons and the one resume: a missing payload, a gap past the payload
    /// (oldest surviving row beyond its successor), a payload ahead of the head, another
    /// lineage; a payload whose successor survives resumes, an exactly-pruned-to-successor log
    /// too, and an empty log (oldest 0) never reads as a gap.
    #[test]
    fn only_a_payload_that_joins_the_surviving_history_resumes() {
        let payload = Payload::new(&at(10));
        assert_eq!(
            resume_point(None, &replay(1, 12)),
            Resume::Rebuild("no payload")
        );
        assert_eq!(
            resume_point(Some(&payload), &replay(12, 20)),
            Resume::Rebuild("history pruned past the payload")
        );
        assert_eq!(
            resume_point(Some(&payload), &replay(1, 9)),
            Resume::Rebuild("payload ahead of the canonical history")
        );
        let mut other = replay(1, 12);
        other.at.identity.pack_lineage_id = "M".into();
        assert_eq!(
            resume_point(Some(&payload), &other),
            Resume::Rebuild("incompatible payload")
        );
        assert_eq!(
            resume_point(Some(&payload), &replay(11, 20)),
            Resume::From(10)
        );
        assert_eq!(
            resume_point(Some(&payload), &replay(1, 10)),
            Resume::From(10)
        );
        assert_eq!(
            resume_point(Some(&payload), &replay(0, 10)),
            Resume::From(10)
        );
    }
}
