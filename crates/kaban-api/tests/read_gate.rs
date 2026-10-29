//! Read path (core-spec S4; review R1-4): one output projection gates every outgoing string,
//! not only note text. Strings accepted under an older policy (a kind, a workspace, a
//! relationship type, a schema description) come back masked when a newer policy redacts them
//! and are hidden when it rejects them.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::Scratch;
use kaban_api::{
    ApiConfig, KabanApi, LinkRequest, RecallRequest, RememberRequest, SchemaExtendRequest,
    StatsRequest, TraverseRequest,
};
use kaban_core::WorkspaceId;
use kaban_policy::Policy;
use kaban_store_sqlite::SqliteStore;
use std::sync::Arc;
use std::time::Duration;

const REDACT: &str = "hunter2";
const REJECT: &str = "nightjar";
const MASK: &str = "[REDACTED:old-password]";
const STRICT: &str = "words:\n  - { term: hunter2, action: redact, id: old-password }\n  - { term: nightjar, action: reject, id: codename }\n";

fn api_with(store: &Arc<SqliteStore>, policy: &str, workspace: &str) -> KabanApi {
    let policy = Policy::from_file(Policy::parse(policy.as_bytes()).unwrap(), "t".into()).unwrap();
    KabanApi::with_config(
        Arc::clone(store) as Arc<dyn kaban_core::GraphStore>,
        policy,
        store.signer(),
        None,
        ApiConfig {
            workspace: WorkspaceId::new(workspace),
            ..Default::default()
        },
    )
}

fn label(name: &str, description: &str) -> SchemaExtendRequest {
    SchemaExtendRequest {
        kind: "label".into(),
        name: name.into(),
        description: description.into(),
        ..Default::default()
    }
}

fn contains_ci(s: &str, needle: &str) -> bool {
    s.to_lowercase().contains(needle)
}

#[test]
fn strings_forbidden_by_a_newer_policy_are_masked_or_hidden_on_every_read_surface() {
    let store = Arc::new(SqliteStore::open_memory().unwrap());
    // 1) everything is written under a permissive policy, in a workspace named after the term
    let lax = api_with(&store, "", REDACT);
    lax.schema_extend(label("Hunter2", "kind named after hunter2"))
        .unwrap();
    lax.schema_extend(label("Nightjar", "codename kind"))
        .unwrap();
    lax.schema_extend(SchemaExtendRequest {
        kind: "rel".into(),
        name: "HUNTER2_LINKS".into(),
        description: "relationship named after the term".into(),
        ..Default::default()
    })
    .unwrap();
    lax.schema_extend(SchemaExtendRequest {
        kind: "rel".into(),
        name: "NIGHTJAR_LINKS".into(),
        description: "codename relationship".into(),
        ..Default::default()
    })
    .unwrap();
    let remember = |kind: &str, text: &str| {
        lax.remember(RememberRequest {
            kind: kind.into(),
            text: text.into(),
            ..Default::default()
        })
        .unwrap()
        .id
    };
    let a = remember("Hunter2", "alpha note in the legacy kind");
    let b = remember("Fact", "beta note linked from alpha");
    let c = remember("Nightjar", "gamma note in the codename kind");
    for (to, rel) in [(&b, "HUNTER2_LINKS"), (&c, "NIGHTJAR_LINKS")] {
        lax.link(LinkRequest {
            from: a.clone(),
            to: to.clone(),
            rel: rel.into(),
            ..Default::default()
        })
        .unwrap();
    }

    // 2) the same store read under a policy that redacts hunter2 and rejects nightjar
    let strict = api_with(&store, STRICT, REDACT);
    let recall = strict
        .recall(RecallRequest {
            query: "note".into(),
            full_text: true,
            limit: Some(10),
            ..Default::default()
        })
        .unwrap();
    let text = serde_json::to_string(&recall).unwrap();
    assert!(!contains_ci(&text, REDACT), "recall: {text}");
    assert!(!contains_ci(&text, REJECT), "recall: {text}");
    let alpha = recall.results.iter().find(|x| x.id == a).unwrap();
    assert_eq!(alpha.kind, MASK);
    assert_eq!(alpha.workspace_id, MASK);
    assert!(
        alpha.redactions.iter().any(|r| r == "kind:old-password")
            && alpha
                .redactions
                .iter()
                .any(|r| r == "workspace_id:old-password"),
        "{:?}",
        alpha.redactions
    );
    assert!(
        recall.results.iter().all(|x| x.id != c),
        "hidden kind returned"
    );

    let t = strict
        .traverse(TraverseRequest {
            start_ids: vec![a.clone()],
            ..Default::default()
        })
        .unwrap();
    let text = serde_json::to_string(&t).unwrap();
    assert!(!contains_ci(&text, REDACT), "traverse: {text}");
    assert!(!contains_ci(&text, REJECT), "traverse: {text}");
    assert!(t.nodes.iter().all(|n| n.kind != "Hunter2" && n.id != c));
    assert_eq!(t.edges.len(), 1, "{t:?}");
    assert_eq!(t.edges[0].rel, format!("{MASK}_LINKS"));

    let schema = strict.schema_describe().unwrap();
    let text = serde_json::to_string(&schema).unwrap();
    assert!(!contains_ci(&text, REDACT), "schema: {text}");
    assert!(!contains_ci(&text, REJECT), "schema: {text}");
    assert!(
        schema
            .labels
            .iter()
            .any(|d| d.name == MASK && d.description == format!("kind named after {MASK}")),
        "{text}"
    );
    assert!(
        schema
            .relationships
            .iter()
            .any(|d| d.name == format!("{MASK}_LINKS")),
        "{text}"
    );

    for workspace_id in [None, Some("*".to_string())] {
        let stats = strict.stats(StatsRequest { workspace_id }).unwrap();
        let text = serde_json::to_string(&stats).unwrap();
        assert!(!contains_ci(&text, REDACT), "stats: {text}");
        assert!(!contains_ci(&text, REJECT), "stats: {text}");
        assert_eq!(stats.store["by_kind"][MASK], serde_json::json!(1), "{text}");
        assert_eq!(
            stats.store["by_rel"][format!("{MASK}_LINKS")],
            serde_json::json!(1),
            "{text}"
        );
    }
}

