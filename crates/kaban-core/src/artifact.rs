//! Source-bound identity for derived publications.

use crate::{Node, NodeId, Producer, WorkspaceId};
use serde::{Deserialize, Serialize};

/// Closed shipped producer identity. Source lineage and complete-set generations are not
/// extractor identities, and configuration hashes remain separate artifact inputs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuiltinProducer {
    /// Logical extractor `gazetteer`, version `1`, stable producer `gazetteer@1`.
    Gazetteer,
}

impl BuiltinProducer {
    /// Stable identity of the shipped descriptor, never inferred from stored publications.
    #[must_use]
    pub const fn producer_id(self) -> &'static str {
        "gazetteer@1"
    }

    /// Logical extractor name.
    #[must_use]
    pub const fn extractor_id(self) -> &'static str {
        "gazetteer"
    }

    /// Descriptor version.
    #[must_use]
    pub const fn extractor_version(self) -> &'static str {
        "1"
    }
}

/// Content-free offline findings about derived publication freshness.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ArtifactDiagnostics {
    /// Tenant artifacts whose source revision, policy or active lifecycle no longer matches.
    pub stale_artifacts: u64,
}

/// An opaque, deterministic derived publication identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ArtifactKey(String);

impl ArtifactKey {
    /// The persisted key.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The source state against which a producer computed its output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceRev {
    /// Canonical source content hash.
    pub content_hash: String,
    /// Persisted source revision, including lifecycle edits.
    pub rev: i64,
}

/// Active tenant-scoped source coordinates for constructing a derived publication.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactSource {
    /// Source storage coordinate, resolved by the store without exposing SQL to producers.
    pub source_rid: i64,
    /// Current source revision to guard at publication time.
    pub source_rev: SourceRev,
}

impl From<&Node> for SourceRev {
    fn from(node: &Node) -> Self {
        Self {
            content_hash: node.content_hash.clone(),
            rev: node.rev,
        }
    }
}

/// Derive an identity using domain-separated, length-delimited inputs.
#[must_use]
pub fn artifact_key(
    source_rev: &SourceRev,
    layer: Producer,
    producer_id: &str,
    config_hash: &str,
    policy_version: &str,
) -> ArtifactKey {
    let mut hash = blake3::Hasher::new_derive_key("kaban.derived-artifact.v1");
    for field in [
        source_rev.content_hash.as_bytes(),
        &source_rev.rev.to_be_bytes(),
        layer.as_str().as_bytes(),
        producer_id.as_bytes(),
        config_hash.as_bytes(),
        policy_version.as_bytes(),
    ] {
        hash.update(&(field.len() as u64).to_be_bytes());
        hash.update(field);
    }
    ArtifactKey(hash.finalize().to_hex().to_string())
}

/// One publication committed together with its derived operations and claims.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DerivedArtifact {
    /// Identity computed by [`artifact_key`].
    pub key: ArtifactKey,
    /// Publication tenant.
    pub workspace_id: WorkspaceId,
    /// The producer of the publication (stored in the `layer` column until T4); authored
    /// publications are refused.
    pub layer: Producer,
    /// Versioned producer identifier.
    pub producer_id: String,
    /// Source storage coordinate, checked inside the transaction.
    pub source_rid: i64,
    /// Expected source state before any batch writes.
    pub source_rev: SourceRev,
    /// Effective producer configuration hash.
    pub config_hash: String,
    /// Existing policy hash, not a numeric policy version.
    pub policy_version: String,
    /// Publication timestamp in Unix seconds.
    pub published_at: i64,
}

/// A complete Gazetteer producer set for one ingest-owned Document.
/// Empty sets explicitly turn off that Document's Gazetteer projections.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GazetteerClaimSet {
    /// The ingest-owned Document, whose source id binds the retirement scope.
    pub document: NodeId,
    /// Source identity from the Document's props; lineage is derived, never caller-selected.
    pub source_id: String,
    /// Expected pre-batch Document revision; `None` requires first publication.
    pub source_rev: Option<SourceRev>,
    /// Optimistic producer-state guard, initially zero.
    pub expected_generation: u64,
    /// Full effective run digest (config, dictionary and enabled/disabled mode), not merely
    /// the raw Gazetteer configuration section.
    pub config_hash: String,
}
