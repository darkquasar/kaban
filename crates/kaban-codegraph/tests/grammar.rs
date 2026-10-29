//! G1-S4 grammar slices (stage1-plan CG2.1): the symbol grammar, its canonical round trip and the
//! discriminated `ref` parse, through the crate's public functions only.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

use kaban_codegraph::SymbolError;
use kaban_codegraph::symbol::{self, Descriptor, MAX_SYMBOL_BYTES, Symbol};

/// The owner's example from the spike report (scip-go on afero).
const AFERO_CREATE: &str =
    "scip-go gomod github.com/spf13/afero eb6a92826ea5 `github.com/spf13/afero`/MemMapFs#Create().";

#[test]
fn symbol_grammar_valid_set() {
    // the real shape: four space-separated segments, a backtick-escaped namespace, a type, a method
    let sym = symbol::parse(AFERO_CREATE).expect("afero symbol parses");
    assert_eq!(sym.scheme, "scip-go");
    assert_eq!(sym.manager, "gomod");
    assert_eq!(sym.package, "github.com/spf13/afero");
    assert_eq!(sym.version, "eb6a92826ea5");
    assert_eq!(
        sym.descriptors,
        vec![
            Descriptor::Namespace("github.com/spf13/afero".into()),
            Descriptor::Type("MemMapFs".into()),
            Descriptor::Method {
                name: "Create".into(),
                disambiguator: String::new(),
            },
        ]
    );
    assert_eq!(symbol::name(&sym), "Create");
    assert_eq!(symbol::format(&sym), AFERO_CREATE);

    // every descriptor kind, a disambiguator, an escaped backtick and an escaped space
    let every = "rust-analyzer cargo my  crate 0.1.0 ns/Ty#[T](p)term.meth(over).`we``ird`:mac!";
    let sym = symbol::parse(every).expect("every descriptor kind parses");
    assert_eq!(sym.package, "my crate");
    assert_eq!(
        sym.descriptors,
        vec![
            Descriptor::Namespace("ns".into()),
            Descriptor::Type("Ty".into()),
            Descriptor::TypeParameter("T".into()),
            Descriptor::Parameter("p".into()),
            Descriptor::Term("term".into()),
            Descriptor::Method {
                name: "meth".into(),
                disambiguator: "over".into(),
            },
            Descriptor::Meta("we`ird".into()),
            Descriptor::Macro("mac".into()),
        ]
    );
    assert_eq!(symbol::format(&sym), every);
    assert_eq!(symbol::name(&sym), "mac");

    // the document-local form
    let local = symbol::parse("local 17").expect("local parses");
    assert_eq!(local.scheme, "local");
    assert_eq!(local.descriptors, vec![Descriptor::Local("17".into())]);
    assert_eq!(symbol::format(&local), "local 17");
    assert_eq!(symbol::parent(&local), None);

    // the normalised key of §4.2 and the CONTAINS parent of §4.3
    let key = symbol::normalise_version(AFERO_CREATE).unwrap();
    assert_eq!(
        key,
        "scip-go gomod github.com/spf13/afero . `github.com/spf13/afero`/MemMapFs#Create()."
    );
    assert_eq!(
        symbol::normalise_version(&key).unwrap(),
        key,
        "normalising is idempotent"
    );
    let parent = symbol::parent(&symbol::parse(&key).unwrap()).expect("Create has a parent");
    assert_eq!(
        symbol::format(&parent),
        "scip-go gomod github.com/spf13/afero . `github.com/spf13/afero`/MemMapFs#"
    );
    let top = symbol::parse("s m p v ns/").unwrap();
    assert_eq!(symbol::parent(&top), None, "one descriptor has no parent");

    // exactly 1,024 bytes accepted, 1,025 rejected
    let head = "s m p v ";
    let pad = MAX_SYMBOL_BYTES - head.len() - 1;
    let at_limit = format!("{head}{}/", "a".repeat(pad));
    assert_eq!(at_limit.len(), MAX_SYMBOL_BYTES);
    assert!(symbol::parse(&at_limit).is_ok(), "1,024 bytes accepted");
    let over = format!("{head}{}/", "a".repeat(pad + 1));
    assert_eq!(symbol::parse(&over), Err(SymbolError::Oversize));

    // a hand-built symbol formats as given
    let built = Symbol {
        scheme: "s".into(),
        manager: "m".into(),
        package: "p".into(),
        version: ".".into(),
        descriptors: vec![Descriptor::Term("x".into())],
    };
    assert_eq!(symbol::format(&built), "s m p . x.");
}

