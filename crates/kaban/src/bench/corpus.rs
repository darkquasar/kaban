//! The deterministic §16 corpus: xorshift RNG, Zipf vocabulary, note kinds and lifecycle mix,
//! workspace shares and the sequential [`Corpus`] generator (C3.1 owns the note props).

/// The §16 seed.
pub(crate) const SEED: u64 = 0x9E37_79B9_7F4A_7C15;
const VOCAB_SIZE: usize = 5_000;
const ZIPF_S: f64 = 1.07;
pub(super) const WORKSPACES: &[(&str, u32)] =
    &[("ws1", 80), ("ws2", 5), ("ws3", 5), ("ws4", 5), ("ws5", 5)];
/// Relationship mix for the random edges ("rels as spike" minus the two with lifecycle side
/// effects, so the lifecycle mix stays the one the corpus defines; see the task report).
pub(super) const RELS: &[&str] = &["RELATES_TO", "CAUSED_BY", "FIXED_BY", "ABOUT", "DEPENDS_ON"];
const SYLLABLES: &[&str] = &[
    "ka", "ba", "no", "ri", "tu", "me", "lo", "sa", "vi", "de", "po", "ru", "ne", "ta", "fi", "go",
    "mu", "le", "zo", "wa",
];

pub(super) struct Rng(u64);
impl Rng {
    /// A second stream seeded independently of the corpus (the `--chunks` corpus, DI1.7), so an
    /// extra phase never shifts the notes' or the read phase's draws.
    #[cfg(feature = "ingest")]
    pub(super) fn seeded(seed: u64) -> Self {
        Self(seed)
    }
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    pub(super) fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    /// Uniform in `[0, 1)`.
    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
    fn percent(&mut self) -> u32 {
        self.below(100) as u32
    }
}

/// Kind of a corpus note (§16 mix).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    Fact,
    Failure,
    Decision,
    Convention,
    Idea,
    Insight,
}
impl Kind {
    pub(super) fn name(self) -> &'static str {
        match self {
            Self::Fact => "Fact",
            Self::Failure => "Failure",
            Self::Decision => "Decision",
            Self::Convention => "Convention",
            Self::Idea => "Idea",
            Self::Insight => "Insight",
        }
    }
}

/// Lifecycle a note is moved to after it is written (§16 mix).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Life {
    Active,
    Archived,
    Tombstoned,
}

/// One generated note, before it is written.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct NoteSpec {
    pub(super) kind: Kind,
    pub(super) text: String,
    pub(super) tags: Vec<String>,
    pub(super) workspace: &'static str,
    pub(super) life: Life,
    /// Decisions only: 1-3 globs.
    pub(super) path_scope: Vec<String>,
    /// Decisions only: accepted with an event (70%).
    pub(super) accepted: bool,
    /// Decisions only: carries a `CodeRef` (10%).
    pub(super) coderef: bool,
}

/// The deterministic §16 corpus generator.
pub(crate) struct Corpus {
    pub(super) rng: Rng,
    vocab: Vec<String>,
    cdf: Vec<f64>,
}

