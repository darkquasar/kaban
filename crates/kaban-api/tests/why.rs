//! Public-behaviour tests of `why` (core-spec F2; core-plan C3.4) and of the gated event read
//! (M3-6, WG-2).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::Scratch;
use kaban_api::currency::CurrencyState;
use kaban_api::{
    AcceptRequest, AcceptResponse, ApiConfig, ApiError, Caller, KabanApi, MatchedBy,
    RememberRequest, WhyRequest, WhyTarget,
};
use kaban_core::ActorId;
use kaban_core::{Actor, Governance, GovernanceStore, SignerKind};
use kaban_policy::Policy;
use kaban_store_sqlite::SqliteStore;
use serde_json::json;
use std::sync::Arc;

/// Hits the builtin credential rule `aws-access-key-id` (always reject) in every view.
const CANARY: &str = "AKIAQWERTYUIOPASDFGH";

fn human() -> Caller {
    Caller {
        kind: Actor::HumanCli,
        ..Default::default()
    }
}

fn api_on(s: &Scratch, policy: Policy, config: ApiConfig) -> (KabanApi, Arc<SqliteStore>) {
    let store = Arc::new(SqliteStore::open(&s.db(), &s.keys()).unwrap());
    let api = KabanApi::with_config(store.clone(), policy, store.signer(), None, config);
    (api, store)
}

fn decision(api: &KabanApi, text: &str, importance: Option<f64>) -> String {
    api.remember(RememberRequest {
        kind: "Decision".into(),
        text: text.into(),
        props: Some(json!({"rationale": "because"})),
        importance,
        ..Default::default()
    })
    .unwrap()
    .id
}

fn accept(api: &KabanApi, id: &str, scope: &[&str]) {
    api.accept(
        AcceptRequest {
            id: id.into(),
            reason: Some("reviewed".into()),
            path_scope: scope.iter().map(|s| (*s).to_owned()).collect(),
            ..Default::default()
        },
        &human(),
    )
    .unwrap();
}

fn path(p: &str) -> WhyRequest {
    WhyRequest {
        targets: vec![WhyTarget::Path(p.into())],
        ..Default::default()
    }
}

/// The record's lifecycle events as the service lists them. A record created outside any
/// repository carries an OWN-65 `unstamped` reason row first (engine-plan E2.0; `repo_root` is
/// unset here), which the hiding assertions below are not about.
fn lifecycle(events: Vec<kaban_api::EventView>) -> Vec<kaban_api::EventView> {
    events
        .into_iter()
        .filter(|e| e.kind != "unstamped")
        .collect()
}

#[test]
fn a_decision_scoped_to_a_directory_glob_governs_a_file_beneath_it() {
    let s = Scratch::new("kaban-why", "governs");
    let (api, _) = api_on(&s, Policy::empty(), ApiConfig::default());
    let governing = decision(&api, "sqlite stays the store engine", None);
    accept(&api, &governing, &["src/store/**"]);
    let elsewhere = decision(&api, "the cli prints json", None);
    accept(&api, &elsewhere, &["src/cli/**"]);
    // a proposed Decision with a literal directory entry: also governs, below the accepted one
    let _ = api.remember(RememberRequest {
        kind: "Decision".into(),
        text: "unscoped proposal".into(),
        props: Some(json!({"rationale": "because"})),
        ..Default::default()
    });

    let r = api.why(path("src/store/sqlite.rs")).unwrap();
    assert_eq!(r.results.len(), 1, "{r:?}");
    let card = &r.results[0];
    assert_eq!(card.node.id, governing);
    assert_eq!(card.governance, Governance::Accepted);
    assert_eq!(card.matched_target, "src/store/sqlite.rs");
    assert_eq!(card.matched_by, MatchedBy::PathScope);
    assert!(!r.partial);
    // a sibling path outside the glob finds nothing; a `*` entry stays inside one component
    assert!(api.why(path("src/api/mod.rs")).unwrap().results.is_empty());
    let shallow = decision(&api, "top-level store files", None);
    accept(&api, &shallow, &["src/store/*.rs"]);
    let r = api.why(path("src/store/nested/deep.rs")).unwrap();
    assert_eq!(
        r.results
            .iter()
            .map(|c| c.node.id.as_str())
            .collect::<Vec<_>>(),
        vec![governing.as_str()],
        "the single-star entry must not match a nested file"
    );
    let r = api.why(path("src/store/sqlite.rs")).unwrap();
    assert_eq!(r.results.len(), 2);
}