/// No byte of the input may appear in an error's text (G1-S4 "no echo").
fn assert_no_echo(input: &str, err: &dyn std::fmt::Display) {
    let text = err.to_string();
    if input.is_empty() {
        return;
    }
    let marker = input
        .split(|c: char| c.is_whitespace() || c == '`')
        .filter(|w| w.len() >= 4)
        .max_by_key(|w| w.len())
        .unwrap_or(input);
    assert!(
        !text.contains(marker) && !text.contains(input),
        "error text {text:?} echoes the input"
    );
}

#[test]
fn symbol_grammar_rejects_control_chars() {
    for (input, class) in [
        ("s m p v ns/\n", SymbolError::ControlCharacter),
        ("s m p v `ns\t`/", SymbolError::ControlCharacter),
        ("s m p v\u{0}ns/", SymbolError::ControlCharacter),
        ("s m p v `n\u{7f}s`/", SymbolError::ControlCharacter),
        ("s m p v `n\u{85}s`/", SymbolError::ControlCharacter),
        ("local 1\r", SymbolError::ControlCharacter),
        // structure
        ("", SymbolError::Malformed),
        ("s m p v", SymbolError::Malformed),
        ("s m p v ", SymbolError::Malformed),
        ("s m p", SymbolError::Malformed),
        ("s m p v ns", SymbolError::Malformed),
        ("s m p v ns/x", SymbolError::Malformed),
        ("s m p v `ns/", SymbolError::Malformed),
        ("s m p v ().", SymbolError::Malformed),
        ("s m p v m(x.", SymbolError::Malformed),
        ("s m p v m()", SymbolError::Malformed),
        ("s m p v [T", SymbolError::Malformed),
        ("s m p v ns%", SymbolError::Malformed),
        ("local ", SymbolError::Malformed),
        ("local a b", SymbolError::Malformed),
        ("local 🧠", SymbolError::Malformed),
        // canonicity: format(parse(s)) would differ
        ("s m p v `plain`/", SymbolError::NonCanonical),
        ("s m p v ünicode/", SymbolError::NonCanonical),
    ] {
        let err = symbol::parse(input).expect_err(input);
        assert_eq!(err, class, "{input:?}");
        assert_no_echo(input, &err);
    }
    let huge = format!("s m p v {}/", "QZQZ".repeat(400));
    let err = symbol::parse(&huge).unwrap_err();
    assert_eq!(err, SymbolError::Oversize);
    assert_no_echo(&huge, &err);
    assert_eq!(
        symbol::normalise_version("s m p v"),
        Err(SymbolError::Malformed)
    );
}

mod roundtrip {
    use super::*;
    use proptest::prelude::*;

    /// A package segment: any non-control characters, non-empty; spaces are escaped by format.
    /// A segment cannot begin with a space (the doubled space after the previous segment's
    /// terminator would read as that segment's escaped space), so none is generated.
    fn segment() -> impl Strategy<Value = String> {
        "[a-zA-Z0-9./:@+_-][a-zA-Z0-9./ :@+_-]{0,11}"
    }

