//! The closed `label` vocabulary (stage1-spec §6.1, G1-S2; stage1-plan CG7.1 default 5):
//! `kaban_codegraph::SCIP_KIND_NAMES` is pinned by hand because `kaban-api` never links `scip`;
//! this crate does, so it checks the pin against the schema's own enum descriptor.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

use std::collections::BTreeSet;

use kaban_codegraph::SCIP_KIND_NAMES;
use protobuf::EnumFull;
use scip::types::symbol_information::Kind;

#[test]
fn label_values_match_pinned_scip_kinds() {
    let from_schema: Vec<String> = Kind::enum_descriptor()
        .values()
        .map(|v| v.name().to_owned())
        .collect();
    let pinned: Vec<String> = SCIP_KIND_NAMES.iter().map(|s| (*s).to_owned()).collect();
    assert_eq!(
        pinned, from_schema,
        "SCIP_KIND_NAMES must list every scip.proto 0.10.0 Kind name in declaration order"
    );
    assert!(
        SCIP_KIND_NAMES.contains(&"File"),
        "the file node's label is one of the kind names"
    );
    let distinct: BTreeSet<&str> = SCIP_KIND_NAMES.iter().copied().collect();
    assert_eq!(distinct.len(), SCIP_KIND_NAMES.len(), "no duplicate label");
}
