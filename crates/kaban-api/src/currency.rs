//! Currency of a governing record (core-spec F14; core-plan C3.3): how much of its
//! `path_scope` changed in committed history since it was accepted.
//!
//! ```text
//!   CurrencyInput { repo, record_repo, anchor, path_scope }
//!        |  repo identity check (M3-23), argv hygiene (M3-6, M3-21)
//!        v
//!   GitRunner  =>  git log --name-status -M --format= <sha>..HEAD -- :(glob)<entry>...
//!        |          (or --since=<accepted_at>; once as a fallback when <sha> is unknown)
//!        |          + git ls-files -- :(glob)<entry>... when a D or R line was seen
//!        v
//!   attribute(): each changed path -> the scope entries it matches (globset, literal
//!        |        separator) ; staleness = changed entries / entries ; gone = an entry with
//!        v        a removal and no tracked file left at HEAD
//!   Currency { state, staleness, changed, gone, checked_at, reason }
//! ```
//!
//! Only committed history counts: the dirty worktree is ignored. `compute` never fails: git
//! absent, not a repository, another repository, a timeout or an unusable scope all yield
//! `Uncheckable` with a reason naming the class.

use crate::git::{GitError, GitRunner};
use serde::{Deserialize, Serialize};
use std::path::Path;
use time::OffsetDateTime;

/// Where committed history is read from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Anchor {
    /// `accepted_commit`: lowercase hex, 7 to 64 digits (anything else is an unknown anchor).
    Commit(String),
    /// `accepted_at` when no commit was recorded.
    Since(OffsetDateTime),
}

/// A changed path recorded by the `post-commit` hook (M5); always absent in M3.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangedPath {
    /// Repository-relative path (the new path of a rename).
    pub path: String,
    /// The path was deleted (or renamed away).
    pub removed: bool,
}

/// What `compute` needs for one record.
#[derive(Debug, Clone)]
pub struct CurrencyInput<'a> {
    /// Root of the repository this process runs in.
    pub repo: &'a Path,
    /// Repository identity stamped on the record (`node.repo`, M3-23); `None` for records
    /// written before stamping, which are checked against the current repository.
    pub record_repo: Option<&'a str>,
    /// Where history starts.
    pub anchor: Anchor,
    /// `accepted_at`, used once when `anchor` is a commit the repository does not know
    /// (M3-23).
    pub fallback_since: Option<OffsetDateTime>,
    /// The record's `path_scope` entries (paths or globs, repository-relative).
    pub path_scope: &'a [String],
    /// Hook-recorded changed paths, used instead of `git log` when present (F14).
    pub hints: Option<&'a [ChangedPath]>,
}

/// Currency state (core-spec F2 vocabulary).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CurrencyState {
    /// Less than half of the scope entries changed.
    Active,
    /// At least half of the scope entries changed.
    NeedsReview,
    /// Every scope entry is gone at HEAD.
    Stale,
    /// Git absent, not a repository, another repository, timeout or an unusable scope.
    Uncheckable,
}

/// The result of a currency check.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Currency {
    /// The state.
    pub state: CurrencyState,
    /// `changed_scope_entries / scope_entries` (0 when uncheckable).
    pub staleness: f32,
    /// Changed paths inside the scope, each once, sorted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changed: Vec<String>,
    /// Scope entries with no tracked file left at HEAD, sorted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gone: Vec<String>,
    /// When the check ran (RFC 3339).
    pub checked_at: String,
    /// Why the record is `Uncheckable` (a failure class, never caller data).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Repository-relative paths and globs that may reach git argv as `:(glob)<entry>` (M3-21):
/// not empty, not absolute, no `..` component, no pathspec magic or option prefix.
fn safe_entry(e: &str) -> bool {
    !e.trim().is_empty()
        && !e.starts_with('/')
        && !e.starts_with(':')
        && !e.starts_with('-')
        && !e.contains('\0')
        && !e.split('/').any(|c| c == "..")
}

/// One scope entry as a matcher over repository-relative paths: a glob (any of `*?[{`,
/// `literal_separator`, so `*` stays inside one path component and `**` crosses them) or a
/// literal path that also matches everything beneath it (git's directory pathspec rule).
pub struct ScopeMatcher {
    entry: String,
    glob: Option<globset::GlobMatcher>,
}

