//! Public-behaviour tests of `recall` v3 (core-spec F3; core-plan C3.5) through `KabanApi`: the
//! "about this `CodeRef`" filter and the card provenance.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

use kaban_api::{CodeRefWire, KabanApi, RecallRequest, RememberRequest};
use kaban_core::CodeRefFilter;
use kaban_policy::Policy;
use kaban_store_sqlite::SqliteStore;
use std::sync::Arc;

fn api() -> KabanApi {
    let policy = Policy::from_file(Policy::parse(b"").unwrap(), "test".into()).unwrap();
    let store = Arc::new(SqliteStore::open_memory().unwrap());
    let signer = store.signer();
    KabanApi::new(store, policy, signer, None)
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

fn note_about(api: &KabanApi, text: &str, refs: Vec<CodeRefWire>) -> String {
    api.remember(RememberRequest {
        kind: "Fact".into(),
        text: text.into(),
        refs,
        ..Default::default()
    })
    .unwrap()
    .id
}

fn recall_ids(api: &KabanApi, r#ref: Option<CodeRefFilter>) -> Vec<String> {
    api.recall(RecallRequest {
        query: "quokka".into(),
        r#ref,
        hops: Some(0),
        ..Default::default()
    })
    .unwrap()
    .results
    .into_iter()
    .map(|c| c.id)
    .collect()
}

/// Plan slice 2: three notes match the query; with `ref` only the note linked `ABOUT` the
/// matching `CodeRef` seeds the recall. `name_path` narrows when given; a non-matching path
/// returns nothing.
#[test]
fn ref_filter_returns_only_notes_about_that_coderef() {
    let api = api();
    let about_a = note_about(
        &api,
        "quokka opens the store",
        vec![coderef("src/a.rs", Some("Store/open"))],
    );
    let about_b = note_about(
        &api,
        "quokka closes the store",
        vec![coderef("src/b.rs", None)],
    );
    let _plain = note_about(&api, "quokka with no code reference", vec![]);

    assert_eq!(
        recall_ids(&api, None).len(),
        3,
        "without a filter every note matches"
    );
    let filter = |relative_path: &str, name_path: Option<&str>| CodeRefFilter {
        relative_path: relative_path.into(),
        name_path: name_path.map(str::to_owned),
        repo: None,
    };
    assert_eq!(
        recall_ids(&api, Some(filter("src/a.rs", None))),
        vec![about_a.clone()]
    );
    assert_eq!(
        recall_ids(&api, Some(filter("src/b.rs", None))),
        vec![about_b]
    );
    assert_eq!(
        recall_ids(&api, Some(filter("src/a.rs", Some("Store/open")))),
        vec![about_a]
    );
    // negative paths: a different symbol, an unknown path, another repository
    let none: Vec<String> = Vec::new();
    assert_eq!(
        recall_ids(&api, Some(filter("src/a.rs", Some("Store/close")))),
        none
    );
    assert_eq!(recall_ids(&api, Some(filter("src/zzz.rs", None))), none);
    let other_repo = CodeRefFilter {
        repo: Some("not-kaban".into()),
        ..filter("src/a.rs", None)
    };
    assert_eq!(recall_ids(&api, Some(other_repo)), none);
}
