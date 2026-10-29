//! Ontology loading at the seam (engine-spec §3.1, EN-F2, EN-S7, EN-S8, EN-N1; engine-plan
//! E1.3): the shipped files reproduce today's builtin `schema_type` rows byte for byte and
//! `schema_describe` exposes their namespace; `OntologyRegistry::load_file` stores a user
//! namespace under `ns:Kind` names, records `index_meta` hashes, is idempotent, refuses builtin
//! redefinitions, gates every string as content and fails closed on a containment escape.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use kaban_api::{ApiConfig, KabanApi};
use kaban_core::{SchemaStore, TypeDef};
use kaban_policy::Policy;
use kaban_store_sqlite::SqliteStore;
use std::sync::Arc;

/// The builtin `schema_type` rows exactly as `schema.rs` `SEED_LABELS` seeded them before E1.3
/// (`b5fadd4`): `(name, description)`, every other column at its default.
const TODAYS_LABELS: [(&str, &str); 14] = [
    (
        "Fact",
        "A verified statement about the world, code or project.",
    ),
    ("Idea", "A proposal or hypothesis not yet acted upon."),
    (
        "Failure",
        "Something that went wrong: a bug, outage, failed approach.",
    ),
    (
        "Improvement",
        "A change that made something better, or a fix.",
    ),
    ("Decision", "A choice made, with its rationale."),
    ("Insight", "A derived reflection or lesson learned."),
    (
        "Entity",
        "A named thing: project, file, symbol, service, person.",
    ),
    (
        "Episode",
        "A sanitized source interaction or event used as provenance.",
    ),
    (
        "Convention",
        "A rule the project follows, with its rationale (governed).",
    ),
    (
        "CodeRef",
        "A pointer to code: repo, commit, path, symbol, line.",
    ),
    ("Document", "An ingested document (docs-ingest)."),
    ("Section", "A section of an ingested document."),
    ("Chunk", "A retrievable chunk of a section."),
    ("Tag", "A tag node for TAGGED edges."),
];

/// `SEED_RELS` before E1.3, the same way.
const TODAYS_RELS: [(&str, &str); 16] = [
    ("RELATES_TO", "Generic association."),
    ("ABOUT", "Memory is about an Entity."),
    ("PART_OF", "Containment / membership."),
    ("DERIVED_FROM", "Derived from a source memory or episode."),
    ("CAUSED_BY", "Failure was caused by the target."),
    ("FIXED_BY", "Failure was fixed by the target."),
    ("SUPERSEDES", "Newer memory replaces the older one."),
    ("CONTRADICTS", "Two memories conflict."),
    ("INVALIDATES", "Source shows the target is no longer valid."),
    ("DEPENDS_ON", "Dependency."),
    ("MENTIONS", "Mentions an entity."),
    ("EVIDENCE_FOR", "Supports the target claim."),
    (
        "GOVERNS",
        "A Decision or Convention governs the target (CodeRef or path).",
    ),
    ("NEXT", "Reading order between sections or chunks."),
    ("LINKS_TO", "A document links to another document."),
    ("TAGGED", "Tagged with a Tag node."),
];

/// Namespace membership of the shipped names (engine-spec §3.1 "Qualified and stored names").
const CORE_NAMES: [&str; 15] = [
    "Document",
    "Section",
    "Chunk",
    "Entity",
    "CodeRef",
    "Tag",
    "MENTIONS",
    "ABOUT",
    "EVIDENCE_FOR",
    "DERIVED_FROM",
    "PART_OF",
    "NEXT",
    "LINKS_TO",
    "TAGGED",
    "RELATES_TO",
];

fn api_over(store: &Arc<SqliteStore>) -> KabanApi {
    KabanApi::with_config(
        Arc::clone(store) as Arc<dyn kaban_core::GraphStore>,
        Policy::empty(),
        store.signer(),
        None,
        ApiConfig::default(),
    )
}

/// A seeded row as `schema_describe` must render it: today's columns untouched, `ns` added.
fn expected(kind: &str, name: &str, description: &str) -> TypeDef {
    let ns = if CORE_NAMES.contains(&name) {
        "core"
    } else {
        "adr"
    };
    TypeDef {
        kind: kind.into(),
        name: name.into(),
        ns: Some(ns.into()),
        description: description.into(),
        from_kinds: vec![],
        to_kinds: vec![],
        directed: true,
        props_schema: None,
        builtin: true,
        declaration: kaban_core::TypeDeclaration::default(),
    }
}

