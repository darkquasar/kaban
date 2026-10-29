//! Denylist gating before identity (docs-ingest-plan.md DI1.4; docs-ingest-spec D6; core-spec
//! S1; PR-2): the canary `Sup3rS3cretValue!` never reaches a hash, a node id, the database, the
//! WAL, the audit chain or the logs; redactions mask, rejections stub, `skip_file` rules block
//! the whole document, and all of it happens before `content_hash` or any id is computed.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

use kaban_policy::{OnDocument, Policy};

/// A builtin reject-class credential assignment; never a real secret.
const CANARY: &str = "Sup3rS3cretValue!";

/// `Rule.on_document` (D6): a rule may say `on_document: skip_file`; policies written before
/// the field exist load unchanged (`serde-default-compat`); the builtin `private-key-block` and
/// `generic-credential-assignment` rules carry the default.
#[test]
fn on_document_skip_file_deserializes_and_old_policies_still_load() {
    let yaml = b"rules:\n  - id: internal-dump\n    regex: 'DUMP-[0-9]{6}'\n    on_document: skip_file\n  - id: plain\n    regex: 'PLAIN-[0-9]{6}'\n";
    let file = Policy::parse(yaml).unwrap();
    assert_eq!(file.rules[0].on_document, Some(OnDocument::SkipFile));
    assert_eq!(
        file.rules[1].on_document, None,
        "absent means per-chunk handling"
    );

    let old = include_bytes!("../../../examples/policy.yaml");
    let policy = Policy::from_file(Policy::parse(old).unwrap(), "example".into()).unwrap();
    let on_document = |id: &str| {
        policy
            .file
            .rules
            .iter()
            .find(|r| r.id == id)
            .unwrap_or_else(|| panic!("rule {id} present"))
            .on_document
    };
    assert_eq!(on_document("private-key-block"), Some(OnDocument::SkipFile));
    assert_eq!(
        on_document("generic-credential-assignment"),
        Some(OnDocument::SkipFile)
    );
    assert_eq!(on_document("aws-access-key-id"), None);
    assert_eq!(on_document("internal-hostname"), None);
    let _ = CANARY;
}

/// RD1-4: a redact rule that matches only the `key=value` view (`^customer=(.+)$` with secret
/// group 1) never fires on the bare value, so its hit must be projected from the view onto the
/// value, unioned with direct-value hits, masked and counted.
#[test]
fn a_redact_hit_visible_only_in_the_key_value_view_masks_the_value_and_is_counted() {
    use kaban_ingest::{MetaDecision, gate_metadata};
    let yaml = b"rules:\n  - id: customer-view\n    regex: '^customer=(.+)$'\n    secret_group: 1\n    action: redact\n";
    let policy = Policy::from_file(Policy::parse(yaml).unwrap(), "test".into()).unwrap();
    assert!(
        policy.scan("alice-7781").hits.is_empty(),
        "the bare value is clean"
    );
    let fm = serde_json::json!({ "customer": "alice-7781", "note": "fine" });
    match gate_metadata(
        &policy,
        signer().as_ref(),
        &WorkspaceId::default(),
        Some("Doc"),
        &fm,
        None,
    ) {
        MetaDecision::Proceed(meta) => {
            assert_eq!(meta.frontmatter["customer"], "[REDACTED:customer-view]");
            assert_eq!(meta.frontmatter["note"], "fine");
            assert_eq!(meta.redactions, vec!["customer-view".to_owned()]);
        }
        other @ MetaDecision::SkipFile { .. } => panic!("{other:?}"),
    }
}

fn redact_policy() -> Policy {
    // the canary is matched by the builtin reject-class assignment rules; this user policy adds
    // a redact-class rule for a hostname shape and a redact-class literal, so both paths exist
    let yaml = b"rules:\n  - id: internal-hostname\n    regex: '\\b[a-z0-9-]+\\.corp\\.acme\\.internal\\b'\n    keywords: ['.corp.acme.internal']\n    action: redact\nwords:\n  - { term: hunter2, action: redact, id: old-password }\n";
    Policy::from_file(Policy::parse(yaml).unwrap(), "test".into()).unwrap()
}

/// Plan stage (1), pre-hash metadata canaries: a title with a redact-class hit comes back masked
/// (and that is the only title the committer may see); a frontmatter string leaf with a
/// reject-class hit (`token: ghp_...`) or a credential-named key skips the file before any
/// hash; clean metadata passes unchanged and the redactions are listed by rule id only.
#[test]
fn gate_metadata_masks_redact_hits_and_skips_the_file_on_reject_hits() {
    use kaban_ingest::{MetaDecision, gate_metadata};
    let policy = redact_policy();
    let fm = serde_json::json!({ "author": "someone", "host": "db1.corp.acme.internal", "n": 3 });
    let signer = signer();
    let ws = WorkspaceId::default();
    match gate_metadata(
        &policy,
        signer.as_ref(),
        &ws,
        Some("Notes on hunter2 and db1.corp.acme.internal"),
        &fm,
        None,
    ) {
        MetaDecision::Proceed(meta) => {
            assert_eq!(
                meta.title.as_deref(),
                Some("Notes on [REDACTED:old-password] and [REDACTED:internal-hostname]")
            );
            assert_eq!(meta.frontmatter["host"], "[REDACTED:internal-hostname]");
            assert_eq!(meta.frontmatter["author"], "someone");
            assert_eq!(meta.frontmatter["n"], 3);
            let mut redactions = meta.redactions.clone();
            redactions.sort();
            redactions.dedup();
            assert_eq!(redactions, vec!["internal-hostname", "old-password"]);
        }
        other @ MetaDecision::SkipFile { .. } => panic!("{other:?}"),
    }

    let fm = serde_json::json!({ "token": "ghp_AbCdEfGhIjKlMnOpQrStUvWxYz0123456789" });
    match gate_metadata(&policy, signer.as_ref(), &ws, Some("clean"), &fm, None) {
        MetaDecision::SkipFile { rule_ids } => {
            assert!(rule_ids.iter().any(|r| r == "github-token"), "{rule_ids:?}");
            let joined = rule_ids.join(" ");
            assert!(!joined.contains("ghp_"), "rule ids never carry the value");
        }
        other @ MetaDecision::Proceed(_) => panic!("{other:?}"),
    }

    let fm = serde_json::json!({ "db_password": CANARY });
    assert!(matches!(
        gate_metadata(&policy, signer.as_ref(), &ws, None, &fm, None),
        MetaDecision::SkipFile { .. }
    ));
    match gate_metadata(
        &policy,
        signer.as_ref(),
        &ws,
        Some("clean title"),
        &serde_json::json!({"a": ["x", "y"]}),
        None,
    ) {
        MetaDecision::Proceed(meta) => {
            assert_eq!(meta.title.as_deref(), Some("clean title"));
            assert_eq!(meta.redactions, Vec::<String>::new());
        }
        other @ MetaDecision::SkipFile { .. } => panic!("{other:?}"),
    }
}

use kaban_core::WorkspaceId;
use kaban_ingest::{
    ChunkConfig, ChunkDraft, ChunkKind, ExtractCtx, GateDecision, SanitizedMeta, SourceId,
    gate_document, gate_path, stream_text,
};
use kaban_policy::AuditSigner;
use kaban_store_sqlite::SqliteStore;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

fn ctx() -> ExtractCtx {
    let gated = gate_path(&Policy::empty(), "docs/fixture.md").unwrap();
    ExtractCtx::new(
        CancellationToken::new(),
        tokio::time::Instant::now() + Duration::from_secs(10),
        SourceId::new(&WorkspaceId::default(), &gated),
    )
}

/// The audit signer of a fresh in-memory store (the real `HMAC(audit_key, ..)`).
fn signer() -> Arc<dyn AuditSigner> {
    SqliteStore::open_memory().unwrap().signer()
}

fn draft(text: &str, prefix: &str) -> ChunkDraft {
    ChunkDraft {
        text: text.to_owned(),
        kind: ChunkKind::Text,
        byte_start: 0,
        byte_end: u64::try_from(text.len()).unwrap(),
        page: None,
        context_prefix: prefix.to_owned(),
        oversize: false,
        table_unknown: false,
    }
}

