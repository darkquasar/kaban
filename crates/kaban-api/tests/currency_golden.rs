//! Golden tests of the currency computation (core-spec F14; core-plan C3.3) on fixture
//! repositories built with fixed author and committer dates, so every run sees the same history.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::Scratch;
use kaban_api::GitRunner;
use kaban_api::currency::{Anchor, Currency, CurrencyInput, CurrencyState, compute};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

const FIXED_DATE: &str = "2026-10-26T08:16:02Z";

/// A throwaway git repository under a scratch directory; every commit is dated `FIXED_DATE`.
struct Repo {
    scratch: Scratch,
    root: PathBuf,
}

impl Repo {
    /// `git init` with four tracked files: `src/store/a.rs`, `src/store/b.rs`, `src/api/c.rs`
    /// and `src/api/nested/deep.rs`, plus `docs/x.md` outside `src/`.
    fn new(name: &str) -> Self {
        let scratch = Scratch::new("kaban-currency", name);
        let root = scratch.0.join("repo");
        std::fs::create_dir_all(root.join("src/store")).unwrap();
        std::fs::create_dir_all(root.join("src/api/nested")).unwrap();
        std::fs::create_dir_all(root.join("docs")).unwrap();
        let repo = Self { scratch, root };
        repo.git(&["init", "-q", "-b", "main"]);
        repo.git(&["config", "user.email", "fixture@example.invalid"]);
        repo.git(&["config", "user.name", "fixture"]);
        repo.write("src/store/a.rs", "a");
        repo.write("src/store/b.rs", "b");
        repo.write("src/api/c.rs", "c");
        repo.write("src/api/nested/deep.rs", "deep");
        repo.write("docs/x.md", "x");
        repo.commit("initial");
        repo
    }

    fn git(&self, args: &[&str]) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(&self.root)
            .args(args)
            .env("GIT_AUTHOR_DATE", FIXED_DATE)
            .env("GIT_COMMITTER_DATE", FIXED_DATE)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    }

    fn write(&self, rel: &str, content: &str) {
        let p = self.root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }

    fn commit(&self, msg: &str) -> String {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "--allow-empty", "-m", msg]);
        self.git(&["rev-parse", "HEAD"])
    }
}

fn scope(entries: &[&str]) -> Vec<String> {
    entries.iter().map(|s| (*s).to_owned()).collect()
}

fn check(repo: &Path, anchor: Anchor, entries: &[&str]) -> Currency {
    let path_scope = scope(entries);
    compute(
        &CurrencyInput {
            repo,
            record_repo: None,
            anchor,
            fallback_since: None,
            path_scope: &path_scope,
            hints: None,
        },
        &GitRunner::default(),
    )
}

#[test]
fn a_rename_inside_the_scope_counts_as_a_change_to_the_new_path() {
    let repo = Repo::new("rename");
    let accepted = repo.commit("accept here");
    repo.git(&["mv", "src/store/a.rs", "src/store/a2.rs"]);
    repo.commit("rename a");

    let c = check(
        &repo.root,
        Anchor::Commit(accepted),
        &["src/store/**", "src/api/**"],
    );
    assert_eq!(c.changed, vec!["src/store/a2.rs".to_owned()]);
    assert!(
        c.gone.is_empty(),
        "the renamed file still exists: {:?}",
        c.gone
    );
    assert_eq!(
        c.state,
        CurrencyState::NeedsReview,
        "1 of 2 entries changed"
    );
    assert!((c.staleness - 0.5).abs() < f32::EPSILON);
    assert_eq!(c.reason, None);
    assert_ne!(c.checked_at, "");
}

#[test]
fn deleting_one_of_two_entries_is_half_stale_and_deleting_both_is_stale() {
    let repo = Repo::new("delete");
    let accepted = repo.commit("accept here");
    repo.git(&["rm", "-q", "src/store/b.rs"]);
    repo.commit("drop b");

    let c = check(
        &repo.root,
        Anchor::Commit(accepted.clone()),
        &["src/store/a.rs", "src/store/b.rs"],
    );
    assert_eq!(c.state, CurrencyState::NeedsReview);
    assert!((c.staleness - 0.5).abs() < f32::EPSILON);
    assert_eq!(
        c.changed,
        Vec::<String>::new(),
        "a deletion is not a changed path"
    );
    assert_eq!(c.gone, vec!["src/store/b.rs".to_owned()]);

    // a rename out of the scope is a deletion for that scope
    repo.git(&["mv", "src/store/a.rs", "docs/a.rs"]);
    repo.commit("move a away");
    let c = check(
        &repo.root,
        Anchor::Commit(accepted),
        &["src/store/a.rs", "src/store/b.rs"],
    );
    assert_eq!(c.state, CurrencyState::Stale);
    assert_eq!(
        c.gone,
        vec!["src/store/a.rs".to_owned(), "src/store/b.rs".to_owned()]
    );
    assert!((c.staleness - 1.0).abs() < f32::EPSILON);
}

