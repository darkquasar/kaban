//! The explanation of one conflict group (engine-spec EN-F14; engine-plan E2.4): what the
//! weighted vote saw and why it decided as it did, every number in the four-decimal rendering
//! of the EN-F12a integers. The numbers are weights of evidence (lane C §3), nothing more; the
//! store assembles the document, this module owns its shape. The evaluation counters of
//! EN-F16 (`priors_obs`) live here too, since `doctor --priors` prints them beside the weights.

use super::{
    AssertionKey, AssertionStatus, ClaimClass, Fixed, Layer, Method, Rank, UnresolvedReason,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The `rule` of every v1 explanation.
pub const WEIGHTED_VOTE_V1: &str = "weighted_vote_v1";

/// The explanation of a conflict group (EN-F14). Optional fields are absent from the JSON when
/// they do not apply (`serde-skip-empty`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Explanation {
    /// The decision rule (`weighted_vote_v1`).
    pub rule: String,
    /// The canonical group key (`group_key`).
    pub group_key: String,
    /// The group's outcome: `winner`, `candidate`, `unresolved` or `rejected`.
    pub verdict: AssertionStatus,
    /// The values that voted, in decision order (the leader first).
    pub candidates: Vec<CandidateView>,
    /// The leader's lead over the runner-up (`1.0000` for a lone candidate).
    pub margin: String,
    /// The margin gate the leader had to clear for its conflict class.
    pub threshold: String,
    /// The minimum confidence of the leader's kind or relationship (`0.0000` when undeclared).
    pub min_confidence: String,
    /// The `compare_candidates` level that split two equal sums, when one did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tie_break: Option<String>,
    /// Why the group needs an explicit action, when it does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unresolved_reason: Option<UnresolvedReason>,
    /// The human claim that protects the group's row (EN-F12c), when one does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protected_by: Option<AssertionKey>,
    /// The entity-resolution waterfall (EN-F51; E2.10), absent until then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub er_contributions: Option<Vec<Contribution>>,
}

/// One value of a group and the lineages behind it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CandidateView {
    /// The claimed value (a kind name, a canonical id, edge props).
    pub value: Value,
    /// The value's share of the group's weight.
    pub p: String,
    /// The evidence lineages backing the value, by lineage id.
    pub lineages: Vec<LineageView>,
}

/// One lineage behind a value: its strongest supporter and how many more it holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineageView {
    /// The lineage (EN-F12b).
    pub lineage_id: String,
    /// The rank of the lineage's best supporter.
    pub rank: Rank,
    /// The supporter that counts for the lineage.
    pub best: SupporterView,
    /// Further supporters of the same lineage and value (counted once, EN-F12b).
    pub others: u32,
}

/// One supporting claim as the vote weighed it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SupporterView {
    /// The claim's stable key.
    pub assertion_key: AssertionKey,
    /// Source layer.
    pub layer: Layer,
    /// Extraction method.
    pub method: Method,
    /// The claim's own confidence.
    pub raw_score: String,
    /// The configured reliability of `(layer, method, claim_class)`.
    pub prior: String,
}

/// One field's contribution to an entity-resolution score (EN-F51; E2.10).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Contribution {
    /// The compared field.
    pub field: String,
    /// The agreement level observed.
    pub level: String,
    /// The field's weight.
    pub w: String,
}

/// What a human action said about the reconciled claims of a node (EN-F16).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PriorsOutcome {
    /// `accept`: the claims held.
    Accepted,
    /// `forget`: the claims did not.
    Rejected,
}

/// One `priors_obs` row beside its configured prior (EN-F16): the evidence `doctor --priors`
/// prints; v1 never applies it (`calibration_version` stays 0).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PriorsObs {
    /// Source layer.
    pub layer: Layer,
    /// Extraction method.
    pub method: Method,
    /// Conflict class.
    pub claim_class: ClaimClass,
    /// The configured reliability of the triple.
    pub prior: Fixed,
    /// Reconciled winners and candidates of the triple a human accepted.
    pub accepted: u64,
    /// Reconciled winners and candidates of the triple a human forgot.
    pub rejected: u64,
}

impl PriorsObs {
    /// The Beta posterior mean `(accepted + 1) / (accepted + rejected + 2)` (EN-F16), rounded
    /// half up to four decimals; `0.5000` before any observation.
    pub fn observed(&self) -> Fixed {
        let num = u128::from(self.accepted + 1) * 10_000;
        let den = u128::from(self.accepted + self.rejected + 2);
        let rounded = (num * 2 + den) / (den * 2);
        Fixed(u32::try_from(rounded).unwrap_or(10_000).min(10_000))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn four_decimals_and_the_posterior_mean() {
        assert_eq!(Fixed(6556).to_string(), "0.6556");
        assert_eq!(Fixed(10_000).to_string(), "1.0000");
        assert_eq!(Fixed(7).to_string(), "0.0007");
        let fresh = PriorsObs {
            layer: Layer::Llm,
            method: Method::Llm,
            claim_class: ClaimClass::Type,
            prior: Fixed(7500),
            accepted: 0,
            rejected: 0,
        };
        assert_eq!(fresh.observed(), Fixed(5000));
        let seen = PriorsObs {
            accepted: 2,
            rejected: 0,
            ..fresh
        };
        assert_eq!(seen.observed(), Fixed(7500));
    }
}