/// Spec 05 SS-11 (correction fix8b): the read gate decides eligibility inside the store's
/// recall, where the store holds its own connection, so the predicate reads no store state
/// and the hidden read's audit row is written after the recall returns. A higher-ranked
/// record the policy rejects is skipped and the eligible one delivered. Under a timeout: a
/// recall blocked on its own store is a failure to report, never one to wait out (the first
/// predicate audited from inside the lock and hung every policy recall that hid a record).
#[test]
fn a_recall_that_hides_a_record_returns_the_eligible_one_and_audits_the_hidden_read() {
    let s = Scratch::new("kaban-api-read-gate", "hidden-refill");
    let store = Arc::new(SqliteStore::open(&s.db(), &s.keys()).unwrap());
    let lax = api_with(&store, "", "ws");
    let remember = |text: &str| {
        lax.remember(RememberRequest {
            kind: "Fact".into(),
            text: text.into(),
            ..Default::default()
        })
        .unwrap()
        .id
    };
    let hidden = remember("nightjar heron count at the marsh");
    let shown = remember("heron count at the estuary");
    let (tx, rx) = std::sync::mpsc::channel();
    let reader = Arc::clone(&store);
    std::thread::spawn(move || {
        let strict = api_with(&reader, STRICT, "ws");
        let ids = strict
            .recall(RecallRequest {
                query: "heron count".into(),
                limit: Some(10),
                ..Default::default()
            })
            .map(|r| r.results.into_iter().map(|c| c.id).collect::<Vec<_>>());
        let _ = tx.send(ids);
    });
    let ids = rx
        .recv_timeout(Duration::from_secs(3))
        .expect("the recall did not return within 3 s: a lock held across the read gate")
        .unwrap();
    assert_eq!(ids, vec![shown], "hidden {hidden}");
    // the hidden read is audited once, by its reject verdict, after the recall
    let raw = rusqlite::Connection::open(s.db()).unwrap();
    let ops: Vec<String> = raw
        .prepare("SELECT op FROM audit ORDER BY seq")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(ops, vec!["reject".to_string()]);
    let seen: Vec<String> = raw
        .prepare("SELECT node_id FROM read_audit_seen")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(seen, vec![hidden]);
}