impl Corpus {
    pub(crate) fn new(seed: u64) -> Self {
        let vocab = (0..VOCAB_SIZE).map(word).collect();
        let mut cdf = Vec::with_capacity(VOCAB_SIZE);
        let mut acc = 0.0;
        for k in 1..=VOCAB_SIZE {
            acc += 1.0 / (k as f64).powf(ZIPF_S);
            cdf.push(acc);
        }
        for c in &mut cdf {
            *c /= acc;
        }
        Self {
            rng: Rng(seed),
            vocab,
            cdf,
        }
    }
    /// A path under the `i`-th vocabulary directory (the shape of the Decision globs
    /// `src/<vocab>/**`): `src/<vocab[i]>/<vocab[j]>.rs`.
    pub(super) fn vocab_path(&mut self) -> String {
        let dir = self.rng.below(VOCAB_SIZE);
        let file = self.rng.below(VOCAB_SIZE);
        format!("src/{}/{}.rs", self.vocab[dir], self.vocab[file])
    }
    /// The vocabulary word at `i` (wrapping).
    pub(super) fn vocab_word(&self, i: usize) -> &str {
        &self.vocab[i % VOCAB_SIZE]
    }
    /// Rank of one Zipf-distributed term (0 = most frequent).
    fn zipf_rank(&mut self) -> usize {
        let u = self.rng.unit();
        self.cdf.partition_point(|c| *c < u).min(VOCAB_SIZE - 1)
    }
    pub(super) fn term(&mut self) -> String {
        let r = self.zipf_rank();
        self.vocab[r].clone()
    }
    /// One Zipf-distributed term drawn from `rng` instead of the corpus stream: the same
    /// vocabulary and distribution as the notes, so a second corpus shares their candidate sets.
    #[cfg(feature = "ingest")]
    pub(super) fn term_from(&self, rng: &mut Rng) -> &str {
        let u = rng.unit();
        &self.vocab[self.cdf.partition_point(|c| *c < u).min(VOCAB_SIZE - 1)]
    }
    /// A term among the `count` most frequent ranks.
    pub(super) fn common_term(&mut self, count: usize) -> String {
        let r = self.rng.below(count);
        self.vocab[r].clone()
    }
    /// A term among the `count` least frequent ranks.
    pub(super) fn rare_term(&mut self, count: usize) -> String {
        let r = VOCAB_SIZE - 1 - self.rng.below(count);
        self.vocab[r].clone()
    }
    pub(super) fn identifier(&mut self) -> String {
        if self.rng.below(2) == 0 {
            format!("Err{:04x}", self.rng.below(0x10000))
        } else {
            let a = self.rng.below(VOCAB_SIZE);
            let b = self.rng.below(VOCAB_SIZE);
            format!("src/{}/{}.rs", self.vocab[a], self.vocab[b])
        }
    }
    /// The `i`-th note of the corpus (the generator is sequential: notes are produced in order).
    pub(crate) fn note(&mut self, i: usize) -> NoteSpec {
        let kind = match self.rng.percent() {
            0..=39 => Kind::Fact,
            40..=59 => Kind::Failure,
            60..=74 => Kind::Decision,
            75..=79 => Kind::Convention,
            80..=89 => Kind::Idea,
            _ => Kind::Insight,
        };
        let life = match self.rng.percent() {
            0..=84 => Life::Active,
            85..=94 => Life::Archived,
            _ => Life::Tombstoned,
        };
        let p = self.rng.percent();
        let mut acc = 0;
        let mut workspace = WORKSPACES[0].0;
        for (name, share) in WORKSPACES {
            acc += share;
            if p < acc {
                workspace = name;
                break;
            }
        }
        let tokens = 8 + self.rng.below(17);
        let mut text = format!("note {i}:");
        for _ in 0..tokens {
            text.push(' ');
            if self.rng.percent() < 20 {
                let id = self.identifier();
                text.push_str(&id);
            } else {
                let t = self.term();
                text.push_str(&t);
            }
        }
        let tags = vec![self.term(), self.term()];
        let (path_scope, accepted, coderef) = if kind == Kind::Decision {
            let globs = 1 + self.rng.below(3);
            let scope = (0..globs)
                .map(|_| {
                    let a = self.rng.below(VOCAB_SIZE);
                    format!("src/{}/**", self.vocab[a])
                })
                .collect();
            (scope, self.rng.percent() < 70, self.rng.percent() < 10)
        } else {
            (Vec::new(), false, false)
        };
        NoteSpec {
            kind,
            text,
            tags,
            workspace,
            life,
            path_scope,
            accepted,
            coderef,
        }
    }
}

/// Deterministic pseudo-word for vocabulary rank `k` (three or four syllables, unique per rank).
fn word(k: usize) -> String {
    let n = SYLLABLES.len();
    let mut s = String::new();
    s.push_str(SYLLABLES[k % n]);
    s.push_str(SYLLABLES[(k / n) % n]);
    s.push_str(SYLLABLES[(k / (n * n)) % n]);
    if k >= n * n * n {
        s.push_str(SYLLABLES[(k / (n * n * n)) % n]);
    }
    s
}

