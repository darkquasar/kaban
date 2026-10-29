//! Exact integer assertions are architecture neutral. Writer host: `aarch64`; `x86_64` is CI's run.
#![allow(clippy::unwrap_used)]
use kaban_core::Fixed;

#[test]
fn decimal_inputs_are_rounded_once_before_integer_accumulation() {
    let a = Fixed::from_f64(0.1).unwrap();
    let b = Fixed::from_f64(0.2).unwrap();
    let c = Fixed::from_f64(0.1 + 0.2).unwrap();
    assert_eq!(i64::from(a.0) + i64::from(b.0), i64::from(c.0));
    assert_eq!(c.0, 3000);
}

#[test]
fn confidence_rejects_out_of_range_and_rounds_ties_even() {
    assert!(Fixed::new(10_001).is_err());
    assert!(serde_json::from_str::<Fixed>("10001").is_err());
    for v in [-0.01, 1.01, f64::NAN, f64::INFINITY] {
        assert!(Fixed::from_f64(v).is_err());
    }
    assert_eq!(Fixed::from_f64(0.00005).unwrap(), Fixed(0));
    assert_eq!(Fixed::from_f64(0.00025).unwrap(), Fixed(2));
}

fn permutations(a: &mut [i64], at: usize, expected: i64) {
    if at == a.len() {
        assert_eq!(a.iter().sum::<i64>(), expected);
    }
    for i in at..a.len() {
        a.swap(at, i);
        permutations(a, at + 1, expected);
        a.swap(at, i);
    }
}

#[test]
fn signed_title_weights_and_all_mixed_permutations_are_exact() {
    use kaban_core::LogWeight;
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("../fixtures/reconcile/log-weights.json")).unwrap();
    assert_eq!(
        LogWeight::log2_ratio(Fixed(9000), Fixed(100)),
        LogWeight(64919)
    );
    assert_eq!(
        LogWeight::log2_ratio(Fixed(1000), Fixed(9900)),
        LogWeight(-33074)
    );
    assert_eq!(LogWeight::from_f64(-3.3074).unwrap(), LogWeight(-33074));
    let mut values: Vec<i64> = fixture["mixed"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_i64().unwrap())
        .collect();
    permutations(&mut values, 0, -17610);
    assert!(LogWeight(-1) < LogWeight(0));
}

#[test]
fn signed_overflow_is_carried_in_i64_until_the_only_clamp_seam() {
    use kaban_core::LogWeight;
    let sum = [LogWeight(i32::MAX); 5]
        .iter()
        .map(|w| i64::from(w.0))
        .sum::<i64>();
    assert_eq!(sum, 10_737_418_235);
    assert_eq!(
        LogWeight::raw_score(sum, LogWeight(80000)).unwrap(),
        Fixed::ONE
    );
    assert_eq!(
        LogWeight::raw_score(-sum, LogWeight(80000)).unwrap(),
        Fixed::ZERO
    );
    assert_eq!(
        LogWeight::raw_score(40000, LogWeight(80000)).unwrap(),
        Fixed(5000)
    );
    assert!(LogWeight::raw_score(sum, LogWeight(0)).is_err());
    assert!(LogWeight::from_f64(f64::INFINITY).is_err());
}

fn claim(value: serde_json::Value) -> kaban_core::Assertion {
    use kaban_core::{
        Assertion, AssertionKey, CanonicalJson, ClaimClass, Layer, Method, Rank, Subject,
        WorkspaceId,
    };
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
        layer: Layer::Gliner,
        method: Method::Exact,
        extractor_id: None,
        extractor_version: None,
        source_id: None,
        lineage_id: "a".into(),
        raw_score: Fixed(6000),
        rank: Rank::Normal,
        rank_reason: None,
        calibration_version: 1,
        supersedes: None,
        ts: 0,
    }
    .with_key()
    .unwrap()
}

#[test]
fn stable_keys_ignore_non_identity_fields_and_json_insertion_order() {
    use kaban_core::{AssertionKey, Rank, group_key};
    let a = claim(serde_json::json!({"b": 2, "a": 1}));
    let mut b = claim(serde_json::from_str("{\"a\":1,\"b\":2}").unwrap());
    b.ts = 99;
    b.rank = Rank::Preferred;
    b.rank_reason = Some("human".into());
    b.raw_score = Fixed(5000);
    b.calibration_version = 2;
    b.supersedes = Some(AssertionKey("old".into()));
    b = b.with_key().unwrap();
    assert_eq!(a.key, b.key);
    assert_eq!(a.key.0.len(), 64);
    let identity = br#"{"chunk_rid":17,"claim_class":"type","extractor_id":null,"extractor_version":null,"layer":"gliner","len":5,"lineage_id":"a","method":"exact","object_rid":null,"offset":27,"predicate":null,"source_id":null,"subject_kind":"span","subject_rid":null,"value":{"a":1,"b":2},"workspace_id":"w"}"#;
    assert_eq!(a.key.0, blake3::hash(identity).to_hex().as_str());
    assert_eq!(group_key(&a), group_key(&b));
    b.lineage_id = "other".into();
    assert_ne!(a.key, b.with_key().unwrap().key);
    let mut invalid = a.clone();
    invalid.raw_score = Fixed(10001);
    assert!(invalid.with_key().is_err());
}

