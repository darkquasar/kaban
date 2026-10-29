//! Public-behaviour tests of `remember` v3 (core-spec F1, §6; core-plan C3.1): typed kind
//! props, `CodeRef` refs, scope and near-duplicate candidates, through `KabanApi`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::Scratch;
use kaban_api::{ApiError, KabanApi, RememberRequest, RememberStatus};
use kaban_policy::Policy;
use kaban_store_sqlite::SqliteStore;
use serde_json::json;
use std::sync::Arc;

fn api() -> KabanApi {
    let policy = Policy::from_file(Policy::parse(b"").unwrap(), "test".into()).unwrap();
    let store = Arc::new(SqliteStore::open_memory().unwrap());
    let signer = store.signer();
    KabanApi::new(store, policy, signer, None)
}

fn decision(text: &str, props: serde_json::Value) -> RememberRequest {
    RememberRequest {
        kind: "Decision".into(),
        text: text.into(),
        props: Some(props),
        ..Default::default()
    }
}

#[test]
fn decision_without_rationale_is_rejected_as_invalid_props() {
    let api = api();
    let r = api.remember(decision(
        "Use SQLite for the store",
        json!({"decision": "Use SQLite"}),
    ));
    match r {
        Err(ApiError::Invalid(msg)) => assert_eq!(msg, "props.rationale"),
        other => panic!("expected Invalid(props.rationale), got {other:?}"),
    }
    // with a rationale the Decision is created
    let ok = api
        .remember(decision(
            "Use SQLite for the store",
            json!({"decision": "Use SQLite", "rationale": "single file, zero ops"}),
        ))
        .unwrap();
    assert_eq!(ok.status, RememberStatus::Created);
}

use kaban_api::{CodeRefWire, coderefs_from_serena};
use kaban_core::{AuditStore, BodyLocation, Location, MemoryStore, NodeId, TraverseQuery};

const CANARY: &str = "AKIAQWERTYUIOPASDFGH";

fn store_api() -> (Arc<SqliteStore>, KabanApi) {
    let policy = Policy::from_file(Policy::parse(b"").unwrap(), "test".into()).unwrap();
    let store = Arc::new(SqliteStore::open_memory().unwrap());
    let signer = store.signer();
    (store.clone(), KabanApi::new(store, policy, signer, None))
}

fn wire(relative_path: &str) -> CodeRefWire {
    CodeRefWire {
        repo: "kaban".into(),
        commit: "8d10f88".into(),
        tool: "manual".into(),
        relative_path: relative_path.into(),
        name_path: Some("Store/open".into()),
        kind: Some("Function".into()),
        body_location: Some(BodyLocation {
            start_line: 10,
            end_line: 20,
        }),
        ..Default::default()
    }
}

/// The `CodeRef` nodes linked ABOUT from `id`.
fn coderefs_about(store: &SqliteStore, id: &str) -> Vec<kaban_core::Node> {
    let r = store
        .traverse(&TraverseQuery {
            start_ids: vec![NodeId::parse(id).unwrap()],
            rels: vec!["ABOUT".into()],
            direction: kaban_core::TraverseDirection::Out,
            max_depth: 1,
            node_kinds: vec!["CodeRef".into()],
            governance: vec![],
            limit: 100,
            workspace_id: None,
            ..Default::default()
        })
        .unwrap();
    r.nodes
        .into_iter()
        .filter(|n| n.kind == "CodeRef")
        .collect()
}

#[test]
fn second_decision_with_same_normalized_title_and_scope_reports_the_first_as_candidate() {
    let api = api();
    let props = json!({"rationale": "r"});
    let first = api
        .remember(RememberRequest {
            summary: Some("Use SQLite".into()),
            path_scope: vec!["crates/**".into(), "src/**".into()],
            ..decision("first body", props.clone())
        })
        .unwrap();
    // different body, differently cased and spaced title, same scope in another order
    let second = api
        .remember(RememberRequest {
            summary: Some("use  sqlite".into()),
            path_scope: vec!["src/**".into(), "crates/**".into()],
            ..decision("second body", props.clone())
        })
        .unwrap();
    assert_eq!(second.status, RememberStatus::Created);
    assert_ne!(second.id, first.id, "near-duplicates are never merged");
    let ids: Vec<&str> = second
        .candidate_duplicates
        .iter()
        .map(|c| c.id.as_str())
        .collect();
    assert_eq!(ids, vec![first.id.as_str()]);
    // a different scope is not a candidate, nor is a Decision in another workspace
    let third = api
        .remember(RememberRequest {
            summary: Some("Use SQLite".into()),
            path_scope: vec!["docs/**".into()],
            ..decision("third body", props.clone())
        })
        .unwrap();
    assert!(third.candidate_duplicates.is_empty());
    let other_ws = api
        .remember(RememberRequest {
            summary: Some("Use SQLite".into()),
            path_scope: vec!["crates/**".into(), "src/**".into()],
            workspace_id: Some("other".into()),
            ..decision("fourth body", props)
        })
        .unwrap();
    assert!(other_ws.candidate_duplicates.is_empty());
}