fn clean_meta() -> SanitizedMeta {
    SanitizedMeta {
        title: Some("Doc".into()),
        frontmatter: serde_json::Value::Null,
        redactions: Vec::new(),
        audit: Vec::new(),
    }
}

/// Plan stage (2), the first canary slice: `password = Sup3r` ends chunk 1 and `S3cretValue!`
/// starts chunk 2, so neither chunk alone matches; the overlapping-window scan of the whole
/// stream catches the assignment, whose rule carries `on_document: skip_file`, and the document
/// is skipped with the rule id and one `reject` audit row that carries only a keyed hash.
#[test]
fn a_secret_split_across_two_chunks_is_caught_by_the_window_scan_and_skips_the_file() {
    let policy = redact_policy();
    let (head, tail) = CANARY.split_at(5);
    let chunks = vec![
        draft(&format!("Config notes.\n\npassword = {head}"), "Doc"),
        draft(&format!("{tail}\n\nmore notes after."), "Doc"),
    ];
    for c in &chunks {
        assert!(
            !policy.scan(&c.text).rejected,
            "neither half rejects alone: {:?}",
            c.text
        );
    }
    let full = stream_text(&chunks);
    let decision = gate_document(
        &policy,
        signer().as_ref(),
        &ctx(),
        &WorkspaceId::default(),
        clean_meta(),
        &full,
        chunks,
        None,
        &ChunkConfig::default(),
    )
    .unwrap();
    match decision {
        GateDecision::SkipFile { rule_ids, audit } => {
            assert!(
                rule_ids
                    .iter()
                    .any(|r| r == "generic-credential-assignment"),
                "{rule_ids:?}"
            );
            assert_eq!(audit.len(), 1);
            let row = serde_json::to_string(&audit[0]).unwrap();
            assert!(!row.contains("Sup3r") && !row.contains("S3cret"), "{row}");
            assert_eq!(audit[0].input_hmac.len(), 64, "keyed hash, not text");
        }
        GateDecision::Proceed(g) => panic!("must skip: {g:?}"),
    }
}

fn proceed(
    policy: &Policy,
    signer: &dyn AuditSigner,
    chunks: Vec<ChunkDraft>,
    error: Option<&str>,
) -> kaban_ingest::Gated {
    let full = stream_text(&chunks);
    match gate_document(
        policy,
        signer,
        &ctx(),
        &WorkspaceId::default(),
        clean_meta(),
        &full,
        chunks,
        error,
        &ChunkConfig::default(),
    )
    .unwrap()
    {
        GateDecision::Proceed(g) => g,
        GateDecision::SkipFile { rule_ids, .. } => panic!("unexpected skip: {rule_ids:?}"),
    }
}

/// Plan stage (3): a redact-class hostname in chunk 2 is masked in place and its siblings are
/// untouched; a reject-class hit without `skip_file` (an AWS key id) in chunk 3 turns that chunk
/// into a `Stub` with empty text and `stub_hmac = HMAC(audit_key, original)` while chunks 1, 2
/// and 4 keep their positions (the `NEXT` chain stays whole); a redact hit in a `context_prefix`
/// masks the prefix; counts and audit rows name rules and keyed hashes only.
#[test]
fn redact_hits_mask_in_place_and_reject_hits_stub_the_chunk_keeping_the_chain() {
    let policy = redact_policy();
    let aws = "AKIAQWERTYUIOPASDFGH";
    let chunks = vec![
        draft("first paragraph, clean", "Doc > A"),
        draft("talk to db1.corp.acme.internal tonight", "Doc > A"),
        draft(&format!("the key {aws} leaked"), "Doc > B"),
        draft("fourth paragraph, clean", "Doc > hunter2 section"),
    ];
    let signer = signer();
    let g = proceed(&policy, signer.as_ref(), chunks, None);
    assert_eq!(g.chunks.len(), 4, "every chunk keeps its slot");
    assert_eq!(g.chunks[0].draft.text, "first paragraph, clean");
    assert_eq!(
        g.chunks[1].draft.text,
        "talk to [REDACTED:internal-hostname] tonight"
    );
    assert_eq!(g.chunks[1].draft.kind, ChunkKind::Text);
    assert!(g.chunks[1].stub_hmac.is_none());
    let stub = &g.chunks[2];
    assert_eq!(stub.draft.kind, ChunkKind::Stub);
    assert_eq!(stub.draft.text, "");
    let expected_hmac = signer.hmac_hex(format!("the key {aws} leaked").as_bytes());
    assert_eq!(stub.stub_hmac.as_deref(), Some(expected_hmac.as_str()));
    assert_eq!(g.chunks[3].draft.text, "fourth paragraph, clean");
    assert_eq!(
        g.chunks[3].draft.context_prefix,
        "Doc > [REDACTED:old-password] section"
    );
    assert_eq!(g.blocked_chunks, 1);
    assert_eq!(g.rule_ids.get("internal-hostname"), Some(&1));
    assert_eq!(g.rule_ids.get("aws-access-key-id"), Some(&1));
    assert_eq!(g.rule_ids.get("old-password"), Some(&1));
    let audit = serde_json::to_string(&g.audit).unwrap();
    assert!(
        !audit.contains(aws) && !audit.contains("db1.corp") && !audit.contains("hunter2"),
        "{audit}"
    );
    assert_eq!(g.audit.len(), 3, "redact, reject, redact: {audit}");
}

/// RD1-6: one oversized code chunk of 20,000 matched characters is scanned in overlapping
/// 8,192-character windows; the hits `[0,8192)`, `[4096,12288)`, `[8192,16384)` and
/// `[12288,20000)` must be unioned before masking, so not one matched character survives (the
/// old skip of "a hit starting before the mask position" left 3,616 of them). Same for a
/// multibyte alphabet, where window offsets are byte offsets of three-byte scalars.
#[test]
fn overlapping_window_redactions_are_unioned_so_no_matched_character_survives() {
    let yaml = b"rules:\n  - id: all-s\n    regex: 'S+'\n    action: redact\n  - id: all-e\n    regex: '\xc3\xa9+'\n    action: redact\n";
    let policy = Policy::from_file(Policy::parse(yaml).unwrap(), "test".into()).unwrap();
    let ascii = "S".repeat(20_000);
    let wide = "\u{e9}".repeat(20_000);
    let mut chunks = vec![draft(&ascii, "Doc"), draft(&wide, "Doc")];
    for c in &mut chunks {
        c.kind = ChunkKind::Code { lang: None };
        c.oversize = true;
    }
    let signer = signer();
    let g = proceed(&policy, signer.as_ref(), chunks, None);
    assert_eq!(g.chunks.len(), 2);
    for (chunk, needle) in g.chunks.iter().zip(['S', '\u{e9}']) {
        assert_eq!(
            chunk.draft.kind,
            ChunkKind::Code { lang: None },
            "masked, not stubbed"
        );
        let unmasked = chunk.draft.text.chars().filter(|c| *c == needle).count();
        assert_eq!(
            unmasked,
            0,
            "unmasked matched characters: {}",
            chunk.draft.text.len()
        );
        assert!(
            chunk.draft.text.contains("[REDACTED:all-"),
            "{}",
            chunk.draft.text
        );
    }
    assert_eq!(g.audit.len(), 2, "one redact row per chunk");
}

/// Extractor error strings are content (S1): one with a redact hit is stored masked, one with a
/// reject hit is replaced by a fixed text and counted under its rule id.
#[test]
fn extractor_error_strings_are_gated_before_they_are_stored() {
    let policy = redact_policy();
    let signer = signer();
    let g = proceed(
        &policy,
        signer.as_ref(),
        vec![draft("clean", "Doc")],
        Some("cannot parse frontmatter near db1.corp.acme.internal"),
    );
    assert_eq!(
        g.error.as_deref(),
        Some("cannot parse frontmatter near [REDACTED:internal-hostname]")
    );
    let g = proceed(
        &policy,
        signer.as_ref(),
        vec![draft("clean", "Doc")],
        Some(&format!("bad input AKIAQWERTYUIOPASDFGH and {CANARY}")),
    );
    assert_eq!(
        g.error.as_deref(),
        Some("extractor error withheld by the denylist")
    );
    assert!(g.rule_ids.contains_key("aws-access-key-id"));
}

use kaban_ingest::{DocStatus, Format, PrepareRequest, PreparedDocument, prepare_document};

