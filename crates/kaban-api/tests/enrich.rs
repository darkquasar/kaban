//! Public-behaviour tests of the enrichment seam and `follow_ups` (core-spec F19; core-plan
//! C3.9, M3-27; M3 review R3-10) through `KabanApi`: an `enrich[]` id nobody configured is
//! refused with the documented error; follow-ups are built from the masked `CodeRef` pointers
//! linked to the visible cards (`recall` and `why`) and a hidden pointer yields none; a
//! configured enricher runs after card assembly and its output passes the read gate.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

use kaban_api::{
    ApiConfig, ApiError, CodeRefWire, EnrichBudget, Enricher, Enrichment, FollowUpTool, KabanApi,
    RecallRequest, RememberRequest, WhyRequest, WhyTarget,
};
use kaban_core::{CodeRef, WorkspaceId};
use kaban_policy::Policy;
use kaban_store_sqlite::SqliteStore;
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Hits the builtin credential rule `aws-access-key-id` (always reject) in every view.
const CANARY: &str = "AKIAQWERTYUIOPASDFGH";
const REDACT: &str = "hunter2";
const REJECT: &str = "nightjar";
const MASK: &str = "[REDACTED:old-password]";
const STRICT: &str = "words:\n  - { term: hunter2, action: redact, id: old-password }\n  - { term: nightjar, action: reject, id: codename }\n";

fn api_over(store: &Arc<SqliteStore>, policy: &str) -> KabanApi {
    let policy = Policy::from_file(Policy::parse(policy.as_bytes()).unwrap(), "t".into()).unwrap();
    KabanApi::with_config(
        Arc::clone(store) as Arc<dyn kaban_core::GraphStore>,
        policy,
        store.signer(),
        None,
        ApiConfig {
            workspace: WorkspaceId::new("enrich"),
            ..Default::default()
        },
    )
}

fn api() -> KabanApi {
    api_over(&Arc::new(SqliteStore::open_memory().unwrap()), "")
}

fn coderef(relative_path: &str, name_path: Option<&str>) -> CodeRefWire {
    CodeRefWire {
        repo: "kaban".into(),
        commit: "8d10f88".into(),
        tool: "manual".into(),
        relative_path: relative_path.into(),
        name_path: name_path.map(str::to_owned),
        ..Default::default()
    }
}

fn remember_about(api: &KabanApi, text: &str, refs: Vec<CodeRefWire>) {
    api.remember(RememberRequest {
        kind: "Fact".into(),
        text: text.into(),
        refs,
        ..Default::default()
    })
    .unwrap();
}

fn recall(api: &KabanApi, enrich: &[&str]) -> Result<kaban_api::RecallResponse, ApiError> {
    api.recall(RecallRequest {
        query: "quokka".into(),
        hops: Some(1),
        enrich: enrich.iter().map(|s| (*s).to_owned()).collect(),
        ..Default::default()
    })
}

fn invalid(r: Result<kaban_api::RecallResponse, ApiError>) -> String {
    match r {
        Err(ApiError::Invalid(m)) => m,
        other => panic!("expected Invalid, got {other:?}"),
    }
}

/// M3-27: `enrich: ["serena"]` with no configured enricher is the documented error; the id is
/// content, so a canary id is rejected by the denylist and never echoed.
#[test]
fn an_enricher_nobody_configured_is_refused_naming_the_configured_ones() {
    let api = api();
    remember_about(&api, "quokka plain", vec![]);
    assert_eq!(
        invalid(recall(&api, &["serena"])),
        "unknown enricher 'serena'; configured: none"
    );
    // an empty list runs nothing and the response has no enrichments
    let r = recall(&api, &[]).unwrap();
    assert_eq!(r.enrichments, Vec::new());
    assert_eq!(r.count, 1);
    match recall(&api, &[CANARY]) {
        Err(ApiError::Rejected { field, rules }) => {
            assert_eq!(field, "enrich");
            assert!(rules.iter().any(|r| r == "aws-access-key-id"), "{rules:?}");
        }
        other => panic!("a canary id must be rejected before anything runs, got {other:?}"),
    }
}