fn key(d: &TypeDef) -> (String, String) {
    (d.kind.clone(), d.name.clone())
}

/// The identity columns of a seed row (`kind`, `name`, `ns`, `description`, `builtin`); the
/// declared columns (`from_kinds`, `to_kinds`, `directed`, `props_schema`, the JSON
/// declaration) are the shipped files' since E1.8 (E1.7 R1/R2) and are asserted separately.
fn identity(d: &TypeDef) -> String {
    format!(
        "{} {} {:?} {:?} builtin={}",
        d.kind, d.name, d.ns, d.description, d.builtin
    )
}

fn assert_same(actual: &[TypeDef], expected: &[TypeDef]) {
    let mut actual: Vec<&TypeDef> = actual.iter().collect();
    let mut expected: Vec<&TypeDef> = expected.iter().collect();
    actual.sort_by_key(|d| key(d));
    expected.sort_by_key(|d| key(d));
    let actual: Vec<String> = actual.iter().map(|d| identity(d)).collect();
    let expected: Vec<String> = expected.iter().map(|d| identity(d)).collect();
    assert_eq!(actual, expected);
}

/// EN-F4 byte-identical behaviour (engine-spec §3.1): a fresh store seeded from the shipped
/// `kaban-core.yaml` and `adr.yaml` holds exactly the 14 labels and 16 rels `SEED_LABELS` /
/// `SEED_RELS` held before, with the same names and descriptions and the other columns at their
/// defaults; `schema_describe` adds only `ns` (`core` or `adr` by the spec's membership lists).
#[test]
fn the_shipped_files_reproduce_todays_seed_rows_and_schema_describe_adds_only_ns() {
    let store = Arc::new(SqliteStore::open_memory().unwrap());
    let api = api_over(&store);

    let described = api.schema_describe().unwrap();

    let labels: Vec<TypeDef> = TODAYS_LABELS
        .iter()
        .map(|(n, d)| expected("label", n, d))
        .collect();
    let rels: Vec<TypeDef> = TODAYS_RELS
        .iter()
        .map(|(n, d)| expected("rel", n, d))
        .collect();
    assert_same(&described.labels, &labels);
    assert_same(&described.relationships, &rels);
    // E1.8 (E1.7 R1/R2): the declared columns are the shipped files' values, not the seed
    // placeholders; the `core` rels and the ungoverned kinds declare nothing
    let rel = |name: &str| {
        described
            .relationships
            .iter()
            .find(|d| d.name == name)
            .unwrap()
    };
    assert_eq!(rel("SUPERSEDES").from_kinds, vec!["Decision".to_owned()]);
    assert_eq!(
        rel("GOVERNS").to_kinds,
        vec!["CodeRef".to_owned(), "path".to_owned()]
    );
    assert!(rel("RELATES_TO").from_kinds.is_empty() && rel("RELATES_TO").to_kinds.is_empty());
    assert!(!rel("CONTRADICTS").directed);
    let label = |name: &str| described.labels.iter().find(|d| d.name == name).unwrap();
    assert!(label("Decision").props_schema.is_some());
    assert!(label("Decision").declaration.governed.is_some());
    assert!(label("Fact").declaration.governed.is_none());
    assert!(
        described
            .relationships
            .iter()
            .all(|d| d.declaration == kaban_core::TypeDeclaration::default())
    );
    // the raw store rows carry their namespace, persisted since 0004 (E1.7): the stored
    // name is bare in the table and `ns` is read from the column
    let raw = store.schema().unwrap();
    assert_eq!(raw.len(), 30);
    assert!(raw.iter().all(|d| d.ns.is_some() && d.builtin));
}

/// EN-N1: the three embedded strings total at most 20 KiB.
#[test]
fn test_ontology_bytes() {
    let total = kaban_core::shipped::META_SCHEMA_YAML.len()
        + kaban_core::shipped::KABAN_CORE_YAML.len()
        + kaban_core::shipped::ADR_YAML.len();
    assert!(
        total <= 20 * 1024,
        "embedded ontology strings: {total} bytes"
    );
    assert!(
        kaban_core::shipped::resolved().is_ok(),
        "the shipped files resolve"
    );
}