fn prepare(
    policy: &Policy,
    signer: &dyn AuditSigner,
    path: &str,
    bytes: &[u8],
) -> PreparedDocument {
    let ws = WorkspaceId::default();
    let gated = gate_path(policy, path).unwrap();
    let source = SourceId::new(&ws, &gated);
    let format = if std::path::Path::new(path)
        .extension()
        .is_some_and(|e| e == "md")
    {
        Format::Markdown
    } else {
        Format::Text
    };
    prepare_document(
        policy,
        signer,
        &ctx(),
        &ws,
        &PrepareRequest {
            source: &source,
            path: &gated,
            format,
            size: u64::try_from(bytes.len()).unwrap(),
            mtime: 1_793_004_407,
            bytes,
            config_hash: "cfg",
        },
        &ChunkConfig::default(),
    )
    .unwrap()
}

/// RD1-3: YAML typing must not bypass the metadata gate. `password: 12345678` becomes a number
/// after parsing and `secret: true` a boolean; the frontmatter source text is gated before
/// interpretation and every scalar is judged in its `key=value` view afterwards, so the numeric
/// credential blocks the document, a malformed block (no YAML value at all) carrying the canary
/// blocks it too, and a boolean matched only by a `key=value` rule is masked.
#[test]
fn numeric_boolean_and_malformed_frontmatter_credentials_cannot_bypass_the_gate() {
    let yaml = b"rules:\n  - id: flag-view\n    regex: '^flag=true$'\n    action: redact\n";
    let policy = Policy::from_file(Policy::parse(yaml).unwrap(), "test".into()).unwrap();
    let signer = signer();
    let numeric = "---\ntitle: Doc\npassword: 12345678\n---\n\n# Doc\n\nbody text\n";
    match prepare(&policy, signer.as_ref(), "n.md", numeric.as_bytes()) {
        PreparedDocument::Blocked { props, .. } => {
            assert_eq!(props.status, DocStatus::Blocked);
            assert!(
                props
                    .frontmatter
                    .as_object()
                    .is_some_and(serde_json::Map::is_empty)
            );
            assert!(!props.rule_ids.is_empty(), "{:?}", props.rule_ids);
        }
        other => panic!("a numeric credential must block: {other:?}"),
    }
    let malformed = format!("---\npassword: {CANARY}\nbroken: [\n---\n\n# Doc\n\nbody\n");
    match prepare(&policy, signer.as_ref(), "m.md", malformed.as_bytes()) {
        PreparedDocument::Blocked { props, .. } => {
            let blob = serde_json::to_string(&props).unwrap();
            assert!(!blob.contains(CANARY), "{blob}");
        }
        other => panic!("a malformed block with a credential must block: {other:?}"),
    }
    let boolean = "---\nflag: true\nother: false\n---\n\n# Doc\n\nbody\n";
    match prepare(&policy, signer.as_ref(), "b.md", boolean.as_bytes()) {
        PreparedDocument::Ready { graph, .. } => {
            let fm = &graph.document.props.frontmatter;
            assert_eq!(fm["flag"], "[REDACTED:flag-view]", "{fm}");
            assert_eq!(fm["other"], false);
            assert_eq!(graph.document.props.rule_ids.get("flag-view"), Some(&1));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn clean_frontmatter_preserves_yaml_types_and_malformed_blocks_are_partial() {
    let policy = Policy::empty();
    let signer = signer();
    let clean = b"---\ntitle: YAML title\nstatus: on\nready: true\ncount: 3\nratio: 0.5\nlabels: [one, two]\nnote: |\n  first\n  second\n---\n\n# Heading\n\nordinary prose\n";
    match prepare(&policy, signer.as_ref(), "clean.md", clean) {
        PreparedDocument::Ready { graph, .. } => {
            let props = &graph.document.props;
            assert_eq!(props.status, DocStatus::Ok);
            assert_eq!(props.title.as_deref(), Some("YAML title"));
            assert_eq!(
                props.frontmatter,
                serde_json::json!({
                    "title": "YAML title", "status": "on", "ready": true,
                    "count": 3, "ratio": 0.5, "labels": ["one", "two"],
                    "note": "first\nsecond\n"
                })
            );
        }
        other => panic!("clean YAML should prepare: {other:?}"),
    }
    for bad in [
        "title: [unfinished",
        "title: *undefined",
        "? [one, two]\n: value",
    ] {
        let document = format!("---\n{bad}\n---\n\n# Heading\n\nordinary prose\n");
        match prepare(&policy, signer.as_ref(), "bad.md", document.as_bytes()) {
            PreparedDocument::Ready { graph, .. } => {
                let props = &graph.document.props;
                assert_eq!(props.status, DocStatus::Partial);
                assert!(
                    props.frontmatter.is_null()
                        || props.frontmatter.as_object().unwrap().is_empty()
                );
                assert_eq!(props.title.as_deref(), Some("Heading"));
            }
            other => panic!("malformed YAML should leave a partial document: {other:?}"),
        }
    }
}

/// RD1-2: the fence-info token is a persisted string (`ChunkKind::Code { lang }`, S1) and is
/// gated like every other one: a reject-class token (an AWS key id) is dropped from the
/// prepared graph, counted and audited under `lang`; a redact-class token is masked; the body
/// of the fence is untouched in both cases.
#[test]
fn the_fence_info_language_is_gated_before_it_reaches_the_prepared_graph() {
    let policy = redact_policy();
    let signer = signer();
    let aws = "AKIAQWERTYUIOPASDFGH";
    let src = format!(
        "# Fence\n\n```{aws}\nlet x = 1;\n```\n\n```db1.corp.acme.internal\nlet y = 2;\n```\n"
    );
    match prepare(&policy, signer.as_ref(), "fence.md", src.as_bytes()) {
        PreparedDocument::Ready { graph, audit } => {
            let chunks: Vec<_> = graph.sections.iter().flat_map(|s| &s.chunks).collect();
            let props: Vec<_> = chunks.iter().map(|c| &c.props).collect();
            let blob = serde_json::to_string(&(&graph.document.props, &props, &audit)).unwrap();
            assert!(!blob.contains(aws) && !blob.contains("db1.corp"), "{blob}");
            assert_eq!(chunks.len(), 2);
            assert_eq!(chunks[0].props.kind, ChunkKind::Code { lang: None });
            assert_eq!(chunks[0].props.text, "let x = 1;");
            assert_eq!(
                chunks[1].props.kind,
                ChunkKind::Code {
                    lang: Some("[REDACTED:internal-hostname]".to_owned())
                }
            );
            assert_eq!(
                graph.document.props.rule_ids.get("aws-access-key-id"),
                Some(&1)
            );
            assert_eq!(
                graph.document.props.rule_ids.get("internal-hostname"),
                Some(&1)
            );
            assert!(
                audit
                    .iter()
                    .any(|a| a.field == "lang" && a.rules.iter().any(|r| r == "aws-access-key-id")),
                "{audit:?}"
            );
        }
        other => panic!("{other:?}"),
    }
}

/// RD1-10: every sanitisation stage leaves a keyed audit receipt with the document write. A
/// frontmatter-only redaction yields exactly one `redact` row under `frontmatter`; a redaction
/// visible only in a standard link's destination yields one row under `link`; a redact-only
/// extractor error yields one row under `error`. Every row carries the rule id and a 64-hex
/// keyed hash, never the text.
#[test]
fn metadata_only_link_target_only_and_error_only_redactions_leave_keyed_audit_receipts() {
    use kaban_core::AuditOp;
    let policy = redact_policy();
    let signer = signer();
    let host = "db1.corp.acme.internal";
    let check = |audit: &[kaban_core::AuditEntry], field: &str| {
        assert_eq!(audit.len(), 1, "{field}: {audit:?}");
        let row = &audit[0];
        assert_eq!(row.op, AuditOp::Redact);
        assert_eq!(row.field, field);
        assert_eq!(row.rules, vec!["internal-hostname".to_owned()]);
        assert_eq!(row.input_hmac.len(), 64);
        assert_eq!(row.tool, "ingest");
        assert!(!serde_json::to_string(row).unwrap().contains(host));
    };
    let meta_only = format!("---\nhost: {host}\n---\n\n# Doc\n\nclean body\n");
    match prepare(&policy, signer.as_ref(), "meta.md", meta_only.as_bytes()) {
        PreparedDocument::Ready { graph, audit } => {
            assert_eq!(
                graph.document.props.frontmatter["host"],
                "[REDACTED:internal-hostname]"
            );
            check(&audit, "frontmatter");
        }
        other => panic!("{other:?}"),
    }
    let link_only = format!("# Doc\n\nsee [the db](https://{host}/status) tonight\n");
    match prepare(&policy, signer.as_ref(), "link.md", link_only.as_bytes()) {
        PreparedDocument::Ready { graph, audit } => {
            assert_eq!(graph.links.len(), 1);
            assert_eq!(
                graph.links[0].raw_target,
                "https://[REDACTED:internal-hostname]/status"
            );
            check(&audit, "link");
        }
        other => panic!("{other:?}"),
    }
    let g = proceed(
        &policy,
        signer.as_ref(),
        vec![draft("clean", "Doc")],
        Some(&format!("cannot parse frontmatter near {host}")),
    );
    check(&g.audit, "error");
}

/// RD1-5: the whitespace-dependent case. Paragraph 1 ends in `Bearer`, paragraph 2 starts with
/// the 24-character token; they become two chunks. The builtin `bearer\s+<token>` rule matches
/// the document as written (a blank line separates the paragraphs) but neither chunk alone, and
/// not the separator-less concatenation `BearerQQQ…` either. The stream gate must run over the
/// pre-chunk extracted stream with its paragraph boundary, so the token never reaches a chunk.
#[test]
fn a_bearer_token_split_from_its_keyword_by_a_paragraph_boundary_is_caught() {
    let policy = redact_policy();
    let signer = signer();
    let token = "Q".repeat(24);
    let p1 = format!("{}Bearer", "padding ".repeat(256));
    let p2 = format!("{token}{}", " prose".repeat(50));
    assert!(policy.scan(&p1).hits.is_empty() && policy.scan(&p2).hits.is_empty());
    assert!(
        policy.scan(&format!("{p1}{p2}")).hits.is_empty(),
        "no separator, no hit"
    );
    assert!(
        !policy.scan(&format!("{p1}\n\n{p2}")).hits.is_empty(),
        "as written, a hit"
    );
    let src = format!("# Doc\n\n{p1}\n\n{p2}\n");
    match prepare(&policy, signer.as_ref(), "bearer.md", src.as_bytes()) {
        PreparedDocument::Ready { graph, .. } => {
            assert!(
                graph.document.props.rule_ids.contains_key("bearer-token"),
                "{:?}",
                graph.document.props.rule_ids
            );
            for c in graph.sections.iter().flat_map(|s| &s.chunks) {
                assert!(
                    !c.props.text.contains(&token),
                    "token stored: {}",
                    c.props.text
                );
            }
        }
        PreparedDocument::Blocked { props, .. } => {
            assert!(
                props.rule_ids.contains_key("bearer-token"),
                "{:?}",
                props.rule_ids
            );
        }
        other @ PreparedDocument::Failed { .. } => panic!("{other:?}"),
    }
}

/// RD1-1: the stored `content_hash` is the digest of the canonical sanitized representation,
/// never of the raw bytes: it differs from `blake3(bytes)` whenever something was masked, and
/// two documents that differ only inside a masked span share it (nothing about the secret is
/// in the preimage). A stubbed chunk contributes its keyed `stub_hmac`, so an edit inside a
/// stubbed secret still changes the hash and re-ingests.
#[test]
fn content_hash_is_computed_over_the_sanitized_representation_not_the_raw_bytes() {
    let policy = redact_policy();
    let signer = signer();
    let a = "# Doc\n\nconnect to db1.corp.acme.internal tonight\n";
    let b = "# Doc\n\nconnect to db2.corp.acme.internal tonight\n";
    let hash_of = |src: &str| match prepare(&policy, signer.as_ref(), "d.md", src.as_bytes()) {
        PreparedDocument::Ready { graph, .. } => {
            let stored = graph.document.props.content_hash.clone().unwrap();
            assert_eq!(stored.len(), 64);
            (stored, graph)
        }
        other => panic!("{other:?}"),
    };
    let (ha, graph_a) = hash_of(a);
    let (hb, _) = hash_of(b);
    let raw_a = blake3::hash(a.as_bytes()).to_hex().to_string();
    assert_ne!(ha, raw_a, "the raw digest must not be stored");
    assert_eq!(
        ha, hb,
        "the two documents sanitize to the same representation"
    );
    let chunk = &graph_a.sections[0].chunks[0];
    assert_eq!(
        chunk.props.text,
        "connect to [REDACTED:internal-hostname] tonight"
    );
    assert_eq!(ha, hash_of(a).0, "deterministic");
    let aws = "AKIAQWERTYUIOPASDFGH";
    let (hs1, g1) = hash_of(&format!("# Doc\n\nkey {aws} here\n"));
    let (hs2, _) = hash_of(&format!("# Doc\n\nkey {} here\n", aws.replace('Q', "Z")));
    assert_eq!(g1.sections[0].chunks[0].props.kind, ChunkKind::Stub);
    assert_ne!(hs1, hs2, "stub hmacs differ, so the hashes differ");
}

/// Plan slices: a `.env`-style assignment (`DB_PASSWORD=<canary>`) in a text file and a PEM
/// block inside a Markdown file are `blocked` documents with zero chunks, no `content_hash`
/// and a `blocked_hmac` (the keyed identity of the bytes); the canary and the PEM header appear
/// in no prop and no audit row.
#[test]
fn env_assignments_and_pem_blocks_block_the_whole_document_before_any_hash() {
    let policy = redact_policy();
    let signer = signer();
    let env = format!("# settings\nDB_HOST=localhost\nDB_PASSWORD={CANARY}\n");
    match prepare(
        &policy,
        signer.as_ref(),
        "config/settings.txt",
        env.as_bytes(),
    ) {
        PreparedDocument::Blocked { props, audit } => {
            assert_eq!(props.status, DocStatus::Blocked);
            assert!(props.content_hash.is_none(), "{props:?}");
            assert_eq!(
                props.blocked_hmac.as_deref(),
                Some(kaban_ingest::keyed_id(signer.as_ref(), "bytes", env.as_bytes()).as_str())
            );
            assert!(props.title.is_none());
            assert_eq!(props.frontmatter, serde_json::json!({}));
            assert!(
                props.rule_ids.contains_key("generic-credential-assignment"),
                "{:?}",
                props.rule_ids
            );
            let bytes = serde_json::to_string(&(&props, &audit)).unwrap();
            assert!(!bytes.contains(CANARY), "{bytes}");
            assert_ne!(audit.len(), 0);
        }
        other => panic!("{other:?}"),
    }
    let pem = "# Keys\n\nSome prose.\n\n```\n-----BEGIN RSA PRIVATE KEY-----\nMIIEpAIBAAKCAQEA\n-----END RSA PRIVATE KEY-----\n```\n";
    match prepare(&policy, signer.as_ref(), "docs/keys.md", pem.as_bytes()) {
        PreparedDocument::Blocked { props, audit } => {
            assert!(props.rule_ids.contains_key("private-key-block"));
            assert!(props.content_hash.is_none());
            let bytes = serde_json::to_string(&(&props, &audit)).unwrap();
            assert!(!bytes.contains("BEGIN RSA") && !bytes.contains("MIIEpAIBAAKCAQEA"));
        }
        other => panic!("{other:?}"),
    }
}

/// Pre-hash metadata canaries and masked-text identity: a title with a redact-class hit is
/// stored masked and the stored ids are exactly the ids recomputed from the stored
/// (sanitized) props, so no id preimage ever saw the hit; a redactable hostname in one chunk
/// is masked, its siblings are intact, its `chunk_id` is the id of the masked text and a
/// second run produces the identical graph (re-run is a no-op); a frontmatter `token: ghp_...`
/// blocks the document before any `content_hash`.
#[test]
fn masked_titles_and_chunks_get_ids_from_the_masked_text_and_rerun_is_identical() {
    use kaban_ingest::{PIPELINE_VERSION, chunk_id, section_id};
    let policy = redact_policy();
    let signer = signer();
    let src = "# Runbook for db1.corp.acme.internal\n\nFirst paragraph is clean.\n\nConnect to db1.corp.acme.internal on port 5432.\n\nLast paragraph is clean too.\n";
    let first = prepare(&policy, signer.as_ref(), "ops/runbook.md", src.as_bytes());
    let second = prepare(&policy, signer.as_ref(), "ops/runbook.md", src.as_bytes());
    let PreparedDocument::Ready { graph, audit } = first else {
        panic!("{first:?}")
    };
    let PreparedDocument::Ready { graph: again, .. } = second else {
        panic!("{second:?}")
    };
    assert_eq!(*graph, *again, "re-run yields the identical graph");

    let doc = &graph.document;
    assert_eq!(
        doc.props.title.as_deref(),
        Some("Runbook for [REDACTED:internal-hostname]")
    );
    assert!(doc.props.content_hash.is_some());
    assert_eq!(doc.props.status, DocStatus::Ok);
    let source = SourceId::new(
        &WorkspaceId::default(),
        &gate_path(&policy, "ops/runbook.md").unwrap(),
    );
    assert_eq!(graph.sections.len(), 1);
    let section = &graph.sections[0];
    assert_eq!(
        section.props.heading_path,
        vec!["Runbook for [REDACTED:internal-hostname]".to_owned()],
        "the heading path is the masked heading"
    );
    assert_eq!(
        section.id,
        section_id(&source, &section.props.heading_path, 0, PIPELINE_VERSION)
    );
    // one text chunk (the three paragraphs accumulate), masked in the middle
    assert_eq!(section.chunks.len(), 1);
    let c = &section.chunks[0];
    assert!(
        c.props
            .text
            .contains("Connect to [REDACTED:internal-hostname] on port 5432.")
    );
    assert!(c.props.text.starts_with("First paragraph is clean."));
    assert!(c.props.text.ends_with("Last paragraph is clean too."));
    assert_eq!(
        c.props.text_hash,
        blake3::hash(c.props.text.as_bytes()).to_hex().to_string(),
        "text_hash is the blake3 of the masked text"
    );
    assert_eq!(
        c.id,
        chunk_id(
            &source,
            &c.section_path,
            &c.props.text_hash,
            0,
            PIPELINE_VERSION
        ),
        "the chunk id is recomputable from the stored props alone"
    );
    assert_eq!(
        c.props.context_prefix,
        "Runbook for [REDACTED:internal-hostname]"
    );
    assert_eq!(c.props.bytes, u32::try_from(c.props.text.len()).unwrap());
    assert_eq!(c.props.slices, 1);
    let all = serde_json::to_string(&(&doc.props, &section.props, &c.props, &audit)).unwrap();
    assert!(!all.contains("db1.corp"), "{all}");
    assert_eq!(
        doc.props.rule_ids.get("internal-hostname").copied(),
        Some(3),
        "title, heading prefix and chunk"
    );

    let fm = "---\ntitle: Tokens\ntoken: ghp_AbCdEfGhIjKlMnOpQrStUvWxYz0123456789\n---\n\n# Body\n\nclean prose\n";
    match prepare(&policy, signer.as_ref(), "docs/tokens.md", fm.as_bytes()) {
        PreparedDocument::Blocked { props, .. } => {
            assert!(
                props.content_hash.is_none(),
                "blocked before any content hash"
            );
            assert!(
                props.rule_ids.contains_key("github-token"),
                "{:?}",
                props.rule_ids
            );
            assert!(props.title.is_none(), "no title on a blocked document");
        }
        other => panic!("{other:?}"),
    }
}

mod common;

/// Write a prepared document through `apply_batch` the way the committer will (one transaction:
/// Document, Section and Chunk nodes with `PART_OF` edges, the audit rows), so the on-disk
/// bytes can be inspected. Returns the node ids written.
fn commit(
    store: &SqliteStore,
    policy: &Policy,
    prepared: PreparedDocument,
) -> Vec<kaban_core::NodeId> {
    use kaban_core::{Actor, IngestStore, NewNode, Op, WriteBatch, WriteCtx};
    let ws = WorkspaceId::default();
    let mut ctx = WriteCtx::new(policy.effective_hash(), ws.clone(), Actor::Agent);
    let mut ops = Vec::new();
    let mut ids = Vec::new();
    let node = |kind: &str, text: String, props: serde_json::Value| NewNode {
        kind: kind.into(),
        text,
        props: Some(props),
        ..NewNode::default()
    };
    match prepared {
        PreparedDocument::Ready { graph, audit } => {
            ctx.audit = audit;
            let doc = graph.document;
            ops.push(Op::UpsertNode {
                id: doc.id.clone(),
                node: node(
                    "Document",
                    String::new(),
                    serde_json::to_value(&doc.props).unwrap(),
                ),
            });
            ids.push(doc.id.clone());
            for s in graph.sections {
                ops.push(Op::UpsertNode {
                    id: s.id.clone(),
                    node: node(
                        "Section",
                        String::new(),
                        serde_json::to_value(&s.props).unwrap(),
                    ),
                });
                ops.push(Op::UpsertEdge {
                    src: s.id.clone(),
                    dst: doc.id.clone(),
                    rel: "PART_OF".into(),
                    props: None,
                });
                ids.push(s.id.clone());
                for c in s.chunks {
                    let text = format!("{} {}", c.props.context_prefix, c.props.text);
                    ops.push(Op::UpsertNode {
                        id: c.id.clone(),
                        node: node("Chunk", text, serde_json::to_value(&c.props).unwrap()),
                    });
                    ops.push(Op::UpsertEdge {
                        src: c.id.clone(),
                        dst: s.id.clone(),
                        rel: "PART_OF".into(),
                        props: None,
                    });
                    ids.push(c.id);
                }
            }
        }
        PreparedDocument::Blocked { props, audit } => {
            ctx.audit = audit;
            let id = kaban_ingest::document_id(&SourceId::new(
                &ws,
                &gate_path(policy, &props.path).unwrap(),
            ));
            ops.push(Op::UpsertNode {
                id: id.clone(),
                node: node(
                    "Document",
                    String::new(),
                    serde_json::to_value(&props).unwrap(),
                ),
            });
            ids.push(id);
        }
        PreparedDocument::Failed { props } => {
            let id = kaban_ingest::document_id(&SourceId::new(
                &ws,
                &gate_path(policy, &props.path).unwrap(),
            ));
            ops.push(Op::UpsertNode {
                id: id.clone(),
                node: node(
                    "Document",
                    String::new(),
                    serde_json::to_value(&props).unwrap(),
                ),
            });
            ids.push(id);
        }
    }
    store.apply_batch(WriteBatch::new(ws, ops), &ctx).unwrap();
    ids
}

/// Black-box canary (plan DI1.4, through the `canary_prehash.rs` pattern): three documents, one
/// with the reject canary `password = Sup3rS3cretValue!` (`skip_file`: blocked), one with an AWS
/// key id in a paragraph (that chunk stubbed, siblings stored) and one with a redactable
/// hostname in title and body (masked), are prepared under a `trace` capture and written
/// through `apply_batch` to a file-backed store. The canary, the key and the hostname are absent
/// from the database, WAL and SHM bytes, the audit chain, every stored node's text and props, a
/// `recall` over the index and the trace log; the stubbed chunk's `PART_OF` edge exists.
#[test]
fn canaries_are_absent_from_db_wal_shm_audit_recall_and_logs() {
    use kaban_core::{AuditStore, MemoryStore, RecallQuery};
    let root = common::Scratch::new("kaban-ingest-canary", "blackbox");
    let db = root.join("kaban.db");
    let store = SqliteStore::open(&db, &root.join("keys")).unwrap();
    let signer = store.signer();
    let policy = redact_policy();
    let aws = "AKIAQWERTYUIOPASDFGH";
    let host = "db1.corp.acme.internal";
    let docs: Vec<(&str, String)> = vec![
        (
            "notes/secret.md",
            format!("# Settings\n\nThe line is\n\npassword = {CANARY}\n\nmore prose.\n"),
        ),
        (
            "notes/keys.md",
            format!("# Keys\n\nFirst paragraph.\n\n```\n{aws}\n```\n\nLast paragraph.\n"),
        ),
        (
            "notes/hosts.md",
            format!("# Hosts {host}\n\nConnect to {host} tonight.\n"),
        ),
        (
            // RD1-2: the credential sits in the fence-info token, the body is clean
            "notes/fence.md",
            format!("# Fence\n\n```{aws}\nprintln!(\"hello\");\n```\n"),
        ),
    ];

    let logs = common::pool::LogBuf::default();
    let sink = logs.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new("trace"))
        .with_writer(move || sink.clone())
        .with_ansi(false)
        .finish();
    let written = tracing::subscriber::with_default(subscriber, || {
        let mut written = Vec::new();
        for (path, src) in &docs {
            let prepared = prepare(&policy, signer.as_ref(), path, src.as_bytes());
            tracing::trace!(path, "prepared one document in the canary test");
            written.push((path, commit(&store, &policy, prepared)));
        }
        written
    });

    // shapes: blocked, stubbed, masked
    let blocked = store.get(&written[0].1[0], true).unwrap().unwrap();
    assert_eq!(blocked.props["status"], "blocked");
    assert!(blocked.props.get("content_hash").is_none());
    assert!(blocked.props.get("blocked_hmac").is_some());
    assert_eq!(
        written[0].1.len(),
        1,
        "a blocked document has no sections or chunks"
    );
    let keys_nodes: Vec<_> = written[1]
        .1
        .iter()
        .map(|id| store.get(id, true).unwrap().unwrap())
        .collect();
    let stubs: Vec<_> = keys_nodes
        .iter()
        .filter(|n| n.props.get("kind") == Some(&serde_json::json!("stub")))
        .collect();
    assert_eq!(
        stubs.len(),
        1,
        "the fence with the key is a stub: {keys_nodes:?}"
    );
    assert_eq!(stubs[0].props["stub_hmac"].as_str().unwrap().len(), 64);
    assert_eq!(stubs[0].props["text"], "");
    assert!(
        keys_nodes.iter().filter(|n| n.kind == "Chunk").count() >= 3,
        "siblings stored"
    );
    let hosts_doc = store.get(&written[2].1[0], true).unwrap().unwrap();
    assert_eq!(
        hosts_doc.props["title"],
        "Hosts [REDACTED:internal-hostname]"
    );

    // bytes: db/wal/shm, audit chain, node texts and props, recall, logs
    let mut disk = std::fs::read(&db).unwrap();
    for suffix in ["-wal", "-shm"] {
        let side = std::path::PathBuf::from(format!("{}{suffix}", db.display()));
        if side.exists() {
            disk.extend(std::fs::read(side).unwrap());
        }
    }
    let contains =
        |hay: &[u8], needle: &str| hay.windows(needle.len()).any(|w| w == needle.as_bytes());
    for needle in [CANARY, aws, host] {
        assert!(!contains(&disk, needle), "{needle} in db/wal/shm bytes");
    }
    let audit = store.audit_verify().unwrap();
    assert!(audit.ok && audit.entries >= 3, "{audit:?}");
    for (_, ids) in &written {
        for id in ids {
            let n = store.get(id, true).unwrap().unwrap();
            let blob = format!("{} {}", n.text, n.props);
            for needle in [CANARY, aws, host] {
                assert!(!blob.contains(needle), "{needle} in node {id}: {blob}");
            }
        }
    }
    for needle in [CANARY, aws, host, "Sup3r", "S3cretValue"] {
        // the fuzzy layer may seed a neighbour of the needle (spec 05 OWN-M: every store
        // recalls through its own index); no hit may carry the needle itself
        let hits = store
            .recall(&RecallQuery {
                query: needle.to_owned(),
                limit: 10,
                ..RecallQuery::default()
            })
            .unwrap();
        for hit in &hits {
            let blob = format!("{} {}", hit.node.text, hit.node.props);
            assert!(!blob.contains(needle), "recall({needle}) returned {blob}");
        }
    }
    let log = logs.0.lock().unwrap().clone();
    assert!(!log.is_empty(), "the trace capture saw events");
    for needle in [CANARY, aws, host] {
        assert!(
            !contains(&log, needle),
            "{needle} in trace logs: {}",
            String::from_utf8_lossy(&log)
        );
    }
    drop(store);
    let _ = &root;
}

mod properties {
    use super::*;
    use kaban_ingest::Extractor as _;
    use proptest::prelude::*;

    /// Plain words and punctuation around the planted secret (no `#`, `[`, `]` so the Markdown
    /// structure stays a paragraph).
    fn filler() -> impl Strategy<Value = String> {
        "[a-z ,.]{0,40}"
    }

    /// A planted secret of a redact class (hostname) or a reject class (AWS key id, assignment
    /// with the canary), with the shape the policy matches.
    fn secret() -> impl Strategy<Value = String> {
        prop_oneof![
            "[a-z0-9]{2,8}".prop_map(|h| format!("{h}.corp.acme.internal")),
            "[A-Z0-9]{16}".prop_map(|k| format!("AKIA{k}")),
            Just(format!("password = {CANARY}")),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 64, ..ProptestConfig::default() })]

        /// Property (plan DI1.4 preamble): whatever surrounds a planted hit, no gated chunk text,
        /// no prefix, no stub hash, no audit row and no id or `text_hash` contains the hit, and
        /// a masked chunk's `text_hash` differs from the hash of the raw text (the id preimage
        /// never saw the secret).
        #[test]
        fn gated_output_and_ids_never_contain_the_hit(
            before in filler(),
            secret in secret(),
            after in filler(),
            heading in "[A-Za-z ]{1,12}",
        ) {
            let policy = redact_policy();
            let signer = signer();
            let src = format!("# {heading}\n\n{before} {secret} {after}\n\nA clean second paragraph.\n");
            let prepared = prepare(&policy, signer.as_ref(), "prop/doc.md", src.as_bytes());
            // the secret's distinctive token: the hostname, the key, or the canary
            let needle = secret.rsplit(' ').next().unwrap().to_owned();
            match prepared {
                PreparedDocument::Blocked { props, audit } => {
                    let blob = serde_json::to_string(&(&props, &audit)).unwrap();
                    prop_assert!(!blob.contains(&needle), "{blob}");
                    prop_assert!(props.content_hash.is_none());
                }
                PreparedDocument::Ready { graph, audit } => {
                    let blob = serde_json::to_string(&(&graph.document.props, &audit)).unwrap();
                    prop_assert!(!blob.contains(&needle), "{blob}");
                    // the hashes the ungated chunker would have produced for the chunks that
                    // carry the hit: no stored chunk may carry one of them
                    let extracted = kaban_ingest::MarkdownExtractor.extract(src.as_bytes(), &ctx()).unwrap();
                    let raw_hashes: Vec<String> = kaban_ingest::chunk(&extracted, &ChunkConfig::default())
                        .iter()
                        .flat_map(|(_, cs)| cs.iter())
                        .filter(|c| c.text.contains(&needle))
                        .map(|c| blake3::hash(c.text.as_bytes()).to_hex().to_string())
                        .collect();
                    prop_assert!(!raw_hashes.is_empty(), "the hit sits in at least one raw chunk");
                    for s in &graph.sections {
                        prop_assert!(!s.id.as_str().contains(&needle));
                        for c in &s.chunks {
                            prop_assert!(!c.props.text.contains(&needle), "{:?}", c.props.text);
                            prop_assert!(!c.props.context_prefix.contains(&needle));
                            prop_assert!(!c.id.as_str().contains(&needle));
                            prop_assert!(!c.props.text_hash.contains(&needle));
                            prop_assert!(!raw_hashes.contains(&c.props.text_hash), "a chunk hashed the raw text");
                            if let Some(h) = &c.props.stub_hmac {
                                prop_assert!(h != &needle && h.len() == 64);
                            }
                        }
                    }
                }
                PreparedDocument::Failed { .. } => prop_assert!(false, "no extractor failure expected"),
            }
        }
    }
}