/// M3-27, R3-10: follow-ups come from the projected (masked) `CodeRef` pointers linked to the
/// visible cards: a redacted path stays masked in `args`, a pointer hidden by a reject rule
/// yields no follow-up, and a named symbol adds `serena.find_referencing_symbols`.
#[test]
fn follow_ups_use_the_masked_pointer_and_a_hidden_card_yields_none() {
    let store = Arc::new(SqliteStore::open_memory().unwrap());
    let lax = api_over(&store, "");
    remember_about(
        &lax,
        "quokka touches two files",
        vec![
            coderef(&format!("src/{REDACT}/a.rs"), Some("Store/open")),
            coderef(&format!("src/{REJECT}/b.rs"), None),
        ],
    );
    // under the lax policy both pointers are suggested: find_symbol for each, references for a
    let r = recall(&lax, &[]).unwrap();
    assert_eq!(r.count, 3, "note plus two CodeRef cards");
    let tools: Vec<FollowUpTool> = r.follow_ups.iter().map(|f| f.tool).collect();
    assert_eq!(
        tools,
        vec![
            FollowUpTool::SerenaFindSymbol,
            FollowUpTool::SerenaFindReferencingSymbols,
            FollowUpTool::SerenaFindSymbol
        ]
    );
    assert_eq!(
        r.follow_ups[0].args,
        json!({"name_path": "Store/open", "relative_path": format!("src/{REDACT}/a.rs")})
    );

    // under the strict policy the same store answers with masked and hidden cards
    let strict = api_over(&store, STRICT);
    let r = recall(&strict, &[]).unwrap();
    assert_eq!(r.count, 2, "the nightjar CodeRef card is hidden");
    let tools: Vec<FollowUpTool> = r.follow_ups.iter().map(|f| f.tool).collect();
    assert_eq!(
        tools,
        vec![
            FollowUpTool::SerenaFindSymbol,
            FollowUpTool::SerenaFindReferencingSymbols
        ],
        "{:?}",
        r.follow_ups
    );
    let masked = format!("src/{MASK}/a.rs");
    for f in &r.follow_ups {
        assert_eq!(f.args["relative_path"], json!(masked), "{f:?}");
        assert!(f.reason.contains(&masked), "{}", f.reason);
    }
    let bytes = serde_json::to_string(&r).unwrap().to_lowercase();
    assert!(!bytes.contains(REDACT), "{bytes}");
    assert!(!bytes.contains(REJECT), "{bytes}");
}

/// An enricher that records how many refs it saw and echoes each pointer, plus one entry
/// carrying a reject-class canary.
struct Fake {
    calls: AtomicUsize,
}

impl Enricher for Fake {
    #[allow(clippy::unnecessary_literal_bound)] // the trait signature binds the lifetime
    fn id(&self) -> &str {
        "fake"
    }

    fn enrich(&self, refs: &[CodeRef], budget: EnrichBudget) -> Result<Vec<Enrichment>, ApiError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        assert!(refs.len() <= budget.max_refs);
        let mut out: Vec<Enrichment> = refs
            .iter()
            .map(|r| Enrichment {
                enricher: "fake".into(),
                r#ref: r.clone(),
                text: format!("about {}", r.relative_path),
            })
            .collect();
        if let Some(first) = refs.first() {
            out.push(Enrichment {
                enricher: "fake".into(),
                r#ref: first.clone(),
                text: format!("leaked {CANARY}"),
            });
        }
        Ok(out)
    }
}

