//! Id and timestamp helpers shared by the store and the audit log, and the validated
//! [`NodeId`] newtype (core-plan C2.6 shape, landed in C2.2 per MD-10).

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Current UTC time as an RFC 3339 string (empty string only if formatting fails, which the
/// well-known format cannot do).
pub fn now_iso() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

/// A fresh `UUIDv7` (time-ordered) id as a lowercase hyphenated string.
pub fn new_id() -> String {
    uuid::Uuid::now_v7().to_string()
}

/// A node id: a `UUIDv7` in lowercase hyphenated form, validated at construction (version
/// nibble 7 and RFC 4122 variant; no timestamp plausibility check, OD-D1). Derived ids written
/// by docs-ingest are `UUIDv7` by construction and parse the same way.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
#[repr(transparent)]
pub struct NodeId(String);

/// Why a string is not a node id. The message never echoes the input (it may be a secret that
/// the adapter has not yet masked).
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum InvalidNodeId {
    /// Not a hyphenated UUID at all.
    #[error("node ids are UUIDs in hyphenated form")]
    NotAUuid,
    /// A UUID, but not version 7.
    #[error("node ids are UUIDv7 (version nibble was {0})")]
    WrongVersion(usize),
    /// A UUID, but not the RFC 4122 variant.
    #[error("node ids are RFC 4122 UUIDs (variant bits did not match)")]
    WrongVariant,
}

impl NodeId {
    /// A fresh time-ordered id.
    pub fn new() -> Self {
        Self(new_id())
    }

    /// Validate and normalise (lowercase) a hyphenated `UUIDv7` string.
    ///
    /// # Errors
    /// [`InvalidNodeId`] when the string is not a hyphenated RFC 4122 UUID of version 7.
    pub fn parse(s: &str) -> Result<Self, InvalidNodeId> {
        let u = uuid::Uuid::try_parse(s).map_err(|_| InvalidNodeId::NotAUuid)?;
        if s.len() != 36 {
            // `try_parse` also accepts simple/braced/urn forms; ids are stored hyphenated
            return Err(InvalidNodeId::NotAUuid);
        }
        Self::try_from(u)
    }

    /// The id as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for NodeId {
    fn default() -> Self {
        Self::new()
    }
}

impl TryFrom<uuid::Uuid> for NodeId {
    type Error = InvalidNodeId;
    fn try_from(u: uuid::Uuid) -> Result<Self, InvalidNodeId> {
        if u.get_variant() != uuid::Variant::RFC4122 {
            return Err(InvalidNodeId::WrongVariant);
        }
        let v = u.get_version_num();
        if v != 7 {
            return Err(InvalidNodeId::WrongVersion(v));
        }
        Ok(Self(u.hyphenated().to_string()))
    }
}

impl TryFrom<String> for NodeId {
    type Error = InvalidNodeId;
    fn try_from(s: String) -> Result<Self, InvalidNodeId> {
        Self::parse(&s)
    }
}

impl std::str::FromStr for NodeId {
    type Err = InvalidNodeId;
    fn from_str(s: &str) -> Result<Self, InvalidNodeId> {
        Self::parse(s)
    }
}

impl From<NodeId> for String {
    fn from(id: NodeId) -> Self {
        id.0
    }
}

impl AsRef<str> for NodeId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for NodeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion
mod tests {
    use super::*;

    #[test]
    fn node_ids_are_hyphenated_lowercase_uuidv7_and_everything_else_is_rejected() {
        let fresh = NodeId::new();
        assert_eq!(fresh.as_str().len(), 36);
        assert_eq!(NodeId::parse(fresh.as_str()).unwrap(), fresh);
        // uppercase input is accepted and normalised
        let upper = fresh.as_str().to_ascii_uppercase();
        assert_eq!(NodeId::parse(&upper).unwrap(), fresh);
        assert_eq!(
            NodeId::parse("not-a-uuid"),
            Err(InvalidNodeId::NotAUuid),
            "C2.6 slice: NodeId::parse(\"not-a-uuid\") fails"
        );
        // a v4 UUID has the right shape but the wrong version nibble
        assert_eq!(
            NodeId::parse("3f2504e0-4f89-41d3-9a0c-0305e82c3301"),
            Err(InvalidNodeId::WrongVersion(4))
        );
        // version 7 but a non-RFC variant (bits 110x) is rejected
        assert_eq!(
            NodeId::parse("01a17a03-5722-7000-c000-000000000001"),
            Err(InvalidNodeId::WrongVariant)
        );
        // the simple (unhyphenated) form is not an id
        let simple = fresh.as_str().replace('-', "");
        assert_eq!(NodeId::parse(&simple), Err(InvalidNodeId::NotAUuid));
        // serde round trip validates on the way in
        let json = serde_json::to_string(&fresh).unwrap();
        assert_eq!(json, format!("\"{fresh}\""));
        assert!(serde_json::from_str::<NodeId>("\"nope\"").is_err());
        let err = NodeId::parse("hunter2-secret").unwrap_err().to_string();
        assert!(
            !err.contains("hunter2"),
            "errors never echo the input: {err}"
        );
    }
}
