//! Formed strings are re-scanned before they are written or reported (stage1-spec G1-S1 step 3;
//! review unit U1 RU1-1): the joined documentation block, the `indexer:<lang>` join of the
//! producer's `tool_info`, a file node's `ref` and `name`, and the caller-supplied `Meta`
//! strings at publication. Every case runs the public pipeline (`decode_and_gate`, `admit_index`,
//! `Rows::derive`, `publish`, the reader) over a synthetic `scip::types` index.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use kaban_codegraph::{CodeGraphReader, HeadInput, IsolationTier, SqliteSnapshot, discover};
use kaban_index::WarningClass;
use kaban_index::admit_index;
use kaban_index::writer::{self, Rows};
use kaban_policy::Policy;
use scip::types::symbol_information::Kind;

fn contains(hay: &[u8], needle: &str) -> bool {
    hay.windows(needle.len()).any(|w| w == needle.as_bytes())
}

/// Two documentation elements that are clean on their own and match the `joined-doc`
/// credential rule only once joined with the blank line: the field is dropped, counted, and on
/// no byte of the snapshot (the FTS shadow tables included) or of the report.
#[test]
fn joined_documentation_hit_drops_the_field() {
    let policy = common::test_policy();
    let v = common::GO_DRIVERS_COMMIT;
    let sym = common::go_sym(v, "Doc#");
    let mut info = common::sym_info_kind(&sym, Kind::Struct);
    info.documentation = vec!["ALPHA_HALF".to_owned(), "BETA_HALF".to_owned()];
    let idx = common::index(vec![common::document(
        "drivers/doc.go",
        vec![info],
        vec![common::definition(&sym, &[1, 5, 8])],
    )]);
    let bytes = common::bytes(&idx);
    let gated = common::gated(&bytes, &policy);
    let admitted = admit_index(&gated);
    assert_eq!(
        admitted.counts().policy_dropped,
        0,
        "each element is clean on its own"
    );
    let rows = Rows::derive(&admitted, None, &policy);
    assert_eq!(
        rows.counts().policy_dropped,
        1,
        "the joined block is dropped"
    );
    assert_eq!(rows.warnings(), vec![WarningClass::PolicyDropped]);
    let dir = common::Scratch::new();
    let p = writer::publish(
        dir.path(),
        &rows,
        &common::meta(&common::commit(0x61)),
        &policy,
    )
    .unwrap();
    let file = common::read(&p);
    assert!(!contains(&file, "BETA_HALF") && !contains(&file, "ALPHA_HALF"));
    let snap = SqliteSnapshot::open_ro(&p).unwrap();
    let row = snap.node(&format!("scip:{sym}")).unwrap().unwrap();
    assert_eq!(row.doc, None, "the field is dropped, the row stays");
    let meta = snap.meta().unwrap();
    assert_eq!(meta["counts:policy_dropped"], "1");
    let report = serde_json::to_string(
        &rows
            .meta_rows(&common::meta(&common::commit(0x61)), &policy)
            .rows,
    )
    .unwrap();
    assert!(!report.contains("BETA_HALF"));
}

/// A rule that matches `file:drivers/formed.go` and not `drivers/formed.go`, and one that
/// matches the basename `basename-only.go` and not its path: the file (and everything in it)
/// is skipped under `policy_skipped_row`, a third clean file stays.
#[test]
fn file_node_ref_or_name_hit_skips_the_file() {
    let policy = common::test_policy();
    let v = common::GO_DRIVERS_COMMIT;
    let (sym_a, sym_b, sym_c) = (
        common::go_sym(v, "A#"),
        common::go_sym(v, "B#"),
        common::go_sym(v, "C#"),
    );
    let idx = common::index(vec![
        common::document(
            "drivers/formed.go",
            vec![common::sym_info_kind(&sym_a, Kind::Struct)],
            vec![common::definition(&sym_a, &[1, 5, 6])],
        ),
        common::document(
            "drivers/basename-only.go",
            vec![common::sym_info_kind(&sym_b, Kind::Struct)],
            vec![common::definition(&sym_b, &[1, 5, 6])],
        ),
        common::document(
            "drivers/clean.go",
            vec![common::sym_info_kind(&sym_c, Kind::Struct)],
            vec![common::definition(&sym_c, &[1, 5, 6])],
        ),
    ]);
    let bytes = common::bytes(&idx);
    let gated = common::gated(&bytes, &policy);
    let admitted = admit_index(&gated);
    assert_eq!(admitted.files().len(), 3, "the raw paths pass the gate");
    let dir = common::Scratch::new();
    let path = common::publish(dir.path(), &common::commit(0x62), &bytes, None, &policy);
    let file = common::read(&path);
    assert!(!contains(&file, "formed.go") && !contains(&file, "basename-only"));
    let snap = SqliteSnapshot::open_ro(&path).unwrap();
    assert!(snap.node("file:drivers/formed.go").unwrap().is_none());
    assert!(
        snap.node("file:drivers/basename-only.go")
            .unwrap()
            .is_none()
    );
    assert!(snap.node("file:drivers/clean.go").unwrap().is_some());
    assert!(snap.node(&format!("scip:{sym_a}")).unwrap().is_none());
    assert!(snap.node(&format!("scip:{sym_c}")).unwrap().is_some());
    let meta = snap.meta().unwrap();
    assert_eq!(meta["counts:files"], "1");
    assert_eq!(
        meta["counts:policy_skipped_row"], "2",
        "one per skipped file"
    );
}

