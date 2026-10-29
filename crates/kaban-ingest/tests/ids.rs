//! Deterministic identities (docs-ingest-plan.md DI1.1; docs-ingest-spec D2, SR-6; OD-D1):
//! the same inputs give the same id, every identity-bearing input changes it, positional
//! props never enter it, and the distinct purposes never collide.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

use kaban_core::WorkspaceId;
use kaban_ingest::{PIPELINE_VERSION, SourceId, chunk_id, gate_path, section_id};
use kaban_policy::Policy;

fn source(path: &str) -> SourceId {
    let gated = gate_path(&Policy::empty(), path).unwrap();
    SourceId::new(&WorkspaceId::default(), &gated)
}

fn text_hash(text: &str) -> String {
    blake3::hash(text.as_bytes()).to_hex().to_string()
}

fn path(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|p| (*p).to_owned()).collect()
}

/// Plan slice 1: the same inputs give the same section and chunk ids, across calls and across
/// two equal `SourceId` values; a different source gives different ids.
#[test]
fn same_inputs_give_the_same_section_and_chunk_ids() {
    let src = source("docs/a.md");
    let same = source("docs/a.md");
    let heading = path(&["Intro"]);
    let th = text_hash("hello");

    let s1 = section_id(&src, &heading, 0, PIPELINE_VERSION);
    let s2 = section_id(&same, &heading, 0, PIPELINE_VERSION);
    assert_eq!(s1, s2, "section id is a pure function of its inputs");

    let c1 = chunk_id(&src, &heading, &th, 0, PIPELINE_VERSION);
    let c2 = chunk_id(&same, &heading, &th, 0, PIPELINE_VERSION);
    assert_eq!(c1, c2, "chunk id is a pure function of its inputs");

    let other = source("docs/b.md");
    assert_ne!(
        chunk_id(&other, &heading, &th, 0, PIPELINE_VERSION),
        c1,
        "another source, another chunk id"
    );
    assert_ne!(s1, c1, "a section and a chunk never share an id");
}

/// Plan slice 2: a different `pipeline_version` re-ids every section and chunk (bumping it
/// re-chunks the corpus on the next run).
#[test]
fn a_different_pipeline_version_gives_different_section_and_chunk_ids() {
    let src = source("docs/a.md");
    let heading = path(&["Intro"]);
    let th = text_hash("hello");
    assert_ne!(
        section_id(&src, &heading, 0, PIPELINE_VERSION),
        section_id(&src, &heading, 0, PIPELINE_VERSION + 1)
    );
    assert_ne!(
        chunk_id(&src, &heading, &th, 0, PIPELINE_VERSION),
        chunk_id(&src, &heading, &th, 0, PIPELINE_VERSION + 1)
    );
}

/// Plan slice 3: two chunks with identical text in one section are told apart by their
/// ordinal among identical chunks (0 and 1), and so are two sections with the same heading
/// path; the ordinal is the only difference in their inputs.
#[test]
fn identical_chunks_in_one_section_get_distinct_ids_by_ordinal() {
    let src = source("docs/a.md");
    let heading = path(&["Notes"]);
    let th = text_hash("repeated paragraph");
    let first = chunk_id(&src, &heading, &th, 0, PIPELINE_VERSION);
    let second = chunk_id(&src, &heading, &th, 1, PIPELINE_VERSION);
    assert_ne!(first, second, "ordinal 0 and 1 are distinct chunks");
    assert_ne!(
        section_id(&src, &heading, 0, PIPELINE_VERSION),
        section_id(&src, &heading, 1, PIPELINE_VERSION),
        "two sections with the same heading path are distinct by ordinal"
    );
}

/// Ids of a 3-section fixture: every `(section_path, text)` pair, ordinals counted among
/// identical texts in the same section, in document order.
fn fixture_ids(sections: &[(&[&str], &[&str])]) -> std::collections::BTreeSet<String> {
    let src = source("docs/three.md");
    let mut out = std::collections::BTreeSet::new();
    for (section_ordinal, (heading, paragraphs)) in sections.iter().enumerate() {
        let heading = path(heading);
        out.insert(
            section_id(
                &src,
                &heading,
                u32::try_from(section_ordinal).unwrap(),
                PIPELINE_VERSION,
            )
            .as_str()
            .to_owned(),
        );
        let mut seen: std::collections::BTreeMap<String, u32> = std::collections::BTreeMap::new();
        for p in *paragraphs {
            let th = text_hash(p);
            let ordinal = seen.entry(th.clone()).or_default();
            out.insert(
                chunk_id(&src, &heading, &th, *ordinal, PIPELINE_VERSION)
                    .as_str()
                    .to_owned(),
            );
            *ordinal += 1;
        }
    }
    out
}