/// The seam: a configured enricher runs once per recall on the result's pointers (after card
/// assembly) and its output passes the read gate; an unknown id names the configured one.
#[test]
fn a_configured_enricher_runs_on_the_pointers_and_its_output_is_gated() {
    let fake = Arc::new(Fake {
        calls: AtomicUsize::new(0),
    });
    let api = api().with_enrichers(vec![Arc::clone(&fake) as Arc<dyn Enricher>]);
    remember_about(
        &api,
        "quokka reads the index",
        vec![coderef("src/index.rs", Some("Index/read"))],
    );
    assert_eq!(
        invalid(recall(&api, &["serena"])),
        "unknown enricher 'serena'; configured: fake"
    );
    assert_eq!(fake.calls.load(Ordering::SeqCst), 0, "nothing ran");

    let r = recall(&api, &["fake", "fake"]).unwrap();
    assert_eq!(
        fake.calls.load(Ordering::SeqCst),
        1,
        "named twice, run once"
    );
    assert_eq!(r.enrichments.len(), 1, "{:?}", r.enrichments);
    assert_eq!(r.enrichments[0].enricher, "fake");
    assert_eq!(r.enrichments[0].r#ref.relative_path, "src/index.rs");
    assert_eq!(r.enrichments[0].text, "about src/index.rs");
    let bytes = serde_json::to_string(&r).unwrap();
    assert!(!bytes.contains(CANARY), "{bytes}");

    // the pointer belongs to the note, not to the graph expansion (R3-10): with `hops: 0` the
    // CodeRef card is absent from the result and the hint is still made
    let r = api
        .recall(RecallRequest {
            query: "quokka".into(),
            hops: Some(0),
            enrich: vec!["fake".into()],
            ..Default::default()
        })
        .unwrap();
    assert_eq!(r.count, 1, "the note alone: {:?}", r.results);
    assert_eq!(fake.calls.load(Ordering::SeqCst), 2);
    assert_eq!(r.enrichments.len(), 1, "{:?}", r.enrichments);
    assert_eq!(
        r.follow_ups[0].args,
        json!({"name_path": "Index/read", "relative_path": "src/index.rs"})
    );

    // a recall without CodeRef pointers hands the enricher nothing and does not call it
    remember_about(&api, "wombat has no pointer", vec![]);
    let r = api
        .recall(RecallRequest {
            query: "wombat".into(),
            enrich: vec!["fake".into()],
            ..Default::default()
        })
        .unwrap();
    assert_eq!(r.count, 1);
    assert_eq!(fake.calls.load(Ordering::SeqCst), 2);
    assert_eq!(r.enrichments, Vec::new());
    assert_eq!(r.follow_ups, Vec::new());
}

fn why_path(api: &KabanApi, path: &str) -> kaban_api::WhyResponse {
    api.why(WhyRequest {
        targets: vec![WhyTarget::Path(path.into())],
        ..Default::default()
    })
    .unwrap()
}

/// F19 names `why` too (M3 review R3-10): the follow-ups of a governing record come from the
/// `CodeRef` nodes linked to it (`refs` are stored as `ABOUT` edges), projected through the
/// read gate like a card: a masked path stays masked in `args`, a `CodeRef` hidden by a reject
/// rule yields nothing, and a record without pointers yields an empty list.
#[test]
fn why_suggests_follow_ups_from_the_coderefs_linked_to_a_governing_record() {
    let store = Arc::new(SqliteStore::open_memory().unwrap());
    let lax = api_over(&store, "");
    lax.remember(RememberRequest {
        kind: "Decision".into(),
        text: "quokka stays on sqlite".into(),
        path_scope: vec!["src/store/**".into()],
        props: Some(json!({"rationale": "no server to run"})),
        refs: vec![
            coderef(&format!("src/{REDACT}/open.rs"), Some("Store/open")),
            coderef(&format!("src/{REJECT}/pool.rs"), None),
        ],
        ..Default::default()
    })
    .unwrap();
    lax.remember(RememberRequest {
        kind: "Convention".into(),
        text: "quokka names tests after behaviour".into(),
        path_scope: vec!["tests/**".into()],
        ..Default::default()
    })
    .unwrap();

    // the Decision governs the path; its two pointers are suggested in link order
    let r = why_path(&lax, "src/store/sqlite.rs");
    assert_eq!(r.results.len(), 1, "{r:?}");
    assert_eq!(r.results[0].node.kind, "Decision");
    let tools: Vec<FollowUpTool> = r.follow_ups.iter().map(|f| f.tool).collect();
    assert_eq!(
        tools,
        vec![
            FollowUpTool::SerenaFindSymbol,
            FollowUpTool::SerenaFindReferencingSymbols,
            FollowUpTool::SerenaFindSymbol
        ],
        "{:?}",
        r.follow_ups
    );
    assert_eq!(
        r.follow_ups[0].args,
        json!({"name_path": "Store/open", "relative_path": format!("src/{REDACT}/open.rs")})
    );
    assert_eq!(
        r.follow_ups[2].args,
        json!({"relative_path": format!("src/{REJECT}/pool.rs")})
    );

    // a governing record without pointers: present on the wire, empty
    let r = why_path(&lax, "tests/why.rs");
    assert_eq!(r.results.len(), 1, "{r:?}");
    assert_eq!(r.follow_ups, Vec::new());
    let v = serde_json::to_value(&r).unwrap();
    assert_eq!(v["follow_ups"], json!([]), "{v}");

    // under the strict policy the same store answers with a masked and a hidden pointer
    let strict = api_over(&store, STRICT);
    let r = why_path(&strict, "src/store/sqlite.rs");
    assert_eq!(r.results.len(), 1, "the Decision itself is clean: {r:?}");
    let tools: Vec<FollowUpTool> = r.follow_ups.iter().map(|f| f.tool).collect();
    assert_eq!(
        tools,
        vec![
            FollowUpTool::SerenaFindSymbol,
            FollowUpTool::SerenaFindReferencingSymbols
        ],
        "the nightjar CodeRef is hidden: {:?}",
        r.follow_ups
    );
    let masked = format!("src/{MASK}/open.rs");
    for f in &r.follow_ups {
        assert_eq!(f.args["relative_path"], json!(masked), "{f:?}");
        assert!(f.reason.contains(&masked), "{}", f.reason);
    }
    let bytes = serde_json::to_string(&r).unwrap().to_lowercase();
    assert!(!bytes.contains(REDACT), "{bytes}");
    assert!(!bytes.contains(REJECT), "{bytes}");
}