/// A reject-class canary (the builtin `aws-access-key-id` rule) that must never be stored.
const SECRET_CANARY: &str = "AKIAQWERTYUIOPASDFGH";

/// A file-backed repository scratch: `<root>/.kaban/ontology/`, a store and an API whose
/// containment root is `root`.
struct Repo {
    scratch: common::Scratch,
    store: Arc<SqliteStore>,
    api: KabanApi,
    registry: kaban_api::OntologyRegistry,
}

impl Repo {
    fn new(name: &str) -> Self {
        let scratch = common::Scratch::new("kaban-ontology", name);
        std::fs::create_dir_all(scratch.0.join(".kaban/ontology")).unwrap();
        let store = Arc::new(SqliteStore::open(&scratch.db(), &scratch.keys()).unwrap());
        let api = api_over(&store);
        let registry = kaban_api::OntologyRegistry::new(Some(scratch.0.clone())).unwrap();
        Self {
            scratch,
            store,
            api,
            registry,
        }
    }

    fn root(&self) -> &std::path::Path {
        &self.scratch.0
    }

    fn write(&self, name: &str, text: &str) -> std::path::PathBuf {
        let path = self.root().join(".kaban/ontology").join(name);
        std::fs::write(&path, text).unwrap();
        path
    }

    fn meta(&self, key: &str) -> Option<String> {
        kaban_core::IngestStore::index_meta(&*self.store)
            .unwrap()
            .get(key)
            .map(str::to_owned)
    }

    fn changes_len(&self) -> usize {
        kaban_core::ChangeLogStore::changes_read(&*self.store, 0, 10_000)
            .unwrap()
            .len()
    }

    /// Every byte of the database, its WAL and its audit export side (what a leak would land
    /// in), as one lossy string.
    fn disk_bytes(&self) -> String {
        let mut all = Vec::new();
        for suffix in ["", "-wal", "-shm"] {
            let mut name = self.scratch.db().into_os_string();
            name.push(suffix);
            if let Ok(bytes) = std::fs::read(&name) {
                all.extend(bytes);
            }
        }
        String::from_utf8_lossy(&all).into_owned()
    }
}

/// The shipped `adr.yaml` renamed to namespace `incidents` (the plan's fixture): the same eight
/// kinds and seven rels, the `Governed` mixin and Decision's EN-F3a schema. Every bare
/// reference is qualified with `incidents:` because a bare name declared both here and in
/// `adr` is `ambiguous_kind` by engine-spec §3.1 (no owning-namespace exception; E1.3a).
const INCIDENTS_YAML: &str = r#"ontology: incidents
version: 1
extends: [core]
mixins:
  Governed: {governed: {acceptance: human, serialize: none, export: {digest: false, marker: false},
                        display: {label: Record, plural: Records}, governs_paths: false, dedup: none}}
kinds:
  Fact: {description: "A verified statement.", props_schema: {type: object}}
  Idea: {description: "A proposal.", props_schema: {type: object}}
  Insight: {description: "A lesson learned.", props_schema: {type: object}}
  Improvement: {description: "A fix.", props_schema: {type: object}}
  Episode: {description: "Provenance.", props_schema: {type: object}}
  Decision:
    description: "A choice made, with its rationale."
    mixins: [incidents:Governed]
    props_schema: {"type": "object", "additionalProperties": false, "required": ["rationale"], "properties": {"decision": {"type": "string"}, "rationale": {"type": "string", "minLength": 1}}}
    governed: {acceptance: human, serialize: {dir: decisions, format: md-frontmatter},
               export: {digest: true, marker: true, heading: Decisions}, display: {label: Decision, plural: Decisions},
               governs_paths: true, dedup: title+scope}
    rank: {boost: 1.2, expand_rels: [CAUSED_BY, FIXED_BY, SUPERSEDES], expand_depth: 2}
    fts_fields: [props.rationale]
    min_confidence: 0.6
  Convention:
    description: "A rule the project follows."
    mixins: [incidents:Governed]
    props_schema: {type: object, additionalProperties: false, properties: {rule: {type: string}}}
    governed: {acceptance: agent, serialize: {dir: conventions, format: md-frontmatter},
               export: {digest: true, marker: true, heading: Conventions}, display: {label: Convention, plural: Conventions},
               governs_paths: true, dedup: title+scope}
  Failure:
    description: "Something that went wrong."
    mixins: [incidents:Governed]
    props_schema: {type: object, additionalProperties: false, properties: {error_text: {type: string}}}