    /// A descriptor identifier: simple (bare) or arbitrary non-control text (backticked), never
    /// empty, never all-simple when it contains an escape-needing character by construction.
    fn identifier() -> impl Strategy<Value = String> {
        prop_oneof![
            4 => "[a-zA-Z0-9_+$-]{1,10}",
            1 => "[a-zA-Z0-9./ `#(){}\u{e9}\u{3b1}]{1,10}".prop_filter("needs escaping", |s| {
                !s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '+' | '-' | '$'))
            }),
        ]
    }

    fn descriptor() -> impl Strategy<Value = Descriptor> {
        prop_oneof![
            identifier().prop_map(Descriptor::Namespace),
            identifier().prop_map(Descriptor::Type),
            identifier().prop_map(Descriptor::Term),
            identifier().prop_map(Descriptor::Meta),
            identifier().prop_map(Descriptor::Macro),
            identifier().prop_map(Descriptor::TypeParameter),
            identifier().prop_map(Descriptor::Parameter),
            (identifier(), prop_oneof![Just(String::new()), identifier()]).prop_map(
                |(name, disambiguator)| Descriptor::Method {
                    name,
                    disambiguator
                }
            ),
        ]
    }

    fn global_symbol() -> impl Strategy<Value = Symbol> {
        (
            segment(),
            segment(),
            segment(),
            segment(),
            prop::collection::vec(descriptor(), 1..6),
        )
            .prop_map(|(scheme, manager, package, version, descriptors)| Symbol {
                scheme,
                manager,
                package,
                version,
                descriptors,
            })
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(512))]

        #[test]
        fn symbol_grammar_roundtrip(sym in global_symbol()) {
            let text = symbol::format(&sym);
            prop_assume!(text.len() <= MAX_SYMBOL_BYTES);
            let parsed = symbol::parse(&text).expect("a formatted symbol parses");
            prop_assert_eq!(&parsed, &sym);
            prop_assert_eq!(symbol::format(&parsed), text.clone());
            let key = symbol::normalise_version(&text).unwrap();
            prop_assert_eq!(symbol::normalise_version(&key).unwrap(), key.clone());
            let keyed = symbol::parse(&key).unwrap();
            prop_assert_eq!(keyed.version, ".");
            prop_assert_eq!(keyed.descriptors, sym.descriptors);
        }

        #[test]
        fn local_symbol_roundtrip(id in "[a-zA-Z0-9_+$-]{1,8}") {
            let text = format!("local {id}");
            let parsed = symbol::parse(&text).unwrap();
            prop_assert_eq!(symbol::format(&parsed), text.clone());
            prop_assert_eq!(symbol::name(&parsed), id.as_str());
        }
    }
}

mod refs {
    use super::*;
    use kaban_codegraph::r#ref::{self, Ref};
    use kaban_codegraph::{PathError, RefError};

    /// Shapes from both fixtures: scip-go rows (the spike's afero snapshot) and the
    /// scip-typescript form.
    const SCIP_REFS: [&str; 3] = [
        "scip:scip-go gomod github.com/spf13/afero eb6a92826ea5 `github.com/spf13/afero`/MemMapFs#Create().",
        "scip:scip-go gomod github.com/golang/go/src go1.26.0 `io/fs`/DirEntry#",
        "scip:scip-typescript npm my-pkg 1.2.3 src/`util.ts`/helper().",
    ];

