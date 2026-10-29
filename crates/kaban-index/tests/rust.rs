//! Rust consumer regressions over invented and accepted A8 SCIP; no repository execution.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::collections::BTreeSet;

use kaban_codegraph::{CodeGraphReader, EdgeType, SqliteSnapshot};
use kaban_index::RelationshipFlags;
use kaban_index::derive::{derive_edge_type, language_of};
use protobuf::Message;
use scip::types::symbol_information::Kind;

#[test]
fn rust_language_fallback_preserves_explicit_language_and_unknown_extensions() {
    assert_eq!(language_of("src/lib.rs", ""), ("rust".into(), true));
    assert_eq!(language_of("src/lib.rs", "Rust"), ("Rust".into(), false));
    assert_eq!(language_of("Cargo.toml", ""), (String::new(), false));
    assert_eq!(language_of("src/main.go", ""), ("go".into(), true));
}

#[test]
fn rust_method_kinds_form_member_edges_but_fields_do_not() {
    let implementation = RelationshipFlags::from_bits(2);
    for source in [Kind::Method, Kind::StaticMethod, Kind::TraitMethod] {
        for target in [Kind::TraitMethod, Kind::MethodSpecification] {
            assert_eq!(
                derive_edge_type(source, target, implementation),
                Some(EdgeType::ImplementsMember),
            );
        }
        assert_eq!(derive_edge_type(source, Kind::Field, implementation), None);
        assert_eq!(derive_edge_type(Kind::Field, source, implementation), None);
    }
    assert_eq!(
        derive_edge_type(Kind::Struct, Kind::Trait, implementation),
        Some(EdgeType::Implements),
    );
    assert_eq!(
        derive_edge_type(
            Kind::StaticMethod,
            Kind::TraitMethod,
            RelationshipFlags::from_bits(0)
        ),
        None,
    );
}

#[test]
fn rust_trait_members_remain_member_implementations_in_published_snapshots() {
    let policy = common::test_policy();
    let symbol =
        |descriptor: &str| format!("rust-analyzer cargo invented 0.1.0 lib/fixture/{descriptor}");
    let trait_method = symbol("Driver#open().");
    let postgres_method = symbol("Postgres#open().");
    let sqlite_method = symbol("Sqlite#open().");
    let unrelated_method = symbol("Unrelated#open().");
    let mut postgres = common::sym_info_kind(&postgres_method, Kind::Method);
    postgres
        .relationships
        .push(common::implementation(&trait_method));
    let mut sqlite = common::sym_info_kind(&sqlite_method, Kind::StaticMethod);
    sqlite
        .relationships
        .push(common::implementation(&trait_method));
    let mut document = common::document(
        "src/lib.rs",
        vec![
            common::sym_info_kind(&trait_method, Kind::TraitMethod),
            postgres,
            sqlite,
            common::sym_info_kind(&unrelated_method, Kind::Method),
        ],
        vec![
            common::definition(&trait_method, &[0, 3, 7]),
            common::definition(&postgres_method, &[2, 3, 7]),
            common::definition(&sqlite_method, &[4, 3, 7]),
            common::definition(&unrelated_method, &[6, 3, 7]),
        ],
    );
    document.language.clear();
    let bytes = common::bytes(&common::index(vec![document]));
    let dir = common::Scratch::new();
    let path = common::publish(dir.path(), &common::commit(0x72), &bytes, None, &policy);
    let snapshot = SqliteSnapshot::open_ro(&path).unwrap();
    let target = snapshot
        .node(&format!("scip:{trait_method}"))
        .unwrap()
        .unwrap();
    let members = snapshot
        .neighbours(
            target.id,
            kaban_codegraph::Depth::new(1, &kaban_codegraph::Limits::DEFAULT).unwrap(),
            &[EdgeType::ImplementsMember],
        )
        .unwrap();
    let sources: BTreeSet<_> = members.edges.iter().map(|edge| edge.from.clone()).collect();
    assert_eq!(
        sources,
        BTreeSet::from([
            format!("scip:{postgres_method}"),
            format!("scip:{sqlite_method}"),
        ]),
    );
    assert_eq!(snapshot.implementers(target.id, true).unwrap(), []);
    assert!(!sources.contains(format!("scip:{unrelated_method}").as_str()));
}