#[test]
fn refs_are_stored_as_coderef_nodes_once() {
    let (store, api) = store_api();
    let a = api
        .remember(RememberRequest {
            kind: "Fact".into(),
            text: "first note about open".into(),
            refs: vec![wire("src/store.rs")],
            ..Default::default()
        })
        .unwrap();
    let b = api
        .remember(RememberRequest {
            kind: "Fact".into(),
            text: "second note about open".into(),
            refs: vec![wire("src/store.rs"), wire("src\\other.rs")],
            ..Default::default()
        })
        .unwrap();
    let ra = coderefs_about(&store, &a.id);
    let rb = coderefs_about(&store, &b.id);
    assert_eq!(ra.len(), 1);
    assert_eq!(rb.len(), 2);
    assert!(rb.iter().any(|n| n.id == ra[0].id), "same ref, same node");
    let total: usize = store.stats(None).unwrap()["by_kind"]["CodeRef"]
        .as_u64()
        .unwrap()
        .try_into()
        .unwrap();
    assert_eq!(total, 2);
    let other = rb.iter().find(|n| n.id != ra[0].id).unwrap();
    assert_eq!(other.props["relative_path"], json!("src/other.rs"));
    assert_eq!(other.props["line"], json!(10));
}

#[test]
fn canary_in_a_coderef_field_is_rejected_and_nothing_is_stored() {
    let (store, api) = store_api();
    let mut w = wire("src/store.rs");
    w.relative_path = format!("src/{CANARY}.rs");
    let r = api.remember(RememberRequest {
        kind: "Fact".into(),
        text: "note".into(),
        refs: vec![w],
        ..Default::default()
    });
    match r {
        Err(ApiError::Rejected { field, rules }) => {
            assert_eq!(field, "refs.relative_path");
            assert!(rules.iter().any(|r| r == "aws-access-key-id"), "{rules:?}");
        }
        other => panic!("expected rejection, got {other:?}"),
    }
    assert_eq!(store.stats(None).unwrap()["nodes"], json!(0));
}

#[test]
fn serena_find_symbol_fixture_yields_a_coderef_with_line_from_body_start() {
    let fixture = json!([{
        "name_path": "SqliteStore/open",
        "kind": "Method",
        "relative_path": "crates\\kaban-store-sqlite\\src\\sqlite.rs",
        "body_location": {"start_line": 120, "end_line": 160},
        "body": "pub fn open(path: &Path) -> Result<Self> { ... }"
    }]);
    let refs = coderefs_from_serena(&fixture, "kaban", "8d10f88").unwrap();
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0].tool, "serena");
    let (store, api) = store_api();
    let n = api
        .remember(RememberRequest {
            kind: "Fact".into(),
            text: "open is where the PRAGMAs live".into(),
            refs,
            ..Default::default()
        })
        .unwrap();
    let cr = coderefs_about(&store, &n.id);
    assert_eq!(cr.len(), 1);
    let p = &cr[0].props;
    assert_eq!(p["line"], json!(120));
    assert_eq!(
        p["relative_path"],
        json!("crates/kaban-store-sqlite/src/sqlite.rs")
    );
    assert_eq!(p["kind"], json!("Method"));
    assert_eq!(
        p["body_location"],
        json!({"start_line": 120, "end_line": 160})
    );
    assert!(p.get("location").is_none(), "{p}");
    assert!(p.get("body").is_none(), "{p}");
    assert!(!cr[0].text.contains("pub fn open"), "{}", cr[0].text);
}

#[test]
fn ref_with_location_keeps_the_nested_object_and_derives_line() {
    let (store, api) = store_api();
    let mut w = wire("src/a.rs");
    w.body_location = None;
    w.location = Some(Location {
        line: 12,
        column: 4,
    });
    let n = api
        .remember(RememberRequest {
            kind: "Fact".into(),
            text: "note".into(),
            refs: vec![w],
            ..Default::default()
        })
        .unwrap();
    let cr = coderefs_about(&store, &n.id);
    assert_eq!(cr[0].props["location"], json!({"line": 12, "column": 4}));
    assert_eq!(cr[0].props["line"], json!(12));
}

