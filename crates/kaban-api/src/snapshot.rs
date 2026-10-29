//! The engine's CG-JOIN v1 adapter (§2, §4; engine-plan E2.6; `GM5a` RU3-2, RU3-5): the one
//! production constructor of [`SnapshotAttach`] from a [`Resolved`], the `policy` projection of
//! a [`SnapshotRef`], and the memory-card view of a resolution. No resolver logic of its own and
//! no git: [`KabanApi::resolve_current`] (`code/head.rs`, CG7.1 default 2) is the one head fetch
//! for the code tools, `remember` and `why(symbol)` alike.
//!
//! ```text
//!   why(symbol) ==> resolve_for_card(api)  ==> resolve_current (rev-parse HEAD, HeadCache, provider)
//!                     | Ok(Resolved)           ==> Resolved { lease, ref, attach }
//!                     | NoSnapshot             ==> Absent  (card: snapshot_absent, leg A only)
//!                     | SnapshotTooNew         ==> TooNew  (card: snapshot_too_new, leg A only)
//!   attach_from(&Resolved) ==> SnapshotAttach::from_admitted(encoded, commit, schema_version, generation)
//!     ==> store.attach_snapshot(&attach)     (the dedicated read connection; the lease drops at return)
//! ```
//!
//! `impl From<&Resolved> for SnapshotAttach` (the contract's spelling) is not expressible here:
//! both types are foreign to this crate (orphan rule), so the constructor is the free function
//! [`attach_from`]; `attach_built_only_from_resolved` (`tests/crate_graph.rs`) still asserts
//! that this file is the only production site of the constructor.

use kaban_codegraph::{Policy, Resolved, SnapshotRef};
use kaban_core::SnapshotAttach;

use crate::api::KabanApi;
use crate::code::CodeError;

/// The core-owned attach of a resolution (contract §2), minted from the admission's evidence
/// (spec 05 SS-25): the admitted URI verbatim, the resolved `SnapshotRef.commit`, the admitted
/// major and the file generation the admission inspected (read from `r.attach`, never from a
/// second `symlink_metadata`: one `Resolved` carries one generation, RU3-2).
pub(crate) fn attach_from(r: &Resolved) -> SnapshotAttach {
    SnapshotAttach::from_admitted(
        r.attach.as_sql_param().to_owned(),
        r.r#ref.commit.clone(),
        r.attach.schema_version(),
        format!("{:?}", r.attach.generation()),
    )
}

/// The contract §4 projection rule ([`Policy::of`]): `current` when the snapshot's
/// `meta.policy_version` is the live policy version, `off` when the scan was off at index time,
/// else `stale`.
pub(crate) fn policy_of(r: &SnapshotRef, live_policy_version: &str) -> Policy {
    Policy::of(Some(&r.policy_version), live_policy_version)
}

/// What a memory card learns from a resolution (contract §6): the degraded classes are card
/// fields, never errors.
#[derive(Debug)]
pub(crate) enum SnapshotOutcome {
    /// A snapshot resolved; the lease lives as long as this value.
    Resolved(Resolved),
    /// `NoSnapshot::{NoDirectory, NoFile, CorruptOnly}`, no provider, no repository, or git
    /// could not name the head.
    Absent,
    /// `NoSnapshot::TooNewOnly`.
    TooNew,
}

/// [`KabanApi::resolve_current`] for a card: `NoSnapshot` is [`SnapshotOutcome::Absent`],
/// `SnapshotTooNew` is [`SnapshotOutcome::TooNew`]; nothing else is an error (the head fetch
/// has no other outcome, and an unexpected class degrades like absence, traced).
pub(crate) fn resolve_for_card(api: &KabanApi) -> SnapshotOutcome {
    match api.resolve_current() {
        Ok(r) => SnapshotOutcome::Resolved(r),
        Err(CodeError::NoSnapshot) => SnapshotOutcome::Absent,
        Err(CodeError::SnapshotTooNew) => SnapshotOutcome::TooNew,
        Err(other) => {
            tracing::debug!(class = %other, "snapshot resolution degraded to absent");
            SnapshotOutcome::Absent
        }
    }
}

impl KabanApi {
    /// Run a symbol consumer with the admitted current snapshot attached and leased.
    /// Only its canonical commit is exposed; a missing/too-new snapshot detaches any stale read.
    ///
    /// # Errors
    /// Store attachment/detachment or consumer errors.
    pub fn with_current_snapshot<T, E>(
        &self,
        consume: impl FnOnce(Option<&str>) -> Result<T, E>,
    ) -> Result<T, E>
    where
        E: From<kaban_core::StoreError>,
    {
        match resolve_for_card(self) {
            SnapshotOutcome::Resolved(resolved) => {
                self.store.attach_snapshot(&attach_from(&resolved))?;
                consume(Some(&resolved.r#ref.commit))
            }
            SnapshotOutcome::Absent | SnapshotOutcome::TooNew => {
                self.store.detach_snapshot()?;
                consume(None)
            }
        }
    }
}
