//! DI1.8b (astra milestone review RD2-1, RD2-2, RD2-3; core-spec S1; docs-ingest-spec D6):
//! material the gate decided against must not re-enter storage through a derived channel:
//! the frontmatter parser (RD2-1), the chunker's trimming and merging (RD2-2), or the links and
//! tags extracted beside the masked text (RD2-3). Every case runs through the real `Ingestor`
//! over a file-backed store and inspects props, nodes, edges, ids, db/wal/shm bytes and `recall`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::real::{Rig, doc_id, rig_on_disk};
use kaban_core::{AuditEntry, MemoryStore, RecallQuery, WorkspaceId};
use kaban_ingest::{
    ChunkConfig, ExtractCtx, Extractor as _, Format, MarkdownExtractor, PrepareRequest,
    PreparedDocument, SourceId, gate_path, prepare_document, tag_id,
};
use kaban_policy::Policy;
use std::fs;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

const CANARY: &str = "KABANCANARY-9f3b2c1a-SECRET";

fn policy_with(extra_rules: &str) -> Policy {
    let yaml = format!("rules:\n{extra_rules}");
    Policy::from_file(Policy::parse(yaml.as_bytes()).unwrap(), "test".into()).unwrap()
}

fn contains_bytes(hay: &[u8], needle: &str) -> bool {
    hay.windows(needle.len()).any(|w| w == needle.as_bytes())
}

fn recall_is_empty(rig: &Rig, needle: &str) -> bool {
    rig.store
        .recall(&RecallQuery {
            query: needle.to_owned(),
            limit: 10,
            ..RecallQuery::default()
        })
        .unwrap()
        .is_empty()
}

/// Every stored node of the document (the Document itself, its sections and chunks) as one
/// JSON blob, for needle checks.
fn document_blob(rig: &Rig, rel: &str) -> String {
    let doc = rig.document(rel).expect("document stored");
    let mut blob = serde_json::to_string(&(&doc.text, &doc.props)).unwrap();
    for id in rig.chunk_ids(rel).iter().chain(rig.section_ids(rel).iter()) {
        let n = rig
            .store
            .get(&kaban_core::NodeId::parse(id).unwrap(), true)
            .unwrap()
            .unwrap();
        blob.push_str(&serde_json::to_string(&(&n.text, &n.props)).unwrap());
    }
    blob
}

/// The audit rows the pipeline attaches to `path`'s bytes under `policy` (the store exposes no
/// audit read, so the receipts are observed at the `prepare_document` seam with the rig's
/// signer), filtered to `field` and `rule`.
fn receipts(rig: &Rig, policy: &Policy, rel: &str, field: &str, rule: &str) -> Vec<AuditEntry> {
    let bytes = fs::read(rig.root.join(rel)).unwrap();
    let gated = gate_path(policy, rel).unwrap();
    let ws = WorkspaceId::default();
    let source = SourceId::new(&ws, &gated);
    let prepared = prepare_document(
        policy,
        rig.store.signer().as_ref(),
        &ctx(),
        &ws,
        &PrepareRequest {
            source: &source,
            path: &gated,
            format: Format::Markdown,
            size: u64::try_from(bytes.len()).unwrap(),
            mtime: 1_793_004_407,
            bytes: &bytes,
            config_hash: "cfg",
        },
        &ChunkConfig::default(),
    )
    .unwrap();
    let audit = match prepared {
        PreparedDocument::Ready { audit, .. } | PreparedDocument::Blocked { audit, .. } => audit,
        PreparedDocument::Failed { .. } => Vec::new(),
    };
    audit
        .into_iter()
        .filter(|a| a.field == field && a.rules.iter().any(|r| r == rule))
        .collect()
}

fn ctx() -> ExtractCtx {
    let gated = gate_path(&Policy::empty(), "docs/fixture.md").unwrap();
    ExtractCtx::new(
        CancellationToken::new(),
        tokio::time::Instant::now() + Duration::from_secs(10),
        SourceId::new(&WorkspaceId::default(), &gated),
    )
}

// ---- RD2-1: the frontmatter source is gated before any YAML interpretation ----