#[test]
fn ref_with_flat_line_key_is_rejected_by_the_wire_shape() {
    let raw = json!({"kind": "Fact", "text": "note", "refs": [{
        "repo": "kaban", "commit": "8d10f88", "tool": "manual",
        "relative_path": "src/a.rs", "line": 12
    }]});
    let err = serde_json::from_value::<RememberRequest>(raw).unwrap_err();
    assert!(err.to_string().contains("unknown field `line`"), "{err}");
    // the Serena adapter is as strict: an unknown key in an entry is Invalid, no value echoed
    let bad = json!([{"relative_path": "src/a.rs", "line": 12}]);
    match coderefs_from_serena(&bad, "kaban", "8d10f88") {
        Err(ApiError::Invalid(msg)) => assert_eq!(msg, "refs.line"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn canary_in_info_is_rejected_and_absent_from_db_audit_and_response() {
    let dir = std::env::temp_dir().join(format!("kaban-c31-info-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("k.db");
    let store = Arc::new(SqliteStore::open(&db, &dir.join("keys")).unwrap());
    let policy = Policy::from_file(Policy::parse(b"").unwrap(), "test".into()).unwrap();
    let signer = store.signer();
    let api = KabanApi::new(store.clone(), policy, signer, None);
    let mut w = wire("src/a.rs");
    w.info = Some(format!("key {CANARY} leaked here"));
    let err = api
        .remember(RememberRequest {
            kind: "Fact".into(),
            text: "note".into(),
            refs: vec![w],
            ..Default::default()
        })
        .unwrap_err();
    let text = err.to_string();
    assert!(
        text.contains("aws-access-key-id") && text.contains("refs.info"),
        "{text}"
    );
    assert!(!text.contains(CANARY));
    assert_eq!(store.stats(None).unwrap()["nodes"], json!(0));
    // the rejection is audited (rule id and keyed hash only)
    let audit = store.audit_verify().unwrap();
    assert!(audit.entries >= 1, "{audit:?}");
    drop(api);
    drop(store);
    for f in ["k.db", "k.db-wal", "k.db-shm"] {
        if let Ok(bytes) = std::fs::read(dir.join(f)) {
            let hay = String::from_utf8_lossy(&bytes);
            assert!(!hay.contains(CANARY), "{f} contains the canary");
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn clean_info_is_scanned_but_never_stored() {
    let (store, api) = store_api();
    let mut w = wire("src/a.rs");
    w.info = Some("the constructor; nothing sensitive".into());
    let n = api
        .remember(RememberRequest {
            kind: "Fact".into(),
            text: "note".into(),
            refs: vec![w],
            ..Default::default()
        })
        .unwrap();
    let cr = coderefs_about(&store, &n.id);
    assert_eq!(cr.len(), 1);
    assert!(cr[0].props.get("info").is_none(), "{}", cr[0].props);
    assert!(!cr[0].text.contains("constructor"));
}

#[test]
fn grouped_find_referencing_symbols_fixture_yields_one_ref_per_entry() {
    let fixture = json!({
        "crates/kaban-api/src/api.rs": {
            "Method": [
                {"name_path": "KabanApi/remember", "body_location": {"start_line": 130, "end_line": 270},
                 "content_around_reference": "   ...   "},
                {"name_path": "KabanApi/link", "body_location": {"start_line": 280, "end_line": 310}}
            ]
        },
        "crates/kaban/src/bench.rs": {
            "Function": [
                {"name_path": "run", "reference_line": 404, "children": []}
            ]
        }
    });
    let mut refs = coderefs_from_serena(&fixture, "kaban", "8d10f88").unwrap();
    refs.sort_by(|a, b| a.name_path.cmp(&b.name_path));
    assert_eq!(refs.len(), 3);
    assert_eq!(refs[0].name_path.as_deref(), Some("KabanApi/link"));
    assert_eq!(refs[0].relative_path, "crates/kaban-api/src/api.rs");
    assert_eq!(refs[0].kind.as_deref(), Some("Method"));
    assert_eq!(refs[2].name_path.as_deref(), Some("run"));
    assert_eq!(refs[2].relative_path, "crates/kaban/src/bench.rs");
    assert_eq!(refs[2].kind.as_deref(), Some("Function"));
    assert_eq!(refs[2].reference_line, Some(404));
}

#[test]
fn overview_bare_strings_yield_no_refs_and_no_error() {
    let fixture = json!(["KabanApi", "KabanServer", "run"]);
    let refs = coderefs_from_serena(&fixture, "kaban", "8d10f88").unwrap();
    assert_eq!(refs, Vec::new());
}

#[test]
fn scope_personal_is_not_available_before_m6() {
    let api = api();
    let r = api.remember(RememberRequest {
        kind: "Fact".into(),
        text: "note".into(),
        scope: Some(kaban_core::StoreScope::Personal),
        ..Default::default()
    });
    match r {
        Err(ApiError::Invalid(msg)) => assert!(msg.contains("M6"), "{msg}"),
        other => panic!("{other:?}"),
    }
    assert!(
        api.remember(RememberRequest {
            kind: "Fact".into(),
            text: "note".into(),
            scope: Some(kaban_core::StoreScope::Project),
            ..Default::default()
        })
        .is_ok()
    );
}

#[test]
fn convention_and_failure_props_are_typed_and_unknown_keys_are_rejected() {
    let api = api();
    assert!(
        api.remember(RememberRequest {
            kind: "Convention".into(),
            text: "snake_case for files".into(),
            props: Some(json!({"rule": "snake_case"})),
            ..Default::default()
        })
        .is_ok()
    );
    match api.remember(RememberRequest {
        kind: "Failure".into(),
        text: "build broke".into(),
        props: Some(json!({"command": "cargo build", "stak_head": "x"})),
        ..Default::default()
    }) {
        Err(ApiError::Invalid(msg)) => assert_eq!(msg, "props.stak_head"),
        other => panic!("{other:?}"),
    }
}

/// The `runbooks` fixture ontology loaded into the service's registry (engine-plan E1.5): the
/// scratch directory is the containment root, the file is copied beneath it.
fn with_runbooks(s: &Scratch, api: KabanApi) -> KabanApi {
    let registry = Arc::new(kaban_api::OntologyRegistry::new(Some(s.0.clone())).unwrap());
    let api = api.with_registry(registry);
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../kaban-core/fixtures/ontology/runbooks.yaml");
    let target = s.0.join("runbooks.yaml");
    std::fs::copy(fixture, &target).unwrap();
    api.registry().load_file(&api, &target).unwrap();
    api
}

fn runbook(text: &str, summary: &str) -> RememberRequest {
    RememberRequest {
        kind: "runbooks:Runbook".into(),
        text: text.into(),
        summary: Some(summary.into()),
        path_scope: vec!["ops/**".into()],
        ..Default::default()
    }
}

/// E1.5 (engine-spec EN-F4): the near-duplicate key follows the kind's `dedup: title+scope`
/// in the registry: a second `Runbook` with the same normalized title and scope reports the
/// first as a candidate (same kind only: a Decision with that title is not one), and a kind
/// with `dedup: none` (the shipped Convention) reports nothing.
#[test]
fn a_second_runbook_with_the_same_title_and_scope_reports_the_first_as_candidate() {
    // Arrange
    let s = Scratch::new("kaban-remember", "runbook-dedup");
    let api = with_runbooks(&s, api());
    let first = api
        .remember(runbook("first body", "Rotate the queue"))
        .unwrap();
    let _decision = api
        .remember(RememberRequest {
            summary: Some("Rotate the queue".into()),
            path_scope: vec!["ops/**".into()],
            ..decision(
                "a decision with the runbook's title",
                json!({"rationale": "r"}),
            )
        })
        .unwrap();

    // Act
    let second = api
        .remember(runbook("second body", "rotate  THE queue"))
        .unwrap();
    let convention = api
        .remember(RememberRequest {
            kind: "Convention".into(),
            text: "one convention".into(),
            summary: Some("Rotate the queue".into()),
            path_scope: vec!["ops/**".into()],
            ..Default::default()
        })
        .unwrap();
    let convention_again = api
        .remember(RememberRequest {
            kind: "Convention".into(),
            text: "another convention".into(),
            summary: Some("Rotate the queue".into()),
            path_scope: vec!["ops/**".into()],
            ..Default::default()
        })
        .unwrap();

    // Assert
    assert_eq!(second.status, RememberStatus::Created);
    let ids: Vec<&str> = second
        .candidate_duplicates
        .iter()
        .map(|c| c.id.as_str())
        .collect();
    assert_eq!(ids, vec![first.id.as_str()], "{second:?}");
    assert_eq!(convention.status, RememberStatus::Created);
    assert_eq!(convention_again.status, RememberStatus::Created);
    assert!(
        convention_again.candidate_duplicates.is_empty(),
        "dedup: none reports no candidates"
    );
}

/// E1.5 (engine-spec EN-F3): a user kind's `props_schema` is enforced through the same seam:
/// an `enum` value outside the list is `props.severity`, an unknown key `props.<key>`, and a
/// conforming record is created.
#[test]
fn a_runbook_severity_outside_the_enum_is_rejected_with_the_pointer_text() {
    // Arrange
    let s = Scratch::new("kaban-remember", "runbook-enum");
    let api = with_runbooks(&s, api());

    // Act
    let outside = api.remember(RememberRequest {
        props: Some(json!({"severity": "medium"})),
        ..runbook("drain the queue", "Drain")
    });
    let unknown = api.remember(RememberRequest {
        props: Some(json!({"severity": "low", "owner": "ops"})),
        ..runbook("drain the queue", "Drain")
    });
    let ok = api.remember(RememberRequest {
        props: Some(json!({"severity": "high", "steps": ["stop", "drain"]})),
        ..runbook("drain the queue", "Drain")
    });

    // Assert
    match outside {
        Err(ApiError::Invalid(msg)) => assert_eq!(msg, "props.severity"),
        other => panic!("expected Invalid(props.severity), got {other:?}"),
    }
    match unknown {
        Err(ApiError::Invalid(msg)) => assert_eq!(msg, "props.owner"),
        other => panic!("expected Invalid(props.owner), got {other:?}"),
    }
    assert_eq!(ok.unwrap().status, RememberStatus::Created);
}

/// E1.7a RE2-8 (Medium): the `GOVERNS` source rule (engine-spec EN-F4, E1.5) holds in the
/// common edge-write path, so an inline link of `remember` is judged like `api.link`: a new
/// Fact -> `CodeRef` `GOVERNS` link is refused with the same fixed class and the enclosing
/// `remember` is rolled back (no Fact row); the reverse direction (a new `CodeRef` as the target
/// of a Fact's `GOVERNS`) is refused too; a new `runbooks:Runbook` (governs paths) as the
/// source is accepted; a source in another workspace stays `NotFound`.
#[test]
fn an_inline_governs_link_from_a_non_governing_kind_is_refused_and_the_remember_rolled_back() {
    // Arrange
    let s = Scratch::new("kaban-remember", "inline-governs");
    let (store, api) = store_api();
    let api = with_runbooks(&s, api);
    let code = api
        .remember(RememberRequest {
            kind: "CodeRef".into(),
            text: "src/queue.rs".into(),
            ..Default::default()
        })
        .unwrap()
        .id;
    let fact = api
        .remember(RememberRequest {
            kind: "Fact".into(),
            text: "the queue is a table".into(),
            ..Default::default()
        })
        .unwrap()
        .id;
    let governs_to_code = |kind: &str, text: &str| RememberRequest {
        kind: kind.into(),
        text: text.into(),
        summary: Some(text.into()),
        path_scope: vec!["ops/**".into()],
        links: vec![kaban_api::LinkSpec {
            rel: "GOVERNS".into(),
            to: Some(code.clone()),
            from: None,
            confidence: None,
        }],
        ..Default::default()
    };
    let facts_matching = |store: &SqliteStore, query: &str| -> usize {
        kaban_core::MemoryStore::recall(
            store,
            &kaban_core::RecallQuery {
                query: query.into(),
                kinds: vec!["Fact".into()],
                limit: 50,
                ..Default::default()
            },
        )
        .unwrap()
        .len()
    };
    assert_eq!(
        facts_matching(&store, "queue table"),
        1,
        "the arranged Fact is found"
    );

    // Act
    let from_fact = api.remember(governs_to_code("Fact", "a fact that would govern the file"));
    let to_new_coderef = api.remember(RememberRequest {
        kind: "CodeRef".into(),
        text: "src/other.rs".into(),
        links: vec![kaban_api::LinkSpec {
            rel: "GOVERNS".into(),
            to: None,
            from: Some(fact.clone()),
            confidence: None,
        }],
        ..Default::default()
    });
    let from_runbook = api.remember(governs_to_code("runbooks:Runbook", "Rotate the queue"));

    // Assert
    for (name, result) in [
        ("Fact source", &from_fact),
        ("Fact source, new target", &to_new_coderef),
    ] {
        match result {
            Err(ApiError::Store(kaban_core::StoreError::EndpointNotAllowed { rel, from, to })) => {
                assert_eq!(
                    (rel.as_str(), from.as_str(), to.as_str()),
                    ("GOVERNS", "Fact", "CodeRef"),
                    "{name}"
                );
            }
            other => panic!("{name}: expected EndpointNotAllowed, got {other:?}"),
        }
    }
    assert_eq!(
        facts_matching(&store, "govern file"),
        0,
        "the refused remember left no Fact"
    );
    let runbook = from_runbook.unwrap();
    assert_eq!(runbook.links_created, 1, "{runbook:?}");
}