#[test]
fn proposed_decisions_rank_below_accepted_ones_then_by_importance_then_newest_first() {
    let s = Scratch::new("kaban-why", "ranking");
    let (api, _) = api_on(&s, Policy::empty(), ApiConfig::default());
    let low = decision(&api, "accepted low importance", Some(0.2));
    accept(&api, &low, &["src/**"]);
    let proposed_old = api
        .remember(RememberRequest {
            kind: "Decision".into(),
            text: "proposed, older".into(),
            props: Some(json!({"rationale": "because"})),
            path_scope: vec!["src/**".into()],
            importance: Some(0.9),
            ..Default::default()
        })
        .unwrap()
        .id;
    let high = decision(&api, "accepted high importance", Some(0.8));
    accept(&api, &high, &["src/api/**"]);
    let proposed_new = api
        .remember(RememberRequest {
            kind: "Decision".into(),
            text: "proposed, newer".into(),
            props: Some(json!({"rationale": "because"})),
            path_scope: vec!["src/api/mod.rs".into()],
            importance: Some(0.9),
            ..Default::default()
        })
        .unwrap()
        .id;

    let r = api.why(path("src/api/mod.rs")).unwrap();
    let ids: Vec<&str> = r.results.iter().map(|c| c.node.id.as_str()).collect();
    assert_eq!(
        ids,
        vec![&high[..], &low[..], &proposed_new[..], &proposed_old[..]]
    );
    assert_eq!(r.results[2].governance, Governance::Proposed);
    assert_eq!(
        r.results[2].currency.state,
        CurrencyState::Uncheckable,
        "a proposed record has no acceptance to measure from"
    );
    assert_eq!(
        r.results[2].currency.reason.as_deref(),
        Some("not accepted")
    );
    // limit cuts after ordering and reports the cut
    let r = api
        .why(WhyRequest {
            limit: Some(1),
            ..path("src/api/mod.rs")
        })
        .unwrap();
    assert_eq!(r.results.len(), 1);
    assert_eq!(r.results[0].node.id, high);
    assert!(r.partial);
}

/// M3 review R3-5: the store's glob prefilter admits every brace entry, so higher-ranked
/// records whose brace glob does not match the target are false positives. `why` pages through
/// the candidate order until `limit + 1` exact matches or exhaustion: the lower-ranked true
/// matches behind more than `2 * limit` false positives are returned, and `partial` reports
/// the cut only when a further exact match exists.
#[test]
fn true_matches_behind_more_than_twice_the_limit_of_glob_false_positives_are_still_found() {
    let s = Scratch::new("kaban-why", "paging");
    let (api, _) = api_on(&s, Policy::empty(), ApiConfig::default());
    // five accepted, important records on brace globs that do not cover the target
    for i in 0..5 {
        let id = decision(&api, &format!("docs convention {i}"), Some(1.0));
        accept(&api, &id, &[&format!("docs/{{guide{i},ref{i}}}/**")]);
    }
    // one true match, ranked below them
    let governing = decision(&api, "sqlite stays the store engine", Some(0.1));
    accept(&api, &governing, &["src/store/**"]);

    let r = api
        .why(WhyRequest {
            limit: Some(2),
            ..path("src/store/sqlite.rs")
        })
        .unwrap();
    let ids: Vec<&str> = r.results.iter().map(|c| c.node.id.as_str()).collect();
    assert_eq!(ids, vec![&governing[..]], "{r:?}");
    assert!(!r.partial, "one exact match within the limit: {r:?}");

    // two more true matches below: the limit cuts after the exact matches and says so
    let second = decision(&api, "wal mode stays on", Some(0.05));
    accept(&api, &second, &["src/store/sqlite.rs"]);
    let third = decision(&api, "busy timeout is five seconds", Some(0.01));
    accept(&api, &third, &["src/{store,api}/**"]);
    let r = api
        .why(WhyRequest {
            limit: Some(2),
            ..path("src/store/sqlite.rs")
        })
        .unwrap();
    let ids: Vec<&str> = r.results.iter().map(|c| c.node.id.as_str()).collect();
    assert_eq!(ids, vec![&governing[..], &second[..]], "{r:?}");
    assert!(r.partial, "a third exact match exists: {r:?}");
}