rels:
  SUPERSEDES: {from: [incidents:Decision], to: [incidents:Decision], directed: true, description: "Newer replaces older.", on_link: archive_target, confidence_required: 0.95, governed_semantics: true}
  CONTRADICTS: {from: [], to: [], directed: false, description: "Two memories conflict."}
  INVALIDATES: {from: [], to: [], directed: true, description: "Target no longer valid."}
  DEPENDS_ON: {from: [], to: [], directed: true, description: "Dependency."}
  CAUSED_BY: {from: [incidents:Failure], to: [], directed: true, description: "Caused by the target."}
  FIXED_BY: {from: [incidents:Failure], to: [], directed: true, description: "Fixed by the target."}
  GOVERNS: {from: [incidents:Decision, incidents:Convention], to: [CodeRef, path], directed: true, description: "Governs the target.", governed_semantics: true}
discovered: {policy: review, cap: 500, prune_after_days: 90, promote_when: {min_count: 20, min_sources: 3, min_mean_score: 0.7}}
source_types: {adr: {kinds: [incidents:Decision, incidents:Fact], paths: ["docs/adr/**"]}}
default_kind: incidents:Fact
questions:
  governing_decisions: {kinds: governs_paths, where: {path_scope: {governs: $path}}, exclude_via: incidents:SUPERSEDES, order: [governance, currency], depth: 1}
"#;

fn incidents_yaml() -> String {
    INCIDENTS_YAML.to_owned()
}

/// EN-F2: a user namespace loads under stored `ns:Kind` names, `schema_describe` shows the
/// namespace and the stored name, `index_meta` records the blake3 hash and the version, and a
/// reload with the same bytes writes nothing (`changed: false`, zero `changes` rows).
#[test]
fn a_user_namespace_loads_under_stored_names_records_its_hash_and_reloads_idempotently() {
    let repo = Repo::new("load");
    let text = incidents_yaml();
    let path = repo.write("incidents.yaml", &text);
    let before = repo.changes_len();

    let report = repo.registry.load_file(&repo.api, &path).unwrap();

    assert_eq!(report.ns, "incidents");
    assert_eq!(report.version, 1);
    assert_eq!(
        report.hash,
        blake3::hash(text.as_bytes()).to_hex().to_string()
    );
    assert_eq!((report.kinds, report.rels), (8, 7));
    assert!(report.changed);
    let described = repo.api.schema_describe().unwrap();
    let decision = described
        .labels
        .iter()
        .find(|d| d.name == "incidents:Decision")
        .expect("stored under the qualified name");
    assert_eq!(decision.ns.as_deref(), Some("incidents"));
    assert!(!decision.builtin);
    assert!(
        decision.props_schema.is_some(),
        "the merged props_schema is stored"
    );
    let supersedes = described
        .relationships
        .iter()
        .find(|d| d.name == "incidents:SUPERSEDES")
        .unwrap();
    assert_eq!(
        supersedes.from_kinds,
        vec!["incidents:Decision".to_owned()],
        "endpoints are stored names"
    );
    // the shipped rows are untouched
    assert!(
        described
            .labels
            .iter()
            .any(|d| d.name == "Decision" && d.builtin)
    );
    assert_eq!(
        repo.meta("ontology_hash:incidents").as_deref(),
        Some(report.hash.as_str())
    );
    assert_eq!(
        repo.meta("ontology_version:incidents").as_deref(),
        Some("1")
    );
    assert_eq!(
        repo.changes_len(),
        before,
        "schema rows write no changes rows"
    );

    // the registry sees the kind
    let view = repo.registry.kind("incidents:Decision").unwrap();
    assert_eq!(view.id.to_string(), "incidents:Decision");
    assert!(view.governed.is_some());
    assert!(
        repo.registry
            .governing_kinds()
            .contains(&"incidents:Decision".to_owned())
    );

    // reload: same bytes, nothing written
    let again = repo.registry.load_file(&repo.api, &path).unwrap();
    assert!(!again.changed);
    assert_eq!(again.hash, report.hash);
    assert_eq!(repo.changes_len(), before);
    let list = repo.registry.list();
    assert_eq!(
        list.iter().map(|o| o.ns.as_str()).collect::<Vec<_>>(),
        ["core", "adr", "incidents"]
    );
}

