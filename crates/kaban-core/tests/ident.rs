//! Stable identifier vocabulary and lexical query classification fixtures.
use kaban_core::ident::{QueryClass, classify, ident_terms};

#[test]
fn twenty_identifier_fixtures_have_stable_first_seen_terms() {
    for fixture in [
        include_str!("../fixtures/ident/camel.txt"),
        include_str!("../fixtures/ident/snake.txt"),
        include_str!("../fixtures/ident/qualified.txt"),
        include_str!("../fixtures/ident/mixed.txt"),
    ] {
        for line in fixture.lines() {
            let (input, expected) = line.split_once('\t').unwrap_or((line, ""));
            assert_eq!(ident_terms(input, None, &[], &[], &[]), expected, "{input}");
        }
    }
}

#[test]
fn twenty_queries_classify_by_lexical_shape() {
    for query in [
        "apply_batch",
        "store::read",
        "node.value",
        "path/name",
        "parseFile",
        "SqliteStore",
        "\"two words\"",
        "foo_bar",
        "reader.load",
        "a/b",
    ] {
        assert_eq!(classify(query), QueryClass::Identifier, "{query}");
    }
    for query in [
        "ordinary words",
        "sqlite",
        "HTTP",
        "",
        "123",
        "read file",
        "WHY",
        "café",
        "two-words",
        "archive",
    ] {
        assert_eq!(classify(query), QueryClass::NaturalLanguage, "{query}");
    }
}

#[test]
fn all_inputs_are_deduplicated_and_plain_prose_is_ignored() {
    assert_eq!(
        ident_terms(
            "read_file",
            Some("read_file"),
            &["read_file".into()],
            &["read_file"],
            &["read_file"]
        ),
        "read_file read file"
    );
    assert_eq!(
        ident_terms("ordinary short words 42", None, &[], &[], &[]),
        ""
    );
}

#[test]
fn sentence_periods_and_ellipses_never_make_an_identifier() {
    for query in [
        "ordinary prose.",
        "wait...",
        "it ran... then stopped.",
        "One sentence. Another one.",
        "...",
    ] {
        assert_eq!(classify(query), QueryClass::NaturalLanguage, "{query}");
    }
    for query in [
        "node.value.",
        "SqliteStore::apply_batch.",
        "see config.toml...",
        "v1.2",
    ] {
        assert_eq!(classify(query), QueryClass::Identifier, "{query}");
    }
    for (input, expected) in [
        ("Ordinary prose ends here.", ""),
        ("Wait... then it ran... done.", ""),
        (
            "Call SqliteStore::apply_batch.",
            "SqliteStore::apply_batch SqliteStore Sqlite Store apply_batch apply batch \
             sqlitestore::apply_batch sqlitestore sqlite store",
        ),
        ("Read node.value.", "node.value node value"),
        ("see config.toml...", "config.toml config toml"),
        (
            "then Reader.loadCache... later",
            "Reader.loadCache Reader loadCache load Cache reader.loadcache reader loadcache cache",
        ),
    ] {
        assert_eq!(ident_terms(input, None, &[], &[], &[]), expected, "{input}");
    }
}
