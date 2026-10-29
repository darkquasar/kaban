//! Table-driven denylist corpus (core-plan §7: `fixtures/corpus.yaml`, >= 200 cases, false-positive
//! budget 0). Each case names the text, the expected outcome and the rule ids that must appear.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

use kaban_policy::{Policy, PolicyFile};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Expect {
    Reject,
    Redact,
    Clean,
}

#[derive(Debug, Deserialize)]
struct Case {
    text: String,
    expect: Expect,
    #[serde(default)]
    rules: Vec<String>,
    /// When set, the case is a structured `key=value` leaf scanned through `scan_kv`.
    #[serde(default)]
    key: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Corpus {
    policy: PolicyFile,
    cases: Vec<Case>,
}

const CORPUS: &str = include_str!("../fixtures/corpus.yaml");

#[test]
fn policy_yaml_round_trip_preserves_fixture_values_and_scan_results() {
    let original = Policy::parse(include_bytes!("../../../examples/policy.yaml")).unwrap();
    let rendered = yaml_serde::to_string(&original).unwrap();
    let reparsed = Policy::parse(rendered.as_bytes()).unwrap();
    assert_eq!(
        serde_json::to_value(&original).unwrap(),
        serde_json::to_value(&reparsed).unwrap()
    );
    assert_eq!(yaml_serde::to_string(&reparsed).unwrap(), rendered);
    let before = Policy::from_file(original, "round-trip".into()).unwrap();
    let after = Policy::from_file(reparsed, "round-trip".into()).unwrap();
    for text in ["harmless notes", "old pw hunter2", "about Project Nightjar"] {
        assert_eq!(
            serde_json::to_value(before.scan(text)).unwrap(),
            serde_json::to_value(after.scan(text)).unwrap()
        );
    }
}

#[test]
fn policy_yaml_rejects_malformed_aliases_duplicates_and_multiple_documents() {
    for malformed in [
        "rules: [unfinished",
        "rules: *undefined\n",
        "rules: []\nrules: []\n",
        "rules: []\n---\nrules: []\n",
    ] {
        assert!(matches!(
            Policy::parse(malformed.as_bytes()),
            Err(kaban_policy::PolicyError::Yaml(_))
        ));
    }
}

#[test]
fn corpus_cases_scan_as_expected() {
    let corpus: Corpus = yaml_serde::from_str(CORPUS).unwrap();
    let policy = Policy::from_file(corpus.policy, "corpus".into()).unwrap();
    assert!(
        corpus.cases.len() >= 200,
        "corpus has {} cases, need >= 200",
        corpus.cases.len()
    );
    let (mut positive, mut negative) = (0usize, 0usize);
    let mut failures = Vec::new();
    for c in &corpus.cases {
        let v = match &c.key {
            Some(key) => policy.scan_kv(key, &c.text),
            None => policy.scan(&c.text),
        };
        let hit_ids: Vec<&str> = v.hits.iter().map(|h| h.rule.as_str()).collect();
        let ok = match c.expect {
            Expect::Reject => {
                positive += 1;
                v.rejected && c.rules.iter().all(|r| v.reject_rules.contains(r))
            }
            Expect::Redact => {
                positive += 1;
                !v.rejected
                    && !v.hits.is_empty()
                    && c.rules.iter().all(|r| hit_ids.contains(&r.as_str()))
            }
            Expect::Clean => {
                negative += 1;
                !v.rejected && v.hits.is_empty()
            }
        };
        if !ok {
            failures.push(format!(
                "{:?}: expected {:?} {:?}, got rejected={} hits={:?}",
                c.text, c.expect, c.rules, v.rejected, hit_ids
            ));
        }
        // verdicts never echo the input (rule ids and offsets only)
        let json = serde_json::to_string(&v).unwrap();
        for word in c.text.split_whitespace().filter(|w| w.len() >= 8) {
            let in_rule_ids = v.hits.iter().any(|h| h.rule.contains(word));
            assert!(
                !json.contains(word) || v.redacted.contains(word) || in_rule_ids,
                "verdict echoes input word {word:?}"
            );
        }
    }
    assert!(
        failures.is_empty(),
        "{} corpus failures:\n{}",
        failures.len(),
        failures.join("\n")
    );
    assert!(
        positive >= 100 && negative >= 100,
        "positive={positive} negative={negative}"
    );
}

/// The shipped example policy must load under the strict schema (core-spec S7) and keep its
/// documented behaviour (redact `hunter2`, reject the codename, allow only the anchored test key).
#[test]
fn example_policy_loads_under_strict_schema() {
    let yaml = include_str!("../../../examples/policy.yaml");
    let file = Policy::parse(yaml.as_bytes()).unwrap();
    let policy = Policy::from_file(file, "example".into()).unwrap();
    assert!(
        !policy.scan("sk-test-AbCdEfGhIjKlMnOpQrStUv").rejected,
        "documented test key allowed"
    );
    assert!(policy.scan("sk-test-short").rejected || policy.scan("sk-test-short").hits.is_empty());
    assert!(
        policy.scan("sk-AbCdEfGhIjKlMnOpQrStUvWxYz0123").rejected,
        "real key still rejected"
    );
    assert!(policy.scan("about Project Nightjar").rejected);
    assert_eq!(
        policy.scan("old pw hunter2").redacted,
        "old pw [REDACTED:old-password]"
    );
}