/// RD2-1: a redact rule that matches only the YAML source line (`customer: alice-7781`; neither
/// the bare value nor the `customer=alice-7781` view matches) is applied, not discarded: the
/// value is in no Document prop, in no db/wal byte and in no audit text, the rule is counted and
/// one keyed `frontmatter` receipt exists.
#[test]
fn a_source_only_frontmatter_redact_hit_leaves_no_value_in_props_db_wal_or_audit() {
    let policy = || {
        policy_with("  - id: fm-source\n    regex: 'customer: alice-7781'\n    action: redact\n")
    };
    assert!(policy().scan("alice-7781").hits.is_empty());
    assert!(policy().scan_kv("customer", "alice-7781").hits.is_empty());
    let rig = rig_on_disk("kaban-ingest-derived", "fm-source", policy());
    fs::write(
        rig.root.join("a.md"),
        "---\ntitle: Doc\ncustomer: alice-7781\nnote: fine\n---\n\n# Doc\n\nclean body\n",
    )
    .unwrap();

    let report = rig.run();

    assert_eq!(report.documents, 1, "{report:?}");
    assert_eq!(report.rule_ids.get("fm-source"), Some(&1), "{report:?}");
    let blob = document_blob(&rig, "a.md");
    assert!(!blob.contains("alice-7781"), "{blob}");
    assert!(!contains_bytes(&rig.disk_bytes(), "alice-7781"));
    let rows = receipts(&rig, &policy(), "a.md", "frontmatter", "fm-source");
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].input_hmac.len(), 64);
    let audit = serde_json::to_string(&rows).unwrap();
    assert!(!audit.contains("alice-7781"), "{audit}");
}

/// RD2-1 (the mapping): a source hit whose matched text lies inside one scalar's value (a rule
/// that needs the previous line as context, `secret_group` on the value) masks that scalar and
/// keeps the others, instead of withholding the whole block.
#[test]
fn a_source_hit_inside_one_scalar_masks_that_scalar_and_keeps_the_rest() {
    let policy = || {
        policy_with(
            "  - id: ctx-secret\n    regex: '(?s)note: fine\\nref: (\\w+)'\n    secret_group: 1\n    action: redact\n",
        )
    };
    assert!(policy().scan_kv("ref", "abc12345").hits.is_empty());
    let rig = rig_on_disk("kaban-ingest-derived", "fm-mapped", policy());
    fs::write(
        rig.root.join("a.md"),
        "---\nnote: fine\nref: abc12345\nkeep: yes\n---\n\n# Doc\n\nclean body\n",
    )
    .unwrap();

    let report = rig.run();

    assert_eq!(report.documents, 1, "{report:?}");
    let fm = &rig.document("a.md").unwrap().props["frontmatter"];
    assert_eq!(fm["ref"], "[REDACTED:ctx-secret]", "{fm}");
    assert_eq!(fm["keep"], "yes", "{fm}");
    assert_eq!(fm["note"], "fine", "{fm}");
    assert!(!contains_bytes(&rig.disk_bytes(), "abc12345"));
    assert_eq!(
        receipts(&rig, &policy(), "a.md", "frontmatter", "ctx-secret").len(),
        1
    );
}

/// RD2-1 (the boundary): the extractor interprets no frontmatter on its own. A malformed block
/// carrying a reject-class canary leaves `frontmatter` null and `partial` false after
/// extraction (the parser did not run: a parse failure would have marked the document
/// `partial`), and the document is blocked with the rule id through the real ingestor.
#[test]
fn a_reject_canary_in_malformed_frontmatter_never_reaches_the_yaml_parser() {
    let src = format!("---\ntitle: [unclosed\npassword: {CANARY}\n---\n\n# Doc\n\nbody\n");
    let extracted = MarkdownExtractor.extract(src.as_bytes(), &ctx()).unwrap();
    assert!(
        extracted.frontmatter.is_null(),
        "{:?}",
        extracted.frontmatter
    );
    assert!(
        !extracted.partial,
        "a parse failure would mark the document partial: the parser ran before the gate"
    );
    assert!(extracted.frontmatter_source.is_some());

    let rig = rig_on_disk("kaban-ingest-derived", "fm-malformed", Policy::empty());
    fs::write(rig.root.join("a.md"), &src).unwrap();
    let report = rig.run();
    assert_eq!(report.blocked, 1, "{report:?}");
    assert!(
        report
            .rule_ids
            .contains_key("generic-credential-assignment"),
        "{report:?}"
    );
    assert!(!contains_bytes(&rig.disk_bytes(), CANARY));
}

// ---- RD2-2: exact source-to-output mapping through trimming and merging ----

/// RD2-2 (the reviewer's counterexample): a paragraph of 3,800 `a`, 500 spaces and 20 `S` is
/// split by the chunker, its pieces trimmed and merged back; the redact rule ` {10}S{20}` hits
/// the stream, and the `S` run must be absent from every chunk text, from `recall` and from the
/// db/wal bytes.
#[test]
fn the_trimmed_and_merged_split_paragraph_counterexample_is_masked_through_the_real_ingestor() {
    trimmed_and_merged_counterexample_is_masked(false);
}

/// Spec 05 SS-17 (T7 review t7-5): the same counterexample over a store that encodes bodies.
/// The gate sees the plaintext and the chunk is compressed from the masked text, never from
/// its pre-redaction bytes: the stored bodies are `BLOB`s, they decode to the masked text
/// (no `S` run, the mask present), and the run is in no byte of the database or its WAL.
#[test]
fn the_trimmed_and_merged_counterexample_is_masked_before_the_body_is_encoded() {
    trimmed_and_merged_counterexample_is_masked(true);
}