/// M3 review R3-6: a scope that names one entry twice (also with backslash separators) is one
/// entry; when it is gone the whole effective scope is gone and the record is `stale`, not
/// `needs_review` on a denominator of two.
#[test]
fn a_duplicated_scope_entry_counts_once_so_its_deletion_is_stale_not_half_stale() {
    let repo = Repo::new("duplicate");
    let accepted = repo.commit("accept here");
    repo.git(&["rm", "-q", "src/store/b.rs"]);
    repo.commit("drop b");

    let c = check(
        &repo.root,
        Anchor::Commit(accepted),
        &["src/store/b.rs", "src/store/b.rs", "src\\store\\b.rs"],
    );
    assert_eq!(c.state, CurrencyState::Stale, "{c:?}");
    assert!((c.staleness - 1.0).abs() < f32::EPSILON, "{c:?}");
    assert_eq!(c.gone, vec!["src/store/b.rs".to_owned()]);
}

#[test]
fn a_recursive_glob_matches_a_nested_change_and_a_single_star_does_not() {
    let repo = Repo::new("glob");
    let accepted = repo.commit("accept here");
    repo.write("src/api/nested/deep.rs", "deeper");
    repo.commit("touch deep");

    let c = check(
        &repo.root,
        Anchor::Commit(accepted.clone()),
        &["src/**/*.rs"],
    );
    assert_eq!(c.changed, vec!["src/api/nested/deep.rs".to_owned()]);
    assert_eq!(c.state, CurrencyState::NeedsReview);

    // `*` stays inside one path component (literal separator): nothing in `src/api/*.rs`
    // changed, so the record is current
    let c = check(&repo.root, Anchor::Commit(accepted), &["src/api/*.rs"]);
    assert!(c.changed.is_empty(), "{:?}", c.changed);
    assert_eq!(c.state, CurrencyState::Active);
    assert_eq!(c.staleness, 0.0);
}

#[test]
fn several_commits_to_one_path_count_it_once() {
    let repo = Repo::new("multi");
    let accepted = repo.commit("accept here");
    for i in 0..3 {
        repo.write("src/api/c.rs", &format!("c{i}"));
        repo.commit("edit c");
    }
    repo.write("docs/x.md", "outside");
    repo.commit("docs only");

    let c = check(
        &repo.root,
        Anchor::Commit(accepted),
        &["src/api/**", "src/store/**", "docs/**"],
    );
    assert_eq!(
        c.changed,
        vec!["docs/x.md".to_owned(), "src/api/c.rs".to_owned()]
    );
    assert_eq!(
        c.state,
        CurrencyState::NeedsReview,
        "2 of 3 entries changed"
    );
    assert!((c.staleness - 2.0 / 3.0).abs() < 1e-6);
}

#[test]
fn no_commit_since_acceptance_is_active_and_the_dirty_worktree_is_ignored() {
    let repo = Repo::new("active");
    let accepted = repo.commit("accept here");
    repo.write("src/store/a.rs", "uncommitted edit");

    let c = check(&repo.root, Anchor::Commit(accepted), &["src/store/**"]);
    assert_eq!(c.state, CurrencyState::Active);
    assert_eq!(c.staleness, 0.0);
    assert_eq!(c.changed, Vec::<String>::new());
    assert_eq!(c.gone, Vec::<String>::new());

    // the same question by time: the fixture dates every commit 2026-10-26, so a `since`
    // anchor after it sees nothing and one before it sees the whole history
    let after = time::OffsetDateTime::from_unix_timestamp(1_793_002_562 + 60).unwrap();
    let c = check(&repo.root, Anchor::Since(after), &["src/store/**"]);
    assert_eq!(c.state, CurrencyState::Active);
    let before = time::OffsetDateTime::from_unix_timestamp(1_793_002_562 - 60).unwrap();
    let c = check(&repo.root, Anchor::Since(before), &["src/store/**"]);
    assert_eq!(c.state, CurrencyState::NeedsReview);
    assert_eq!(
        c.changed,
        vec!["src/store/a.rs".to_owned(), "src/store/b.rs".to_owned()]
    );
}

/// Whether `pid` is still a live process (a reaped or zombie process is not).
fn alive(pid: &str) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|s| {
            // "<pid> (<comm>) <state> ..."; comm may contain spaces, so split after ')'
            let after = s.rsplit(')').next()?.trim_start();
            after.chars().next()
        })
        .is_some_and(|state| state != 'Z' && state != 'X')
}