#[test]
fn priors_defaults_and_unknown_key_fail_closed() {
    use kaban_core::{ClaimClass, Layer, Method, Priors};
    let p = Priors::defaults();
    let defaults = [
        (Layer::Gazetteer, Method::Id, 10000),
        (Layer::Gazetteer, Method::Path, 9000),
        (Layer::Gazetteer, Method::Exact, 9000),
        (Layer::Gazetteer, Method::Alias, 8000),
        (Layer::Gazetteer, Method::Symbol, 6000),
        (Layer::Gazetteer, Method::Tag, 9000),
        (Layer::Embed, Method::Embed, 5000),
        (Layer::Gliner, Method::Gliner, 6000),
        (Layer::Llm, Method::Llm, 7500),
        (Layer::Human, Method::Human, 10000),
        (Layer::V4, Method::Measured, 10000),
        (Layer::V4, Method::Rule, 8000),
        (Layer::V4, Method::Heuristic, 5000),
    ];
    for (layer, method, score) in defaults {
        for &class in ClaimClass::ALL {
            assert_eq!(p.lookup(layer, method, class), Some(Fixed(score)));
        }
    }
    assert_eq!(
        p.lookup(Layer::Gazetteer, Method::Alias, ClaimClass::Type),
        Some(Fixed(8000))
    );
    assert_eq!(
        p.lookup(Layer::Llm, Method::ErJudge, ClaimClass::Identity),
        Some(Fixed(7500))
    );
    assert_eq!(
        p.lookup(Layer::Script, Method::Link, ClaimClass::Edge),
        None
    );
    assert_eq!(
        Priors::load("priors:\n  gazetteer.alias.typo: 0.8\n")
            .unwrap_err()
            .to_string(),
        "priors_unknown_key"
    );
    assert!(Priors::load("unknown: {}\n").is_err());
    assert!(Priors::load("priors:\n  gliner.exact.type: 1.01\n").is_err());
    let loaded = Priors::load("priors:\n  gazetteer.alias.type: 0.5\n").unwrap();
    assert_eq!(
        loaded.lookup(Layer::Gazetteer, Method::Alias, ClaimClass::Type),
        Some(Fixed(5000))
    );
    assert_eq!(
        loaded.lookup(Layer::Llm, Method::Exact, ClaimClass::Type),
        None
    );
}

#[test]
fn conflict_keys_use_exact_workspace_class_and_subject_coordinates() {
    use kaban_core::{ClaimClass, Subject, WorkspaceId, group_key, same_group};
    let a = claim(serde_json::json!("infra:Vendor"));
    let mut b = a.clone();
    b.value = serde_json::json!("infra:Team").into();
    assert_eq!(group_key(&a), group_key(&b));
    b.subject = Subject::Span {
        chunk_rid: 17,
        offset: 27,
        len: 13,
    };
    assert_ne!(group_key(&a), group_key(&b));
    b = a.clone();
    b.claim_class = ClaimClass::CanonicalId;
    assert_ne!(group_key(&a), group_key(&b));
    b = a.clone();
    b.workspace_id = WorkspaceId::new("other");
    assert_ne!(group_key(&a), group_key(&b));
    let mut a = a;
    a.subject = Subject::Node(4);
    a.claim_class = ClaimClass::PropValue;
    a.predicate = Some("title".into());
    b = a.clone();
    b.predicate = Some("summary".into());
    assert!(!same_group(&a, &b));
    assert_ne!(group_key(&a), group_key(&b));
    a.claim_class = ClaimClass::Edge;
    a.subject = Subject::Edge {
        src: 1,
        rel: "R".into(),
        dst: 2,
    };
    a = a.with_key().unwrap();
    b = a.clone();
    b.subject = Subject::Edge {
        src: 1,
        rel: "R".into(),
        dst: 3,
    };
    b = b.with_key().unwrap();
    assert_ne!(a.key, b.key);
    assert_ne!(group_key(&a), group_key(&b));
    assert_eq!(a.predicate.as_deref(), Some("R"));
    assert_eq!(a.object_rid, Some(2));
}
