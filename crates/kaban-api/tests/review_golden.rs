//! The `kaban review` golden (engine-plan E2.5): over the coverage fixture, one item per
//! pending status in review order, each with its sentence and its first action; settled
//! items never listed; the filters narrow by status and by kind. The expected document is
//! pinned inline (the crate has no snapshot dependency): ids are replaced by their role name
//! before the comparison, so the golden is stable across runs.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

#[path = "common/status.rs"]
mod status;

use kaban_api::{PendingStatusKind, ReviewQuery};
use kaban_core::{BatchSubject, NodeId};
use serde_json::{Value, json};
use status::{Items, Scratch, claim, fixture, human_claim, items, rows, submit};

/// The `proposed` record the reconciler materialised for the fixture's span winner without
/// a target (EN-F13: `infra:Hardware`, span 27..40), as `accept_proposed.rs` reads it.
fn hardware_record(s: &Scratch) -> String {
    let conn = rusqlite::Connection::open(s.db()).unwrap();
    conn.query_row(
        "SELECT id FROM node WHERE kind = 'infra:Hardware' AND governance = 'proposed'",
        [],
        |r| r.get(0),
    )
    .unwrap()
}

/// The review document with every id replaced by the item's role name and every claim key
/// by `<key>`.
fn normalised(it: &Items, q: &ReviewQuery) -> Value {
    normalised_in(None, it, q)
}

/// [`normalised`] with the materialised Hardware record `hardware` named `<hardware>`.
fn normalised_in(hardware: Option<&str>, it: &Items, q: &ReviewQuery) -> Value {
    let resp = it.api.review(q).unwrap();
    let mut text = serde_json::to_string(&resp).unwrap();
    if let Some(id) = hardware {
        text = text.replace(id, "<hardware>");
    }
    for (id, name) in [
        (it.chunk.to_string(), "<chunk>"),
        (it.chunk_unresolved.to_string(), "<chunk_unresolved>"),
        (it.discovered.clone(), "<discovered>"),
        (it.proposed.clone(), "<proposed>"),
        (it.conflict.clone(), "<conflict>"),
    ] {
        text = text.replace(&id, name);
    }
    let mut v: Value = serde_json::from_str(&text).unwrap();
    for item in v["items"].as_array_mut().unwrap() {
        let cmd = item["first_action"]["command"].as_str().unwrap().to_owned();
        let masked: Vec<String> = cmd
            .split(' ')
            .map(|w| {
                if w.len() == 64 && w.bytes().all(|b| b.is_ascii_hexdigit()) {
                    "<key>".to_owned()
                } else if w.starts_with("'[") && w.ends_with("]'") {
                    // the action's group key (RW3-3), canonical JSON naming the row id
                    "<group>".to_owned()
                } else {
                    w.to_owned()
                }
            })
            .collect();
        item["first_action"]["command"] = json!(masked.join(" "));
        // a group key is canonical JSON naming the chunk's row id: masked to `<group>`; the
        // `x:` label of a discovered row stays
        if item["group_key"]
            .as_str()
            .is_some_and(|k| k.starts_with('['))
        {
            item["group_key"] = json!("<group>");
        }
    }
    v
}

