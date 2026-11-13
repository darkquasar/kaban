//! PC-5: the sections of removed features (`[reconcile]`, `[er]`, `[why]`, `[briefing]`) are
//! accepted and ignored with one warning naming each, so an older `config.yaml` still loads.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

use std::path::PathBuf;

use kaban_core::config::{
    ConfigError, ConfigWarning, EngineConfig, REMOVED_SECTIONS, load, load_reporting,
};

/// A scratch `config.yaml` holding `yaml`, unique per test.
fn write(name: &str, yaml: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "kaban-config-compat-{}-{name}.yaml",
        std::process::id()
    ));
    std::fs::write(&path, yaml).unwrap();
    path
}

#[test]
fn a_reconcile_section_loads_with_one_warning_naming_the_section() {
    let path = write(
        "reconcile",
        "reconcile:\n  tau_type: 0.3\n  priors: /tmp/priors.yaml\nlink:\n  default: none\n",
    );
    let (cfg, warnings) = load_reporting(&path).unwrap();
    assert_eq!(cfg, EngineConfig::default());
    assert_eq!(
        warnings,
        vec![ConfigWarning::RemovedSection {
            section: "reconcile"
        }]
    );
    assert_eq!(
        warnings[0].to_string(),
        "config section [reconcile] is no longer used and is ignored"
    );
    assert_eq!(load(&path).unwrap(), EngineConfig::default());
    std::fs::remove_file(path).unwrap();
}

#[test]
fn every_removed_section_is_ignored_once_and_the_rest_still_applies() {
    let path = write(
        "all",
        "why:\n  anchor_prefilter: false\nbriefing:\n  max_candidates: 9\n\
         er:\n  m:\n    colour: 0.5\nreconcile: {}\nontology:\n  pattern_size_limit_kib: 2048\n",
    );
    let (cfg, warnings) = load_reporting(&path).unwrap();
    assert_eq!(cfg.ontology.pattern_size_limit_kib, 2048);
    assert_eq!(
        cfg.effective_overrides(),
        vec![(
            "ontology.pattern_size_limit_kib".to_owned(),
            "2048".to_owned()
        )]
    );
    let named: Vec<&str> = warnings
        .iter()
        .map(|ConfigWarning::RemovedSection { section }| *section)
        .collect();
    assert_eq!(named, REMOVED_SECTIONS);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn a_file_without_removed_sections_has_no_warning_and_unknown_keys_still_fail() {
    let path = write("clean", "link:\n  default: none\n");
    assert_eq!(load_reporting(&path).unwrap().1, Vec::new());
    // only the top-level section is accepted: a removed name nested elsewhere is still unknown
    std::fs::write(&path, "link:\n  reconcile: {}\n").unwrap();
    assert!(matches!(
        load_reporting(&path),
        Err(ConfigError::Parse { ref path }) if path == "link.reconcile"
    ));
    std::fs::remove_file(path).unwrap();
}
