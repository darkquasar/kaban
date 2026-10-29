mod common;
fn owner() -> u32 { 1 }
#[test]
fn alpha() { assert_eq!(common::helper(), 1); }