/// EN-F2: a user file redefining `adr:Decision` fails `ontology_builtin_immutable` and stores
/// nothing; so does a bare `user` file that reuses a shipped name.
#[test]
fn a_user_file_redefining_a_builtin_fails_ontology_builtin_immutable_and_stores_nothing() {
    let repo = Repo::new("builtin");
    let adr = repo.write("adr.yaml", kaban_core::shipped::ADR_YAML);
    let err = repo.registry.load_file(&repo.api, &adr).unwrap_err();
    assert_eq!(err.to_string(), "ontology_builtin_immutable");
    assert!(repo.meta("ontology_hash:adr").is_none());

    let user = repo.write(
        "user.yaml",
        "ontology: user\nversion: 1\nkinds:\n  Decision:\n    description: \"mine\"\nrels: {}\n",
    );
    let err = repo.registry.load_file(&repo.api, &user).unwrap_err();
    assert_eq!(err.to_string(), "ontology_builtin_immutable");
    assert!(repo.meta("ontology_hash:user").is_none());
    let described = repo.api.schema_describe().unwrap();
    let decision = described
        .labels
        .iter()
        .find(|d| d.name == "Decision")
        .unwrap();
    assert_eq!(decision.description, "A choice made, with its rationale.");
}

/// EN-S8: a reject-class canary in a kind description fails the load with rule ids only,
/// `schema_type` is unchanged, and the canary reaches neither the database, its WAL, the
/// audit chain nor the error text.
#[test]
fn a_canary_in_a_description_is_rejected_with_rule_ids_only_and_nothing_is_stored() {
    let repo = Repo::new("canary");
    let text = incidents_yaml().replacen(
        "description: \"A choice made, with its rationale.\"",
        &format!("description: \"A choice made, key {SECRET_CANARY} leaked.\""),
        1,
    );
    assert!(
        text.contains(SECRET_CANARY),
        "the fixture carries the canary"
    );
    let path = repo.write("incidents.yaml", &text);
    let before = repo.api.schema_describe().unwrap();

    let err = repo.registry.load_file(&repo.api, &path).unwrap_err();

    match &err {
        kaban_api::ApiError::Rejected { rules, .. } => {
            assert!(rules.iter().any(|r| r == "aws-access-key-id"), "{rules:?}");
        }
        other => panic!("expected a rejection, got {other}"),
    }
    assert!(!err.to_string().contains(SECRET_CANARY));
    let after = repo.api.schema_describe().unwrap();
    assert_eq!(
        format!("{before:?}"),
        format!("{after:?}"),
        "schema_type unchanged"
    );
    assert!(repo.meta("ontology_hash:incidents").is_none());
    assert!(repo.registry.kind("incidents:Decision").is_none());
    // the rejection is audited (rule ids and keyed hashes only), never the value
    let audit = kaban_core::AuditStore::audit_verify(&*repo.store).unwrap();
    assert!(
        audit.ok && audit.entries >= 1,
        "the rejection is on the chain: {audit:?}"
    );
    assert!(!repo.disk_bytes().contains(SECRET_CANARY), "canary on disk");
}

