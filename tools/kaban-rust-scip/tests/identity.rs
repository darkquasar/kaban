//! Target collision and escaping regression tests for the proposed SCIP identity.

use kaban_rust_scip::identity::{TargetIdentity, TargetKind};
use scip::types::{Descriptor, descriptor::Suffix};

fn item(name: &str) -> Descriptor {
    Descriptor {
        name: name.to_owned(),
        suffix: Suffix::Type.into(),
        ..Descriptor::default()
    }
}

#[test]
fn library_and_binary_with_same_name_and_source_have_distinct_symbols() {
    let lib = TargetIdentity::new(
        "demo",
        "0.1.0",
        TargetKind::Library,
        "demo",
        "src/shared.rs",
    )
    .expect("valid fixture identity");
    let bin = TargetIdentity::new("demo", "0.1.0", TargetKind::Binary, "demo", "src/shared.rs")
        .expect("valid fixture identity");
    assert_ne!(lib.symbol([item("Driver")]), bin.symbol([item("Driver")]));
    assert_eq!(
        lib.symbol([item("Driver")]),
        "kaban-rust-scip cargo demo 0.1.0 `lib:demo:src/shared.rs`/Driver#"
    );
}

#[test]
fn unicode_and_backticks_round_trip_through_standard_scip_parser() {
    let identity = TargetIdentity::new("demo", "0.1.0", TargetKind::Library, "demo", "src/é`x.rs")
        .expect("valid fixture identity");
    let symbol = identity.symbol([item("café")]);
    assert_eq!(
        symbol,
        "kaban-rust-scip cargo demo 0.1.0 `lib:demo:src/é``x.rs`/`café`#"
    );
    let decoded = scip::symbol::parse_symbol(&symbol).expect("valid SCIP");
    assert_eq!(decoded.descriptors[0].name, "lib:demo:src/é`x.rs");
    assert_eq!(decoded.descriptors[1].name, "café");
}

#[test]
fn rejects_absolute_and_non_normalized_roots() {
    for root in [
        "/checkout/src/lib.rs",
        "../lib.rs",
        "src/../lib.rs",
        "./lib.rs",
        "src//lib.rs",
        "",
    ] {
        assert!(TargetIdentity::new("demo", "0.1.0", TargetKind::Library, "demo", root).is_err());
    }
}

#[test]
fn different_relative_roots_disambiguate_same_package_target() {
    let left = TargetIdentity::new("demo", "0.1.0", TargetKind::Library, "demo", "a/src/lib.rs")
        .expect("valid fixture identity");
    let right = TargetIdentity::new("demo", "0.1.0", TargetKind::Library, "demo", "b/src/lib.rs")
        .expect("valid fixture identity");
    assert_ne!(
        left.symbol([item("Driver")]),
        right.symbol([item("Driver")])
    );
}