#[test]
fn two_targets_in_one_call_return_their_own_matches_in_request_order() {
    let s = Scratch::new("kaban-why", "targets");
    let (api, _) = api_on(&s, Policy::empty(), ApiConfig::default());
    let store_d = decision(&api, "sqlite wal mode for the store", None);
    accept(&api, &store_d, &["src/store/**"]);
    let cli_d = decision(&api, "json output on the cli", None);
    accept(&api, &cli_d, &["src/cli/**"]);

    let r = api
        .why(WhyRequest {
            targets: vec![
                WhyTarget::Path("src/cli/output.rs".into()),
                WhyTarget::Path("src/store/sqlite.rs".into()),
                WhyTarget::Query("wal mode".into()),
            ],
            ..Default::default()
        })
        .unwrap();
    let got: Vec<(&str, &str, MatchedBy)> = r
        .results
        .iter()
        .map(|c| (c.node.id.as_str(), c.matched_target.as_str(), c.matched_by))
        .collect();
    assert_eq!(
        got,
        vec![
            (cli_d.as_str(), "src/cli/output.rs", MatchedBy::PathScope),
            (
                store_d.as_str(),
                "src/store/sqlite.rs",
                MatchedBy::PathScope
            ),
            (store_d.as_str(), "wal mode", MatchedBy::Query),
        ]
    );
    // bounds
    assert!(matches!(
        api.why(WhyRequest::default()),
        Err(ApiError::Invalid(_))
    ));
    assert!(matches!(api.why(path("")), Err(ApiError::Invalid(_))));
    // a canary as a target is rejected before any lookup
    let policy = Policy::load(std::path::Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../examples/policy.yaml"
    )))
    .unwrap();
    let (strict, _) = api_on(
        &Scratch::new("kaban-why", "targets-policy"),
        policy,
        ApiConfig::default(),
    );
    assert!(matches!(
        strict.why(path(&format!("src/{CANARY}.rs"))),
        Err(ApiError::Rejected { .. })
    ));
}

#[test]
fn currency_is_uncheckable_outside_a_repository_and_computed_inside_one() {
    let s = Scratch::new("kaban-why", "currency");
    // no repo_root configured: every record is uncheckable
    let (api, _) = api_on(&s, Policy::empty(), ApiConfig::default());
    let d = decision(&api, "no repository here", None);
    accept(&api, &d, &["src/**"]);
    let r = api.why(path("src/lib.rs")).unwrap();
    assert_eq!(r.results[0].currency.state, CurrencyState::Uncheckable);
    assert_eq!(
        r.results[0].currency.reason.as_deref(),
        Some("no repository")
    );

    // a repo_root that is not a repository: git fails, still uncheckable, never an error
    let s2 = Scratch::new("kaban-why", "currency-notrepo");
    let (api, _) = api_on(
        &s2,
        Policy::empty(),
        ApiConfig {
            repo_root: Some(s2.0.clone()),
            repo: Some("kaban-why-notrepo".into()),
            ..Default::default()
        },
    );
    let d = decision(&api, "not a repository", None);
    accept(&api, &d, &["src/**"]);
    let r = api.why(path("src/lib.rs")).unwrap();
    assert_eq!(r.results[0].currency.state, CurrencyState::Uncheckable);
    assert_eq!(r.results[0].currency.reason.as_deref(), Some("git failed"));

    // inside this repository: the record is stamped with HEAD at acceptance, so nothing
    // changed since and it is active; a record from another clone is uncheckable
    let s3 = Scratch::new("kaban-why", "currency-repo");
    let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let (api, store) = api_on(
        &s3,
        Policy::empty(),
        ApiConfig {
            repo_root: Some(root.clone()),
            repo: root
                .canonicalize()
                .unwrap()
                .file_name()
                .map(|n| n.to_string_lossy().into_owned()),
            ..Default::default()
        },
    );
    let d = decision(&api, "currency of this very repository", None);
    accept(&api, &d, &["crates/kaban-api/src/**"]);
    let r = api.why(path("crates/kaban-api/src/why.rs")).unwrap();
    let c = &r.results[0].currency;
    assert_eq!(c.state, CurrencyState::Active, "{c:?}");
    assert_eq!(c.staleness, 0.0);
    let other = decision(&api, "written in another clone", None);
    accept(&api, &other, &["crates/kaban-api/src/**"]);
    let conn = rusqlite::Connection::open(s3.db()).unwrap();
    SqliteStore::bind_writer(&conn, &ActorId::default()).unwrap();
    conn.execute(
        "UPDATE node SET repo = 'some-other-clone' WHERE id = ?",
        [&other],
    )
    .unwrap();
    drop(conn);
    let _ = store;
    let r = api.why(path("crates/kaban-api/src/why.rs")).unwrap();
    let by_id = |id: &str| {
        r.results
            .iter()
            .find(|c| c.node.id == id)
            .unwrap()
            .currency
            .clone()
    };
    assert_eq!(by_id(&d).state, CurrencyState::Active);
    assert_eq!(by_id(&other).state, CurrencyState::Uncheckable);
    assert_eq!(by_id(&other).reason.as_deref(), Some("another repository"));
}