impl ScopeMatcher {
    /// Compile `entry`; an unparsable glob matches nothing but is kept as an entry.
    #[must_use]
    pub fn new(entry: &str) -> Self {
        let glob = entry
            .contains(['*', '?', '[', '{'])
            .then(|| {
                globset::GlobBuilder::new(entry)
                    .literal_separator(true)
                    .build()
                    .ok()
                    .map(|g| g.compile_matcher())
            })
            .flatten();
        Self {
            entry: entry.to_owned(),
            glob,
        }
    }

    /// The entry as written.
    #[must_use]
    pub fn entry(&self) -> &str {
        &self.entry
    }

    /// Whether `path` lies inside this entry.
    #[must_use]
    pub fn matches(&self, path: &str) -> bool {
        match &self.glob {
            Some(g) => g.is_match(path),
            None if self.entry.contains(['*', '?', '[', '{']) => false,
            None => {
                path == self.entry
                    || path
                        .strip_prefix(self.entry.as_str())
                        .is_some_and(|rest| rest.starts_with('/'))
            }
        }
    }
}

/// Committed changes under a set of pathspecs since an anchor: the output of one `git log`
/// (plus one `git ls-files` when a removal was seen), shared by every record with the same
/// anchor (M3-22) and attributed per record by [`attribute`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Snapshot {
    /// Paths changed in the range (the new path of a rename), each once.
    pub changed: Vec<String>,
    /// Paths deleted or renamed away in the range, each once.
    pub removed: Vec<String>,
    /// Tracked files at HEAD under the pathspecs; `None` when no removal was seen (then
    /// nothing can be gone).
    pub present: Option<Vec<String>>,
}

fn pathspecs(entries: &[String]) -> Vec<String> {
    let mut specs: Vec<String> = entries.iter().map(|e| format!(":(glob){e}")).collect();
    specs.sort();
    specs.dedup();
    specs
}

fn reason_of(e: &GitError) -> &'static str {
    match e {
        GitError::Spawn(_) => "git unavailable",
        GitError::Timeout(_) => "git timeout",
        GitError::Status(_) => "git failed",
        GitError::Output => "git output unusable",
    }
}

fn since_arg(at: OffsetDateTime) -> Option<String> {
    at.format(&time::format_description::well_known::Rfc3339)
        .ok()
        .map(|s| format!("--since={s}"))
}

/// Parse `--name-status` lines into `changed` and `removed`.
fn parse_name_status(out: &str, snap: &mut Snapshot) {
    for line in out.lines() {
        let mut cols = line.split('\t');
        let Some(status) = cols.next() else { continue };
        let Some(first) = cols.next() else { continue };
        match status.chars().next() {
            Some('D') => snap.removed.push(first.to_owned()),
            Some('R') => {
                snap.removed.push(first.to_owned());
                if let Some(new) = cols.next() {
                    snap.changed.push(new.to_owned());
                }
            }
            Some('C') => {
                if let Some(new) = cols.next() {
                    snap.changed.push(new.to_owned());
                }
            }
            Some(_) => snap.changed.push(first.to_owned()),
            None => {}
        }
    }
}

/// Run the history query for `entries` (the union over the records sharing `anchor`).
///
/// # Errors
/// The `Uncheckable` reason: an unusable entry, an unknown anchor without a fallback, or a git
/// failure class.
pub fn snapshot(
    repo: &Path,
    anchor: &Anchor,
    fallback_since: Option<OffsetDateTime>,
    entries: &[String],
    hints: Option<&[ChangedPath]>,
    git: &GitRunner,
) -> Result<Snapshot, &'static str> {
    if entries.is_empty() {
        return Err("empty path_scope");
    }
    if !entries.iter().all(|e| safe_entry(e)) {
        return Err("unusable path_scope entry");
    }
    let specs = pathspecs(entries);
    let mut snap = Snapshot::default();
    if let Some(hints) = hints {
        for h in hints {
            if h.removed {
                snap.removed.push(h.path.clone());
            } else {
                snap.changed.push(h.path.clone());
            }
        }
    } else {
        let range = match anchor {
            Anchor::Commit(sha) if crate::git::is_commit_hex(sha) => Some(format!("{sha}..HEAD")),
            Anchor::Commit(_) => None,
            Anchor::Since(at) => Some(since_arg(*at).ok_or("anchor unusable")?),
        };
        let fallback = fallback_since.and_then(since_arg);
        let first = range
            .clone()
            .or_else(|| fallback.clone())
            .ok_or("anchor unknown")?;
        let out = match log(repo, &first, &specs, git) {
            // an unknown commit (exit 128) falls back once to the acceptance time (M3-23)
            Err(GitError::Status(128)) if range.is_some() && fallback.is_some() => {
                let since = fallback.as_deref().unwrap_or_default();
                log(repo, since, &specs, git).map_err(|e| reason_of(&e))?
            }
            other => other.map_err(|e| reason_of(&e))?,
        };
        parse_name_status(&out, &mut snap);
    }
    snap.changed.sort();
    snap.changed.dedup();
    snap.removed.sort();
    snap.removed.dedup();
    if !snap.removed.is_empty() {
        let mut args = vec!["ls-files", "--"];
        args.extend(specs.iter().map(String::as_str));
        let out = git.run(repo, &args).map_err(|e| reason_of(&e))?;
        snap.present = Some(out.lines().map(str::to_owned).collect());
    }
    Ok(snap)
}