/// EN-S7 (E1.3a RE1-2): a FIFO named `blocked.yaml` under `.kaban/ontology/` cannot hold the
/// startup load: discovery skips the non-regular entry and loads the regular files beside it;
/// the load is bounded by a 2 s test-side timeout so a regression fails instead of hanging.
#[test]
fn startup_discovery_skips_a_fifo_and_loads_the_regular_files_without_blocking() {
    let repo = Repo::new("fifo");
    repo.write("incidents.yaml", &incidents_yaml());
    let fifo = repo.root().join(".kaban/ontology/blocked.yaml");
    let status = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .unwrap();
    assert!(status.success(), "mkfifo(1)");

    let (tx, rx) = std::sync::mpsc::channel();
    let api = repo.api.clone();
    let registry = repo.registry.clone();
    std::thread::spawn(move || {
        let _ = tx.send(
            registry
                .load_startup(&api)
                .map(|reports| reports.into_iter().map(|r| r.ns).collect::<Vec<_>>()),
        );
    });

    let loaded = rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("the startup load returned within 2 s (a FIFO must not block it)")
        .unwrap();
    assert_eq!(loaded, ["incidents".to_owned()]);
    assert!(repo.registry.kind("incidents:Decision").is_some());
    // the FIFO itself, named explicitly, is refused as not a regular file, at once
    let err = repo.registry.load_file(&repo.api, &fifo).unwrap_err();
    assert!(
        err.to_string()
            .contains("is a symlink, lies outside the repository, or changed"),
        "{err}"
    );
}

/// EN-F2 (E1.3a RE1-7): two concurrent loads of different namespaces into one registry both end
/// up published; a lost update (both starting from the shipped state, the second publication
/// overwriting the first) would drop one. Twenty rounds on a fresh registry each, two scoped
/// threads per round.
#[test]
fn two_concurrent_loads_of_different_namespaces_are_both_published_every_time() {
    let repo = Repo::new("concurrent");
    let minimal = |ns: &str| {
        format!(
            "ontology: {ns}\nversion: 1\nkinds:\n  Thing: {{description: \"a {ns} thing\"}}\nrels: {{}}\n"
        )
    };
    let a = repo.write("a.yaml", &minimal("alpha"));
    let b = repo.write("b.yaml", &minimal("beta"));

    for round in 0..20 {
        let registry = kaban_api::OntologyRegistry::new(Some(repo.root().to_path_buf())).unwrap();
        let barrier = std::sync::Barrier::new(2);
        std::thread::scope(|s| {
            for path in [&a, &b] {
                let (registry, api, barrier) = (&registry, &repo.api, &barrier);
                s.spawn(move || {
                    barrier.wait();
                    registry.load_file(api, path).unwrap();
                });
            }
        });
        let loaded: Vec<String> = registry.list().into_iter().map(|o| o.ns).collect();
        assert!(
            loaded.contains(&"alpha".to_owned()) && loaded.contains(&"beta".to_owned()),
            "round {round}: {loaded:?}"
        );
        assert!(registry.kind("alpha:Thing").is_some() && registry.kind("beta:Thing").is_some());
    }
}

/// The pre-0004 store seam (RP-1): `schema_extend` keeps today's texts for `vendor`,
/// `Vendor-X` and `INCIDENTS:Decision`, accepts the stored name `incidents:Decision`, still
/// refuses a builtin row, and a `remember` with kind `incidents:Decision` persists and recalls
/// under that stored name.
#[test]
fn schema_extend_accepts_stored_names_keeps_todays_rejections_and_remember_round_trips() {
    let store = Arc::new(SqliteStore::open_memory().unwrap());
    let api = api_over(&store);
    let extend = |name: &str| {
        api.schema_extend(kaban_api::SchemaExtendRequest {
            kind: "label".into(),
            name: name.into(),
            description: "an incident decision".into(),
            from_kinds: vec![],
            to_kinds: vec![],
            directed: None,
            props_schema: None,
        })
    };
    for bad in ["vendor", "Vendor-X", "INCIDENTS:Decision"] {
        let err = extend(bad).unwrap_err().to_string();
        assert_eq!(
            err,
            format!("invalid name '{bad}': labels are PascalCase alphanumerics, <=64 chars"),
            "{bad}"
        );
    }
    let builtin = extend("Decision").unwrap_err().to_string();
    assert_eq!(builtin, "type 'Decision' is builtin and cannot be changed");

    let def = extend("incidents:Decision").unwrap();
    assert_eq!(def.name, "incidents:Decision");
    let described = api.schema_describe().unwrap();
    let row = described
        .labels
        .iter()
        .find(|d| d.name == "incidents:Decision")
        .unwrap();
    assert_eq!(row.ns.as_deref(), Some("incidents"));
    let custom = described.labels.iter().find(|d| d.name == "Custom");
    assert!(custom.is_none());

    let id = api
        .remember(kaban_api::RememberRequest {
            kind: "incidents:Decision".into(),
            text: "retry the flaky deploy step once".into(),
            ..Default::default()
        })
        .unwrap()
        .id;
    let hits = api
        .recall(kaban_api::RecallRequest {
            query: "flaky deploy".into(),
            ..Default::default()
        })
        .unwrap();
    let card = hits.results.iter().find(|c| c.id == id).expect("recalled");
    assert_eq!(card.kind, "incidents:Decision");
    // a bare user row reads as the `user` namespace
    extend("Custom").unwrap();
    let described = api.schema_describe().unwrap();
    let custom = described
        .labels
        .iter()
        .find(|d| d.name == "Custom")
        .unwrap();
    assert_eq!(custom.ns.as_deref(), Some("user"));
}

