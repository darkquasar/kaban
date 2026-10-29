//! Tier availability probes (stage1-spec G1-S5.1 "every tier is probed, never assumed";
//! stage1-plan CG4.3, CG4.4). `namespaces`: [`namespaces::probe_namespaces`] (the `unshare`
//! handle from the fixed list runs `kaban-index ns-helper --probe-only`, whose in-run
//! self-check must pass; the sysctls that disable user namespaces are read first).
//! `container`: deferred past 3.0.0 by OWN-68 SC-11; the probe reports the tier
//! `unavailable (not_in_this_release)` without resolving the CLI or reading the image key (the
//! more honest of the two spellings the brief offered: a configured image is not the reason
//! the tier does not run, and nothing is attempted). `Tier::Container`, the candidate list and
//! the `container_image`/`container_cli` keys stay reserved. Off Linux the namespaces probe is
//! `handle_exec_unsupported` (RV-9). Nothing from a probe's output reaches the report except
//! the helper's fixed `overlay=` word: the detail is kaban-built from the candidate path and
//! `uname`.

use std::path::Path;

use kaban_core::config::Index;

use super::namespaces;
use super::{Probe, ReasonClass, Tier};

pub use namespaces::PROBE_DEADLINE;

/// Probe both isolating tiers under `cfg` (`index.container_cli`, `index.container_image`);
/// `helper` is the `kaban-index` binary the namespaces probe runs (`/proc/self/exe`).
#[must_use]
pub fn probe_all(cfg: &Index, helper: &Path) -> Vec<Probe> {
    vec![namespaces::probe_namespaces(helper), probe_container(cfg)]
}

/// The `container` probe: reserved, never available in this release (OWN-68 SC-11). The
/// configuration keys are accepted and ignored here; `_cfg` keeps the signature CG4.5 will fill.
#[must_use]
pub fn probe_container(_cfg: &Index) -> Probe {
    Probe {
        tier: Tier::Container,
        available: Err(ReasonClass::NotInThisRelease),
    }
}
