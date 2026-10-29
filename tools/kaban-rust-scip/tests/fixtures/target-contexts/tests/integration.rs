#[test]
fn integration_test() { assert_eq!(collision::common(), 1); }

#[test]
fn library_dependency_has_production_cfg() { assert_eq!(collision::production_only(), 1); }