/// Plan slice 4 (SR-6): editing the first paragraph of a 3-section fixture changes exactly
/// one chunk id; sections 2 and 3 and their chunks keep theirs even though every byte offset
/// after the edit moved (positional props are not identity).
#[test]
fn a_prefix_edit_re_ids_only_the_edited_chunk_of_a_three_section_fixture() {
    let before = fixture_ids(&[
        (&["Intro"], &["first paragraph", "second paragraph"]),
        (&["Intro", "Details"], &["detail one", "detail two"]),
        (&["Outro"], &["closing words"]),
    ]);
    let after = fixture_ids(&[
        (&["Intro"], &["first paragraph, edited", "second paragraph"]),
        (&["Intro", "Details"], &["detail one", "detail two"]),
        (&["Outro"], &["closing words"]),
    ]);
    let removed: Vec<_> = before.difference(&after).collect();
    let added: Vec<_> = after.difference(&before).collect();
    assert_eq!(
        removed.len(),
        1,
        "exactly the edited chunk disappears: {removed:?}"
    );
    assert_eq!(added.len(), 1, "exactly its replacement appears: {added:?}");
    assert_eq!(before.len(), after.len());
}

/// Plan slices 5 and 6: `document_id` depends on the source alone (stable across content
/// edits); `document_version_id` differs from `document_id` and from itself for two different
/// content hashes (a snapshot never collides with the current node).
#[test]
fn document_id_is_stable_across_edits_and_version_ids_never_collide_with_it() {
    use kaban_ingest::{document_id, document_version_id};
    let src = source("docs/a.md");
    // two distinct sanitized-content hashes (the real ones come from `content_hash(&graph)`)
    let v1 = blake3::hash(b"# a\n\nfirst\n").to_hex().to_string();
    let v2 = blake3::hash(b"# a\n\nsecond\n").to_hex().to_string();
    let current = document_id(&src);
    assert_eq!(current, document_id(&source("docs/a.md")));

    let snap1 = document_version_id(&src, &v1);
    let snap2 = document_version_id(&src, &v2);
    assert_ne!(snap1, current, "a snapshot is not the current node");
    assert_ne!(snap2, current);
    assert_ne!(snap1, snap2, "two content versions, two snapshots");
    assert_eq!(snap1, document_version_id(&src, &v1), "deterministic");
}

/// Plan slice 7: a `blocked_document_id` (keyed path id, no `SourceId`) never equals a
/// `document_id`, even when the keyed id bytes equal a source id's hex; every derived id
/// parses as a `NodeId` (v7 layout, OD-D1); `tag_id` is per workspace and per tag.
#[test]
fn blocked_document_ids_never_collide_with_document_ids_and_tag_ids_are_per_workspace() {
    use kaban_core::NodeId;
    use kaban_ingest::{blocked_document_id, document_id, tag_id};
    let src = source("docs/a.md");
    let doc = document_id(&src);
    // the adversarial case: a keyed id whose bytes equal the source id's hex
    assert_ne!(blocked_document_id(src.as_str()), doc);
    assert_ne!(blocked_document_id(&"0".repeat(64)), doc);
    for id in [
        &doc,
        &blocked_document_id(src.as_str()),
        &section_id(&src, &path(&["H"]), 0, PIPELINE_VERSION),
        &chunk_id(&src, &path(&["H"]), &text_hash("t"), 0, PIPELINE_VERSION),
    ] {
        assert!(NodeId::parse(id.as_str()).is_ok(), "{id} is a NodeId");
    }

    let ws = WorkspaceId::default();
    let other_ws = WorkspaceId::new("team");
    assert_eq!(tag_id(&ws, "project/alpha"), tag_id(&ws, "project/alpha"));
    assert_ne!(tag_id(&ws, "project/alpha"), tag_id(&ws, "project/beta"));
    assert_ne!(
        tag_id(&ws, "project/alpha"),
        tag_id(&other_ws, "project/alpha")
    );
    assert_ne!(
        tag_id(&ws, src.as_str()),
        doc,
        "a tag never collides with a document"
    );
}

