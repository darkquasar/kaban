//! The E1-end correction through `KabanApi` (E1.9a; astra review RE3-1, RE3-2, RE3-4, RE3-6
//! as dispositioned in `../specs/kaban/specs/00-project/evidence/e1-end-astra-review-dispositions.copy.md`): human
//! acceptance of a review candidate reaches the store's EN-F5a branch, the gated kind is
//! canonicalised before every registry lookup, a promotion target is judged against the
//! registry before anything moves, and `schema_describe` projects `x:` labels through the
//! bound workspace's `kind_stats` with their persisted namespace.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::Scratch;
use kaban_api::{
    AcceptRequest, AcceptResponse, ApiConfig, ApiError, Caller, KabanApi, OntologyRegistry,
    RecallRequest, RememberRequest, RememberStatus,
};
use kaban_core::{Actor, IngestStore, InvalidTransition, SignerKind, StoreError, WorkspaceId};
use kaban_policy::Policy;
use kaban_store_sqlite::SqliteStore;
use std::sync::Arc;

/// A file-backed repository scratch (`<root>/.kaban/ontology/`) with one store and one
/// registry whose containment root is the scratch; services bound to any workspace are built
/// over both.
struct Repo {
    scratch: Scratch,
    store: Arc<SqliteStore>,
    registry: Arc<OntologyRegistry>,
}

impl Repo {
    fn new(name: &str) -> Self {
        let scratch = Scratch::new("kaban-e19a", name);
        std::fs::create_dir_all(scratch.0.join(".kaban/ontology")).unwrap();
        let store = Arc::new(SqliteStore::open(&scratch.db(), &scratch.keys()).unwrap());
        let registry = Arc::new(OntologyRegistry::new(Some(scratch.0.clone())).unwrap());
        Self {
            scratch,
            store,
            registry,
        }
    }

    /// A service bound to workspace `ws`, reading the shared registry.
    fn api(&self, ws: &str) -> KabanApi {
        KabanApi::with_config(
            Arc::clone(&self.store) as Arc<dyn kaban_core::GraphStore>,
            Policy::empty(),
            self.store.signer(),
            None,
            ApiConfig {
                workspace: WorkspaceId::new(ws),
                ..ApiConfig::default()
            },
        )
        .with_registry(Arc::clone(&self.registry))
    }

    /// Write `text` under `.kaban/ontology/<name>` and load it through `api`'s registry.
    fn load(&self, api: &KabanApi, name: &str, text: &str) {
        let path = self.scratch.0.join(".kaban/ontology").join(name);
        std::fs::write(&path, text).unwrap();
        self.registry.load_file(api, &path).unwrap();
    }
}

fn human() -> Caller {
    Caller {
        kind: Actor::HumanCli,
        ..Default::default()
    }
}

fn agent() -> Caller {
    Caller {
        kind: Actor::Agent,
        host: Some("test-host".into()),
        session: Some("sess-1".into()),
    }
}