/// The corpus's document cases (`crates/kaban-policy/fixtures/corpus.yaml`, field `document`):
/// `skip_file` texts block the whole document through `gate_document`, `stub` texts stub only
/// their chunk and leave a clean sibling stored.
#[test]
fn corpus_document_cases_skip_the_file_or_stub_the_chunk_as_declared() {
    #[derive(serde::Deserialize)]
    struct Case {
        text: String,
        #[serde(default)]
        document: Option<String>,
    }
    #[derive(serde::Deserialize)]
    struct Corpus {
        policy: kaban_policy::PolicyFile,
        cases: Vec<Case>,
    }
    let corpus: Corpus =
        yaml_serde::from_str(include_str!("../../kaban-policy/fixtures/corpus.yaml")).unwrap();
    let policy = Policy::from_file(corpus.policy, "corpus".into()).unwrap();
    let signer = signer();
    let cases: Vec<&Case> = corpus
        .cases
        .iter()
        .filter(|c| c.document.is_some())
        .collect();
    assert!(cases.len() >= 6, "{} document cases", cases.len());
    for case in cases {
        let chunks = vec![
            draft("a clean first chunk.", "Doc"),
            draft(&case.text, "Doc"),
        ];
        let full = stream_text(&chunks);
        let decision = gate_document(
            &policy,
            signer.as_ref(),
            &ctx(),
            &WorkspaceId::default(),
            clean_meta(),
            &full,
            chunks,
            None,
            &ChunkConfig::default(),
        )
        .unwrap();
        match (case.document.as_deref(), decision) {
            (Some("skip_file"), GateDecision::SkipFile { .. }) => {}
            (Some("stub"), GateDecision::Proceed(g)) => {
                assert_eq!(g.chunks[0].draft.text, "a clean first chunk.");
                assert_eq!(g.chunks[1].draft.kind, ChunkKind::Stub, "{:?}", case.text);
                assert_eq!(g.blocked_chunks, 1);
            }
            (want, got) => panic!("{:?}: wanted {want:?}, got {got:?}", case.text),
        }
    }
}