fn git(repo: &std::path::Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(repo)
        .env("GIT_AUTHOR_NAME", "fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
        .env("GIT_COMMITTER_NAME", "fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(out.status.success());
    String::from_utf8(out.stdout).unwrap()
}

#[test]
fn actual_rust_scip_refuses_bad_source_and_metadata_at_admission() {
    use kaban_core::process::InheritedFds;
    use kaban_index::writer::{Commit, Rows};
    use kaban_index::{IndexError, admit_index, decode_and_gate};

    let scratch = common::Scratch::new();
    let repo = scratch.path();
    git(repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("README"), "trusted fixture").unwrap();
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-q", "-m", "missing Rust sources"]);
    let policy = common::test_policy();
    let bytes = common::fixture_bytes("rust-drivers/minimal/index.scip");
    let gated = decode_and_gate(&bytes, &policy).unwrap();
    let admitted = admit_index(&gated);
    for invalid in [None, Some("\0"), Some("#[test] fn malformed(")] {
        if let Some(source) = invalid {
            std::fs::create_dir_all(repo.join("src")).unwrap();
            std::fs::write(repo.join("src/lib.rs"), source).unwrap();
            git(repo, &["add", "-A"]);
            git(repo, &["commit", "-q", "-m", "invalid committed Rust"]);
        }
        let commit = Commit::parse(git(repo, &["rev-parse", "HEAD"]).trim()).unwrap();
        assert!(matches!(
            Rows::derive_committed_with(
                &admitted,
                None,
                &policy,
                repo,
                &commit,
                InheritedFds::Tolerate,
            ),
            Err(IndexError::SourceClassification)
        ));
    }
    assert!(decode_and_gate(b"\xff", &policy).is_err());
    let mut hostile = scip::types::Index::parse_from_bytes(&bytes).unwrap();
    // Metadata hits are masked and counted (DF-3); structural hits skip their owning row.
    hostile.metadata.as_mut().unwrap().project_root = "file:///CANARY_SYM".into();
    let gated = decode_and_gate(&common::bytes(&hostile), &policy).unwrap();
    let admitted = admit_index(&gated);
    assert!(
        admitted.metadata().project_root().starts_with("[policy:"),
        "{}",
        admitted.metadata().project_root()
    );
    assert_eq!(admitted.counts().policy_redacted_meta, 1);
}

#[test]
fn rust_source_admission_refuses_missing_binary_malformed_oversize_and_deep_inputs() {
    use kaban_core::process::InheritedFds;
    use kaban_index::writer::{Commit, Rows};
    use kaban_index::{IndexError, admit_index, decode_and_gate};

    let scratch = common::Scratch::new();
    let repo = scratch.path();
    git(repo, &["init", "-q", "-b", "main"]);
    let cases = [
        ("binary.rs", vec![0]),
        ("utf8.rs", vec![0xff]),
        ("malformed.rs", b"#[test] fn broken(".to_vec()),
        ("oversize.rs", vec![b' '; 256 * 1024 + 1]),
        (
            "tokens.rs",
            format!("const VALUE: usize = {}0;", "1 + ".repeat(9000)).into_bytes(),
        ),
        (
            "depth.rs",
            format!("fn deep() {{ {}0{}; }}", "(".repeat(80), ")".repeat(80)).into_bytes(),
        ),
        (
            "tokenizer-depth.rs",
            format!("{}0{}", "(".repeat(32_768), ")".repeat(32_768)).into_bytes(),
        ),
        (
            "redacted.rs",
            b"// REDACT_ME\n#[test] fn check() {}".to_vec(),
        ),
        ("clean.rs", b"pub fn fine() {}".to_vec()),
        (
            "clean-shebang-only.rs",
            b"#!/usr/bin/env rust-script".to_vec(),
        ),
        (
            "clean-inner-attribute.rs",
            "\u{feff}#! /* ordinary /* nested */ comment */ \n [cfg(test)]\nfn fine() {}"
                .as_bytes()
                .to_vec(),
        ),
        (
            "shebang-redacted.rs",
            b"#!/usr/bin/env REDACT_ME\nfn fine() {}".to_vec(),
        ),
        (
            "bom-redacted.rs",
            "\u{feff}// REDACT_ME\nfn fine() {}".as_bytes().to_vec(),
        ),
        (
            "shebang-oversize.rs",
            format!("#!{}\nfn fine() {{}}", "x".repeat(256 * 1024)).into_bytes(),
        ),
        (
            "shebang-raw-delimiters.rs",
            format!("#!{}\nfn fine() {{}}", "(".repeat(1025)).into_bytes(),
        ),
        (
            "prefix-tokens.rs",
            format!(
                "\u{feff}#!/usr/bin/env rust-script\nconst VALUE: usize = {}0;",
                "1 + ".repeat(9000)
            )
            .into_bytes(),
        ),
        (
            "malformed-inner-attribute.rs",
            b"#![cfg(test)\nfn fine() {}".to_vec(),
        ),
        // DF-3: over the classifier's per-file bound (16384 raw open delimiters, 131072
        // tokens): classified by the path only, counted, never a refusal
        (
            "over-delimiters.rs",
            format!(
                "// {}\n#[test] fn looks_like_a_test() {{}}",
                "(".repeat(16_385)
            )
            .into_bytes(),
        ),
        ("tests/over-tokens.rs", "+".repeat(131_073).into_bytes()),
    ];
    std::fs::create_dir_all(repo.join("tests")).unwrap();
    for (path, bytes) in &cases {
        std::fs::write(repo.join(path), bytes).unwrap();
    }
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-q", "-m", "source admission fixtures"]);
    let commit = Commit::parse(git(repo, &["rev-parse", "HEAD"]).trim()).unwrap();
    // A working-tree-only path must never satisfy a document's source requirement.
    std::fs::write(repo.join("missing.rs"), "pub fn working_tree_only() {}").unwrap();
    let policy = common::test_policy();
    for path in cases.iter().map(|(path, _)| *path).chain(["missing.rs"]) {
        let mut doc = common::document(path, vec![], vec![]);
        doc.language = "rust".into();
        let input = common::bytes(&common::index(vec![doc]));
        let gated = decode_and_gate(&input, &policy).unwrap();
        let admitted = admit_index(&gated);
        let result = Rows::derive_committed_with(
            &admitted,
            None,
            &policy,
            repo,
            &commit,
            InheritedFds::Tolerate,
        );
        match path {
            "clean.rs"
            | "clean-shebang-only.rs"
            | "tokens.rs"
            | "prefix-tokens.rs"
            | "shebang-raw-delimiters.rs" => {
                let rows = result.unwrap_or_else(|e| panic!("{path}: within the bounds: {e}"));
                assert_eq!(rows.counts().source_classified_by_path, 0, "{path}");
            }
            "over-delimiters.rs" | "tokenizer-depth.rs" => {
                let rows = result.unwrap_or_else(|e| panic!("{path}: path-only, not refused: {e}"));
                assert_eq!(rows.counts().source_classified_by_path, 1, "{path}");
                assert!(!rows.files()[0].is_test, "{path}: a src path is production");
            }
            "tests/over-tokens.rs" => {
                let rows = result.unwrap_or_else(|e| panic!("{path}: path-only, not refused: {e}"));
                assert_eq!(rows.counts().source_classified_by_path, 1, "{path}");
                assert!(rows.files()[0].is_test, "{path}: a tests/ path is a test");
            }
            "clean-inner-attribute.rs" => assert!(
                result.unwrap().files()[0].is_test,
                "#! followed by trivia and [ is an inner attribute, never a shebang"
            ),
            "redacted.rs" | "shebang-redacted.rs" | "bom-redacted.rs" => {
                // DF-3: a policy hit skips the file's in-file classification (redaction would
                // shift the spans) and counts; the run goes on
                let rows = result.unwrap_or_else(|e| panic!("{path}: published, not refused: {e}"));
                assert_eq!(rows.counts().policy_skipped_source, 1, "{path}");
                assert!(!rows.policy_skipped_source_rules().is_empty(), "{path}");
            }
            _ => assert!(
                matches!(result, Err(IndexError::SourceClassification)),
                "source boundary: {path}"
            ),
        }
    }
}

#[test]
fn rust_test_paths_require_rust_sources_and_do_not_mark_production_filenames() {
    use kaban_index::derive::is_test_file;
    assert!(is_test_file("tests/integration.rs", "rust"));
    assert!(is_test_file("packages/p/tests/support/mod.rs", "Rust"));
    for path in [
        "src/lib.rs",
        "src/tests.rs",
        "contest/lib.rs",
        "tests/readme.md",
    ] {
        assert!(!is_test_file(path, "rust"), "{path}");
    }
    assert!(!is_test_file("tests/integration.rs", "go"));
    assert!(is_test_file("legacy_test.go", "go"));
}

#[test]
fn rust_file_prefixes_preserve_original_unicode_test_and_reference_coordinates() {
    use kaban_core::process::InheritedFds;
    use kaban_index::writer::{self, Commit, Rows};
    use kaban_index::{admit_index, decode_and_gate};

    for prefix in [
        "\u{feff}",
        "#!/usr/bin/env rust-script 🦀\n",
        "\u{feff}#!/usr/bin/env rust-script\r\n",
    ] {
        let scratch = common::Scratch::new();
        let repo = scratch.path();
        git(repo, &["init", "-q", "-b", "main"]);
        let source = format!(
            "{prefix}const É: &str = \"🦀\"; #[test] fn café() {{ target(); }}\npub fn target() {{}}\npub fn production() {{ target(); }}\n#[cfg(test)] use crate::target as test_target;\n"
        );
        std::fs::write(repo.join("lib.rs"), &source).unwrap();
        git(repo, &["add", "-A"]);
        git(repo, &["commit", "-q", "-m", "prefix fixture"]);
        let commit = Commit::parse(git(repo, &["rev-parse", "HEAD"]).trim()).unwrap();
        let target = "kaban-rust-scip cargo invented 0.1.0 lib/target().";
        let test = "kaban-rust-scip cargo invented 0.1.0 lib/`café`().";
        let production = "kaban-rust-scip cargo invented 0.1.0 lib/production().";
        let mut definitions = Vec::new();
        let mut occurrences = Vec::new();
        let coordinate = |v| i32::try_from(v).unwrap();
        for (line, text) in source.lines().enumerate() {
            for (name, symbol) in [
                ("café", test),
                ("target", target),
                ("production", production),
            ] {
                if let Some(col) = text.find(&format!("fn {name}(")) {
                    let col = col + 3;
                    let mut def = common::definition(
                        symbol,
                        &[
                            coordinate(line),
                            coordinate(col),
                            coordinate(col + name.len()),
                        ],
                    );
                    def.enclosing_range = vec![coordinate(line), 0, coordinate(text.len())];
                    definitions.push(common::sym_info_kind(symbol, Kind::Function));
                    occurrences.push(def);
                }
            }
            if let Some(col) = text
                .find("target();")
                .or_else(|| text.find("::target").map(|c| c + 2))
            {
                occurrences.push(common::occurrence(
                    target,
                    &[coordinate(line), coordinate(col), coordinate(col + 6)],
                ));
            }
        }
        let mut doc = common::document("lib.rs", definitions, occurrences);
        doc.language = "rust".into();
        let input = common::bytes(&common::index(vec![doc]));
        let policy = common::test_policy();
        let gated = decode_and_gate(&input, &policy).unwrap();
        let admitted = admit_index(&gated);
        let rows = Rows::derive_committed_with(
            &admitted,
            None,
            &policy,
            repo,
            &commit,
            InheritedFds::Tolerate,
        );
        assert!(
            rows.is_ok(),
            "valid BOM/shebang source must be admitted: {prefix:?}"
        );
        let rows = rows.unwrap();
        let test_row = rows
            .symbols()
            .iter()
            .find(|s| s.scip.as_deref() == Some(test))
            .unwrap();
        assert!(
            test_row.is_test,
            "Unicode test after prefix must be classified"
        );
        assert!(!rows.files()[0].is_test);
        let meta = common::meta(commit.as_str());
        let path = writer::publish(repo, &rows, &meta, &policy).unwrap();
        let snapshot = SqliteSnapshot::open_ro(&path).unwrap();
        let target = snapshot.node(&format!("scip:{target}")).unwrap().unwrap();
        let production_refs = snapshot.references(target.id, false).unwrap();
        assert_eq!(
            production_refs.iter().map(|g| g.sites.len()).sum::<usize>(),
            1
        );
        let all = snapshot.references(target.id, true).unwrap();
        assert_eq!(all.iter().map(|g| g.sites.len()).sum::<usize>(), 3);
        for site in all.iter().flat_map(|g| &g.sites) {
            let line = source
                .lines()
                .nth(usize::try_from(site.site.sl).unwrap())
                .unwrap();
            assert_eq!(
                &line[usize::try_from(site.site.sc).unwrap()
                    ..usize::try_from(site.site.ec).unwrap()],
                "target"
            );
        }
    }
}
