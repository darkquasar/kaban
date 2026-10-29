//! Entity resolution, the pure contract (engine-spec EN-F51; engine-plan E2.10 items 1 and 5):
//! the 20-pair golden is exact `LogWeight` integers with negative contributions and negative
//! totals, the threshold edge is `>=` without rounding, blocking never compares a pair outside
//! its blocks (counted on the 1,000-node spec), a fuzzy index's empty answer differs from
//! an unavailable one (spec 05 SS-15), the `[er]` seam fails closed, and a judgement on an
//! unrequested pair is skipped.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

use kaban_core::config::Er;
use kaban_core::er::{self, DUPLICATE, FuzzyIndex, FuzzyUnavailable, NOT_REQUESTED};
use kaban_core::{
    Agreement, BlockKey, ErConfigError, ErDecision, ErPriors, Field, Fixed, LogWeight, NodeView,
    PairJudgement, PairRef,
};
use serde_json::Value;
use std::collections::BTreeMap;

fn golden() -> Value {
    serde_json::from_str(include_str!("../fixtures/er/golden-20.json")).unwrap()
}

/// The pair's priors: the defaults, with the fixture's per-pair overrides applied.
fn priors_of(pair: &Value) -> ErPriors {
    let mut cfg = Er::default();
    if let Some(over) = pair["priors"].as_object() {
        for (table, map) in [("m", &mut cfg.m), ("u", &mut cfg.u)] {
            for (field, v) in over[table].as_object().unwrap() {
                map.insert(field.clone(), v.as_f64().unwrap() / 10_000.0);
            }
        }
    }
    ErPriors::from_config(&cfg).unwrap()
}

#[test]
fn the_twenty_pair_golden_is_exact_signed_integers() {
    let fx = golden();
    let pairs = fx["pairs"].as_array().unwrap();
    assert_eq!(pairs.len(), 20);
    let (mut negative_contribution, mut negative_total) = (0, 0);
    let (mut below, mut at) = (false, false);
    for pair in pairs {
        let a: NodeView = serde_json::from_value(pair["a"].clone()).unwrap();
        let b: NodeView = serde_json::from_value(pair["b"].clone()).unwrap();
        let p = priors_of(pair);
        let score = er::score_pair(&a, &b, &p);
        let name = pair["name"].as_str().unwrap();
        let expected: Vec<(&str, &str, i64)> = pair["contributions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| {
                (
                    c["field"].as_str().unwrap(),
                    c["level"].as_str().unwrap(),
                    c["w"].as_i64().unwrap(),
                )
            })
            .collect();
        let got: Vec<(&str, &str, i64)> = score
            .contributions
            .iter()
            .map(|c| (c.field.as_str(), c.level.as_str(), i64::from(c.w.0)))
            .collect();
        assert_eq!(got, expected, "{name}");
        assert_eq!(i64::from(score.w.0), pair["W"].as_i64().unwrap(), "{name}");
        let decision = match er::decide(&score, &p) {
            ErDecision::Match => "match",
            ErDecision::Possible => "possible",
            ErDecision::None => "none",
        };
        assert_eq!(decision, pair["decision"].as_str().unwrap(), "{name}");
        assert_eq!(
            er::raw_score(&score, &p).unwrap(),
            Fixed(u32::try_from(pair["raw_score"].as_u64().unwrap()).unwrap()),
            "{name}"
        );
        if score.contributions.iter().any(|c| c.w.0 < 0) {
            negative_contribution += 1;
        }
        if score.w.0 < 0 {
            negative_total += 1;
            // the negative test of the plan: a negative W clamps to zero and never panics
            assert_eq!(er::raw_score(&score, &p).unwrap(), Fixed::ZERO, "{name}");
        }
        below |= score.w == LogWeight(p.possible_threshold.0 - 3)
            && er::decide(&score, &p) == ErDecision::None;
        at |= score.w == p.possible_threshold && er::decide(&score, &p) == ErDecision::Possible;
    }
    assert!(negative_contribution >= 6, "{negative_contribution}");
    assert!(negative_total >= 2, "{negative_total}");
    assert!(below && at, "threshold edge pairs: below {below} at {at}");
    // the fixture's own arithmetic for the headline weight (fixtures/reconcile/log-weights.json)
    assert_eq!(fx["weights"]["title"]["agreement"], 64919);
    assert_eq!(fx["weights"]["title"]["disagreement"], -33074);
}