/// The `indexer:<lang>` text is the space join of the producer's `tool_info`: `canary` and
/// `joined` pass the meta gate one by one, the join matches `joined-canary`. The publication
/// writes the masked row and counts it (DF-3), never a refusal.
#[test]
fn joined_tool_text_hit_is_masked_at_publication() {
    let policy = common::test_policy();
    let v = common::GO_DRIVERS_COMMIT;
    let sym = common::go_sym(v, "T#");
    let mut idx = common::index(vec![common::document(
        "drivers/t.go",
        vec![common::sym_info_kind(&sym, Kind::Struct)],
        vec![common::definition(&sym, &[1, 5, 6])],
    )]);
    let tool = idx.metadata.as_mut().unwrap().tool_info.as_mut().unwrap();
    tool.name = "canary".to_owned();
    tool.version = "joined".to_owned();
    let bytes = common::bytes(&idx);
    let gated = common::gated(&bytes, &policy);
    let admitted = admit_index(&gated);
    assert_eq!(
        admitted.counts().policy_redacted_meta,
        0,
        "each value is clean on its own"
    );
    let rows = Rows::derive(&admitted, None, &policy);
    assert_eq!(rows.indexers()["go"], "canary joined");
    let dir = common::Scratch::new();
    let meta = common::meta(&common::commit(0x63));
    let scanned = rows.meta_rows(&meta, &policy);
    assert_eq!(scanned.policy_redacted_meta, 1);
    assert_eq!(scanned.rule_ids, ["joined-canary"]);
    let p = writer::publish(dir.path(), &rows, &meta, &policy).expect("published");
    let snap = SqliteSnapshot::open_ro(&p).unwrap();
    let m = snap.meta().unwrap();
    assert_eq!(m["indexer:go"], "[policy:joined-canary]");
    assert_eq!(m["counts:policy_redacted_meta"], "1");
    assert!(
        !common::read(&p).windows(13).any(|w| w == b"canary joined"),
        "the joined text is on no byte of the snapshot"
    );
}

/// A caller-supplied descriptive `Meta` string carrying the canary (`container_image_digest`,
/// then `isolation_detail`): published with the value masked by the rule id and counted
/// (DF-3); `meta_rows` (the report's source) says the same.
#[test]
fn meta_canary_is_masked_at_publication() {
    let policy = common::test_policy();
    let v = common::GO_DRIVERS_COMMIT;
    let sym = common::go_sym(v, "M#");
    let idx = common::index(vec![common::document(
        "drivers/m.go",
        vec![common::sym_info_kind(&sym, Kind::Struct)],
        vec![common::definition(&sym, &[1, 5, 6])],
    )]);
    let bytes = common::bytes(&idx);
    let gated = common::gated(&bytes, &policy);
    let admitted = admit_index(&gated);
    let rows = Rows::derive(&admitted, None, &policy);
    for (case, meta) in [
        ("container_image_digest", {
            let mut m = common::meta(&common::commit(0x64));
            m.provenance.container_image_digest = Some("sha256:CANARY_DIGEST".to_owned());
            m
        }),
        ("isolation_detail", {
            let mut m = common::meta(&common::commit(0x65));
            m.provenance.isolation_detail = "tier CANARY_DETAIL".to_owned();
            m
        }),
    ] {
        let dir = common::Scratch::new();
        let scanned = rows.meta_rows(&meta, &policy);
        assert_eq!(scanned.policy_redacted_meta, 1, "{case}");
        assert_eq!(scanned.rule_ids, ["canary-cred"], "{case}");
        let p = writer::publish(dir.path(), &rows, &meta, &policy).expect(case);
        let snap = SqliteSnapshot::open_ro(&p).unwrap();
        let m = snap.meta().unwrap();
        assert_eq!(m[case], "[policy:canary-cred]", "{case}");
        assert_eq!(m["counts:policy_redacted_meta"], "1", "{case}");
        assert!(
            !common::read(&p).windows(6).any(|w| w == b"CANARY"),
            "{case}: the value is on no byte of the snapshot"
        );
    }
    let dir = common::Scratch::new();
    // the same rows publish once the meta is clean
    writer::publish(
        dir.path(),
        &rows,
        &common::meta(&common::commit(0x64)),
        &policy,
    )
    .unwrap();
}

