mod common;
fn owner() -> u32 { 2 }
#[test]
fn beta() { assert_eq!(common::helper(), 2); }