/// The props structs are the wire shape of the Document, Section and Chunk nodes: the keys DI0's
/// skip decision reads (`content_hash`, `blocked_hmac`, `pipeline_version`, `extractor_version`,
/// `config_hash`) serialize under exactly those names, absent options and empty maps are
/// omitted, and a chunk round-trips through JSON including its `&'static str` estimator.
#[test]
fn props_serialize_under_the_skip_key_names_and_round_trip() {
    use kaban_ingest::{
        ChunkKind, ChunkProps, DocStatus, DocumentProps, Format, SectionKind, SectionProps,
        TOKEN_ESTIMATOR,
    };
    let src = source("docs/a.md");
    let doc = DocumentProps {
        source_id: src.as_str().to_owned(),
        path: "docs/a.md".into(),
        content_hash: Some("ab".repeat(32)),
        blocked_hmac: None,
        position_fingerprint: None,
        version: 0,
        current: true,
        format: Format::Markdown,
        size: 12,
        mtime: 1_793_004_407,
        title: Some("A".into()),
        lang: None,
        frontmatter: serde_json::json!({}),
        pipeline_version: PIPELINE_VERSION,
        extractor: "pulldown-cmark@0.13.4".into(),
        extractor_version: "pulldown-cmark@0.13.4".into(),
        config_hash: "cfg".into(),
        status: DocStatus::Ok,
        error: None,
        needs_ocr: false,
        page_count: None,
        blocked_chunks: 0,
        table_unknown_chunks: 0,
        rule_ids: std::collections::BTreeMap::new(),
    };
    let json = serde_json::to_value(&doc).unwrap();
    assert_eq!(json["content_hash"], "ab".repeat(32));
    assert_eq!(json["pipeline_version"], PIPELINE_VERSION);
    assert_eq!(json["extractor_version"], "pulldown-cmark@0.13.4");
    assert_eq!(json["config_hash"], "cfg");
    assert_eq!(json["status"], "ok");
    assert_eq!(json["format"], "markdown");
    for absent in [
        "blocked_hmac",
        "lang",
        "error",
        "page_count",
        "table_unknown_chunks",
        "rule_ids",
    ] {
        assert!(
            json.get(absent).is_none(),
            "{absent} omitted when empty: {json}"
        );
    }
    let back: DocumentProps = serde_json::from_value(json).unwrap();
    assert_eq!(back, doc);

    let section = SectionProps {
        heading_path: path(&["Intro"]),
        level: 1,
        order: 0,
        byte_start: 0,
        byte_end: 40,
        page_start: None,
        page_end: None,
        kind: SectionKind::Heading,
    };
    let json = serde_json::to_value(&section).unwrap();
    assert_eq!(json["kind"], "heading");
    assert_eq!(json["heading_path"], serde_json::json!(["Intro"]));

    let chunk = ChunkProps {
        text: "hello".into(),
        text_hash: text_hash("hello"),
        token_estimate: 2,
        token_estimator: TOKEN_ESTIMATOR,
        order: 0,
        byte_start: 0,
        byte_end: 5,
        page: None,
        context_prefix: "A > Intro".into(),
        lang: Some("eng".into()),
        kind: ChunkKind::Code {
            lang: Some("rust".into()),
        },
        oversize: false,
        table_unknown: false,
        stub_hmac: None,
        bytes: 5,
        slices: 1,
    };
    let json = serde_json::to_string(&chunk).unwrap();
    assert!(json.contains("\"token_estimator\":\"chars4\""), "{json}");
    let back: ChunkProps = serde_json::from_str(&json).unwrap();
    assert_eq!(back, chunk);
    assert!(
        serde_json::from_str::<ChunkProps>(&json.replace("chars4", "bpe")).is_err(),
        "an unknown estimator is not accepted"
    );
}
