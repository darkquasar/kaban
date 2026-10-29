//! Ontology meta-schema, file form, name resolution, inheritance and conflicts (engine-spec
//! §3.1, EN-F1, EN-F7, EN-S11; engine-plan E1.2). Fixtures under `fixtures/ontology/`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

use kaban_core::ontology::{Index, Stamp};
use kaban_core::{
    NamespaceSet, Ns, ONTOLOGY_ERROR_TEXTS, Ontology, OntologyError, OntologyFile, Qualified,
    resolve, split_stored, stored_name,
};

fn fixture_text(name: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/ontology")
        .join(name);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn file(name: &str) -> OntologyFile {
    OntologyFile::from_yaml(&fixture_text(name)).unwrap_or_else(|e| panic!("{name}: {e} {e:?}"))
}

fn load(names: &[&str]) -> Result<Ontology, OntologyError> {
    let files: Vec<OntologyFile> = names.iter().map(|n| file(n)).collect();
    resolve(&files, &NamespaceSet::new())
}

fn ns(s: &str) -> Ns {
    Ns::parse(s).unwrap()
}

/// `(ns, kind, name)` as the store will hold them.
fn rows(o: &Ontology) -> BTreeSet<(String, &'static str, String)> {
    o.type_rows()
        .into_iter()
        .map(|(ns, kind, name)| (ns.to_string(), kind, name))
        .collect()
}

#[test]
fn ontology_fixture_yaml_round_trips_and_rejects_malformed_documents() {
    for name in [
        "kaban-core.yaml",
        "adr.yaml",
        "incidents.yaml",
        "runbooks.yaml",
    ] {
        let text = fixture_text(name);
        let before = OntologyFile::from_yaml(&text).unwrap();
        let value: yaml_serde::Value = yaml_serde::from_str(&text).unwrap();
        let rendered = yaml_serde::to_string(&value).unwrap();
        let after = OntologyFile::from_yaml(&rendered).unwrap();
        assert_eq!(before.ontology, after.ontology, "{name}");
        assert_eq!(before.version, after.version, "{name}");
        assert_eq!(before.kinds, after.kinds, "{name}");
        assert_eq!(before.rels, after.rels, "{name}");
        let reparsed: yaml_serde::Value = yaml_serde::from_str(&rendered).unwrap();
        assert_eq!(reparsed, value, "{name}");
    }
    for malformed in [
        "ontology: [unfinished",
        "ontology: adr\nversion: 1\nkinds: *undefined\nrels: {}\n",
        "ontology: adr\nversion: 1\nkinds: {}\nrels: {}\n---\n{}\n",
    ] {
        assert!(OntologyFile::from_yaml(malformed).is_err());
    }
}

#[test]
fn the_shipped_fixtures_reproduce_todays_schema_type_seed_rows_by_ns_kind_name() {
    // `schema.rs` SEED_LABELS (14) and SEED_RELS (16) at the E1.1 base, split by engine-spec
    // §3.1 into the `core` and `adr` namespaces
    let o = load(&["kaban-core.yaml", "adr.yaml"]).unwrap();
    let mut expected = BTreeSet::new();
    for k in ["Document", "Section", "Chunk", "Entity", "CodeRef", "Tag"] {
        expected.insert(("core".to_owned(), "label", k.to_owned()));
    }
    for r in [
        "MENTIONS",
        "ABOUT",
        "EVIDENCE_FOR",
        "DERIVED_FROM",
        "PART_OF",
        "NEXT",
        "LINKS_TO",
        "TAGGED",
        "RELATES_TO",
    ] {
        expected.insert(("core".to_owned(), "rel", r.to_owned()));
    }
    for k in [
        "Fact",
        "Idea",
        "Insight",
        "Improvement",
        "Decision",
        "Convention",
        "Failure",
        "Episode",
    ] {
        expected.insert(("adr".to_owned(), "label", k.to_owned()));
    }
    for r in [
        "SUPERSEDES",
        "CONTRADICTS",
        "INVALIDATES",
        "DEPENDS_ON",
        "CAUSED_BY",
        "FIXED_BY",
        "GOVERNS",
    ] {
        expected.insert(("adr".to_owned(), "rel", r.to_owned()));
    }
    assert_eq!(rows(&o), expected);
    assert_eq!(expected.iter().filter(|r| r.1 == "label").count(), 14);
    assert_eq!(expected.iter().filter(|r| r.1 == "rel").count(), 16);
    // every stored name of the two shipped namespaces is bare
    for q in o.kinds.keys().chain(o.rels.keys()) {
        assert_eq!(stored_name(q), q.name, "{q}");
    }
    assert_eq!(o.namespaces, vec![ns("core"), ns("adr")]);
}

fn load_err(name: &str) -> OntologyError {
    match OntologyFile::from_yaml(&fixture_text(name)) {
        Err(e) => e,
        Ok(f) => resolve(&[f], &NamespaceSet::new())
            .err()
            .unwrap_or_else(|| panic!("{name}: loaded")),
    }
}

#[test]
fn the_meta_schema_is_itself_in_the_validator_subset() {
    let value: serde_json::Value =
        yaml_serde::from_str(kaban_core::ontology::meta::META_SCHEMA_YAML).unwrap();
    kaban_core::Schema::parse(&value, &kaban_core::ValidatorLimits::default()).unwrap();
    assert_eq!(
        value["required"],
        serde_json::json!(["ontology", "version", "kinds", "rels"])
    );
}

#[test]
fn an_unknown_key_at_any_depth_fails_with_the_field_path_and_never_the_value() {
    let cases = [
        ("neg_unknown_key_top.yaml", "schema_invalid:file:kind"),
        (
            "neg_unknown_key_depth2.yaml",
            "schema_invalid:file:kinds.Thing.colour",
        ),
        (
            "neg_unknown_key_depth3.yaml",
            "schema_invalid:file:kinds.Thing.rank.boostt",
        ),
        (
            "neg_unknown_key_governed.yaml",
            "schema_invalid:file:kinds.Thing.governed.typo",
        ),
        // the `serialize` mapping is closed too (E1.3a RE1-3)
        (
            "neg_unknown_key_serialize.yaml",
            "schema_invalid:file:kinds.Thing.governed.serialize.typo",
        ),
        ("neg_wrong_type_version.yaml", "schema_invalid:file:version"),
    ];
    for (name, text) in cases {
        let err = load_err(name);
        assert_eq!(err.to_string(), text, "{name}");
        assert!(!format!("{err:?}").contains("red"), "{name}: value leaked");
    }
    // a document that is not a mapping, and a missing required block
    assert_eq!(
        OntologyFile::from_yaml("- a\n- b\n")
            .unwrap_err()
            .to_string(),
        "schema_invalid:file:"
    );
    assert_eq!(
        OntologyFile::from_yaml("ontology: shop\nversion: 1\nkinds: {}\n")
            .unwrap_err()
            .to_string(),
        "schema_invalid:file:rels"
    );
    // a `serialize` mapping missing its `format` is located as well (the hand-written parse
    // forwards the inner field name)
    assert_eq!(
        load_err_text(
            "ontology: shop\nversion: 1\nkinds: {Thing: {description: t, governed: {acceptance: human, serialize: {dir: things}, export: {digest: false, marker: false}, display: {label: T, plural: Ts}, governs_paths: false, dedup: none}}}\nrels: {}\n"
        ),
        "schema_invalid:file:kinds.Thing.governed.serialize.format"
    );
    // a kind without its description fails at resolution with the same text family
    assert_eq!(
        load_err_text("ontology: shop\nversion: 1\nkinds: {Thing: {}}\nrels: {}\n"),
        "schema_invalid:file:kinds.Thing.description"
    );
}

fn load_err_text(yaml: &str) -> String {
    let err = match OntologyFile::from_yaml(yaml) {
        Err(e) => e,
        Ok(f) => resolve(&[f], &NamespaceSet::new()).unwrap_err(),
    };
    err.to_string()
}

#[test]
fn names_failing_a_regex_fail_invalid_identifier() {
    // the payload is the path of the field (E1.3a residual: a fixed class plus a path, never
    // the offending name as a value; a map key is its own path element)
    assert_eq!(
        load_err("neg_lowercase_kind.yaml"),
        OntologyError::InvalidIdentifier {
            path: "kinds.vendor".to_owned()
        }
    );
    let err = load_err("neg_bad_namespace.yaml");
    assert_eq!(
        err,
        OntologyError::InvalidIdentifier {
            path: "ontology".to_owned()
        }
    );
    assert!(!format!("{err:?}").contains('X'), "{err:?}");
    // `x` is reserved for discovered kinds
    assert_eq!(
        load_err("neg_reserved_namespace.yaml"),
        OntologyError::InvalidIdentifier {
            path: "ontology".to_owned()
        }
    );
    // a reference's namespace prefix fails at the reference's path
    let err = OntologyFile::from_yaml(
        "ontology: shop\nversion: 1\nkinds: {T: {description: t, is_a: \"Bad_NS:Thing\"}}\nrels: {}\n",
    )
    .map_err(|e| e.to_string())
    .and_then(|f| resolve(&[f], &NamespaceSet::new()).map_err(|e| format!("{e:?}")))
    .unwrap_err();
    assert_eq!(err, "InvalidIdentifier { path: \"kinds.T.is_a\" }");
    // rel names: upper snake only; a 65th character is one too many
    let long = "A".repeat(65);
    assert_eq!(
        load_err_text(&format!(
            "ontology: shop\nversion: 1\nkinds: {{}}\nrels: {{{long}: {{from: [], to: [], directed: true}}}}\n"
        )),
        "invalid_identifier"
    );
    assert_eq!(
        load_err_text(
            "ontology: shop\nversion: 1\nkinds: {}\nrels: {Lower_Case: {from: [], to: [], directed: true}}\n"
        ),
        "invalid_identifier"
    );
    assert!(Ns::parse("a-b-9").is_ok());
    assert!(Ns::parse("a_b").is_err());
    assert!(kaban_core::KindName::parse("A1b").is_ok());
    assert!(kaban_core::RelName::parse("A_1").is_ok());
    assert!(kaban_core::RelName::parse("A_b").is_err());
}

/// Engine-spec §3.1 admission ranges (E1.3a RE1-4): `version >= 1`, `rank.expand_depth <= 4`,
/// question `depth <= 3`, `priors`, `min_confidence` and `confidence_required` in `[0, 1]` fail
/// `schema_invalid:range:<path>` at resolution; `group_by` is a closed vocabulary and `where`
/// a mapping, refused by the file form with the field path. No text carries the value.
#[test]
fn values_outside_the_admission_ranges_fail_with_the_field_path_and_never_the_value() {
    let cases = [
        ("neg_range_version.yaml", "schema_invalid:range:version"),
        (
            "neg_range_expand_depth.yaml",
            "schema_invalid:range:kinds.Thing.rank.expand_depth",
        ),
        (
            "neg_range_question_depth.yaml",
            "schema_invalid:range:questions.recent.depth",
        ),
        (
            "neg_range_prior.yaml",
            "schema_invalid:range:priors.gazetteer.id.exact",
        ),
        (
            "neg_range_min_confidence.yaml",
            "schema_invalid:range:kinds.Thing.min_confidence",
        ),
        (
            "neg_range_confidence_required.yaml",
            "schema_invalid:range:rels.R.confidence_required",
        ),
        (
            "neg_question_group_by.yaml",
            "schema_invalid:file:questions.recent.group_by",
        ),
        (
            "neg_question_where_scalar.yaml",
            "schema_invalid:file:questions.recent.where",
        ),
    ];
    for (name, text) in cases {
        let err = load_err(name);
        assert_eq!(err.to_string(), text, "{name}");
        let debug = format!("{err:?}");
        for value in ["1.5", "-0.1", "colour", "\"red\"", "version: 0"] {
            assert!(!debug.contains(value), "{name}: value leaked: {debug}");
        }
    }
    // the boundaries themselves are admitted, and the typed question keeps only validated
    // shapes
    let o = resolve(
        &[OntologyFile::from_yaml(
            "ontology: shop\nversion: 1\nkinds:\n  Thing: {description: t, rank: {expand_depth: 4}, min_confidence: 1}\nrels: {}\npriors: {a.b.c: 0}\nquestions:\n  recent: {kinds: [Thing], where: {}, order: [currency], depth: 3, group_by: dup_key}\n",
        )
        .unwrap()],
        &NamespaceSet::new(),
    )
    .unwrap();
    let question = &o.questions[&q("shop", "recent")];
    assert_eq!(question.depth, Some(3));
    assert_eq!(
        question.group_by,
        Some(kaban_core::ontology::meta::GroupBy::DupKey)
    );
    assert!(question.where_.is_empty());
}

fn q(ns_: &str, name: &str) -> Qualified {
    Qualified {
        ns: ns(ns_),
        name: name.to_owned(),
    }
}

#[test]
fn two_namespaces_may_declare_vendor_and_a_bare_vendor_from_a_third_file_is_ambiguous() {
    let o = load(&["infra.yaml", "vendors.yaml"]).unwrap();
    assert!(o.kinds.contains_key(&q("infra", "Vendor")));
    assert!(o.kinds.contains_key(&q("vendors", "Vendor")));
    assert_eq!(stored_name(&q("infra", "Vendor")), "infra:Vendor");
    assert_eq!(
        load(&["infra.yaml", "vendors.yaml", "third_bare_vendor.yaml"]).unwrap_err(),
        OntologyError::AmbiguousKind {
            path: "kinds.Contract.is_a".to_owned()
        }
    );
    // qualified, it resolves; the same mixin twice applies once; inherited properties and
    // hints merge with the kind's own
    let o = load(&["infra.yaml", "vendors.yaml", "third_qualified_vendor.yaml"]).unwrap();
    let contract = &o.kinds[&q("contracts", "Contract")];
    assert_eq!(contract.is_a, Some(q("infra", "Vendor")));
    assert_eq!(contract.mixins, vec![q("contracts", "Dated")]);
    let schema = contract.props_schema.as_value();
    assert_eq!(schema["type"], "object");
    assert_eq!(schema["additionalProperties"], false);
    assert_eq!(schema["required"], serde_json::json!(["region"]));
    let props = schema["properties"].as_object().unwrap();
    let mut names: Vec<&str> = props.keys().map(String::as_str).collect();
    names.sort_unstable();
    assert_eq!(names, ["counterparty", "region", "signed_at"]);
    assert_eq!(
        contract.fts_fields,
        vec!["props.counterparty".to_owned(), "props.region".to_owned()]
    );
    let rel = &o.rels[&q("contracts", "SIGNED_WITH")];
    assert_eq!(
        rel.to,
        vec![kaban_core::ontology::resolve::RelEnd::Kind(q(
            "vendors", "Vendor"
        ))]
    );
    // the merged schema validates like any E1.1 schema
    let errors = kaban_core::validate(&contract.props_schema, &serde_json::json!({"signed_at": 1}));
    assert_eq!(
        kaban_core::first_error_text(&errors).as_deref(),
        Some("invalid_props props.region")
    );
}

#[test]
fn the_conflict_rules_fail_ontology_conflict_with_the_offending_qualified_name() {
    let cases = [
        ("neg_cycle.yaml", q("shop", "Alpha"), "is_a_cycle"),
        ("neg_retype.yaml", q("shop", "Child"), "retype:title"),
        ("neg_mixin_is_a.yaml", q("shop", "Bad"), "mixin_is_a"),
        ("neg_two_mixins.yaml", q("shop", "Thing"), "mixin_key:rank"),
        // conflicts are by declaration presence (E1.3a RE1-5): `description` counts, and an
        // explicit empty list is a declaration
        (
            "neg_two_mixins_description.yaml",
            q("shop", "Thing"),
            "mixin_key:description",
        ),
        (
            "neg_two_mixins_empty_fts.yaml",
            q("shop", "Thing"),
            "mixin_key:fts_fields",
        ),
    ];
    for (file_name, name, detail) in cases {
        let err = load_err(file_name);
        assert_eq!(err.to_string(), "ontology_conflict", "{file_name}");
        assert_eq!(
            err,
            OntologyError::Conflict {
                name,
                detail: detail.to_owned()
            },
            "{file_name}"
        );
    }
    // the same body twice is one declaration; a different body is a conflict
    let o = load(&["infra.yaml", "dup_same_body.yaml"]).unwrap();
    assert_eq!(o.kinds.len(), 1);
    assert_eq!(
        load(&["infra.yaml", "dup_diff_body.yaml"]).unwrap_err(),
        OntologyError::Conflict {
            name: q("infra", "Vendor"),
            detail: "duplicate".to_owned()
        }
    );
}

#[test]
fn nothing_outside_the_loaded_files_is_ever_resolved() {
    // extends a namespace that is neither in the files nor loaded
    assert_eq!(
        load_err("neg_extends_missing.yaml"),
        OntologyError::UnknownNamespace {
            path: "extends".to_owned()
        }
    );
    // the same file with the namespace known as loaded passes the visibility check
    let f = file("neg_extends_missing.yaml");
    let loaded: NamespaceSet = [ns("missing")].into_iter().collect();
    assert!(resolve(&[f], &loaded).is_ok());
    // `$ref` anywhere in a props_schema
    assert_eq!(
        load_err("neg_ref.yaml"),
        OntologyError::Unsupported {
            keyword: "$ref".to_owned()
        }
    );
    assert_eq!(
        load_err("neg_ref.yaml").to_string(),
        "schema_unsupported:$ref"
    );
    // prefixes are opaque strings
    let o = load(&["prefixes_not_url.yaml"]).unwrap();
    assert!(o.kinds.contains_key(&q("shop", "Thing")));
    let f = file("prefixes_not_url.yaml");
    assert_eq!(f.prefixes[&ns("shop")], "not-a-url");
    assert_eq!(f.prefixes[&ns("ex")], "::also not::");
    // an unknown kind and rel reference: the fixed text, the reference's path in the payload
    let unknown_kind = OntologyFile::from_yaml(
        "ontology: shop\nversion: 1\nkinds: {}\nrels: {R: {from: [Nope], to: [], directed: true}}\n",
    )
    .map_err(|e| e.to_string())
    .and_then(|f| resolve(&[f], &NamespaceSet::new()).map_err(|e| format!("{e} {e:?}")))
    .unwrap_err();
    assert_eq!(
        unknown_kind,
        "unknown_kind UnknownKind { path: \"rels.R.from\" }"
    );
    let unknown_rel = OntologyFile::from_yaml(
        "ontology: shop\nversion: 1\nkinds: {T: {description: t}}\nrels: {}\nquestions: {q: {kinds: [T], where: {}, exclude_via: NOPE, order: []}}\n",
    )
    .map_err(|e| e.to_string())
    .and_then(|f| resolve(&[f], &NamespaceSet::new()).map_err(|e| format!("{e} {e:?}")))
    .unwrap_err();
    assert_eq!(
        unknown_rel,
        "unknown_rel UnknownRel { path: \"questions.q.exclude_via\" }"
    );
}

#[test]
fn stored_name_and_split_stored_round_trip_for_every_fixture_kind_and_rel() {
    let o = load(&[
        "kaban-core.yaml",
        "adr.yaml",
        "infra.yaml",
        "vendors.yaml",
        "third_qualified_vendor.yaml",
    ])
    .unwrap();
    let mut seen_qualified = 0;
    for id in o.kinds.keys().chain(o.rels.keys()) {
        let stored = stored_name(id);
        let (split_ns, name) = split_stored(&stored);
        assert_eq!(name, id.name, "{id}");
        let bare = ["core", "adr", "user"].contains(&id.ns.as_str());
        if bare {
            assert_eq!(split_ns, None, "{id}");
            assert_eq!(stored, id.name);
        } else {
            assert_eq!(split_ns.as_ref(), Some(&id.ns), "{id}");
            assert_eq!(stored, format!("{}:{}", id.ns, id.name));
            seen_qualified += 1;
        }
    }
    assert!(seen_qualified >= 4);
    assert_eq!(
        split_stored("incidents:Decision"),
        (Some(ns("incidents")), "Decision")
    );
    assert_eq!(split_stored("Decision"), (None, "Decision"));
    // a prefix that is not a namespace stays part of the name
    assert_eq!(split_stored("Bad:X"), (None, "Bad:X"));
    assert_eq!(stored_name(&q("user", "Widget")), "Widget");
}

#[test]
fn every_error_text_is_in_the_en_f7_list() {
    let samples: Vec<OntologyError> = vec![
        OntologyError::Conflict {
            name: q("a", "B"),
            detail: "duplicate".to_owned(),
        },
        OntologyError::InvalidIdentifier {
            path: "ontology".to_owned(),
        },
        OntologyError::OutOfRange {
            path: "version".to_owned(),
        },
        OntologyError::Unsupported {
            keyword: "$ref".to_owned(),
        },
        OntologyError::SchemaInvalid {
            rule: "items_required".to_owned(),
        },
        OntologyError::SchemaInvalid {
            rule: "file:kinds.X.colour".to_owned(),
        },
        OntologyError::UnknownNamespace {
            path: "extends".to_owned(),
        },
        OntologyError::AmbiguousKind {
            path: "kinds.Contract.is_a".to_owned(),
        },
        OntologyError::UnknownKind {
            path: "rels.R.from".to_owned(),
        },
        OntologyError::UnknownRel {
            path: "questions.q.exclude_via".to_owned(),
        },
        OntologyError::BuiltinImmutable(q("adr", "Decision")),
        OntologyError::DiscoveredCapReached,
    ];
    assert_eq!(ONTOLOGY_ERROR_TEXTS.len(), 10);
    let mut covered = BTreeSet::new();
    for err in &samples {
        let text = err.to_string();
        let entry = ONTOLOGY_ERROR_TEXTS
            .iter()
            .find(|t| match t.split_once(":<") {
                Some((prefix, _)) => text.starts_with(&format!("{prefix}:")),
                None => **t == text,
            })
            .unwrap_or_else(|| panic!("{text} is not an EN-F7 text"));
        covered.insert(*entry);
        // the fixed text never carries the offending name
        assert!(
            !text.contains("Vendor") && !text.contains("missing"),
            "{text}"
        );
    }
    assert_eq!(covered.len(), ONTOLOGY_ERROR_TEXTS.len());
    // the errors the fixtures produce are all covered too
    for name in [
        "neg_cycle.yaml",
        "neg_lowercase_kind.yaml",
        "neg_ref.yaml",
        "neg_extends_missing.yaml",
        "neg_unknown_key_depth3.yaml",
    ] {
        let text = load_err(name).to_string();
        assert!(
            ONTOLOGY_ERROR_TEXTS
                .iter()
                .any(|t| match t.split_once(":<") {
                    Some((prefix, _)) => text.starts_with(&format!("{prefix}:")),
                    None => *t == text,
                }),
            "{name}: {text}"
        );
    }
}

/// E1.7a RE2-3 (Major): `serialize.dir` is one relative path component under
/// `.kaban/<ontology>/` (`^[a-z][a-z0-9-]{0,31}$`, the namespace shape): an absolute path, a
/// parent traversal, a nested path, `.`, an empty string and an uppercase name all fail to
/// load with the fixed class `invalid_identifier` at the field's path; the typed value is a
/// validated newtype, so a loaded ontology cannot name a directory outside the tree.
#[test]
fn a_serialize_dir_that_is_not_one_relative_component_fails_invalid_identifier() {
    for name in [
        "neg_dir_absolute.yaml",
        "neg_dir_parent.yaml",
        "neg_dir_nested.yaml",
    ] {
        let err = load_err(name);
        assert_eq!(
            err,
            OntologyError::InvalidIdentifier {
                path: "kinds.Thing.governed.serialize.dir".to_owned()
            },
            "{name}"
        );
        assert!(!format!("{err:?}").contains("tmp"), "{name}: {err:?}");
    }
    for dir in [".", "..", "", "Things", "a b", "x\\y"] {
        let text = format!(
            "ontology: shop\nversion: 1\nmixins: {{M: {{governed: {{acceptance: agent, serialize: {{dir: {dir:?}, format: md-frontmatter}}, export: {{digest: false, marker: false}}, display: {{label: T, plural: Ts}}, governs_paths: false, dedup: none}}}}}}\nkinds: {{}}\nrels: {{}}\n"
        );
        assert_eq!(
            OntologyFile::from_yaml(&text).map(|_| ()).unwrap_err(),
            OntologyError::InvalidIdentifier {
                path: "mixins.M.governed.serialize.dir".to_owned()
            },
            "{dir:?}"
        );
    }
    assert!(kaban_core::ontology::DirName::parse("decisions").is_ok());
    assert!(kaban_core::ontology::DirName::parse("a-b-9").is_ok());
    assert!(kaban_core::ontology::DirName::parse("a/b").is_err());
    // the shipped files still load (their dirs are `decisions`, `conventions`, `failures`)
    let shipped = kaban_core::shipped::resolved().unwrap();
    let decision = shipped
        .kinds
        .values()
        .find(|k| k.id.name == "Decision")
        .unwrap();
    match &decision.governed.as_ref().unwrap().serialize {
        kaban_core::ontology::meta::Serialize::Dir(d) => assert_eq!(d.dir.as_str(), "decisions"),
        other @ kaban_core::ontology::meta::Serialize::None(_) => panic!("{other:?}"),
    }
}

/// Indexing meta-config (engine-spec §3.1 `index`, OWN-65, AMD-14; engine-plan E2.0 item 5a):
/// `index: {stamp: none}` resolves, a child inherits it through `is_a`, a mixin applies it, a
/// kind without the block keeps the default `commit`.
#[test]
fn index_stamp_none_resolves_and_is_inherited_through_is_a_and_mixins() {
    // Arrange
    let o = load(&["index_stamp.yaml"]).unwrap();
    let kind = |name: &str| {
        o.kinds
            .get(&Qualified {
                ns: ns("shop"),
                name: name.to_owned(),
            })
            .unwrap_or_else(|| panic!("{name} resolved"))
    };

    // Act
    let catalogue = kind("Catalogue").stamp();
    let leaflet = kind("Leaflet").stamp();
    let flyer = kind("Flyer").stamp();
    let receipt = kind("Receipt").stamp();

    // Assert
    assert_eq!(catalogue, Stamp::None, "the kind's own block");
    assert_eq!(leaflet, Stamp::None, "inherited through is_a");
    assert_eq!(flyer, Stamp::None, "applied by the mixin");
    assert_eq!(receipt, Stamp::Commit, "default when the block is absent");
    assert_eq!(kind("Receipt").index, None);
    assert_eq!(kind("Leaflet").index, Some(Index { stamp: Stamp::None }));
}

/// `stamp` is a closed set and the block is closed like every other key: a value outside
/// `commit | none` and an unknown key both fail with the file-form class and the field's
/// path, never the value (the other closed sets of `meta.rs` fail the same way; the spec's
/// `invalid_value` wording is recorded as text drift in the E2.0 report).
#[test]
fn index_with_an_unknown_value_or_key_fails_with_the_field_path() {
    // Arrange
    let weekly = "ontology: shop\nversion: 1\nkinds: {Thing: {description: t, index: {stamp: weekly}}}\nrels: {}\n";
    let typo = "ontology: shop\nversion: 1\nkinds: {Thing: {description: t, index: {stmp: none}}}\nrels: {}\n";

    // Act
    let weekly_err = load_err_text(weekly);
    let typo_err = load_err_text(typo);

    // Assert
    assert_eq!(weekly_err, "schema_invalid:file:kinds.Thing.index.stamp");
    assert!(!weekly_err.contains("weekly"), "{weekly_err}");
    assert_eq!(typo_err, "schema_invalid:file:kinds.Thing.index.stmp");
}

/// The shipped set: `Document`, `Section`, `Chunk` and `Tag` are `none` (ingested document rows
/// carry no commit sha); every other shipped kind, the governed and serializable `Decision`,
/// `Convention` and `Failure` among them, keeps the default `commit`.
#[test]
fn the_shipped_ontologies_leave_document_kinds_unstamped_and_stamp_the_governed_kinds() {
    // Arrange: the shipped bytes themselves (the fixture copies are the EN-F1 goldens)
    let o = kaban_core::shipped::resolved().unwrap();

    // Act
    let unstamped: BTreeSet<String> = o
        .kinds
        .values()
        .filter(|k| k.stamp() == Stamp::None)
        .map(|k| k.id.name.clone())
        .collect();
    let stamped: BTreeSet<String> = o
        .kinds
        .values()
        .filter(|k| k.stamp() == Stamp::Commit)
        .map(|k| k.id.name.clone())
        .collect();

    // Assert
    assert_eq!(
        unstamped,
        ["Chunk", "Document", "Section", "Tag"]
            .into_iter()
            .map(str::to_owned)
            .collect()
    );
    for governed in ["Decision", "Convention", "Failure"] {
        assert!(stamped.contains(governed), "{governed} stamps per commit");
    }
}
