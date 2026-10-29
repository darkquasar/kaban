//! Compatible persisted fusion configuration and refusal branches.
#![allow(clippy::unwrap_used)]
use kaban_core::config::{ConfigError, Recall};

#[test]
fn rrf_precedence_and_conflicts_are_explicit() {
    for (fusion, configured, expected) in [
        ("rrf(60)", 60, 60),
        ("rrf(60)", 12, 12),
        ("rrf(7)", 60, 7),
        ("rrf(7)", 7, 7),
    ] {
        let cfg = Recall {
            fusion: fusion.into(),
            rrf_k: configured,
            ..Default::default()
        };
        assert_eq!(cfg.effective_rrf_k().unwrap(), expected);
    }
    for (fusion, k) in [
        ("rrf(7)", 12),
        ("rrf(0)", 60),
        ("rrf(60)", 0),
        ("rrf(nope)", 60),
        ("garbage", 60),
    ] {
        assert!(
            Recall {
                fusion: fusion.into(),
                rrf_k: k,
                ..Default::default()
            }
            .effective_rrf_k()
            .is_err()
        );
    }
    assert!(matches!(
        Recall {
            fusion: "cc(0.7)".into(),
            ..Default::default()
        }
        .effective_rrf_k(),
        Err(ConfigError::NotAvailable {
            path: "recall.fusion"
        })
    ));
}

#[test]
fn global_weights_reject_unknown_negative_and_nonfinite_values() {
    for (column, weight) in [("text", -1.0), ("text", f64::NAN), ("other", 1.0)] {
        let mut cfg = Recall::default();
        cfg.column_weights.insert(column.into(), weight);
        assert!(cfg.effective_rrf_k().is_err());
    }
    let mut cfg = Recall::default();
    cfg.column_weights.insert("text".into(), 0.0);
    assert_eq!(cfg.effective_rrf_k().unwrap(), 60);
}