#[test]
fn an_event_stored_under_an_old_policy_is_hidden_by_a_newer_rejecting_policy() {
    // WG-2 / M3-6: event strings are gated on the read path like every other persisted string
    let s = Scratch::new("kaban-why", "wg2");
    let (api, store) = api_on(&s, Policy::empty(), ApiConfig::default());
    let d = decision(&api, "gated events", None);
    api.accept(
        AcceptRequest {
            id: d.clone(),
            reason: Some("numbats approved it".into()),
            path_scope: vec!["src/**".into()],
            signer: Some("quokka".into()),
            ..Default::default()
        },
        &human(),
    )
    .unwrap();
    let events = lifecycle(api.events(&d, None).unwrap());
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].reason, "numbats approved it");
    assert_eq!(events[0].signer.as_deref(), Some("quokka"));
    assert_eq!(events[0].path_scope, vec!["src/**".to_owned()]);

    // reload with a policy that rejects the reason's word: the event is hidden, the record
    // itself (whose text does not match) is still returned, the word is absent from the bytes
    let policy = Policy::from_file(
        Policy::parse(b"rules:\n  - id: numbat-reject\n    regex: numbats\n    action: reject\n")
            .unwrap(),
        "wg2".into(),
    )
    .unwrap();
    let api = KabanApi::with_config(
        store.clone(),
        policy,
        store.signer(),
        None,
        ApiConfig::default(),
    );
    let events = lifecycle(api.events(&d, None).unwrap());
    assert!(events.is_empty(), "{events:?}");
    let bytes = serde_json::to_vec(&events).unwrap();
    assert!(!String::from_utf8(bytes).unwrap().contains("numbats"));
    let r = api.why(path("src/lib.rs")).unwrap();
    assert_eq!(
        r.results.len(),
        1,
        "the record text is clean and still governs"
    );
    assert!(!serde_json::to_string(&r).unwrap().contains("numbats"));
    // audited once per event per policy version: a second read adds no row
    let rows = || {
        let conn = rusqlite::Connection::open(s.db()).unwrap();
        let seen: i64 = conn
            .query_row(
                // the hidden event's own seq (the OWN-65 `unstamped` reason row precedes it)
                "SELECT count(*) FROM read_audit_seen
                 WHERE node_id = 'event:' || (SELECT seq FROM events WHERE kind = 'accepted')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let total: i64 = conn
            .query_row("SELECT count(*) FROM audit WHERE tool = 'read'", [], |r| {
                r.get(0)
            })
            .unwrap();
        (seen, total)
    };
    let (seen, total) = rows();
    assert_eq!(seen, 1, "the hidden event is audited under event:<seq>");
    assert!(total >= 1);
    let _ = lifecycle(api.events(&d, None).unwrap());
    assert_eq!(
        rows(),
        (seen, total),
        "no second row for the same event and policy"
    );
    // an unknown or other-workspace record has no events (S6)
    assert!(matches!(
        api.events(&d, Some("other-ws")),
        Err(ApiError::NotFound(_))
    ));
}