fn trimmed_and_merged_counterexample_is_masked(compressed: bool) {
    let policy = policy_with("  - id: spaced-s\n    regex: ' {10}S{20}'\n    action: redact\n");
    let rig = rig_on_disk(
        "kaban-ingest-derived",
        if compressed {
            "trim-merge-encoded"
        } else {
            "trim-merge"
        },
        policy,
    );
    if compressed {
        rig.compress();
    }
    let run = "S".repeat(20);
    let body = format!("{}{}{run}", "a".repeat(3_800), " ".repeat(500));
    fs::write(rig.root.join("a.md"), format!("# Doc\n\n{body}\n")).unwrap();

    let report = rig.run();

    assert_eq!(report.documents, 1, "{report:?}");
    assert_eq!(report.rule_ids.get("spaced-s"), Some(&1), "{report:?}");
    let blob = document_blob(&rig, "a.md");
    assert!(!blob.contains("SSSSS"), "an S run survived: {blob}");
    assert!(blob.contains("[REDACTED:spaced-s]"), "{blob}");
    assert!(!contains_bytes(&rig.disk_bytes(), &run));
    if compressed {
        assert!(
            rig.encoded_bodies() > 0,
            "the masked chunk bodies are stored encoded"
        );
    } else {
        // a store past the retirement serves recall through an attached index only (SS-16)
        assert!(recall_is_empty(&rig, &run));
    }
}

// ---- RD2-3: links and tags carry their source spans ----

/// RD2-3: for `private: #alice-7781` under the contextual redact rule `private:\s+#alice-7781`
/// the chunk is masked and the tag (which alone matches nothing) is removed with it: no Tag
/// node, no `TAGGED` edge, no `tag_id` row, nothing in db/wal or `recall`, one keyed `tag`
/// receipt; the sibling `#public` tag stays. A wikilink under the same context is removed
/// too: no `LINKS_TO` edge, no placeholder, no `unresolved_links` entry, one `link` receipt.
#[test]
fn a_contextual_redact_hit_removes_the_tag_and_link_it_covers_everywhere() {
    let policy = || {
        policy_with(
            "  - id: private-tag\n    regex: 'private:\\s+#alice-7781'\n    action: redact\n  - id: private-link\n    regex: 'private:\\s+\\[\\[Alice-7781\\]\\]'\n    action: redact\n",
        )
    };
    assert!(policy().scan("alice-7781").hits.is_empty());
    assert!(policy().scan("Alice-7781").hits.is_empty());
    let rig = rig_on_disk("kaban-ingest-derived", "tag-link", policy());
    fs::write(
        rig.root.join("a.md"),
        "# Doc\n\nprivate: #alice-7781 and #public here.\n\nAlso private: [[Alice-7781]] and [[Public]].\n",
    )
    .unwrap();

    let report = rig.run();

    assert_eq!(report.documents, 1, "{report:?}");
    assert_eq!(report.rule_ids.get("private-tag"), Some(&1), "{report:?}");
    assert_eq!(report.rule_ids.get("private-link"), Some(&1), "{report:?}");
    let ws = WorkspaceId::default();
    assert!(
        rig.store
            .get(&tag_id(&ws, "alice-7781"), true)
            .unwrap()
            .is_none(),
        "the covered tag has a Tag node"
    );
    assert!(
        rig.store
            .get(&tag_id(&ws, "public"), true)
            .unwrap()
            .is_some(),
        "the sibling tag is kept"
    );
    let (nodes, edges) = rig.hop(&doc_id("a.md"), &["TAGGED", "LINKS_TO"], 1);
    let joined = serde_json::to_string(&(&nodes, &edges)).unwrap();
    assert!(!joined.contains("alice-7781"), "{joined}");
    assert!(!joined.contains("Alice-7781"), "{joined}");
    assert!(
        joined.contains("Public"),
        "the clean wikilink keeps its edge: {joined}"
    );
    let blob = document_blob(&rig, "a.md");
    assert!(
        !blob.contains("alice-7781") && !blob.contains("Alice-7781"),
        "{blob}"
    );
    assert!(!contains_bytes(&rig.disk_bytes(), "alice-7781"));
    assert!(!contains_bytes(&rig.disk_bytes(), "Alice-7781"));
    assert!(recall_is_empty(&rig, "alice-7781"));
    assert_eq!(
        receipts(&rig, &policy(), "a.md", "tag", "private-tag").len(),
        1
    );
    assert_eq!(
        receipts(&rig, &policy(), "a.md", "link", "private-link").len(),
        1
    );
}