// ---- DI1.7 consolidation: one table over every rule family the gate covers ----

/// The DI1.4 policy with extra user rules appended (same hostname and `hunter2` rules as
/// [`redact_policy`], so the two are interchangeable for the shared cases).
fn policy_with(extra_rules: &str) -> Policy {
    let yaml = format!(
        "rules:\n  - id: internal-hostname\n    regex: '\\b[a-z0-9-]+\\.corp\\.acme\\.internal\\b'\n    keywords: ['.corp.acme.internal']\n    action: redact\n{extra_rules}words:\n  - {{ term: hunter2, action: redact, id: old-password }}\n"
    );
    Policy::from_file(Policy::parse(yaml.as_bytes()).unwrap(), "test".into()).unwrap()
}

/// One rule family of the gate: where the secret sits, what the gate must do with the document
/// and which rule must leave the keyed receipt.
struct Family {
    name: &'static str,
    path: &'static str,
    extra_rules: &'static str,
    source: String,
    /// Byte strings that must survive nowhere.
    needles: Vec<String>,
    blocked: bool,
    rule: &'static str,
}

fn families() -> Vec<Family> {
    let host = "db1.corp.acme.internal";
    let token = "Q".repeat(24);
    vec![
        Family {
            name: "fence info string",
            path: "fam/fence.md",
            extra_rules: "",
            source: format!("# Fence\n\n```{host}\nlet y = 2;\n```\n"),
            needles: vec![host.to_owned()],
            blocked: false,
            rule: "internal-hostname",
        },
        Family {
            name: "frontmatter scalar (reject class, skip_file)",
            path: "fam/frontmatter.md",
            extra_rules: "",
            source: format!("---\ntitle: Doc\npassword: {CANARY}\n---\n\n# Doc\n\nclean body\n"),
            needles: vec![CANARY.to_owned()],
            blocked: true,
            rule: "generic-credential-assignment",
        },
        Family {
            name: "key=value view (the bare value is clean)",
            path: "fam/kv.md",
            extra_rules: "  - id: customer-view\n    regex: '^customer=(.+)$'\n    secret_group: 1\n    action: redact\n",
            source: "---\ncustomer: alice-7781\nnote: fine\n---\n\n# Doc\n\nclean body\n"
                .to_owned(),
            needles: vec!["alice-7781".to_owned()],
            blocked: false,
            rule: "customer-view",
        },
        Family {
            name: "stream-spanning hit (keyword and token across a paragraph boundary)",
            path: "fam/stream.md",
            extra_rules: "",
            source: format!(
                "# Doc\n\n{}Bearer\n\n{token}{}\n",
                "padding ".repeat(256),
                " prose".repeat(50)
            ),
            needles: vec![token.clone()],
            blocked: false,
            rule: "bearer-token",
        },
        Family {
            name: "union of touching hits (two rules, adjacent matches)",
            path: "fam/union.md",
            extra_rules: "  - id: all-s\n    regex: 'S+'\n    action: redact\n  - id: all-e\n    regex: '\u{e9}+'\n    action: redact\n",
            source: format!(
                "# Doc\n\nbefore {}{} after\n",
                "S".repeat(12),
                "\u{e9}".repeat(12)
            ),
            needles: vec!["S".repeat(12), "\u{e9}".repeat(12)],
            blocked: false,
            rule: "all-s",
        },
        Family {
            name: "metadata-only redaction (clean body)",
            path: "fam/meta.md",
            extra_rules: "",
            source: format!("---\nhost: {host}\n---\n\n# Doc\n\nclean body\n"),
            needles: vec![host.to_owned()],
            blocked: false,
            rule: "internal-hostname",
        },
    ]
}