#[test]
fn threshold_comparison_is_on_the_integer_with_no_rounding() {
    let p = ErPriors::defaults();
    let score = |w: i32| er::ErScore {
        w: LogWeight(w),
        contributions: Vec::new(),
    };
    assert_eq!(p.possible_threshold, LogWeight(30000));
    assert_eq!(p.match_threshold, LogWeight(80000));
    assert_eq!(er::decide(&score(29999), &p), ErDecision::None);
    assert_eq!(er::decide(&score(30000), &p), ErDecision::Possible);
    assert_eq!(er::decide(&score(79999), &p), ErDecision::Possible);
    assert_eq!(er::decide(&score(80000), &p), ErDecision::Match);
    assert_eq!(er::raw_score(&score(40000), &p).unwrap(), Fixed(5000));
    assert_eq!(er::raw_score(&score(-1), &p).unwrap(), Fixed::ZERO);
    assert_eq!(er::raw_score(&score(i32::MIN), &p).unwrap(), Fixed::ZERO);
    assert_eq!(er::raw_score(&score(i32::MAX), &p).unwrap(), Fixed::ONE);
}

fn node(i: usize) -> NodeView {
    NodeView {
        id: format!("n{i}"),
        workspace_id: if i < 500 { "w0".into() } else { "w1".into() },
        kind: "Note".into(),
        title: if i % 20 >= 10 {
            format!("t{}", i / 20)
        } else {
            String::new()
        },
        aliases: Vec::new(),
        tags: Vec::new(),
        path_scope: Vec::new(),
        anchors: vec![format!("k{}", i % 250)],
        summary: None,
        dup_key: (i % 10 < 4).then(|| format!("d{}", i / 10)),
    }
}

#[test]
fn blocking_compares_only_pairs_inside_a_block_and_never_across_workspaces() {
    let fx: Value = serde_json::from_str(include_str!("../fixtures/er/block-1000.json")).unwrap();
    let n = usize::try_from(fx["nodes"].as_u64().unwrap()).unwrap();
    let nodes: Vec<NodeView> = (0..n).map(node).collect();
    let blocks = er::blocks(&nodes, None).unwrap();
    let pairs = er::pairs(&blocks);
    assert_eq!(
        pairs.len(),
        usize::try_from(fx["expected_pairs"].as_u64().unwrap()).unwrap()
    );
    assert_eq!(n * (n / 2 - 1) / 2, 249_500);
    let shares = |a: &NodeView, b: &NodeView| {
        a.workspace_id == b.workspace_id
            && ((a.dup_key.is_some() && a.dup_key == b.dup_key)
                || (!a.title.is_empty() && a.title == b.title)
                || a.anchors.iter().any(|k| b.anchors.contains(k)))
    };
    for &(i, j) in &pairs {
        assert!(i < j);
        assert!(shares(&nodes[i], &nodes[j]), "{i} {j}");
    }
    // exhaustive within blocks: every pair that shares a key was compared
    let mut expected = 0;
    for i in 0..n {
        for j in i + 1..n {
            if shares(&nodes[i], &nodes[j]) {
                expected += 1;
                assert!(pairs.contains(&(i, j)), "{i} {j}");
            }
        }
    }
    assert_eq!(expected, pairs.len());
    assert!(blocks.iter().all(|b| b.members.len() >= 2));
    assert!(blocks.iter().all(|b| {
        b.members
            .iter()
            .all(|&m| nodes[m].workspace_id == b.workspace_id)
    }));
    // the first block key, by construction, is the lowest dup key of w0
    assert_eq!(blocks[0].key, BlockKey::DupKey("d0".into()));
    // two workspaces, no block (the plan's negative): one node per workspace, same everything
    let mut twin = node(0);
    twin.workspace_id = "w1".into();
    twin.id = "twin".into();
    let cross = er::blocks(&[node(0), twin], None).unwrap();
    assert!(cross.is_empty(), "{cross:?}");
}

