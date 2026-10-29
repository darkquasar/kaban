//! EN-F49 (engine-plan E2.8) through `KabanApi`: a candidate (a `node_candidate` row, EN-F5a)
//! is absent from `recall` and `traverse` unless the request says `include_candidates`; the
//! predicate is the candidate row, not the group or the namespace (the coordinator text of
//! 2026-10-26): the subject of an `unresolved` group carries no row and reads like any record.
//! `explain` answers every address. OD-E-6: the `SessionStart` briefing lists up to
//! `[briefing] max_candidates` pending review items with their `kaban explain` address under a
//! `kaban review:` header; `0` removes the section; a settled item never appears; the section
//! renders with no accepted record at all.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

#[path = "common/status.rs"]
mod status;

use kaban_api::{
    ApiConfig, ExplainStatusRequest, HookEvent, HookHost, KabanApi, LinkRequest, PendingStatus,
    RecallResponse, RememberRequest, Status, TraverseResponse,
};
use kaban_core::config::{Briefing, EngineConfig};
use kaban_core::{GraphStore, NodeId};
use kaban_policy::Policy;
use kaban_store_sqlite::SqliteStore;
use serde_json::{Value, json};
use status::{Items, Scratch, items};
use std::sync::Arc;

/// The coverage fixture plus the links that make its candidates reachable from the plain
/// `active` Fact: `active -> chunk` (a `below_margin` span vote naming no object, so no
/// candidate row: OWN116), `active -> discovered` (an `x:Widget`, `discovered_kind`),
/// `active -> chunk_unresolved` (no candidate row).
fn linked(s: &Scratch) -> Items {
    let it = items(s);
    for to in [
        it.chunk.to_string(),
        it.discovered.clone(),
        it.chunk_unresolved.to_string(),
    ] {
        it.api
            .link(LinkRequest {
                from: it.active.clone(),
                rel: "RELATES_TO".into(),
                to,
                confidence: None,
                props: None,
            })
            .unwrap();
    }
    it
}

/// Requests built from their wire JSON, the shape the MCP router hands the service.
fn recall(api: &KabanApi, v: Value) -> RecallResponse {
    api.recall(serde_json::from_value(v).unwrap()).unwrap()
}
fn traverse(api: &KabanApi, v: Value) -> TraverseResponse {
    api.traverse(serde_json::from_value(v).unwrap()).unwrap()
}
fn node_ids(r: &TraverseResponse) -> Vec<String> {
    let mut v: Vec<String> = r.nodes.iter().map(|n| n.id.clone()).collect();
    v.sort();
    v
}

/// The service over the fixture's store with `[briefing] max_candidates = max`.
fn api_with_max(store: &Arc<SqliteStore>, max: u32) -> KabanApi {
    let engine = EngineConfig {
        briefing: Briefing {
            max_candidates: max,
        },
        ..Default::default()
    };
    KabanApi::with_config(
        Arc::clone(store) as Arc<dyn GraphStore>,
        Policy::empty(),
        store.signer(),
        None,
        ApiConfig {
            engine: Arc::new(engine),
            ..Default::default()
        },
    )
}