fn remember(api: &KabanApi, kind: &str, text: &str, props: Option<serde_json::Value>) -> String {
    let r = api
        .remember(RememberRequest {
            kind: kind.into(),
            text: text.into(),
            props,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(r.status, RememberStatus::Created, "{r:?}");
    r.id
}

fn accept_request(id: &str) -> AcceptRequest {
    AcceptRequest {
        id: id.into(),
        reason: Some("reviewed".into()),
        path_scope: vec!["src/**".into()],
        ..Default::default()
    }
}

fn recall_kinds(api: &KabanApi, query: &str) -> Vec<String> {
    api.recall(RecallRequest {
        query: query.into(),
        hops: Some(0),
        ..Default::default()
    })
    .unwrap()
    .results
    .into_iter()
    .map(|c| c.kind)
    .collect()
}

/// A plain `infra` ontology: `Vendor` requires `site`, `Supplier` has an open schema.
const INFRA_YAML: &str = r#"ontology: infra
version: 1
kinds:
  Vendor:
    description: "A vendor with a site."
    props_schema: {type: object, additionalProperties: false, properties: {site: {type: string}}, required: [site]}
  Supplier:
    description: "A supplier."
rels: {}
"#;

/// RE3-1 (EN-F5a): a `discovered_kind` review candidate remembered through the service is
/// hidden until a human accepts it through `KabanApi::accept`; the acceptance releases the
/// candidate row and the record is recalled; an agent accepts one the same way at the agent
/// tier (OWN-84 (1), E2.5g: the shipped set declares no `discovered.acceptance`), recorded
/// as the agent's. Negatives: another workspace's service does not see the id, and a
/// released (non-candidate) `x:` node keeps `NotGoverned`.
#[test]
fn a_human_accepts_a_review_candidate_through_the_api_and_the_record_is_recalled() {
    let repo = Repo::new("accept");
    let api = repo.api("ws-a");
    let id = remember(&api, "Vendor", "quokka supplies widgets", None);
    assert_eq!(recall_kinds(&api, "quokka"), Vec::<String>::new());

    let accepted = api.accept(accept_request(&id), &human()).unwrap();
    let AcceptResponse::Accepted {
        node, signer_kind, ..
    } = accepted
    else {
        panic!("a human acceptance is immediate: {accepted:?}");
    };
    assert_eq!(signer_kind, SignerKind::HumanCli);
    assert_eq!(node.kind, "x:Vendor");
    assert_eq!(node.governance, kaban_core::Governance::None);
    assert_eq!(recall_kinds(&api, "quokka"), vec!["x:Vendor".to_owned()]);

    // OWN-84 (1) (E2.5g): an agent accepts a candidate at the agent tier, as the agent
    let other = remember(&api, "Supplier", "numbat supplies gadgets", None);
    assert_eq!(recall_kinds(&api, "numbat"), Vec::<String>::new());
    let by_agent = api.accept(accept_request(&other), &agent()).unwrap();
    assert!(
        matches!(
            by_agent,
            AcceptResponse::Accepted {
                signer_kind: SignerKind::Agent,
                ..
            }
        ),
        "{by_agent:?}"
    );
    assert_eq!(recall_kinds(&api, "numbat"), vec!["x:Supplier".to_owned()]);
    // another workspace's service does not see the candidate (S6)
    let elsewhere = repo
        .api("ws-b")
        .accept(accept_request(&other), &human())
        .unwrap_err();
    assert!(matches!(elsewhere, ApiError::NotFound(_)), "{elsewhere:?}");
    // a released `x:` node is no candidate and not governed: today's refusal
    let again = api.accept(accept_request(&id), &human()).unwrap_err();
    assert!(
        matches!(
            again,
            ApiError::Store(StoreError::InvalidTransition(
                InvalidTransition::NotGoverned(_)
            ))
        ),
        "{again:?}"
    );
}

/// RE3-2 (EN-F3, EN-F4): the gated kind is canonicalised before the registry lookups of
/// `remember`, so `adr:Decision` meets Decision's `props_schema` (`rationale` required) and
/// `user:Widget` meets Widget's `dedup: title+scope` rule exactly as the bare spellings do;
/// the stored row carries the canonical name.
#[test]
fn qualified_spellings_of_bare_namespaces_meet_the_registry_rules_through_the_api() {
    let repo = Repo::new("normalise");
    let api = repo.api("ws-a");
    let missing = |kind: &str| {
        api.remember(RememberRequest {
            kind: kind.into(),
            text: "ship on mondays".into(),
            ..Default::default()
        })
        .unwrap_err()
        .to_string()
    };
    let bare = missing("Decision");
    assert_eq!(bare, "props.rationale");
    assert_eq!(missing("adr:Decision"), bare, "the qualified spelling");
    let r = api
        .remember(RememberRequest {
            kind: "adr:Decision".into(),
            text: "ship on mondays".into(),
            props: Some(serde_json::json!({"rationale": "because"})),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(r.kind, "Decision", "stored canonical");

    // a user kind with the title+scope rule, declared under the bare `user` namespace
    repo.load(
        &api,
        "widgets.yaml",
        "ontology: user\nversion: 1\nkinds:\n  Widget:\n    description: \"A widget.\"\n    governed: {acceptance: agent, serialize: none, export: {digest: false, marker: false}, display: {label: Widget, plural: Widgets}, governs_paths: false, dedup: title+scope}\nrels: {}\n",
    );
    let widget = |kind: &str, text: &str| {
        api.remember(RememberRequest {
            kind: kind.into(),
            text: text.into(),
            summary: Some("Quokka cache".into()),
            path_scope: vec!["src/cache/**".into()],
            ..Default::default()
        })
        .unwrap()
    };
    let first = widget("Widget", "the first quokka cache widget");
    assert!(first.candidate_duplicates.is_empty(), "{first:?}");
    let second = widget("user:Widget", "the second quokka cache widget");
    assert_eq!(second.kind, "Widget", "stored canonical");
    assert_eq!(
        second
            .candidate_duplicates
            .iter()
            .map(|c| c.id.as_str())
            .collect::<Vec<_>>(),
        vec![first.id.as_str()],
        "the title+scope duplicate is detected like `Widget`: {second:?}"
    );
}

/// RE3-4 (EN-F4, EN-F5): a promotion target is judged against the registry before any
/// mutation: a governed kind (`Decision`) and a kind whose `props_schema` some renamed node
/// does not satisfy are refused with the fixed class `promote_target_incompatible` and nothing
/// moves; a plain user kind whose schema every node satisfies is promoted.
#[test]
fn promotion_targets_are_checked_against_the_registry_before_anything_moves() {
    let repo = Repo::new("promote-target");
    let api = repo.api("ws-a");
    let ws = WorkspaceId::new("ws-a");
    remember(
        &api,
        "Vendor",
        "quokka vendor with a site",
        Some(serde_json::json!({"site": "perth"})),
    );
    remember(&api, "Vendor", "quokka vendor without a site", None);
    let x_vendors = || {
        repo.store
            .list_nodes("x:Vendor", ws.as_str(), 0, 10)
            .unwrap()
            .len()
    };

    let governed = api
        .registry()
        .promote(&api, "x:Vendor", "Decision", &Actor::HumanCli)
        .unwrap_err();
    assert_eq!(governed.to_string(), "promote_target_incompatible");
    assert_eq!(x_vendors(), 2, "nothing moved");
    assert_eq!(recall_kinds(&api, "quokka vendor"), Vec::<String>::new());

    repo.load(&api, "infra.yaml", INFRA_YAML);
    let unsatisfied = api
        .registry()
        .promote(&api, "x:Vendor", "infra:Vendor", &Actor::HumanCli)
        .unwrap_err();
    assert_eq!(unsatisfied.to_string(), "promote_target_incompatible");
    assert_eq!(x_vendors(), 2, "whole group refused");
    assert_eq!(
        repo.store
            .list_nodes("infra:Vendor", ws.as_str(), 0, 10)
            .unwrap()
            .len(),
        0
    );

    // a group that satisfies the target's schema is promoted
    remember(
        &api,
        "Part",
        "a quokka part with a site",
        Some(serde_json::json!({"site": "perth"})),
    );
    let report = api
        .registry()
        .promote(&api, "x:Part", "infra:Vendor", &Actor::HumanCli)
        .unwrap();
    assert_eq!((report.nodes, report.candidates_released), (1, 1));
    assert_eq!(
        recall_kinds(&api, "quokka part"),
        vec!["infra:Vendor".to_owned()]
    );
}

/// RE3-6 (EN-F6, EN-S3; E1.7 R4): `schema_describe` keeps the persisted namespace (`x`) and
/// shows an `x:` label only to the workspace whose `kind_stats` count it; declared kinds stay
/// database-wide. Before and after a promotion and a prune.
#[test]
fn discovered_labels_are_described_per_workspace_with_their_namespace() {
    let repo = Repo::new("describe");
    let api_a = repo.api("ws-a");
    let api_b = repo.api("ws-b");
    remember(&api_a, "Vendor", "quokka vendor in a", None);
    remember(&api_b, "Gadget", "numbat gadget in b", None);
    let labels = |api: &KabanApi| -> Vec<(String, Option<String>)> {
        api.schema_describe()
            .unwrap()
            .labels
            .into_iter()
            .filter(|d| !d.builtin)
            .map(|d| (d.name, d.ns))
            .collect()
    };

    assert_eq!(
        labels(&api_a),
        vec![("x:Vendor".to_owned(), Some("x".to_owned()))]
    );
    assert_eq!(
        labels(&api_b),
        vec![("x:Gadget".to_owned(), Some("x".to_owned()))]
    );

    // promotion in A: the declared kind is database-wide, the `x:` label is gone everywhere
    repo.load(&api_a, "infra.yaml", INFRA_YAML);
    api_a
        .registry()
        .promote(&api_a, "x:Vendor", "infra:Supplier", &Actor::HumanCli)
        .unwrap();
    let declared = |ns: &str, name: &str| (format!("{ns}:{name}"), Some(ns.to_owned()));
    assert_eq!(
        labels(&api_a),
        vec![declared("infra", "Supplier"), declared("infra", "Vendor")]
    );
    assert_eq!(
        labels(&api_b),
        vec![
            declared("infra", "Supplier"),
            declared("infra", "Vendor"),
            ("x:Gadget".to_owned(), Some("x".to_owned()))
        ]
    );

    // prune in B (the store operation; the thresholds are the caller's): the label is gone
    let pruned = repo
        .store
        .prune_kinds(
            &WorkspaceId::new("ws-b"),
            &kaban_core::config::PromoteWhen {
                min_count: 2,
                ..kaban_core::config::PromoteWhen::default()
            },
            std::time::Duration::ZERO,
            false,
        )
        .unwrap();
    assert_eq!(pruned.kinds, vec!["x:Gadget".to_owned()]);
    assert_eq!(
        labels(&api_b),
        vec![declared("infra", "Supplier"), declared("infra", "Vendor")]
    );
    assert_eq!(
        labels(&api_a),
        vec![declared("infra", "Supplier"), declared("infra", "Vendor")]
    );
}