/// A test fake for the unimplemented EN-F24 index: a fixed answer for every node.
enum FakeFuzzy {
    Empty,
    Near(&'static str),
    Unavailable,
}

impl FuzzyIndex for FakeFuzzy {
    fn neighbours(&self, _view: &NodeView) -> Result<Vec<String>, FuzzyUnavailable> {
        match self {
            Self::Empty => Ok(Vec::new()),
            Self::Near(id) => Ok(vec![(*id).to_owned()]),
            Self::Unavailable => Err(FuzzyUnavailable),
        }
    }
}

#[test]
fn an_empty_fuzzy_answer_differs_from_an_unavailable_index() {
    // two nodes sharing no exact key: only a fuzzy neighbourhood can block them
    let (mut a, mut b) = (node(0), node(1));
    a.dup_key = None;
    b.dup_key = None;
    b.anchors = vec!["other".into()];
    let nodes = [a, b];
    let none = er::blocks(&nodes, None).unwrap();
    assert!(none.is_empty(), "{none:?}");
    let empty = er::blocks(&nodes, Some(&FakeFuzzy::Empty)).unwrap();
    assert_eq!(empty, none, "an empty answer adds no block");
    let near = er::blocks(&nodes, Some(&FakeFuzzy::Near("n1"))).unwrap();
    assert_eq!(near.len(), 1, "{near:?}");
    assert_eq!(near[0].key, BlockKey::Fuzzy("n0".into()));
    assert_eq!(near[0].members, vec![0, 1]);
    assert_eq!(
        er::blocks(&nodes, Some(&FakeFuzzy::Unavailable)),
        Err(FuzzyUnavailable),
        "unavailable is never successful emptiness"
    );
}

#[test]
fn the_er_seam_fails_closed_on_unknown_fields_and_endpoint_probabilities() {
    let mut cfg = Er::default();
    cfg.m.insert("colour".into(), 0.5);
    assert_eq!(
        ErPriors::from_config(&cfg),
        Err(ErConfigError::UnknownField {
            table: er::ErTable::M
        })
    );
    let mut cfg = Er::default();
    cfg.u.insert("title".into(), 0.0);
    assert_eq!(
        ErPriors::from_config(&cfg),
        Err(ErConfigError::OutOfRange {
            path: "u.title".into()
        })
    );
    let mut cfg = Er::default();
    cfg.m.insert("kind".into(), 1.0);
    assert!(matches!(
        ErPriors::from_config(&cfg),
        Err(ErConfigError::OutOfRange { .. })
    ));
    let cfg = Er {
        match_threshold: 0.0,
        ..Er::default()
    };
    assert!(matches!(
        ErPriors::from_config(&cfg),
        Err(ErConfigError::Threshold { .. })
    ));
    let cfg = Er {
        possible_threshold: f64::NAN,
        ..Er::default()
    };
    assert!(matches!(
        ErPriors::from_config(&cfg),
        Err(ErConfigError::Threshold { .. })
    ));
    // the defaults: every field has both tables, the closed vocabulary parses its own spelling
    let p = ErPriors::defaults();
    assert_eq!(p.m.len(), Field::ALL.len());
    assert_eq!(p.u.len(), Field::ALL.len());
    for &f in Field::ALL {
        assert_eq!(Field::parse(f.as_str()), Some(f));
    }
    assert_eq!(Field::parse("Title"), None);
    // an unconfigured field contributes nothing at any level
    let sparse = ErPriors {
        m: BTreeMap::new(),
        u: BTreeMap::new(),
        match_threshold: LogWeight(80000),
        possible_threshold: LogWeight(30000),
    };
    assert_eq!(sparse.weight(Field::Title, Agreement::Agree), LogWeight(0));
}

#[test]
fn a_judgement_on_an_unrequested_pair_is_skipped_not_requested() {
    let asked = [PairRef::new("b", "a"), PairRef::new("c", "d")];
    let judgement = |x: &str, y: &str, same: bool| PairJudgement {
        pair: PairRef {
            a: x.into(),
            b: y.into(),
        },
        same,
    };
    let returned = [
        judgement("a", "b", true),
        judgement("x", "y", false),
        judgement("d", "c", false),
        judgement("b", "a", false),
    ];
    let out = er::judge_filter(&asked, &returned);
    assert_eq!(
        out.accepted,
        vec![judgement("a", "b", true), judgement("d", "c", false)]
    );
    assert_eq!(
        out.skipped,
        vec![
            (judgement("x", "y", false), NOT_REQUESTED),
            (judgement("b", "a", false), DUPLICATE),
        ]
    );
    assert_eq!(er::judge_filter(&[], &returned).accepted, Vec::new());
}

/// E2.10b BR-2: an identity group is one unordered pair, keyed by both endpoints: A-B and A-C
/// are two groups (a distinction about one is no vote on the other); two claims about one
/// pair share a group whatever their values.
#[test]
fn identity_groups_are_keyed_by_both_endpoints_of_the_pair() {
    use kaban_core::{
        Assertion, AssertionKey, CanonicalJson, ClaimClass, Fixed, Layer, Method, Rank, Subject,
        WorkspaceId, group_key, same_group,
    };
    let pair = |object: i64, is_match: bool| Assertion {
        key: AssertionKey::default(),
        workspace_id: WorkspaceId::default(),
        subject: Subject::Node(1),
        claim_class: ClaimClass::Identity,
        predicate: None,
        object_rid: Some(object),
        value: CanonicalJson::new(serde_json::json!({"match": is_match})),
        layer: Layer::Reconcile,
        method: Method::Er,
        extractor_id: None,
        extractor_version: None,
        source_id: None,
        lineage_id: "reconcile:er".into(),
        raw_score: Fixed::ONE,
        rank: Rank::Normal,
        rank_reason: None,
        calibration_version: 0,
        supersedes: None,
        ts: 0,
    };
    let (ab, ab_distinct, ac) = (pair(2, true), pair(2, false), pair(3, true));
    assert_eq!(group_key(&ab), group_key(&ab_distinct));
    assert!(same_group(&ab, &ab_distinct));
    assert_ne!(group_key(&ab), group_key(&ac));
    assert!(!same_group(&ab, &ac));
}

/// RW4-5 (`obs-no-sensitive-data`, `err-custom-type`): an unknown `[er]` key is a caller's
/// bytes, so the error it raises names the table and a fixed `<unknown>` category, never the
/// key: neither `Display` nor `Debug` carries it, for either table.
#[test]
fn an_unknown_er_field_error_never_carries_the_key() {
    const KEY: &str = "AKIA-looks-like-a-secret";
    for table in ["m", "u"] {
        let mut cfg = Er::default();
        let map = if table == "m" { &mut cfg.m } else { &mut cfg.u };
        map.insert(KEY.to_owned(), 0.5);
        let err = ErPriors::from_config(&cfg).unwrap_err();
        assert!(matches!(err, ErConfigError::UnknownField { .. }), "{table}");
        let shown = err.to_string();
        let debug = format!("{err:?}");
        assert!(
            !debug.contains(KEY),
            "Debug of the unknown-field error carries the caller's key (table {table})"
        );
        assert!(
            !shown.contains(KEY),
            "Display of the unknown-field error carries the caller's key (table {table})"
        );
        assert_eq!(shown, "er_unknown_field");
        let ErConfigError::UnknownField { table: found } = err else {
            unreachable!()
        };
        assert_eq!(found.as_str(), table);
        assert_eq!(found.unknown_path(), format!("{table}.<unknown>"));
    }
}
