//! The commit payload (spec 05 SS-09): every commit of the index carries `{seq, identities,
//! versions}` inside Tantivy's `meta.json`, so the segment set and its watermark change
//! together or not at all. Compatibility binds to the pack lineage and the versions this build
//! supports, never to the instance (design amendment 2): an index built for a lineage serves
//! every instance of it. The versions are the build's own (schema, analyzer, the identifier
//! rule `IDENT_TERMS_VERSION` the `ident_terms` field is derived with, the linked Tantivy) plus
//! the projection the database is committed to (the `summary` field is computed from it), so a
//! payload advertising a version this build does not implement is incompatible even when the
//! database advertises the same label, and a rebuild stamps what this build implements.

use crate::analyzers::ANALYZER_VERSION;
use crate::schema::SCHEMA_VERSION;
use crate::{Error, Result};
use kaban_core::ident::IDENT_TERMS_VERSION;
use kaban_core::text_index::Canonical;
use serde::{Deserialize, Serialize};
use tantivy::Index;

/// The payload layout version.
const PAYLOAD_VERSION: u32 = 1;

/// See the module doc.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Payload {
    /// [`PAYLOAD_VERSION`].
    pub(crate) v: u32,
    /// The newest `changes.seq` the index incorporates: the visibility watermark.
    pub(crate) seq: u64,
    /// The database the commit was made on (receipts only).
    pub(crate) instance_id: String,
    /// The lineage the index serves.
    pub(crate) pack_lineage_id: String,
    /// What the projection was computed with.
    pub(crate) versions: Versions,
}

/// The versions a payload is compatible under.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Versions {
    pub(crate) schema: u32,
    pub(crate) analyzer: u32,
    /// The `IDENT_TERMS_VERSION` of the build that derived the `ident_terms` field.
    pub(crate) ident_terms: String,
    /// The database's `fts_projection_signature`.
    pub(crate) projection: String,
    /// The Tantivy version that wrote the index.
    pub(crate) tantivy: String,
}

impl Payload {
    /// The payload of a commit made at `at` by this build.
    pub(crate) fn new(at: &Canonical) -> Self {
        Self {
            v: PAYLOAD_VERSION,
            seq: at.seq,
            instance_id: at.identity.instance_id.clone(),
            pack_lineage_id: at.identity.pack_lineage_id.clone(),
            versions: Versions {
                schema: SCHEMA_VERSION,
                analyzer: ANALYZER_VERSION,
                ident_terms: IDENT_TERMS_VERSION.to_owned(),
                projection: at.identity.projection_signature.clone(),
                tantivy: tantivy::version_string().to_owned(),
            },
        }
    }

    /// The payload of the last commit, `None` for an index that was never committed with one.
    pub(crate) fn read(index: &Index) -> Result<Option<Self>> {
        Self::of(&index.load_metas()?)
    }

    /// The payload `metas` carries, `None` when its commit was made without one.
    pub(crate) fn of(metas: &tantivy::IndexMeta) -> Result<Option<Self>> {
        metas
            .payload
            .as_deref()
            .map(|json| serde_json::from_str(json).map_err(Error::Payload))
            .transpose()
    }

    /// The JSON written into the commit.
    pub(crate) fn to_json(&self) -> Result<String> {
        Ok(serde_json::to_string(self)?)
    }

    /// Whether this build implements the payload's layout and versions (schema, analyzer,
    /// identifier rule, Tantivy): what a captured index must satisfy before its lineage and
    /// projection are compared with a manifest's (SS-19).
    pub(crate) fn implemented(&self) -> bool {
        self.v == PAYLOAD_VERSION
            && self.versions.schema == SCHEMA_VERSION
            && self.versions.analyzer == ANALYZER_VERSION
            && self.versions.ident_terms == IDENT_TERMS_VERSION
            && self.versions.tantivy == tantivy::version_string()
    }

    /// Whether this build may serve the index at `at`: the same lineage, the versions this
    /// build implements ([`Self::implemented`]) and the projection the database is committed
    /// to. The database's own `ident_terms_version` is the FTS5 column's, not an input here:
    /// the field is derived by the build that stamped the payload.
    pub(crate) fn compatible(&self, at: &Canonical) -> bool {
        self.unfit(at).is_none()
    }

    /// Why this build may not serve the index at `at` (the first failing part of
    /// [`Self::compatible`], named for a count: `versions`, `lineage` or `projection`), or
    /// `None` when it may. The one compatibility predicate: serving and the cutover decision
    /// (SS-13b) read the same answer.
    pub(crate) fn unfit(&self, at: &Canonical) -> Option<&'static str> {
        if !self.implemented() {
            Some("versions")
        } else if self.pack_lineage_id != at.identity.pack_lineage_id {
            Some("lineage")
        } else if self.versions.projection != at.identity.projection_signature {
            Some("projection")
        } else {
            None
        }
    }

    /// Whether the watermark is within the surviving canonical history (an index restored
    /// beside an older database claims more than the database has).
    pub(crate) fn within(&self, at: &Canonical) -> bool {
        self.seq <= at.seq
    }
}

#[cfg(test)]
mod tests {
    use super::{IDENT_TERMS_VERSION, Payload};
    use kaban_core::text_index::{Canonical, IndexIdentity};

    fn at(lineage: &str, instance: &str, seq: u64) -> Canonical {
        Canonical {
            seq,
            identity: IndexIdentity {
                instance_id: instance.into(),
                pack_lineage_id: lineage.into(),
                projection_signature: "sig".into(),
                ident_terms_version: "2".into(),
            },
        }
    }

    /// Compatibility follows the lineage and the versions this build implements: another
    /// instance of the lineage (a fork) keeps the index, another lineage or projection does
    /// not; the watermark must be within the canonical history.
    #[test]
    fn compatibility_binds_to_lineage_and_versions_never_to_the_instance() {
        let payload = Payload::new(&at("L", "db-1", 7));
        assert!(payload.compatible(&at("L", "db-1", 7)));
        assert!(payload.compatible(&at("L", "db-2", 9)), "a fork keeps it");
        assert!(!payload.compatible(&at("M", "db-1", 7)), "another lineage");
        let mut other = at("L", "db-1", 7);
        other.identity.projection_signature = "sig2".into();
        assert!(!payload.compatible(&other), "another projection");
        assert!(payload.within(&at("L", "db-1", 7)));
        assert!(!payload.within(&at("L", "db-1", 6)), "ahead of the history");
    }

    /// The identifier rule is checked against what this build implements, never against the
    /// database's label (T1 review t1-2): a payload advertising a rule this build does not
    /// implement is incompatible even when the database advertises the same label, and a
    /// rebuild from such a database stamps the implemented rule, not the label.
    #[test]
    fn an_unsupported_identifier_rule_is_incompatible_whatever_the_database_says() {
        let mut database = at("L", "db-1", 7);
        database.identity.ident_terms_version = "3".into();
        assert_ne!(IDENT_TERMS_VERSION, "3", "the fixture's unsupported rule");
        let mut advertised = Payload::new(&database);
        advertised.versions.ident_terms = "3".into();
        assert!(
            !advertised.compatible(&database),
            "payload and database agree on 3"
        );
        assert!(!advertised.compatible(&at("L", "db-1", 7)));
        let rebuilt = Payload::new(&database);
        assert_eq!(rebuilt.versions.ident_terms, IDENT_TERMS_VERSION);
        assert!(
            rebuilt.compatible(&database),
            "the index derives its own terms"
        );
    }
}
