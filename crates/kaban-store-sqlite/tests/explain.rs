//! `MemoryStore::explain_group` over the EN-F13 fixture (engine-spec EN-F14; engine-plan E2.4
//! item 2): the three forward groups render the fixture's integers as four-decimal strings
//! (`0.6556`/`0.3444`, margin `0.3111`; Hardware alone; the three-way candidate below the
//! margin), `claim_group` finds a claim's group, and an unknown group is `NotFound`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::Scratch;
use common::orange::{fixture, ids_of, store_with_infra, submit};
use kaban_core::{AssertionStatus, Explanation, MemoryStore, NodeId, Rank, StoreError};
use rusqlite::{Connection, OpenFlags};
use serde_json::{Value, json};

fn raw(s: &Scratch) -> Connection {
    Connection::open_with_flags(s.db(), OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap()
}

/// Every group key, ascending, with the span it names (`offset`, `len`).
fn groups(conn: &Connection) -> Vec<(String, (u64, u64))> {
    conn.prepare("SELECT DISTINCT group_key FROM assertion_status ORDER BY group_key")
        .unwrap()
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|k| {
            let key = k.unwrap();
            let v: Value = serde_json::from_str(&key).unwrap();
            let span = &v[2]["span"];
            (
                key,
                (
                    span["offset"].as_u64().unwrap(),
                    span["len"].as_u64().unwrap(),
                ),
            )
        })
        .collect()
}

fn group_at(all: &[(String, (u64, u64))], offset: u64, len: u64) -> &str {
    &all.iter()
        .find(|(_, span)| *span == (offset, len))
        .unwrap()
        .0
}

fn values_and_shares(e: &Explanation) -> Vec<(Value, String)> {
    e.candidates
        .iter()
        .map(|c| (c.value.clone(), c.p.clone()))
        .collect()
}

/// Slice (EN-F13 forward, the three groups): span 27..32 `Vendor 0.6556` over `Fruit 0.3444`,
/// margin `0.3111` against the `0.3000` gate and `min_confidence 0.6000`, the Vendor value
/// backed by two lineages (a1 gazetteer, a3 gliner) with their priors, no tie break, no
/// protection; span 27..40 Hardware alone at `1.0000`; span 56..62 a `candidate` with
/// `0.4710 / 0.2926 / 0.2364` and margin `0.1784`. `claim_group` of a1 is the first group.
#[test]
fn the_three_forward_groups_render_the_fixture_integers() {
    let fx = fixture();
    let s = Scratch::new("e24-explain", "forward");
    let store = store_with_infra(&s, &fx);
    submit(
        &store,
        &fx,
        &NodeId::new(),
        &ids_of(&fx["variants"]["forward"]["submit"]),
        vec![],
    );
    let conn = raw(&s);
    let all = groups(&conn);
    assert_eq!(all.len(), 3, "{all:?}");

    let vendor = store.explain_group(group_at(&all, 27, 5)).unwrap();
    assert_eq!(vendor.rule, "weighted_vote_v1");
    assert_eq!(vendor.group_key, group_at(&all, 27, 5));
    assert_eq!(vendor.verdict, AssertionStatus::Winner);
    assert_eq!(
        values_and_shares(&vendor),
        vec![
            (json!("infra:Vendor"), "0.6556".to_owned()),
            (json!("x:Fruit"), "0.3444".to_owned()),
        ]
    );
    assert_eq!(vendor.margin, "0.3111");
    assert_eq!(vendor.threshold, "0.3000");
    assert_eq!(vendor.min_confidence, "0.6000");
    assert_eq!(vendor.tie_break, None);
    assert_eq!(vendor.unresolved_reason, None);
    assert_eq!(vendor.protected_by, None);
    assert_eq!(vendor.er_contributions, None);
    let lineages: Vec<(&str, &str, &str, &str, u32)> = vendor.candidates[0]
        .lineages
        .iter()
        .map(|l| {
            (
                l.lineage_id.as_str(),
                l.best.layer.as_str(),
                l.best.raw_score.as_str(),
                l.best.prior.as_str(),
                l.others,
            )
        })
        .collect();
    assert_eq!(
        lineages,
        vec![
            ("a1", "gazetteer", "0.4500", "0.8000", 0),
            ("a3", "gliner", "0.5800", "0.6000", 0),
        ]
    );
    assert!(
        vendor.candidates[0]
            .lineages
            .iter()
            .all(|l| l.rank == Rank::Normal),
        "{vendor:?}"
    );
    // the serialised document skips the absent optionals (serde-skip-empty)
    let text = serde_json::to_string(&vendor).unwrap();
    for absent in [
        "tie_break",
        "unresolved_reason",
        "protected_by",
        "er_contributions",
    ] {
        assert!(!text.contains(absent), "{text}");
    }
    // a claim key names its group
    let a1_key: String = conn
        .query_row(
            "SELECT key FROM assertion WHERE extractor_id = 'a1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(store.claim_group(&a1_key).unwrap(), group_at(&all, 27, 5));

    let hardware = store.explain_group(group_at(&all, 27, 13)).unwrap();
    assert_eq!(hardware.verdict, AssertionStatus::Winner);
    assert_eq!(
        values_and_shares(&hardware),
        vec![(json!("infra:Hardware"), "1.0000".to_owned())]
    );
    assert_eq!(hardware.margin, "1.0000");

    let orange = store.explain_group(group_at(&all, 56, 6)).unwrap();
    assert_eq!(orange.verdict, AssertionStatus::Candidate);
    assert_eq!(
        values_and_shares(&orange),
        vec![
            (json!("infra:Team"), "0.4710".to_owned()),
            (json!("x:Fruit"), "0.2926".to_owned()),
            (json!("infra:Vendor"), "0.2364".to_owned()),
        ]
    );
    assert_eq!(orange.margin, "0.1784");
    assert_eq!(orange.threshold, "0.3000");
}

/// Negative: a group no claim belongs to, and a key no claim has, are `NotFound`.
#[test]
fn an_unknown_group_or_claim_is_not_found() {
    let fx = fixture();
    let s = Scratch::new("e24-explain", "unknown");
    let store = store_with_infra(&s, &fx);
    submit(&store, &fx, &NodeId::new(), &["a1"], vec![]);
    assert!(matches!(
        store.explain_group("[\"default\",\"type\",{\"node\":99},null]"),
        Err(StoreError::NotFound(_))
    ));
    assert!(matches!(
        store.claim_group(&"0".repeat(64)),
        Err(StoreError::NotFound(_))
    ));
}