/// Reload `api` with a policy whose one rule rejects `term`.
fn rejecting(store: &Arc<SqliteStore>, id: &str, term: &str) -> KabanApi {
    let policy = Policy::from_file(
        Policy::parse(
            format!("rules:\n  - id: {id}\n    regex: {term}\n    action: reject\n").as_bytes(),
        )
        .unwrap(),
        id.into(),
    )
    .unwrap();
    KabanApi::with_config(
        store.clone(),
        policy,
        store.signer(),
        None,
        ApiConfig::default(),
    )
}

#[test]
fn an_event_whose_kind_hits_a_newer_rejecting_policy_is_hidden() {
    // review R3-4: `Event.kind` is content (S1 lists kind names); the schema does not constrain
    // it, so a later policy that forbids the stored word must hide the event (S4)
    let s = Scratch::new("kaban-why", "r34-kind");
    let (api, store) = api_on(&s, Policy::empty(), ApiConfig::default());
    let d = decision(&api, "kinds are content", None);
    accept(&api, &d, &["src/**"]);
    assert_eq!(lifecycle(api.events(&d, None).unwrap())[0].kind, "accepted");

    let api = rejecting(&store, "kind-reject", "accepted");
    let events = lifecycle(api.events(&d, None).unwrap());
    assert!(events.is_empty(), "{events:?}");
    assert!(!serde_json::to_string(&events).unwrap().contains("accepted"));
}

