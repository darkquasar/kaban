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
