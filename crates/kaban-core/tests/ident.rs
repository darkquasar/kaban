//! Stable identifier vocabulary and lexical query classification fixtures.
use kaban_core::ident::{QueryClass, classify, split_terms};

#[test]
fn twenty_identifier_fixtures_preserve_the_original_spelling_of_the_parts() {
    for (fixture, expected) in [
        (
            include_str!("../fixtures/ident/camel.txt"),
            [
                "Sqlite Store",
                "Archive Reader",
                "parse File",
                "load Cache",
                "HTTP Server",
            ],
        ),
        (
            include_str!("../fixtures/ident/snake.txt"),
            [
                "apply batch",
                "read file",
                "node anchor",
                "index meta",
                "query class",
            ],
        ),
        (
            include_str!("../fixtures/ident/qualified.txt"),
            [
                "store read",
                "core parse",
                "node value",
                "db table",
                "path name",
            ],
        ),
        (
            include_str!("../fixtures/ident/mixed.txt"),
            [
                "Store read file",
                "Reader load Cache",
                "path Archive Reader",
                "foo bar load Cache",
                "Sqlite Store apply batch",
            ],
        ),
    ] {
        assert_eq!(fixture.lines().count(), expected.len());
        for (line, parts) in fixture.lines().zip(expected) {
            let (input, _) = line.split_once('\t').unwrap_or((line, ""));
            assert_eq!(
                split_terms(input),
                parts.split_whitespace().collect::<Vec<_>>(),
                "{input}"
            );
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
fn identifier_parts_follow_case_acronym_digit_and_unicode_boundaries() {
    assert_eq!(
        split_terms("ÜberHTTP2Adapter::parse_XML"),
        ["Über", "HTTP2", "Adapter", "parse", "XML"]
    );
    assert_eq!(split_terms("a 2 x"), ["a", "2", "x"]);
    assert_eq!(split_terms("_::—"), [] as [&str; 0]);
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
    assert_eq!(
        split_terms("see Reader.loadCache..."),
        ["see", "Reader", "load", "Cache"]
    );
}
