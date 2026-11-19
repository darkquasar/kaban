//! The E1-end correction through `KabanApi` (E1.9a; astra review RE3-1, RE3-2, RE3-4, RE3-6
//! as dispositioned in `../specs/kaban/03-engine-v5/archive/reviews/e1-end-astra-review-dispositions.md`): the
//! gated kind is canonicalised before every registry lookup. The promotion and discovered-label
//! tests went with discovered kinds (PC-14).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::Scratch;
use kaban_api::{ApiConfig, KabanApi, OntologyRegistry, RememberRequest};
use kaban_core::WorkspaceId;
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
        "ontology: user\nversion: 1\nkinds:\n  Widget:\n    description: \"A widget.\"\n    rules: {display: {label: Widget, plural: Widgets}, dedup: title+scope}\nrels: {}\n",
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