    #[test]
    fn ref_parse_discriminates() {
        let recorder = symbol::testing::record_inputs();
        for text in SCIP_REFS {
            let parsed = r#ref::parse_ref(text).expect(text);
            assert_eq!(parsed, Ref::Scip(&text["scip:".len()..]));
        }
        assert_eq!(
            r#ref::parse_ref("local:tarfs/fs.go:local 17").unwrap(),
            Ref::Local {
                path: "tarfs/fs.go",
                scip: "local 17"
            }
        );
        // a path with colons: the split is at the last `:` followed by a grammar-valid symbol
        assert_eq!(
            r#ref::parse_ref("local:odd:name.ts:local 3").unwrap(),
            Ref::Local {
                path: "odd:name.ts",
                scip: "local 3"
            }
        );
        // a global symbol after the path (a Meta descriptor carries its own `:`)
        assert_eq!(
            r#ref::parse_ref("local:a/b.go:s m p v x:").unwrap(),
            Ref::Local {
                path: "a/b.go",
                scip: "s m p v x:"
            }
        );
        let seen_before_files = recorder.inputs();
        assert!(
            seen_before_files.iter().all(|s| !s.contains("tarfs/fs.go")),
            "the path part never reaches the grammar: {seen_before_files:?}"
        );

        assert_eq!(
            r#ref::parse_ref("file:tarfs/fs.go").unwrap(),
            Ref::File("tarfs/fs.go")
        );
        assert_eq!(
            r#ref::parse_ref("file:zipfs/zipfs_test.go").unwrap(),
            Ref::File("zipfs/zipfs_test.go")
        );
        let seen = recorder.inputs();
        assert_eq!(
            seen.len(),
            seen_before_files.len(),
            "a file: ref is never fed to the SCIP grammar; parser saw {seen:?}"
        );
        assert!(
            !seen.iter().any(|s| s.contains("zipfs")),
            "parser inputs {seen:?}"
        );
    }

    #[test]
    fn ref_parse_rejects() {
        let cases: [(&str, RefError); 10] = [
            ("overlay:x", RefError::UnknownPrefix),
            ("SCIP:s m p v x/", RefError::UnknownPrefix),
            ("", RefError::UnknownPrefix),
            ("s m p v x/", RefError::UnknownPrefix),
            ("file:/etc/passwd", RefError::Path(PathError::LeadingSlash)),
            ("file:a/../b.go", RefError::Path(PathError::ParentComponent)),
            ("file:a\u{0}b.go", RefError::Path(PathError::Nul)),
            ("file:a\tb.go", RefError::Path(PathError::ControlCharacter)),
            ("file:", RefError::Path(PathError::Empty)),
            ("local:a.go", RefError::Malformed),
        ];
        for (input, class) in cases {
            let err = r#ref::parse_ref(input).expect_err(input);
            assert_eq!(err, class, "{input:?}");
            assert_no_echo(input, &err);
        }
        let oversize = format!("file:{}", "d/".repeat(2049));
        let err = r#ref::parse_ref(&oversize).unwrap_err();
        assert_eq!(err, RefError::Path(PathError::Oversize));
        assert_no_echo(&oversize, &err);
        assert!(r#ref::parse_ref(&format!("file:{}", "d/".repeat(2047))).is_ok());

        let bad_scip = "scip:s m p v ns";
        let err = r#ref::parse_ref(bad_scip).unwrap_err();
        assert_eq!(err, RefError::Symbol(SymbolError::Malformed));
        assert_no_echo(bad_scip, &err);
        let bad_local = "local:a/b.go:local 1 2";
        let err = r#ref::parse_ref(bad_local).unwrap_err();
        assert_eq!(err, RefError::Symbol(SymbolError::Malformed));
        assert_no_echo(bad_local, &err);
        assert_eq!(
            r#ref::parse_ref("local:../b.go:local 1").unwrap_err(),
            RefError::Path(PathError::ParentComponent)
        );
        assert_eq!(
            r#ref::parse_ref("local:a/b.go:s m p v `x\n`/").unwrap_err(),
            RefError::Symbol(SymbolError::ControlCharacter)
        );
        assert!(r#ref::check_repo_relative_path("src/..hidden/x.rs").is_ok());
        assert!(r#ref::check_repo_relative_path("src/./x.rs").is_ok());
        assert_eq!(
            r#ref::check_repo_relative_path(".."),
            Err(PathError::ParentComponent)
        );
        assert_eq!(
            r#ref::check_repo_relative_path("a/.."),
            Err(PathError::ParentComponent)
        );
    }
}