#[test]
fn an_event_whose_commit_hits_a_newer_rejecting_policy_is_hidden() {
    // review R3-4: a stored `commit_sha` is content on the read path like every other event
    // string (M3-6); a commit-shaped value gets no exemption outside the control registry
    let s = Scratch::new("kaban-why", "r34-commit");
    let (api, store) = api_on(&s, Policy::empty(), ApiConfig::default());
    let d = decision(&api, "commits are content", None);
    let ctx =
        kaban_core::WriteCtx::new("seed", kaban_core::WorkspaceId::default(), Actor::HumanCli);
    store
        .transition(
            &kaban_core::NodeId::parse(&d).unwrap(),
            Governance::Accepted,
            kaban_core::NewEvent {
                reason: "stamped".into(),
                path_scope: vec!["src/**".into()],
                signer: None,
                signer_kind: kaban_core::SignerKind::HumanCli,
                host: None,
                session_id: None,
                commit_sha: Some("deadbeefcafe0123".into()),
            },
            &ctx,
        )
        .unwrap();
    assert_eq!(
        lifecycle(api.events(&d, None).unwrap())[0]
            .commit_sha
            .as_deref(),
        Some("deadbeefcafe0123")
    );

    let api = rejecting(&store, "commit-reject", "deadbeef");
    let events = lifecycle(api.events(&d, None).unwrap());
    assert!(events.is_empty(), "{events:?}");
    assert!(!serde_json::to_string(&events).unwrap().contains("deadbeef"));
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

/// E1.5 (engine-spec EN-F4): `why` answers with the kinds whose mixin says `governs_paths:
/// true`, read from the registry: a user `Runbook` scoped to a directory governs a file
/// beneath it like a Decision does, and a user kind without the mixin never does.
#[test]
fn a_runbook_from_a_loaded_ontology_governs_the_paths_in_its_scope() {
    // Arrange
    let s = Scratch::new("kaban-why", "runbook");
    let (api, _) = api_on(&s, Policy::empty(), ApiConfig::default());
    let api = with_runbooks(&s, api);
    let shipped: Vec<String> = {
        let mut kinds = api.registry().governing_kinds();
        kinds.retain(|k| !k.starts_with("runbooks:"));
        kinds
    };
    assert_eq!(
        shipped,
        ["Convention", "Decision"],
        "the shipped set governs with exactly Decision and Convention"
    );
    let runbook = api
        .remember(RememberRequest {
            kind: "runbooks:Runbook".into(),
            text: "rotate the numbat queue credentials".into(),
            path_scope: vec!["ops/**".into()],
            ..Default::default()
        })
        .unwrap()
        .id;
    let decision = decision(&api, "the queue stays on sqlite", None);
    accept(&api, &decision, &["ops/queue/**"]);

    // Act
    let r = api.why(path("ops/queue/rotate.sh")).unwrap();

    // Assert
    let ids: Vec<&str> = r.results.iter().map(|c| c.node.id.as_str()).collect();
    assert_eq!(ids, [decision.as_str(), runbook.as_str()], "{r:?}");
    let card = &r.results[1];
    assert_eq!(card.node.kind, "runbooks:Runbook");
    assert_eq!(card.governance, Governance::Proposed);
    assert_eq!(card.matched_by, MatchedBy::PathScope);
    assert!(api.why(path("docs/README.md")).unwrap().results.is_empty());
}

/// E1.5 (core-spec F4, engine-spec EN-F4): the acceptance tier is the kind's
/// `governed.acceptance` in the registry. An agent accepts a `Runbook{acceptance: agent}`
/// outright (it then governs as accepted), is asked for the confirmation handshake on an
/// `Incident{acceptance: human}` (the prompt names the kind's label), and a user kind
/// without the mixin stays not governed; the shipped Decision keeps its token tier.
#[test]
fn a_runbook_is_accepted_by_an_agent_at_the_agent_tier_and_an_incident_needs_the_handshake() {
    // Arrange
    let s = Scratch::new("kaban-why", "runbook-accept");
    let (api, _) = api_on(&s, Policy::empty(), ApiConfig::default());
    let api = with_runbooks(&s, api);
    let incidents = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../kaban-core/fixtures/ontology/incidents.yaml");
    let target = s.0.join("incidents.yaml");
    std::fs::copy(incidents, &target).unwrap();
    api.registry().load_file(&api, &target).unwrap();
    let agent = Caller {
        kind: Actor::Agent,
        ..Default::default()
    };
    let remember = |kind: &str, text: &str| -> String {
        api.remember(RememberRequest {
            kind: kind.into(),
            text: text.into(),
            ..Default::default()
        })
        .unwrap()
        .id
    };
    let runbook = remember("runbooks:Runbook", "rotate the numbat queue credentials");
    let incident = remember("incidents:Incident", "the numbat queue stalled");
    let plain = remember("incidents:Decision", "a decision taken during the incident");
    let decision = decision(&api, "the queue stays on sqlite", None);
    let request = |id: &str| AcceptRequest {
        id: id.into(),
        reason: Some("reviewed".into()),
        path_scope: vec!["ops/**".into()],
        ..Default::default()
    };

    // Act
    let accepted = api.accept(request(&runbook), &agent).unwrap();
    let handshake = api.accept(request(&incident), &agent).unwrap();
    let not_governed = api.accept(request(&plain), &agent);
    let token_tier = api.accept(request(&decision), &agent).unwrap();

    // Assert
    match &accepted {
        AcceptResponse::Accepted {
            node, signer_kind, ..
        } => {
            assert_eq!(*signer_kind, SignerKind::Agent);
            assert_eq!(node.governance, Governance::Accepted);
        }
        other @ AcceptResponse::ConfirmationRequired { .. } => {
            panic!("expected accepted, got {other:?}")
        }
    }
    match &handshake {
        AcceptResponse::ConfirmationRequired { prompt, .. } => {
            assert!(prompt.starts_with("Accept the Incident "), "{prompt}");
        }
        other @ AcceptResponse::Accepted { .. } => {
            panic!("expected confirmation_required, got {other:?}")
        }
    }
    assert!(
        matches!(not_governed, Err(ApiError::Store(_))),
        "{not_governed:?}"
    );
    assert!(matches!(
        token_tier,
        AcceptResponse::ConfirmationRequired { .. }
    ));
    let r = api.why(path("ops/rotate.sh")).unwrap();
    let card = r.results.iter().find(|c| c.node.id == runbook).unwrap();
    assert_eq!(card.governance, Governance::Accepted);
}
