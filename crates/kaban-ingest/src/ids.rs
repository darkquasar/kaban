//! Deterministic node ids for ingested documents (docs-ingest-plan.md DI1.1 shape, the two
//! functions DI0 needs pulled forward per D-5; OD-D1 layout).
//!
//! Every derived id is the first 16 bytes of `blake3` over a D-4 preimage
//! (`kaban-ingest/v1/<purpose>` plus length-prefixed fields), shaped into the `UUIDv7` layout:
//! version nibble 7 and the RFC 4122 variant bits are forced, the other 122 bits are the hash.
//! Core F17 asks for `UUIDv7` ids everywhere; derived ids keep that layout so `NodeId::parse`
//! accepts them and sorting stays stable, while the "timestamp" bits carry no time. Distinct
//! purposes have distinct domain tags, so a `blocked_document_id` never equals a `document_id`.
//! The encoding is frozen in `docs/ids.md` (DI1.1).

use crate::hash::preimage;
use crate::model::SourceId;
use kaban_core::{NodeId, WorkspaceId};

/// Version of the ingestion pipeline recorded on every Document and compared by the skip
/// decision; bump it to re-id and re-chunk every document on the next run.
pub const PIPELINE_VERSION: u32 = 1;

/// Shape 16 hash bytes into a UUIDv7-layout [`NodeId`].
fn derive(purpose: &str, fields: &[&[u8]]) -> NodeId {
    let digest = blake3::hash(&preimage(purpose, fields));
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest.as_bytes()[..16]);
    let uuid = uuid::Builder::from_bytes(bytes)
        .with_version(uuid::Version::SortRand)
        .with_variant(uuid::Variant::RFC4122)
        .into_uuid();
    // Invariant: the builder just forced version 7 and the RFC 4122 variant, which is all
    // `NodeId` validates (no timestamp plausibility check, OD-D1).
    #[allow(clippy::expect_used)]
    NodeId::try_from(uuid).expect("a v7-shaped uuid is a valid NodeId")
}

/// The current `Document` node of a source: stable across content edits, so links, `PART_OF`
/// edges and recall cards keep pointing at it (D2, SR-6).
pub fn document_id(source: &SourceId) -> NodeId {
    derive("document", &[source.as_str().as_bytes()])
}

/// The archived snapshot node of a superseded version of a source (`--history` only, D3,
/// DI3.4): derived from the source and the old `content_hash`, under its own domain tag, so it
/// never collides with [`document_id`] and two versions never collide with each other.
pub fn document_version_id(source: &SourceId, content_hash: &str) -> NodeId {
    derive(
        "document-version",
        &[source.as_str().as_bytes(), content_hash.as_bytes()],
    )
}

/// The `Tag` node of a workspace: one node per `(workspace, tag)`, shared by every document
/// that carries the tag (`TAGGED` edges, D3). The tag is used as written (nested `a/b` kept);
/// case folding, if any, is the graph assembler's call (DI1.5).
pub fn tag_id(workspace: &WorkspaceId, tag: &str) -> NodeId {
    derive("tag", &[workspace.as_str().as_bytes(), tag.as_bytes()])
}

/// The `Document{status: blocked}` node of a file whose path the denylist rejected. No
/// `SourceId` exists for such a file; the id derives from its keyed path id alone (PR-2).
pub fn blocked_document_id(keyed_path_id: &str) -> NodeId {
    derive("blocked-document", &[keyed_path_id.as_bytes()])
}

/// A heading path as preimage fields: its element count, then every element (each field is
/// length-prefixed by `preimage`, so `["a b"]` and `["a", "b"]` differ by the count field).
fn path_fields<'a>(path: &'a [String], count: &'a [u8; 8]) -> Vec<&'a [u8]> {
    let mut fields: Vec<&[u8]> = Vec::with_capacity(path.len() + 1);
    fields.push(count);
    fields.extend(path.iter().map(String::as_bytes));
    fields
}

/// A `Section` node: the source, its heading path and the ordinal among sections with the same
/// heading path (D2; positional props are not part of the id).
pub fn section_id(
    source: &SourceId,
    heading_path: &[String],
    ordinal: u32,
    pipeline_version: u32,
) -> NodeId {
    let count = (heading_path.len() as u64).to_le_bytes();
    let ordinal = ordinal.to_le_bytes();
    let version = pipeline_version.to_le_bytes();
    let mut fields = vec![source.as_str().as_bytes()];
    fields.extend(path_fields(heading_path, &count));
    fields.push(&ordinal);
    fields.push(&version);
    derive("section", &fields)
}

/// A `Chunk` node: `blake3(source_id, section_path, text_hash, ordinal_among_identical,
/// pipeline_version)` (D2, SR-6): content and structure only, so an edit re-ids exactly the
/// chunks whose text or section changed. `text_hash` is the blake3 of the sanitized text.
pub fn chunk_id(
    source: &SourceId,
    section_path: &[String],
    text_hash: &str,
    ordinal_among_identical: u32,
    pipeline_version: u32,
) -> NodeId {
    let count = (section_path.len() as u64).to_le_bytes();
    let ordinal = ordinal_among_identical.to_le_bytes();
    let version = pipeline_version.to_le_bytes();
    let mut fields = vec![source.as_str().as_bytes()];
    fields.extend(path_fields(section_path, &count));
    fields.push(text_hash.as_bytes());
    fields.push(&ordinal);
    fields.push(&version);
    derive("chunk", &fields)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn derived_ids_parse_as_node_ids_and_differ_by_purpose() {
        let keyed = "ab".repeat(32);
        let blocked = blocked_document_id(&keyed);
        assert!(NodeId::parse(blocked.as_str()).is_ok());
        let doc = document_id(&SourceId::from_hex(keyed.clone()));
        assert_ne!(blocked, doc, "distinct domain tags over the same bytes");
        assert_eq!(blocked, blocked_document_id(&keyed), "deterministic");
    }
}