/// A nested workspace path (backlog 17, NEST-1) is repository text gated under the policy
/// before the `coverage:nested_workspace_paths` list is formed (winB-1): the anchored
/// `file-name` rule matches the raw directory name `basename-only.go` and not its JSON form
/// `["basename-only.go"]`, and the literal newline of `joined-doc` is defeated by JSON escaping.
/// Each hit is published as its mask in place, counted once (DF-3) and on no byte of the
/// snapshot; the clean path beside it and the count row are kept.
#[test]
fn nested_workspace_path_canary_is_masked_before_the_list_is_formed() {
    let policy = common::test_policy();
    let v = common::GO_DRIVERS_COMMIT;
    let sym = common::go_sym(v, "N#");
    let idx = common::index(vec![common::document(
        "drivers/n.go",
        vec![common::sym_info_kind(&sym, Kind::Struct)],
        vec![common::definition(&sym, &[1, 5, 6])],
    )]);
    let bytes = common::bytes(&idx);
    let gated = common::gated(&bytes, &policy);
    let admitted = admit_index(&gated);
    for (case, path, rule, needle) in [
        ("anchored", "basename-only.go", "file-name", "basename-only"),
        (
            "escaped",
            "ALPHA_HALF\nBETA_HALF",
            "joined-doc",
            "ALPHA_HALF",
        ),
    ] {
        let mut rows = Rows::derive(&admitted, None, &policy);
        rows.set_nested_workspaces(vec!["tools/clean".to_owned(), path.to_owned()]);
        let meta = common::meta(&common::commit(0x66));
        let scanned = rows.meta_rows(&meta, &policy);
        assert_eq!(scanned.policy_redacted_meta, 1, "{case}");
        assert_eq!(scanned.rule_ids, [rule], "{case}");
        let dir = common::Scratch::new();
        let p = writer::publish(dir.path(), &rows, &meta, &policy).expect(case);
        let snap = SqliteSnapshot::open_ro(&p).unwrap();
        let m = snap.meta().unwrap();
        assert_eq!(m["coverage:nested_workspaces_not_indexed"], "2", "{case}");
        assert_eq!(
            m["coverage:nested_workspace_paths"],
            format!(r#"["tools/clean","[policy:{rule}]"]"#),
            "{case}"
        );
        assert_eq!(m["counts:policy_redacted_meta"], "1", "{case}");
        assert!(
            !contains(&common::read(&p), needle),
            "{case}: the path is on no byte of the snapshot"
        );
    }
}

/// Operational metadata is typed by what produced it (a commit, a tier name, a count) and is
/// never scanned (df23-4): a rule matching the tier name `none` masks the descriptive
/// `indexer:<lang>` text and `isolation_detail` and counts both, while `isolation`, `commit`,
/// `schema_version` and `counts:*` keep their values and `discover` admits the snapshot.
#[test]
fn operational_meta_is_never_redacted_and_the_snapshot_stays_discoverable() {
    let yaml = b"
version: 1
defaults: { action: reject }
extend: { use_builtin: false }
rules:
  - { id: tier-word, class: credential, regex: 'none' }
";
    let policy = Policy::from_file(Policy::parse(yaml).unwrap(), "test".into()).unwrap();
    let v = common::GO_DRIVERS_COMMIT;
    let sym = common::go_sym(v, "N#");
    let mut idx = common::index(vec![common::document(
        "drivers/n.go",
        vec![common::sym_info_kind(&sym, Kind::Struct)],
        vec![common::definition(&sym, &[1, 5, 6])],
    )]);
    idx.metadata
        .as_mut()
        .unwrap()
        .tool_info
        .as_mut()
        .unwrap()
        .name = "scip-none".to_owned();
    let gated = common::gated(&common::bytes(&idx), &policy);
    let admitted = admit_index(&gated);
    assert_eq!(admitted.counts().policy_redacted_meta, 1, "the tool name");
    let rows = Rows::derive(&admitted, None, &policy);
    let commit = common::commit(0x66);
    let mut meta = common::meta(&commit);
    meta.provenance.isolation_detail = "tier none".to_owned();
    let scanned = rows.meta_rows(&meta, &policy);
    assert_eq!(
        scanned.policy_redacted_meta, 2,
        "tool name and isolation_detail"
    );
    assert_eq!(scanned.rule_ids, ["tier-word"]);
    let dir = common::Scratch::new();
    let p = writer::publish(dir.path(), &rows, &meta, &policy).expect("published");
    let snap = SqliteSnapshot::open_ro(&p).unwrap();
    let m = snap.meta().unwrap();
    assert!(
        m["indexer:go"].starts_with("[policy:tier-word]"),
        "{}",
        m["indexer:go"]
    );
    assert_eq!(m["isolation_detail"], "[policy:tier-word]");
    assert_eq!(m["isolation"], "none", "operational, never scanned");
    assert_eq!(m["commit"], commit);
    assert_eq!(
        m["schema_version"],
        kaban_codegraph::SCHEMA_VERSION.to_string()
    );
    assert_eq!(m["counts:policy_redacted_meta"], "2");
    let head = HeadInput {
        commit: commit.clone(),
        ancestors: Vec::new(),
    };
    let (path, r#ref) = discover(dir.path(), &head).expect("the snapshot is admitted");
    assert_eq!(path, p);
    assert!(r#ref.exact);
    assert_eq!(r#ref.isolation_tier, IsolationTier::None);
}