fn log(repo: &Path, range: &str, specs: &[String], git: &GitRunner) -> Result<String, GitError> {
    let mut args = vec!["log", "--name-status", "-M", "--format=", range, "--"];
    args.extend(specs.iter().map(String::as_str));
    git.run(repo, &args)
}

/// The canonical set of a `path_scope` (M3 review R3-6): `\` becomes `/` and an entry named
/// twice is kept once (first occurrence order). Applied before persistence (`accept`,
/// `remember`) and as the denominator of [`attribute`], so a record whose whole effective
/// scope is gone is `stale` whatever the stored list looked like.
#[must_use]
pub fn canonical_path_scope(entries: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(entries.len());
    for e in entries {
        let c = e.replace('\\', "/");
        if !out.contains(&c) {
            out.push(c);
        }
    }
    out
}

/// Attribute a snapshot to one record's `path_scope` (F14): `changed` = the changed paths
/// inside the scope, `gone` = entries with a removal and no tracked file left, `staleness` =
/// changed entries over entries; every entry gone => `Stale`, `>= 0.5` => `NeedsReview`.
/// Entries are counted on the canonical set ([`canonical_path_scope`], R3-6).
#[must_use]
pub fn attribute(snap: &Snapshot, path_scope: &[String]) -> Currency {
    let path_scope = canonical_path_scope(path_scope);
    if path_scope.is_empty() {
        return Currency::uncheckable("empty path_scope");
    }
    let matchers: Vec<ScopeMatcher> = path_scope.iter().map(|e| ScopeMatcher::new(e)).collect();
    let mut changed_entries = 0usize;
    let mut changed = Vec::new();
    let mut gone = Vec::new();
    for m in &matchers {
        let touched: Vec<&String> = snap.changed.iter().filter(|p| m.matches(p)).collect();
        let removed = snap.removed.iter().any(|p| m.matches(p));
        if !touched.is_empty() || removed {
            changed_entries += 1;
        }
        changed.extend(touched.into_iter().cloned());
        if removed
            && snap
                .present
                .as_deref()
                .is_some_and(|present| !present.iter().any(|p| m.matches(p)))
        {
            gone.push(m.entry().to_owned());
        }
    }
    changed.sort();
    changed.dedup();
    gone.sort();
    gone.dedup();
    #[allow(clippy::cast_precision_loss)] // path_scope has at most 32 entries
    let staleness = changed_entries as f32 / path_scope.len() as f32;
    let state = if gone.len() == path_scope.len() {
        CurrencyState::Stale
    } else if staleness >= 0.5 {
        CurrencyState::NeedsReview
    } else {
        CurrencyState::Active
    };
    Currency {
        state,
        staleness,
        changed,
        gone,
        checked_at: kaban_core::now_iso(),
        reason: None,
    }
}

impl Currency {
    /// An `Uncheckable` result with a failure-class reason.
    pub(crate) fn uncheckable(reason: &str) -> Self {
        Self {
            state: CurrencyState::Uncheckable,
            staleness: 0.0,
            changed: Vec::new(),
            gone: Vec::new(),
            checked_at: kaban_core::now_iso(),
            reason: Some(reason.to_owned()),
        }
    }
}

/// The currency of one record; never fails (see the module docs).
#[must_use]
pub fn compute(input: &CurrencyInput<'_>, git: &GitRunner) -> Currency {
    if let Some(recorded) = input.record_repo {
        let current = input.repo.file_name().and_then(|n| n.to_str());
        if current != Some(recorded) {
            return Currency::uncheckable("another repository");
        }
    }
    match snapshot(
        input.repo,
        &input.anchor,
        input.fallback_since,
        input.path_scope,
        input.hints,
        git,
    ) {
        Ok(snap) => attribute(&snap, input.path_scope),
        Err(reason) => Currency::uncheckable(reason),
    }
}