/// The `additionalContext` of a Claude Code `SessionStart` call (`None` when the hook
/// answered `{}`).
fn session_start(api: &KabanApi) -> Option<String> {
    let outcome = api.hook(
        HookHost::ClaudeCode,
        HookEvent::SessionStart,
        &json!({"session_id": "sess-e28", "hook_event_name": "SessionStart", "cwd": "/tmp"}),
    );
    assert_eq!(outcome.note, None, "{outcome:?}");
    outcome.output["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .map(str::to_owned)
}

/// The lines after the `kaban review:` header.
fn review_lines(context: &str) -> Vec<String> {
    context
        .split_once("kaban review:\n")
        .map(|(_, rest)| rest.lines().map(str::to_owned).collect())
        .unwrap_or_default()
}

/// EN-F49: by default `recall` and `traverse` omit the candidate (the `x:` record) and keep
/// both chunks (OWN116: a span claim's candidate state is never its source chunk's, so the
/// `below_margin` vote's chunk stays visible beside the unresolved group's);
/// `include_candidates: true` returns the candidate too; `explain` answers each of the three
/// addresses by default.
#[test]
fn default_reads_omit_candidates_and_the_flag_includes_them() {
    let s = Scratch::new("e28-candidates-reads");
    let it = linked(&s);
    let api = &it.api;

    // recall: the `x:Widget` record neither seeds the result nor joins through the hop
    let plain = recall(api, json!({"query": "widget"}));
    assert!(
        plain.results.iter().all(|c| c.id != it.discovered),
        "{plain:?}"
    );
    let with = recall(api, json!({"query": "widget", "include_candidates": true}));
    assert!(
        with.results.iter().any(|c| c.id == it.discovered),
        "{with:?}"
    );

    // traverse: the candidate is not a far endpoint; both chunks are
    let plain = traverse(api, json!({"start_ids": [it.active]}));
    let mut want = vec![
        it.active.clone(),
        it.chunk.to_string(),
        it.chunk_unresolved.to_string(),
    ];
    want.sort();
    assert_eq!(node_ids(&plain), want, "{plain:?}");
    let with = traverse(
        api,
        json!({"start_ids": [it.active], "include_candidates": true}),
    );
    let mut want = vec![
        it.active.clone(),
        it.chunk.to_string(),
        it.discovered.clone(),
        it.chunk_unresolved.to_string(),
    ];
    want.sort();
    assert_eq!(node_ids(&with), want, "{with:?}");
    // a candidate as a start: absent by default, found with the flag
    assert!(
        api.traverse(serde_json::from_value(json!({"start_ids": [it.discovered]})).unwrap())
            .is_err(),
        "a candidate start is absent by default"
    );
    let from_candidate = traverse(
        api,
        json!({"start_ids": [it.discovered], "include_candidates": true}),
    );
    assert!(from_candidate.nodes.iter().any(|n| n.id == it.discovered));

    // explain answers each address
    let status = |address: String| {
        api.explain_status(ExplainStatusRequest { address })
            .unwrap()
            .status
    };
    assert!(matches!(
        status(it.discovered.clone()),
        Status::Pending(PendingStatus::Candidate(_))
    ));
    assert!(matches!(
        status(it.chunk.to_string()),
        Status::Pending(PendingStatus::Candidate(_))
    ));
    assert!(matches!(
        status(it.chunk_unresolved.to_string()),
        Status::Pending(PendingStatus::Unresolved(_))
    ));
}

/// OD-E-6: with seven pending review items the default briefing (`max_candidates = 5`) lists
/// the first five in review order, one line each with the `explain` address, after the
/// record lines; the two beyond the cap are absent; a settled record never appears.
#[test]
fn session_start_lists_up_to_max_candidates_review_items_with_their_explain_address() {
    let s = Scratch::new("e28-candidates-briefing");
    let it = linked(&s);
    let extra: Vec<String> = (1..=5)
        .map(|i| {
            it.api
                .remember(RememberRequest {
                    kind: "Widget".into(),
                    text: format!("widget number {i}, undeclared"),
                    ..Default::default()
                })
                .unwrap()
                .id
        })
        .collect();
    let context = session_start(&it.api).expect("a briefing");
    let (records, review) = context
        .split_once("kaban review:\n")
        .expect("the review section after the records");
    assert!(
        records.starts_with("kaban briefing:\n") && records.contains(&it.accepted),
        "the record lines come first: {context}"
    );
    let mut got = review.to_owned();
    for (id, name) in [
        (it.chunk.to_string(), "<chunk>"),
        (it.chunk_unresolved.to_string(), "<chunk_unresolved>"),
        (it.discovered.clone(), "<discovered>"),
        (extra[0].clone(), "<widget1>"),
        (extra[1].clone(), "<widget2>"),
    ] {
        got = got.replace(&id, name);
    }
    let want = [
        "- [review] candidate: below_margin, the vote on span 56:6 did not clear its gate (kaban explain <chunk>)",
        "- [review] unresolved: preferred_conflict, two claims disagree on span 56:6 (kaban explain <chunk_unresolved>)",
        "- [review] candidate: discovered_kind, the vote on x:Widget did not clear its gate (kaban explain <discovered>)",
        "- [review] candidate: discovered_kind, the vote on x:Widget did not clear its gate (kaban explain <widget1>)",
        "- [review] candidate: discovered_kind, the vote on x:Widget did not clear its gate (kaban explain <widget2>)",
    ]
    .join("\n");
    assert_eq!(got, want, "{context}");
    for beyond in &extra[2..] {
        assert!(!context.contains(beyond), "beyond the cap: {context}");
    }
    for settled in [&it.accepted, &it.active, &it.superseded, &it.forgotten] {
        assert!(
            !review.contains(settled.as_str()),
            "a settled item is never a review line: {context}"
        );
    }
}

/// OD-E-6: `max_candidates = 0` removes the section; the record lines stay.
#[test]
fn max_candidates_zero_removes_the_review_section() {
    let s = Scratch::new("e28-candidates-zero");
    let it = linked(&s);
    let context = session_start(&api_with_max(&it.store, 0)).expect("the record lines");
    assert!(context.contains(&it.accepted), "{context}");
    assert!(!context.contains("kaban review:"), "{context}");
    assert_eq!(review_lines(&context), Vec::<String>::new());
    // the same store under the default cap has the section
    assert!(
        session_start(&it.api).unwrap().contains("kaban review:"),
        "the control"
    );
}

/// OD-E-6: a workspace with a candidate and no accepted record still gets the section; a lone
/// `x:` record is two review items, the record (`candidate: discovered_kind`) and its kind
/// (`discovered`, addressed as `x:Gadget`).
#[test]
fn the_review_section_renders_without_any_record_line() {
    let policy = Policy::from_file(Policy::parse(b"").unwrap(), "test".into()).unwrap();
    let store = Arc::new(SqliteStore::open_memory().unwrap());
    let signer = store.signer();
    let api = KabanApi::new(
        Arc::clone(&store) as Arc<dyn GraphStore>,
        policy,
        signer,
        None,
    );
    let candidate = api
        .remember(RememberRequest {
            kind: "Gadget".into(),
            text: "a gadget nobody declared".into(),
            ..Default::default()
        })
        .unwrap()
        .id;
    assert!(NodeId::parse(&candidate).is_ok());
    let context = session_start(&api).expect("a briefing with the section alone");
    assert!(
        context.starts_with("kaban review:\n"),
        "no record line, the section first: {context}"
    );
    let lines = review_lines(&context);
    assert_eq!(
        lines,
        vec![
            format!(
                "- [review] candidate: discovered_kind, the vote on x:Gadget did not clear its gate (kaban explain {candidate})"
            ),
            "- [review] discovered: x:Gadget is an undeclared kind with 1 record(s) awaiting promotion or pruning (kaban explain x:Gadget)".to_owned(),
        ],
        "{context}"
    );
}