#[test]
fn review_lists_one_item_per_pending_status_with_its_sentence_and_first_action() {
    let s = Scratch::new("review-golden");
    let it = items(&s);
    let got = normalised_in(Some(&hardware_record(&s)), &it, &ReviewQuery::default());
    let want = json!({
        "items": [
            {
                "subject": {"node": "<chunk>"},
                "status": {"candidate": "below_margin"},
                "sentence": "candidate: below_margin, the vote on span 56:6 did not clear its gate",
                "first_action": {
                    "label": "name the winning value as a person; the group is decided and leaves review",
                    "command": "kaban resolve <chunk> --winner infra:Team --group <group>"
                },
                "kind": "Chunk",
                "group_key": "<group>"
            },
            {
                "subject": {"node": "<chunk_unresolved>"},
                "status": {"unresolved": "preferred_conflict"},
                "sentence": "unresolved: preferred_conflict, two claims disagree on span 56:6",
                "first_action": {
                    "label": "deprecate the losing claim so the group is re-decided without it",
                    "command": "kaban explain <key> --deprecate reviewed"
                },
                "kind": "Chunk",
                "group_key": "<group>"
            },
            {
                "subject": {"node": "<discovered>"},
                "status": {"candidate": "discovered_kind"},
                "sentence": "candidate: discovered_kind, the vote on x:Widget did not clear its gate",
                "first_action": {
                    "label": "accept the record as a person",
                    "command": "kaban accept <discovered> --reason reviewed --path-scope '**'"
                },
                "kind": "x:Widget",
                "group_key": "x:Widget"
            },
            {
                "subject": {"node": "<conflict>"},
                "status": "conflict",
                "sentence": "conflict: the record's file and database disagree at the same rev",
                "first_action": {
                    "label": "keep the database record and rewrite the file",
                    "command": "kaban resolve <conflict> --take db"
                },
                "kind": "Convention"
            },
            {
                "subject": {"node": "<hardware>"},
                "status": "proposed",
                "sentence": "proposed: the record awaits a human acceptance",
                "first_action": {
                    "label": "accept the record as a person",
                    "command": "kaban accept <hardware> --reason reviewed --path-scope '**'"
                },
                "kind": "infra:Hardware"
            },
            {
                "subject": {"node": "<proposed>"},
                "status": "proposed",
                "sentence": "proposed: the record awaits a human acceptance",
                "first_action": {
                    "label": "accept the record as a person",
                    "command": "kaban accept <proposed> --reason reviewed --path-scope '**'"
                },
                "kind": "Decision"
            },
            {
                "subject": {"discovered_kind": "x:Widget"},
                "status": {"discovered": {"count": 1}},
                "sentence": "discovered: x:Widget is an undeclared kind with 1 record(s) awaiting promotion or pruning",
                "first_action": {
                    "label": "promote the discovered kind to a declared kind",
                    "command": "kaban ontology promote x:Widget Entity"
                },
                "kind": "x:Widget"
            }
        ],
        "partial": false
    });
    assert_eq!(got, want, "{}", serde_json::to_string_pretty(&got).unwrap());
}

#[test]
fn review_filters_by_status_and_kind_and_lists_no_settled_item() {
    let s = Scratch::new("review-filters");
    let it = items(&s);
    let proposed = normalised_in(
        Some(&hardware_record(&s)),
        &it,
        &ReviewQuery {
            status: Some(PendingStatusKind::Proposed),
            ..ReviewQuery::default()
        },
    );
    // the materialised Hardware record (EN-F13) and the proposed Decision, id order
    assert_eq!(proposed["items"].as_array().unwrap().len(), 2, "{proposed}");
    assert_eq!(
        proposed["items"][0]["subject"],
        json!({"node": "<hardware>"})
    );
    assert_eq!(
        proposed["items"][1]["subject"],
        json!({"node": "<proposed>"})
    );
    let x_kinds = normalised(
        &it,
        &ReviewQuery {
            ontology: Some("x".into()),
            ..ReviewQuery::default()
        },
    );
    assert_eq!(
        x_kinds["items"].as_array().unwrap().len(),
        2,
        "the node and the kind"
    );
    assert_eq!(x_kinds["items"][0]["kind"], json!("x:Widget"));
    assert_eq!(
        x_kinds["items"][1]["subject"],
        json!({"discovered_kind": "x:Widget"})
    );
    let none = normalised(
        &it,
        &ReviewQuery {
            kind: Some("Fact".into()),
            ..ReviewQuery::default()
        },
    );
    assert_eq!(none["items"], json!([]), "settled Facts are never listed");
    let all = normalised(&it, &ReviewQuery::default());
    let text = serde_json::to_string(&all).unwrap();
    for settled in [&it.accepted, &it.superseded, &it.forgotten, &it.active] {
        assert!(
            !text.contains(settled.as_str()),
            "settled item listed: {text}"
        );
    }
}