fn contains_bytes(hay: &[u8], needle: &str) -> bool {
    hay.windows(needle.len()).any(|w| w == needle.as_bytes())
}

/// DI1.7 (plan DI1.4 families, consolidated): for every rule family the gate covers, the
/// document is prepared under its policy and written through `apply_batch` to a file-backed
/// store; the secret is absent from the prepared graph (props, chunks, links, audit rows), from
/// the db/wal/shm bytes and from `recall`, the document takes the declared shape (blocked or
/// ready with the rule counted), and the keyed audit receipt (the rule id, a 64-hex HMAC, never
/// the text) is present and chains.
#[test]
fn every_rule_family_leaves_the_secret_out_of_graph_bytes_and_audit_with_a_keyed_receipt() {
    use kaban_core::{AuditStore, MemoryStore, RecallQuery};
    for fam in families() {
        // Arrange
        let root = common::Scratch::new(
            "kaban-ingest-canary-fam",
            fam.path.rsplit('/').next().unwrap(),
        );
        let db = root.join("kaban.db");
        let store = SqliteStore::open(&db, &root.join("keys")).unwrap();
        let signer = store.signer();
        let policy = policy_with(fam.extra_rules);
        let name = fam.name;

        // Act
        let prepared = prepare(&policy, signer.as_ref(), fam.path, fam.source.as_bytes());

        // Assert: the prepared shape and its audit receipt
        let (blob, audit, rule_ids) = match &prepared {
            PreparedDocument::Ready { graph, audit } => (
                serde_json::to_string(&(
                    &graph.document.props,
                    graph
                        .sections
                        .iter()
                        .flat_map(|s| &s.chunks)
                        .map(|c| &c.props)
                        .collect::<Vec<_>>(),
                    &graph.links,
                    &graph.tags,
                    audit,
                ))
                .unwrap(),
                audit.clone(),
                graph.document.props.rule_ids.clone(),
            ),
            PreparedDocument::Blocked { props, audit } => (
                serde_json::to_string(&(props, audit)).unwrap(),
                audit.clone(),
                props.rule_ids.clone(),
            ),
            PreparedDocument::Failed { props } => panic!("{name}: failed: {props:?}"),
        };
        assert_eq!(
            matches!(prepared, PreparedDocument::Blocked { .. }),
            fam.blocked,
            "{name}: shape"
        );
        assert!(rule_ids.contains_key(fam.rule), "{name}: {rule_ids:?}");
        for needle in &fam.needles {
            assert!(
                !blob.contains(needle.as_str()),
                "{name}: {needle} in the prepared graph"
            );
        }
        let receipt = audit
            .iter()
            .find(|a| a.rules.iter().any(|r| r == fam.rule))
            .unwrap_or_else(|| panic!("{name}: no receipt for {}: {audit:?}", fam.rule));
        assert_eq!(receipt.tool, "ingest", "{name}");
        assert_eq!(receipt.input_hmac.len(), 64, "{name}: {receipt:?}");
        assert!(
            receipt.input_hmac.bytes().all(|b| b.is_ascii_hexdigit()),
            "{name}"
        );

        // Act: through `apply_batch` to disk
        let ids = commit(&store, &policy, prepared);

        // Assert: bytes, stored rows, recall, chain
        let mut disk = std::fs::read(&db).unwrap();
        for suffix in ["-wal", "-shm"] {
            let side = std::path::PathBuf::from(format!("{}{suffix}", db.display()));
            if side.exists() {
                disk.extend(std::fs::read(side).unwrap());
            }
        }
        assert!(!disk.is_empty(), "{name}: database bytes read");
        for needle in &fam.needles {
            assert!(
                !contains_bytes(&disk, needle),
                "{name}: {needle} in db/wal/shm bytes"
            );
            for id in &ids {
                let n = store.get(id, true).unwrap().unwrap();
                let row = format!("{} {} {}", n.text, n.summary.unwrap_or_default(), n.props);
                assert!(
                    !row.contains(needle.as_str()),
                    "{name}: {needle} in node {id}: {row}"
                );
            }
            let hits = store
                .recall(&RecallQuery {
                    query: needle.clone(),
                    limit: 10,
                    ..RecallQuery::default()
                })
                .unwrap();
            assert!(
                hits.is_empty(),
                "{name}: recall({needle}) found {}",
                hits.len()
            );
        }
        let chain = store.audit_verify().unwrap();
        assert!(chain.ok && chain.entries >= 1, "{name}: {chain:?}");
        drop(store);
        let _ = &root;
    }
}

