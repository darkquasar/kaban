//! Public-trait vote tests over the store's `WeightedVoteV1`. The pure v1 module is included
//! by path (it depends on `kaban-core` only) so the permutation proptest drives the decision
//! table directly; `apply_batch` reaches the same type through `SqliteStore` (E2.2).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![allow(dead_code)] // the included module's store-facing helpers are unused here
#[path = "../src/reconcile/v1.rs"]
mod v1;
use kaban_core::*;
use proptest::prelude::*;
use serde_json::{Value, json};
use v1::{WeightedVoteV1, supporters};

fn ontology() -> Ontology {
    let file = OntologyFile::from_yaml(r"
ontology: infra
version: 1
rels: {}
kinds:
  Vendor: {description: vendor, min_confidence: 0.6}
  Team: {description: team, min_confidence: 0.6}
  Hardware: {description: hardware, min_confidence: 0.6}
  Required: {description: required, props_schema: {type: object, required: [title], properties: {title: {type: string}}}}
").unwrap();
    resolve(&[file], &NamespaceSet::new()).unwrap()
}
fn assertion(value: Value, layer: Layer, method: Method, raw: u32, lineage: &str) -> Assertion {
    Assertion {
        key: AssertionKey::default(),
        workspace_id: WorkspaceId::new("w"),
        subject: Subject::Span {
            chunk_rid: 17,
            offset: 27,
            len: 5,
        },
        claim_class: ClaimClass::Type,
        predicate: None,
        object_rid: None,
        value: CanonicalJson::new(value),
        layer,
        method,
        extractor_id: None,
        extractor_version: None,
        source_id: None,
        lineage_id: lineage.into(),
        raw_score: Fixed::new(raw).unwrap(),
        rank: Rank::Normal,
        rank_reason: None,
        calibration_version: 1,
        supersedes: None,
        ts: 0,
    }
    .with_key()
    .unwrap()
}
fn human(value: &str, raw: u32, lineage: &str) -> Assertion {
    assertion(json!(value), Layer::Human, Method::Link, raw, lineage)
}
fn decide(group: &[Assertion], loaded: &Ontology, priors: &Priors, tau: u32) -> Verdict {
    let protection = ProtectionSet::default();
    let mut ctx =
        ReconcileCtx::new(loaded, priors, &config::Reconcile::default(), &protection).unwrap();
    ctx.tau_type = Fixed(tau);
    let reconciler: &dyn Reconciler = &WeightedVoteV1;
    reconciler.decide(group, &ctx)
}
fn winner(v: Verdict) -> (AssertionKey, Fixed, Fixed) {
    match v {
        Verdict::Winner {
            assertion_key,
            confidence,
            margin,
            ..
        } => (assertion_key, confidence, margin),
        other => panic!("{other:?}"),
    }
}
fn ranked(v: Verdict, expected: CandidateReason) -> Vec<(AssertionKey, Fixed)> {
    match v {
        Verdict::Candidate { ranked, reason } => {
            assert_eq!(reason, expected);
            ranked
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn normative_orange_apple_spans_and_no_llm_variant() {
    let fixture: Value = serde_json::from_str(include_str!(
        "../../kaban-core/fixtures/reconcile/orange-apple.json"
    ))
    .unwrap();
    let loaded = ontology();
    let priors = Priors::defaults();
    for (i, g) in fixture["groups"].as_array().unwrap().iter().enumerate() {
        let group: Vec<_> = g["supporters"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| {
                let mut a = assertion(
                    s[0].clone(),
                    Layer::parse(s[1].as_str().unwrap()).unwrap(),
                    Method::parse(s[2].as_str().unwrap()).unwrap(),
                    u32::try_from(s[3].as_u64().unwrap()).unwrap(),
                    s[4].as_str().unwrap(),
                );
                a.subject = Subject::Span {
                    chunk_rid: 17,
                    offset: u32::try_from(g["offset"].as_u64().unwrap()).unwrap(),
                    len: u32::try_from(g["len"].as_u64().unwrap()).unwrap(),
                };
                a.with_key().unwrap()
            })
            .collect();
        // Observe each S through the public minimum-confidence gate, independently
        // of normalization: S accepts at the exact integer and rejects at S + 1.
        for (name, score) in g["score_values"]
            .as_array()
            .unwrap()
            .iter()
            .zip(g["scores"].as_array().unwrap())
        {
            let expected = u32::try_from(score.as_u64().unwrap()).unwrap();
            let isolated: Vec<_> = group
                .iter()
                .filter(|a| a.kind_name() == name.as_str())
                .cloned()
                .map(|mut a| {
                    a.value = json!("infra:Vendor").into();
                    a.with_key().unwrap()
                })
                .collect();
            let mut thresholds = loaded.clone();
            for boundary in [expected, expected + 1] {
                thresholds
                    .kinds
                    .values_mut()
                    .find(|kind| kind.id.name == "Vendor")
                    .unwrap()
                    .min_confidence = Some(f64::from(boundary) / 10000.0);
                let verdict = decide(&isolated, &thresholds, &priors, 3000);
                if boundary == expected {
                    assert_eq!(winner(verdict).1, Fixed::ONE);
                } else {
                    ranked(verdict, CandidateReason::BelowMinConfidence);
                }
            }
        }
        let verdict = decide(&group, &loaded, &priors, 3000);
        if i < 2 {
            let (key, confidence, margin) = winner(verdict);
            assert_eq!(u64::from(confidence.0), g["p"][0].as_u64().unwrap());
            assert_eq!(u64::from(margin.0), g["margin"].as_u64().unwrap());
            assert_eq!(
                group.iter().find(|a| a.key == key).unwrap().kind_name(),
                Some(if i == 0 {
                    "infra:Vendor"
                } else {
                    "infra:Hardware"
                })
            );
        } else {
            let r = ranked(verdict, CandidateReason::BelowMargin);
            let exact_margin = u32::try_from(g["margin"].as_u64().unwrap()).unwrap();
            if i == 2 {
                assert_eq!(
                    winner(decide(&group, &loaded, &priors, exact_margin)).1,
                    Fixed(4710)
                );
            } else {
                ranked(
                    decide(&group, &loaded, &priors, exact_margin),
                    CandidateReason::DiscoveredKind,
                );
            }
            ranked(
                decide(&group, &loaded, &priors, exact_margin + 1),
                CandidateReason::BelowMargin,
            );
            assert_eq!(
                r.iter()
                    .map(|(_, priors)| u64::from(priors.0))
                    .collect::<Vec<_>>(),
                g["p"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|v| v.as_u64().unwrap())
                    .collect::<Vec<_>>()
            );
        }
    }
}

#[test]
fn single_candidate_and_summed_lineages_gate_on_s_never_p() {
    let loaded = ontology();
    let priors = Priors::defaults();
    let low = human("infra:Vendor", 5999, "one");
    ranked(
        decide(std::slice::from_ref(&low), &loaded, &priors, 3000),
        CandidateReason::BelowMinConfidence,
    );
    let equal = human("infra:Vendor", 6000, "one");
    assert_eq!(
        winner(decide(&[equal], &loaded, &priors, 3000)).1,
        Fixed::ONE
    );
    let same_lineage = human("infra:Vendor", 5000, "one");
    ranked(
        decide(&[low.clone(), same_lineage], &loaded, &priors, 3000),
        CandidateReason::BelowMinConfidence,
    );
    let independent = human("infra:Vendor", 1, "two");
    assert_eq!(
        winner(decide(&[low, independent], &loaded, &priors, 3000)).1,
        Fixed::ONE
    );
}

#[test]
fn conflict_class_table_uses_margin_max_proposal_and_merge_rules() {
    let loaded = ontology();
    let priors = Priors::defaults();
    for class in ClaimClass::ALL {
        let mut a = human("infra:Vendor", 7000, "a");
        a.claim_class = *class;
        match class {
            ClaimClass::Edge => {
                a.subject = Subject::Edge {
                    src: 1,
                    rel: "RELATES_TO".into(),
                    dst: 2,
                }
            }
            ClaimClass::KindOfRecord => {
                a.subject = Subject::Record(1);
                a.value = CanonicalJson::new(json!({"kind":"infra:Vendor","props":{}}));
            }
            ClaimClass::PropValue => {
                a.subject = Subject::Node(1);
                a.predicate = Some("title".into());
            }
            ClaimClass::Identity => a.subject = Subject::Node(1),
            _ => {}
        }
        a = a.with_key().unwrap();
        let mut b = a.clone();
        b.lineage_id = "b".into();
        b.raw_score = Fixed(5000);
        b = b.with_key().unwrap();
        match class {
            ClaimClass::KindOfRecord => {
                assert_eq!(
                    ranked(
                        decide(&[a, b], &loaded, &priors, 3000),
                        CandidateReason::Proposed
                    )
                    .len(),
                    1
                );
            }
            ClaimClass::Identity => {
                ranked(
                    decide(&[a, b], &loaded, &priors, 3000),
                    CandidateReason::Merge,
                );
            }
            ClaimClass::Edge | ClaimClass::PropValue => {
                assert_eq!(
                    winner(decide(&[a, b], &loaded, &priors, 3000)).1,
                    Fixed(7000)
                );
            }
            _ => assert_eq!(
                winner(decide(&[a, b], &loaded, &priors, 3000)).1,
                Fixed::ONE
            ),
        }
    }
}

#[test]
fn canonical_id_uses_its_own_margin_threshold() {
    let loaded = ontology();
    let priors = Priors::defaults();
    let mut a = human("a", 6500, "a");
    let mut b = human("b", 3500, "b");
    a.claim_class = ClaimClass::CanonicalId;
    b.claim_class = ClaimClass::CanonicalId;
    ranked(
        decide(
            &[a.with_key().unwrap(), b.with_key().unwrap()],
            &loaded,
            &priors,
            0,
        ),
        CandidateReason::BelowMargin,
    );
}

#[test]
fn five_tie_break_levels_have_hand_computed_winners() {
    let loaded = ontology();
    let priors = Priors::defaults();
    // Equal total 6000; best single lineage 6000 beats 3000 + 3000.
    let a = human("infra:Vendor", 6000, "a");
    let b = human("infra:Team", 3000, "b");
    let c = human("infra:Team", 3000, "c");
    assert_eq!(
        winner(decide(&[a.clone(), b, c], &loaded, &priors, 0)).0,
        a.key
    );
    // Equal 6000 and best lineage; human precedes gliner, despite value byte order.
    let b = assertion(
        json!("infra:Team"),
        Layer::Gliner,
        Method::Gliner,
        10000,
        "b",
    );
    assert_eq!(
        winner(decide(&[b, a.clone()], &loaded, &priors, 0)).0,
        a.key
    );
    // Declared kind precedes x:, then smaller canonical value wins.
    let x = human("x:A", 6000, "x");
    assert_eq!(
        winner(decide(&[x, a.clone()], &loaded, &priors, 0)).0,
        a.key
    );
    let b = human("infra:Team", 6000, "b");
    assert_eq!(
        winner(decide(&[a.clone(), b.clone()], &loaded, &priors, 0)).0,
        b.key
    );
    // Same candidate and source: smaller stable key selects its representative.
    let c = human("infra:Vendor", 6000, "c");
    assert_eq!(
        winner(decide(&[a.clone(), c.clone()], &loaded, &priors, 0)).0,
        a.key.min(c.key)
    );
}

#[test]
fn record_kind_falls_back_to_first_validating_required_schema() {
    let loaded = ontology();
    let priors = Priors::defaults();
    // machine claims: the record validity gate exempts `layer: human` (E2.3b)
    let mut a = assertion(json!("infra:Required"), Layer::Llm, Method::Llm, 9000, "a");
    a.subject = Subject::Record(1);
    a.claim_class = ClaimClass::KindOfRecord;
    a.value = CanonicalJson::new(json!({"kind":"infra:Required", "props":{}}));
    a = a.with_key().unwrap();
    let mut b = a.clone();
    b.value = CanonicalJson::new(json!({"kind":"infra:Vendor", "props":{}}));
    b.raw_score = Fixed(7000);
    b = b.with_key().unwrap();
    assert_eq!(
        ranked(
            decide(&[a.clone(), b.clone()], &loaded, &priors, 3000),
            CandidateReason::Proposed
        )[0]
        .0,
        b.key
    );
    assert!(matches!(
        decide(&[a.clone()], &loaded, &priors, 3000),
        Verdict::Rejected { .. }
    ));
    a.value = CanonicalJson::new(json!({"kind":"infra:Required", "props":{"title":"ok"}}));
    a = a.with_key().unwrap();
    assert_eq!(
        ranked(
            decide(&[a.clone(), b], &loaded, &priors, 3000),
            CandidateReason::Proposed
        )[0]
        .0,
        a.key
    );
}

#[test]
fn missing_prior_empty_zero_and_mismatched_groups_fail_closed() {
    let loaded = ontology();
    let priors = Priors::defaults();
    assert!(matches!(
        decide(&[], &loaded, &priors, 3000),
        Verdict::Rejected { .. }
    ));
    let mut a = human("infra:Vendor", 8000, "a");
    a.layer = Layer::Script;
    assert!(matches!(
        decide(&[a.clone()], &loaded, &priors, 3000),
        Verdict::Rejected { .. }
    ));
    a.layer = Layer::Human;
    a.raw_score = Fixed(0);
    assert!(matches!(
        decide(&[a.clone()], &loaded, &priors, 3000),
        Verdict::Rejected { .. }
    ));
    a.raw_score = Fixed(10001);
    assert!(matches!(
        decide(&[a.clone()], &loaded, &priors, 3000),
        Verdict::Rejected { .. }
    ));
    a.raw_score = Fixed(8000);
    let mut b = a.clone();
    b.subject = Subject::Node(2);
    assert!(matches!(
        decide(&[a, b], &loaded, &priors, 3000),
        Verdict::Rejected { .. }
    ));
}

#[test]
#[cfg(debug_assertions)]
#[should_panic(expected = "mixed workspaces")]
fn mixed_workspaces_panic_in_debug() {
    let a = human("infra:Vendor", 8000, "a");
    let mut b = a.clone();
    b.workspace_id = WorkspaceId::new("other");
    decide(&[a, b], &ontology(), &Priors::defaults(), 3000);
}

#[test]
fn mixed_workspaces_reject_in_nonasserting_inner_path() {
    let a = human("infra:Vendor", 8000, "a");
    let mut b = a.clone();
    b.workspace_id = WorkspaceId::new("other");
    let loaded = ontology();
    let priors = Priors::defaults();
    let protection = ProtectionSet::default();
    let ctx =
        ReconcileCtx::new(&loaded, &priors, &config::Reconcile::default(), &protection).unwrap();
    assert_eq!(
        v1::decide_without_debug(&[a, b], &ctx),
        Verdict::Rejected {
            reason: "mixed_workspaces".into()
        }
    );
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]
    #[test]
    fn any_permutation_preserves_verdict_bytes_and_supporter_order(rows in prop::collection::vec((0_u8..3, 1_u32..10001, 0_u8..4, any::<u64>()), 1..13)) {
        let loaded = ontology(); let priors = Priors::defaults();
        let mut indexed: Vec<_> = rows.iter().enumerate().map(|(i,(value,score,lineage,order))| {
            let kind = ["infra:Vendor","infra:Team","x:Fruit"][usize::from(*value)];
            let mut a = human(kind, *score, &format!("lineage{lineage}")); a.source_id = Some(i.to_string());
            (*order, a.with_key().unwrap())
        }).collect();
        let first: Vec<_> = indexed.iter().map(|(_,a)| a.clone()).collect();
        indexed.sort_by_key(|(k,_)| *k);
        let second: Vec<_> = indexed.into_iter().map(|(_,a)| a).collect();
        prop_assert_eq!(serde_json::to_vec(&decide(&first,&loaded,&priors,3000)).unwrap(), serde_json::to_vec(&decide(&second,&loaded,&priors,3000)).unwrap());
        prop_assert_eq!(supporters(&first).map(|a| &a.key).collect::<Vec<_>>(), supporters(&second).map(|a| &a.key).collect::<Vec<_>>());
    }
}

#[test]
fn duplicate_identity_and_same_lineage_never_add_weight() {
    let loaded = ontology();
    let priors = Priors::defaults();
    let a = human("infra:Vendor", 6000, "a");
    let b = human("infra:Team", 4000, "b");
    let expected = decide(&[a.clone(), b.clone()], &loaded, &priors, 0);
    let mut copied = a.clone();
    copied.source_id = Some("copy".into());
    copied = copied.with_key().unwrap();
    // Equal-weight same-lineage evidence may change the representative key, but not P.
    assert_eq!(
        winner(decide(&[a.clone(), copied, b.clone()], &loaded, &priors, 0)).1,
        Fixed(6000)
    );
    assert_eq!(
        decide(&[a.clone(), a.clone(), b.clone()], &loaded, &priors, 0),
        expected
    );
    let mut low = a.clone();
    low.raw_score = Fixed(3000);
    assert_eq!(
        decide(&[low.clone(), a.clone(), b.clone()], &loaded, &priors, 0),
        expected
    );
    assert_eq!(decide(&[b, a, low], &loaded, &priors, 0), expected);
}

#[test]
fn every_source_precedes_the_next_and_declared_precedes_discovered() {
    let loaded = ontology();
    let layers = [
        Layer::Human,
        Layer::V4,
        Layer::Reconcile,
        Layer::Script,
        Layer::Llm,
        Layer::Gliner,
        Layer::Gazetteer,
        Layer::Embed,
    ];
    let values = layers
        .iter()
        .map(|layer| (format!("{}.exact.type", layer.as_str()), 1.0))
        .collect();
    let priors = Priors::from_values(&values).unwrap();
    for pair in layers.windows(2) {
        let a = assertion(json!("infra:Vendor"), pair[0], Method::Exact, 6000, "a");
        let b = assertion(json!("infra:Team"), pair[1], Method::Exact, 6000, "b");
        assert_eq!(
            winner(decide(&[b, a.clone()], &loaded, &priors, 0)).0,
            a.key
        );
    }
    let z = OntologyFile::from_yaml(
        "ontology: z\nversion: 1\nkinds: {Last: {description: last}}\nrels: {}\n",
    )
    .unwrap();
    let loaded = resolve(&[z], &NamespaceSet::new()).unwrap();
    let a = human("z:Last", 6000, "a");
    let b = human("x:First", 6000, "b");
    // x sorts before z, so only the declared-kind tie break can choose z.
    assert_eq!(
        winner(decide(&[b, a.clone()], &loaded, &Priors::defaults(), 0)).0,
        a.key
    );
}

#[test]
fn canonical_id_combines_support_for_the_same_object_rid() {
    let loaded = ontology();
    let priors = Priors::defaults();
    let mut a = human("one", 6000, "a");
    a.claim_class = ClaimClass::CanonicalId;
    a.object_rid = Some(1);
    a = a.with_key().unwrap();
    let mut b = a.clone();
    b.value = json!("same object").into();
    b.raw_score = Fixed(2000);
    b.lineage_id = "b".into();
    b = b.with_key().unwrap();
    let mut c = a.clone();
    c.object_rid = Some(2);
    c.raw_score = Fixed(2000);
    c = c.with_key().unwrap();
    let (key, confidence, margin) = winner(decide(&[c, b, a.clone()], &loaded, &priors, 3000));
    assert_eq!((key, confidence, margin), (a.key, Fixed(8000), Fixed(6000)));
}

#[test]
fn ontology_governance_discovery_and_relationship_gates_require_review() {
    let file = OntologyFile::from_yaml(include_str!("../../kaban-core/fixtures/ontology/adr.yaml"))
        .unwrap();
    let core = OntologyFile::from_yaml(include_str!(
        "../../kaban-core/fixtures/ontology/kaban-core.yaml"
    ))
    .unwrap();
    let loaded = resolve(&[core, file], &NamespaceSet::new()).unwrap();
    let priors = Priors::defaults();
    ranked(
        decide(&[human("Decision", 10000, "a")], &loaded, &priors, 3000),
        CandidateReason::Governed,
    );
    ranked(
        decide(&[human("x:Novel", 10000, "a")], &loaded, &priors, 3000),
        CandidateReason::DiscoveredKind,
    );
    let group = [
        human("Entity", 10000, "a"),
        human("Decision", 1, "b"),
        human("Convention", 1, "c"),
    ];
    ranked(
        decide(&group, &loaded, &priors, 3000),
        CandidateReason::Governed,
    );
    let mut edge = human("confidence", 10000, "a");
    edge.claim_class = ClaimClass::Edge;
    edge.subject = Subject::Edge {
        src: 1,
        rel: "SUPERSEDES".into(),
        dst: 2,
    };
    edge = edge.with_key().unwrap();
    ranked(
        decide(&[edge], &loaded, &priors, 3000),
        CandidateReason::GovernedSemantics,
    );
}