/// E1.7 R2 (engine-plan E1.8): the five `schema_type` JSON declaration columns are written
/// from the loaded ontology at the load seam, shipped and user alike, as canonical JSON, and
/// `schema_describe` exposes them: a loaded `incidents:Incident` reads back its `governed`
/// block and `display`; the shipped `Decision` carries its `governed`, `rank` and `extract`;
/// a kind declaring none (`incidents:Decision`, `Fact`) has `None`, never `{}`.
#[test]
fn the_json_declaration_columns_are_written_at_the_load_seam_and_described() {
    let repo = Repo::new("json-columns");
    // the kaban-core fixture: a governed `Incident` with a file, a governed `Outage` without
    // one, an ungoverned `Decision`
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../kaban-core/fixtures/ontology/incidents.yaml");
    let path = repo.write("incidents.yaml", &std::fs::read_to_string(fixture).unwrap());
    repo.registry.load_file(&repo.api, &path).unwrap();
    let described = repo.api.schema_describe().unwrap();
    let label = |name: &str| {
        described
            .labels
            .iter()
            .find(|d| d.name == name)
            .unwrap_or_else(|| panic!("{name}"))
            .clone()
    };

    let incident = label("incidents:Incident");
    let governed = incident.declaration.governed.expect("Incident is governed");
    assert_eq!(governed["acceptance"], "human");
    assert_eq!(governed["serialize"]["dir"], "incidents");
    assert_eq!(governed["serialize"]["format"], "md-frontmatter");
    assert_eq!(governed["export"]["heading"], "Incidents");
    assert_eq!(governed["governs_paths"], true);
    assert_eq!(governed["dedup"], "none");
    // the block as declared (an undeclared optional leaf such as `icon` renders `null`: the
    // meta types serialize by derive only, E1.8)
    let display = incident.declaration.display.clone().unwrap();
    assert_eq!(
        (&display["label"], &display["plural"]),
        (
            &serde_json::json!("Incident"),
            &serde_json::json!("Incidents")
        )
    );
    assert!(
        incident.declaration.rank.is_none()
            && incident.declaration.extract.is_none()
            && incident.declaration.column_weights.is_none()
    );
    // canonical JSON (E1.1 `canon.rs`): sorted keys, no spaces
    assert_eq!(
        serde_json::to_string(&governed).unwrap(),
        kaban_core::canonical_json(&governed)
    );

    let outage = label("incidents:Outage");
    assert_eq!(outage.declaration.governed.unwrap()["serialize"], "none");

    let decision = label("Decision");
    assert!(decision.builtin);
    assert_eq!(
        decision.declaration.governed.unwrap()["dedup"],
        "title+scope"
    );
    assert_eq!(decision.declaration.rank.unwrap()["expand_depth"], 2);
    assert_eq!(decision.declaration.extract.unwrap()["enabled"], true);
    assert_eq!(decision.declaration.display.unwrap()["plural"], "Decisions");

    for plain in ["incidents:Decision", "Fact"] {
        let d = label(plain);
        assert!(
            d.declaration.governed.is_none()
                && d.declaration.rank.is_none()
                && d.declaration.display.is_none(),
            "{plain} declares none"
        );
    }
}