/// RD1-3 defence in depth (DI1.4a deviation 5, untested there): after leaf masking the
/// sanitized frontmatter is rendered as JSON and rescanned. A redact rule that matches only in
/// the rendered form (`x","b":"y` spans two leaves: the YAML source reads `a: x` / `b: y` and
/// the leaf views `a=x` / `b=y` never contain it) withholds the whole frontmatter as an empty
/// object, counts the rule and leaves one keyed receipt under `frontmatter`; the body is
/// untouched.
#[test]
fn a_hit_visible_only_in_the_rendered_frontmatter_withholds_the_whole_block_with_a_receipt() {
    let policy =
        policy_with("  - id: rendered-only\n    regex: 'x\",\"b\":\"y'\n    action: redact\n");
    let src = "---\na: x\nb: y\n---\n\n# Doc\n\nclean body\n";
    assert!(
        policy.scan("a: x\nb: y").hits.is_empty(),
        "the source is clean"
    );
    assert!(policy.scan("a=x").hits.is_empty() && policy.scan("b=y").hits.is_empty());
    match prepare(&policy, signer().as_ref(), "rendered.md", src.as_bytes()) {
        PreparedDocument::Ready { graph, audit } => {
            let fm = &graph.document.props.frontmatter;
            assert!(
                fm.as_object().is_some_and(serde_json::Map::is_empty),
                "{fm}"
            );
            assert_eq!(graph.document.props.rule_ids.get("rendered-only"), Some(&1));
            let receipts: Vec<_> = audit
                .iter()
                .filter(|a| a.field == "frontmatter" && a.rules == ["rendered-only"])
                .collect();
            assert_eq!(receipts.len(), 1, "{audit:?}");
            assert_eq!(receipts[0].input_hmac.len(), 64);
            let chunks: Vec<_> = graph.sections.iter().flat_map(|s| &s.chunks).collect();
            assert_eq!(chunks.len(), 1);
            assert_eq!(chunks[0].props.text, "clean body");
        }
        other => panic!("{other:?}"),
    }
}

