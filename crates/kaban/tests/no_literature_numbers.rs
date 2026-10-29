//! Literature figures are context, not executable acceptance thresholds.
#![allow(clippy::unwrap_used)]

use std::path::Path;

fn forbidden(line: &str) -> bool {
    ["CoIR", "NanoBEIR", "CQADupStack"].iter().any(|name| {
        line.match_indices(name).any(|(at, _)| {
            // Same-line nearby figures, including intervening prose and punctuation.
            line[at + name.len()..]
                .chars()
                .take(80)
                .any(|ch| ch.is_ascii_digit())
        })
    })
}

fn scan_tests(path: &Path, violations: &mut Vec<String>) {
    if !path.exists() {
        return;
    }
    for entry in std::fs::read_dir(path).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            scan_tests(&path, violations);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            for (index, line) in std::fs::read_to_string(&path).unwrap().lines().enumerate() {
                if forbidden(line) {
                    violations.push(format!("{}:{}", path.display(), index + 1));
                }
            }
        }
    }
}

#[test]
fn guard_detects_invented_figures_but_allows_nonnumeric_context() {
    for name in ["CoIR", "NanoBEIR", "CQADupStack"] {
        assert!(forbidden(&format!("{name} score {}", 123)));
        assert!(!forbidden(&format!(
            "{name} is non-normative literature context"
        )));
    }
    assert!(!forbidden("local measured recall threshold"));
}

#[test]
fn test_tree_contains_no_literature_figures() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut violations = Vec::new();
    scan_tests(&root.join("tests"), &mut violations);
    for entry in std::fs::read_dir(root.join("crates")).unwrap() {
        scan_tests(&entry.unwrap().path().join("tests"), &mut violations);
    }
    assert!(
        violations.is_empty(),
        "literature figures in acceptance scope: {violations:?}"
    );
}
