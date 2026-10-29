//! EN-F14 `Verify:` "a grep test that the words `probability` and `truth` do not occur in
//! explanation templates", re-scoped by OWN-81 (5) (engine-plan E2.5 item 5): the grep runs
//! over the user-visible surface, the rendered `sentence` and `next_actions[].label` of every
//! coverage fixture item (`explain` over `common/status.rs`) and the template table
//! `explain.rs` exposes, case-insensitive. The weights are weights (lane C §3).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

#[path = "common/status.rs"]
mod status;

use kaban_api::{ExplainStatusRequest, templates};
use status::{Scratch, items, key_of};

/// Built at run time so the words never sit in this file as literals the grep would find.
fn forbidden() -> Vec<String> {
    vec![["prob", "ability"].concat(), ["tru", "th"].concat()]
}

fn assert_clean(where_: &str, text: &str, words: &[String]) {
    let lower = text.to_lowercase();
    for word in words {
        assert!(
            !lower.contains(word.as_str()),
            "{where_} says {word:?}: {text}"
        );
    }
}

#[test]
fn the_rendered_sentences_labels_and_templates_never_call_a_weight_a_probability_or_a_truth() {
    let words = forbidden();
    for (name, text) in templates() {
        assert_clean(&format!("template {name}"), text, &words);
    }
    let s = Scratch::new("no-words");
    let it = items(&s);
    let addresses = vec![
        it.chunk.to_string(),
        it.chunk_unresolved.to_string(),
        it.discovered.clone(),
        it.proposed.clone(),
        it.accepted.clone(),
        it.superseded.clone(),
        it.forgotten.clone(),
        it.active.clone(),
        it.conflict.clone(),
        "x:Widget".to_owned(),
        key_of(&s, "a1"),
        key_of(&s, "a2"),
        key_of(&s, "a5"),
    ];
    let mut seen = 0;
    for address in addresses {
        let x = it
            .api
            .explain_status(ExplainStatusRequest {
                address: address.clone(),
            })
            .unwrap();
        assert_clean(&format!("sentence of {address}"), &x.sentence, &words);
        for a in &x.next_actions {
            assert_clean(&format!("label of {address}"), &a.label, &words);
        }
        seen += 1;
    }
    assert_eq!(seen, 13, "every coverage item was rendered");
}