mod window_properties {
    use super::*;
    use proptest::prelude::*;

    /// A stream made of `n` chunks of random lengths, with runs of `S` planted at random
    /// positions, long enough that the stream gate scans it in several overlapping windows
    /// (8,192 chars, stride 4,096) and the runs fall on window edges, inside overlaps and
    /// across chunk boundaries.
    fn layout() -> impl Strategy<Value = (Vec<usize>, Vec<(usize, usize)>)> {
        (
            prop::collection::vec(1usize..6_000, 2..5),
            prop::collection::vec((0usize..20_000, 1usize..300), 1..6),
        )
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 48, ..ProptestConfig::default() })]

        /// DI1.4a residual, taken in at DI1.7: whatever the chunk lengths and wherever the runs
        /// sit relative to the window grid, no matched character survives masking in any gated
        /// chunk, and every chunk that carried a run was masked (not stubbed).
        #[test]
        fn no_matched_character_survives_masking_for_any_window_layout((lens, runs) in layout()) {
            let policy = policy_with("  - id: all-s\n    regex: 'S+'\n    action: redact\n");
            let total: usize = lens.iter().sum();
            let mut text: Vec<u8> = "a ".repeat(total / 2 + 1).into_bytes();
            text.truncate(total);
            let mut planted = false;
            for (at, len) in &runs {
                let at = at % total;
                let end = (at + len).min(total);
                for b in &mut text[at..end] { *b = b'S'; }
                planted |= end > at;
            }
            let text = String::from_utf8(text).unwrap();
            let mut chunks = Vec::new();
            let mut from = 0;
            for len in &lens {
                chunks.push(draft(&text[from..from + len], "Doc"));
                from += len;
            }
            let g = proceed(&policy, signer().as_ref(), chunks, None);
            prop_assert_eq!(g.chunks.len(), lens.len());
            for c in &g.chunks {
                prop_assert!(!c.draft.text.contains('S'), "an S survived: {}", c.draft.text);
                prop_assert!(c.draft.kind != ChunkKind::Stub, "masked, not stubbed");
            }
            if planted {
                prop_assert!(g.rule_ids.contains_key("all-s"), "{:?}", g.rule_ids);
            }
        }

        /// DI1.8b (RD2-2): the same property through the actual extractor and chunker. Every
        /// paragraph is a head of filler, a whitespace gap and a short tail (the reviewer's
        /// shape, randomized): a paragraph above the hard max is split at the gap, its pieces
        /// trimmed, and a short tail merges back into its predecessor; the filler is ASCII or
        /// multibyte. A run of `S` is planted from somewhere in the gap into the tail, and the rule
        /// ` {3}S+` needs the spaces the trim removes, so only the stream scan can see it and
        /// only an exact source-to-output mapping can mask it. After `prepare_document` no
        /// chunk text carries an `S`, no chunk is stubbed and the rule is counted when a run
        /// was planted.
        #[test]
        fn no_matched_character_survives_the_real_extractor_and_chunker(
            paras in prop::collection::vec((2_000usize..5_000, 3usize..600, 1usize..300, 0usize..300, 3usize..600), 1..4),
            multibyte in any::<bool>(),
        ) {
            let policy = policy_with("  - id: spaced-s\n    regex: ' {3}S+'\n    action: redact\n");
            let filler: &[char] = if multibyte { &['é', 'ß'] } else { &['a', 'b'] };
            let mut chars: Vec<char> = Vec::new();
            let mut planted = false;
            for (head, gap, tail, run, offset) in &paras {
                chars.extend((0..*head).map(|i| filler[i % filler.len()]));
                // the run starts somewhere in the gap (or exactly at the tail), always with at
                // least three spaces before it
                let run_at = chars.len() + (*offset).min(*gap);
                chars.extend(std::iter::repeat_n(' ', *gap));
                chars.extend((0..*tail).map(|i| filler[(i + 1) % filler.len()]));
                let run_end = (run_at + run).min(chars.len());
                for c in &mut chars[run_at..run_end] { *c = 'S'; planted = true; }
                chars.extend(['\n', '\n']);
            }
            let body: String = chars.into_iter().collect();
            let src = format!("# Doc\n\n{body}");
            match prepare(&policy, signer().as_ref(), "prop/real.md", src.as_bytes()) {
                PreparedDocument::Ready { graph, .. } => {
                    for s in &graph.sections {
                        for c in &s.chunks {
                            prop_assert!(!c.props.text.contains('S'), "an S survived: {}", c.props.text);
                            prop_assert!(c.props.kind != ChunkKind::Stub, "masked, not stubbed");
                        }
                    }
                    if planted {
                        prop_assert!(graph.document.props.rule_ids.contains_key("spaced-s"));
                    }
                }
                other => prop_assert!(false, "{other:?}"),
            }
        }
    }
}