/// RW2-4: a node with two outstanding groups is two review items, each with its own group
/// key and status: the `candidate` vote at 56..62 and an `unresolved` pair of human claims
/// at 10..16 of one chunk. `--status unresolved` keeps the node through its unresolved group
/// although its first group is a candidate; `explain <id>` without a group is one status.
#[test]
fn two_outstanding_groups_on_one_node_are_two_items() {
    let s = Scratch::new("review-two-groups");
    let it = items(&s);
    let fx = fixture();
    let chunk = NodeId::new();
    let at_10 = |value: &str, lineage: &str| {
        let mut c = human_claim(&chunk, value, lineage);
        c.subject = BatchSubject::Span {
            chunk: chunk.clone(),
            offset: 10,
            len: 6,
        };
        c
    };
    submit(
        &it.store,
        &fx,
        &chunk,
        &["a5", "a6", "a7"],
        vec![
            at_10("infra:Team", "human:cli:1"),
            at_10("infra:Vendor", "human:cli:2"),
        ],
    );
    let of_chunk = |q: &ReviewQuery| -> Vec<Value> {
        let v = normalised(&it, q);
        v["items"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|i| i["subject"] == json!({"node": chunk.to_string()}))
            .cloned()
            .collect()
    };
    let both = of_chunk(&ReviewQuery::default());
    assert_eq!(both.len(), 2, "{both:?}");
    let statuses: Vec<&Value> = both.iter().map(|i| &i["status"]).collect();
    assert!(
        statuses.contains(&&json!({"candidate": "below_margin"}))
            && statuses.contains(&&json!({"unresolved": "preferred_conflict"})),
        "{both:?}"
    );
    assert!(
        both.iter().all(|i| i["group_key"] == json!("<group>")),
        "{both:?}"
    );
    let unresolved = of_chunk(&ReviewQuery {
        status: Some(PendingStatusKind::Unresolved),
        ..ReviewQuery::default()
    });
    assert_eq!(unresolved.len(), 1, "{unresolved:?}");
    assert!(
        unresolved[0]["sentence"]
            .as_str()
            .unwrap()
            .contains("span 10:6"),
        "{unresolved:?}"
    );
    // the node itself is one address with one status
    let x = it
        .api
        .explain_status(kaban_api::ExplainStatusRequest {
            address: chunk.to_string(),
        })
        .unwrap();
    assert!(matches!(x.status, kaban_api::Status::Pending(_)), "{x:?}");
}

/// RW2-4: a record of an undeclared kind that is also the subject of a pending vote is two
/// items: its `discovered_kind` admission row and the vote's group. The negative: an
/// accepted record of a declared kind with nothing pending is no item at all.
#[test]
fn a_discovered_record_with_a_pending_group_is_two_items() {
    let s = Scratch::new("review-discovered-group");
    let it = items(&s);
    let fx = fixture();
    let widget = NodeId::parse(&it.discovered).unwrap();
    let spare = NodeId::new();
    let about_widget: Vec<_> = rows(&fx, &["a5", "a6", "a7"])
        .into_iter()
        .map(|r| {
            let mut c = claim(&fx, r, &spare);
            c.subject = BatchSubject::Node(widget.clone());
            c
        })
        .collect();
    submit(&it.store, &fx, &spare, &[], about_widget);
    let v = normalised(&it, &ReviewQuery::default());
    let of_widget: Vec<&Value> = v["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["subject"] == json!({"node": "<discovered>"}))
        .collect();
    assert_eq!(of_widget.len(), 2, "{of_widget:?}");
    assert_eq!(
        of_widget[0]["status"],
        json!({"candidate": "discovered_kind"})
    );
    assert_eq!(of_widget[0]["group_key"], json!("x:Widget"));
    assert_eq!(of_widget[1]["status"], json!({"candidate": "below_margin"}));
    assert_eq!(of_widget[1]["group_key"], json!("<group>"));
    let text = serde_json::to_string(&v).unwrap();
    assert!(!text.contains(it.accepted.as_str()), "{text}");
}

/// RW2-5 (EN-F13): the `proposed` record the reconciler materialised for a span winner is
/// a review item although its kind is neither governed nor counted; once a person accepts
/// it, it is listed no more (the negative), and `--status proposed` then names only the
/// proposed Decision.
#[test]
fn the_materialised_proposal_is_listed_until_a_person_accepts_it() {
    let s = Scratch::new("review-materialised");
    let it = items(&s);
    let hardware = hardware_record(&s);
    let proposed = |it: &Items| -> Vec<Value> {
        normalised_in(
            Some(&hardware),
            it,
            &ReviewQuery {
                status: Some(PendingStatusKind::Proposed),
                ..ReviewQuery::default()
            },
        )["items"]
            .as_array()
            .unwrap()
            .clone()
    };
    let before = proposed(&it);
    assert_eq!(before.len(), 2, "{before:?}");
    assert_eq!(before[0]["subject"], json!({"node": "<hardware>"}));
    assert_eq!(before[0]["kind"], json!("infra:Hardware"));
    it.api
        .accept(
            kaban_api::AcceptRequest {
                id: hardware.clone(),
                reason: Some("reviewed".into()),
                path_scope: vec!["**".into()],
                ..Default::default()
            },
            &status::human(),
        )
        .unwrap();
    let after = proposed(&it);
    assert_eq!(after.len(), 1, "{after:?}");
    assert_eq!(after[0]["subject"], json!({"node": "<proposed>"}));
    let text = serde_json::to_string(&normalised(&it, &ReviewQuery::default())).unwrap();
    assert!(!text.contains(&hardware), "{text}");
}