impl Corpus {
    /// Workspace of the next random edge, drawn with the §16 workspace shares.
    pub(super) fn note_workspace_for_link(&mut self) -> &'static str {
        let p = self.rng.percent();
        let mut acc = 0;
        for (name, share) in WORKSPACES {
            acc += share;
            if p < acc {
                return name;
            }
        }
        WORKSPACES[0].0
    }
}

/// Commits in the N2 fixture repository (core-plan §16 `why_one_path`).
pub(crate) const FIXTURE_COMMITS: usize = 50;
/// Files touched per fixture commit, each under its own `src/<vocab>/` directory.
const FIXTURE_FILES_PER_COMMIT: usize = 4;
/// Fixed dates so the fixture history is identical on every run.
const FIXTURE_DATE: &str = "2026-10-26T08:16:02Z";

/// Build the `why_one_path` fixture repository at `root` (M3-22): `FIXTURE_COMMITS` commits
/// dated `FIXTURE_DATE`, each touching `FIXTURE_FILES_PER_COMMIT` files `src/<vocab>/c<i>.rs`
/// under vocabulary directories the corpus Decisions govern (`src/<vocab>/**`). Returns the id
/// of commit 25, the `accepted_commit` stamped on every accepted Decision, so the currency of a
/// governing Decision reads the 25 commits after it.
///
/// # Errors
/// `git` absent or failing (the bench needs git for the N2 `why` operation).
pub(crate) fn fixture_repo(root: &std::path::Path, corpus: &Corpus) -> anyhow::Result<String> {
    use anyhow::Context as _;
    let _ = std::fs::remove_dir_all(root);
    std::fs::create_dir_all(root)?;
    let git = |args: &[&str]| -> anyhow::Result<String> {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .env("GIT_AUTHOR_DATE", FIXTURE_DATE)
            .env("GIT_COMMITTER_DATE", FIXTURE_DATE)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .with_context(|| format!("running git {args:?} for the bench fixture"))?;
        anyhow::ensure!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        Ok(String::from_utf8(out.stdout)?.trim().to_owned())
    };
    git(&["init", "-q", "-b", "main"])?;
    git(&["config", "user.email", "bench@example.invalid"])?;
    git(&["config", "user.name", "bench"])?;
    let mut commit_25 = String::new();
    for i in 0..FIXTURE_COMMITS {
        for k in 0..FIXTURE_FILES_PER_COMMIT {
            // spread the touched directories over the vocabulary with two coprime strides
            let dir = corpus.vocab_word(i * 97 + k * 1_301);
            let p = root.join("src").join(dir).join(format!("c{i}.rs"));
            std::fs::create_dir_all(p.parent().unwrap_or(root))?;
            std::fs::write(&p, format!("// fixture commit {i} file {k}\n"))?;
        }
        git(&["add", "-A"])?;
        git(&["commit", "-q", "-m", &format!("fixture {i}")])?;
        if i == 24 {
            commit_25 = git(&["rev-parse", "HEAD"])?;
        }
    }
    Ok(commit_25)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // test code: failures are the assertion
mod tests {
    use super::{Corpus, Kind, SEED, VOCAB_SIZE, word};
    use std::collections::HashSet;

    #[test]
    fn the_same_seed_yields_the_same_first_100_notes() {
        let mut a = Corpus::new(SEED);
        let mut b = Corpus::new(SEED);
        let first: Vec<_> = (0..100).map(|i| a.note(i)).collect();
        let again: Vec<_> = (0..100).map(|i| b.note(i)).collect();
        assert_eq!(first, again);
        // and the corpus has the shape §16 describes: kinds, identifiers, tags, a Decision scope
        assert!(
            first
                .iter()
                .any(|n| n.kind == Kind::Decision && !n.path_scope.is_empty())
        );
        assert!(
            first
                .iter()
                .any(|n| n.text.contains("Err") || n.text.contains("src/"))
        );
        assert!(first.iter().all(|n| n.tags.len() == 2));
        let mut other = Corpus::new(SEED + 1);
        assert_ne!(other.note(0), first[0]);
    }

    #[test]
    fn vocabulary_words_are_unique() {
        let words: HashSet<String> = (0..VOCAB_SIZE).map(word).collect();
        assert_eq!(words.len(), VOCAB_SIZE);
    }
}