#[test]
fn a_hanging_git_with_a_grandchild_holding_the_pipe_is_uncheckable_within_the_budget() {
    let repo = Repo::new("timeout");
    let pidfile = repo.root.join("grandchild.pid");
    let wrapper = repo.root.join("slow-git.sh");
    // the wrapper forks a grandchild that inherits the stdout pipe, then hangs itself
    std::fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\nsleep 30 &\necho $! > '{}'\nexec sleep 30\n",
            pidfile.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(
        &wrapper,
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .unwrap();
    let git = GitRunner {
        program: wrapper,
        timeout: Duration::from_millis(200),
    };
    let path_scope = scope(&["src/**"]);
    let started = std::time::Instant::now();
    let c = compute(
        &CurrencyInput {
            repo: &repo.root,
            record_repo: None,
            anchor: Anchor::Commit("0123456789abcdef".into()),
            fallback_since: None,
            path_scope: &path_scope,
            hints: None,
        },
        &git,
    );
    let took = started.elapsed();
    assert_eq!(c.state, CurrencyState::Uncheckable);
    assert_eq!(c.reason.as_deref(), Some("git timeout"));
    // the product deadline is 200 ms; the slice budget (core-plan C3.3) is 250 ms
    assert!(took < Duration::from_millis(250), "took {took:?}");
    // the grandchild inherited the pipe; the group kill reached it (R-C3.2-4)
    let pid = std::fs::read_to_string(&pidfile).unwrap().trim().to_owned();
    assert_ne!(pid, "");
    let deadline = std::time::Instant::now() + Duration::from_millis(200);
    while alive(&pid) && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(!alive(&pid), "grandchild {pid} survived the group kill");
}

#[test]
fn unsafe_entries_bad_anchors_and_another_repository_are_uncheckable_and_unknown_commits_fall_back()
{
    let repo = Repo::new("negative");
    let accepted = repo.commit("accept here");
    repo.write("src/store/a.rs", "edited");
    repo.commit("edit a");

    // `accept` refuses these (C3.2); the computation refuses them again before argv
    for bad in ["../etc/**", "/etc/passwd", ":(top)src", "--output=x", ""] {
        let c = check(
            &repo.root,
            Anchor::Commit(accepted.clone()),
            &["src/**", bad],
        );
        assert_eq!(c.state, CurrencyState::Uncheckable, "{bad:?}");
        assert_eq!(
            c.reason.as_deref(),
            Some("unusable path_scope entry"),
            "{bad:?}"
        );
    }
    // an anchor that is not a commit id never reaches argv (M3-6)
    let c = check(&repo.root, Anchor::Commit("HEAD~1".into()), &["src/**"]);
    assert_eq!(c.state, CurrencyState::Uncheckable);
    assert_eq!(c.reason.as_deref(), Some("anchor unknown"));
    // a well-formed commit the repository does not know: git fails, then the acceptance time
    // is tried once (M3-23)
    let unknown = "0123456789abcdef0123456789abcdef01234567".to_owned();
    let c = check(&repo.root, Anchor::Commit(unknown.clone()), &["src/**"]);
    assert_eq!(c.state, CurrencyState::Uncheckable);
    assert_eq!(c.reason.as_deref(), Some("git failed"));
    let path_scope = scope(&["src/store/**"]);
    let before = time::OffsetDateTime::from_unix_timestamp(1_793_002_562 - 60).unwrap();
    let c = compute(
        &CurrencyInput {
            repo: &repo.root,
            record_repo: None,
            anchor: Anchor::Commit(unknown),
            fallback_since: Some(before),
            path_scope: &path_scope,
            hints: None,
        },
        &GitRunner::default(),
    );
    assert_eq!(c.state, CurrencyState::NeedsReview);
    assert!(c.changed.contains(&"src/store/a.rs".to_owned()));
    // the record belongs to another repository (identity = root directory name, M3-23)
    let c = compute(
        &CurrencyInput {
            repo: &repo.root,
            record_repo: Some("some-other-clone"),
            anchor: Anchor::Commit(accepted.clone()),
            fallback_since: None,
            path_scope: &path_scope,
            hints: None,
        },
        &GitRunner::default(),
    );
    assert_eq!(c.state, CurrencyState::Uncheckable);
    assert_eq!(c.reason.as_deref(), Some("another repository"));
    let c = compute(
        &CurrencyInput {
            repo: &repo.root,
            record_repo: Some("repo"),
            anchor: Anchor::Commit(accepted),
            fallback_since: None,
            path_scope: &path_scope,
            hints: None,
        },
        &GitRunner::default(),
    );
    assert_eq!(c.state, CurrencyState::NeedsReview);
    // not a repository at all
    let c = check(
        &repo.scratch.0,
        Anchor::Commit("0123456789abcdef".into()),
        &["src/**"],
    );
    assert_eq!(c.state, CurrencyState::Uncheckable);
    assert_eq!(c.reason.as_deref(), Some("git failed"));
}
