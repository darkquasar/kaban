//! Strict judged-fixture loading and pure metrics, independent of stores and timing.
//! Labels are authored judgments, not inferred from retrieval output.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, bail};
use serde::Deserialize;

/// Stable class names used by the retained recall evaluation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum QueryClass {
    ExactId,
    ErrorString,
    NaturalLanguage,
    Identifier,
}

/// Inspected source material, identified independently of runtime node UUIDs.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Source {
    pub id: String,
    pub workspace: String,
    pub origin: String,
    pub text: String,
}

/// One manually assigned relevance judgment.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Judgment {
    pub target: String,
    pub grade: u8,
    pub evidence: String,
}

/// A query with a finite explicitly judged candidate pool.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct JudgedQuery {
    pub id: String,
    pub class: QueryClass,
    pub query: String,
    pub workspace: String,
    pub judgments: Vec<Judgment>,
    pub provenance: String,
}

/// A candidate MENTIONS pair, not a claim that a producer emitted the pair.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EdgeJudgment {
    pub id: String,
    pub from: String,
    pub to: String,
    pub workspace: String,
    pub rel: String,
    pub correct: bool,
    pub evidence: String,
    pub provenance: String,
}

/// Validated sources and labels. Serialized fixtures are not benchmark results.
#[derive(Debug)]
pub(crate) struct Fixtures {
    pub sources: Vec<Source>,
    pub queries: Vec<JudgedQuery>,
    pub edges: Vec<EdgeJudgment>,
}

fn lines<T: serde::de::DeserializeOwned>(input: &str) -> Result<Vec<T>> {
    input
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).map_err(Into::into))
        .collect()
}

fn nonempty(value: &str) -> bool {
    !value.trim().is_empty()
}

impl Fixtures {
    /// Load strict JSONL and validate counts, identities, bounds and provenance.
    /// `minimum_queries` can raise, but cannot lower, the required 100 queries.
    pub(crate) fn load(
        sources: &str,
        queries: &str,
        edges: &str,
        minimum_queries: usize,
    ) -> Result<Self> {
        let fixtures = Self {
            sources: lines(sources)?,
            queries: lines(queries)?,
            edges: lines(edges)?,
        };
        let mut catalog = BTreeMap::new();
        for source in &fixtures.sources {
            if !nonempty(&source.id)
                || !nonempty(&source.workspace)
                || !nonempty(&source.origin)
                || !nonempty(&source.text)
                || catalog.insert(source.id.as_str(), source).is_some()
            {
                bail!("invalid or duplicate source");
            }
        }
        if fixtures.queries.len() < minimum_queries.max(100) || fixtures.edges.len() < 100 {
            bail!("insufficient judged fixtures");
        }
        let mut query_ids = BTreeSet::new();
        let mut queries = BTreeSet::new();
        let mut classes = BTreeSet::new();
        for query in &fixtures.queries {
            if !nonempty(&query.id)
                || !nonempty(&query.query)
                || !nonempty(&query.provenance)
                || !query_ids.insert(&query.id)
                || !queries.insert((&query.workspace, &query.query))
                || query.judgments.is_empty()
            {
                bail!("invalid or duplicate query");
            }
            classes.insert(query.class);
            let mut targets = BTreeSet::new();
            for judgment in &query.judgments {
                let Some(source) = catalog.get(judgment.target.as_str()) else {
                    bail!("unresolvable query target");
                };
                if source.workspace != query.workspace
                    || judgment.grade > 2
                    || !nonempty(&judgment.evidence)
                    || !targets.insert(&judgment.target)
                {
                    bail!("invalid query judgment");
                }
            }
        }
        if classes.len() != 4 {
            bail!("missing query class");
        }
        let mut ids = BTreeSet::new();
        let mut pairs = BTreeSet::new();
        let mut verdicts = BTreeSet::new();
        for edge in &fixtures.edges {
            let (Some(from), Some(to)) = (
                catalog.get(edge.from.as_str()),
                catalog.get(edge.to.as_str()),
            ) else {
                bail!("unresolvable edge target");
            };
            if !nonempty(&edge.id)
                || edge.rel != "MENTIONS"
                || !nonempty(&edge.evidence)
                || !nonempty(&edge.provenance)
                || from.workspace != edge.workspace
                || to.workspace != edge.workspace
                || !ids.insert(&edge.id)
                || !pairs.insert((&edge.workspace, &edge.from, &edge.to))
            {
                bail!("invalid or duplicate edge judgment");
            }
            verdicts.insert(edge.correct);
        }
        if verdicts.len() != 2 {
            bail!("edge fixture requires correct and incorrect cases");
        }
        Ok(fixtures)
    }
}

/// nDCG evidence for one query; unknown returned hits are reported explicitly.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Ndcg {
    /// None when an evaluated hit is unjudged, rather than an invented grade zero.
    pub score: Option<f64>,
    pub zero_ideal: bool,
    pub judged_hits: usize,
    pub unjudged_hits: usize,
    pub duplicate_hits: usize,
}

fn gain(grade: u8, rank: usize) -> f64 {
    f64::from((1u32 << grade) - 1) / ((rank + 2) as f64).log2()
}

/// Evaluate ordered hits, first occurrence wins; duplicates do not consume a rank.
/// Ties belong to the caller's stable ordering (score desc, stable source id asc).
/// Unknown hits consume a rank and make the score unavailable. Only ten unique hits count.
pub(crate) fn ndcg_at_10(judgments: &[Judgment], hits: &[String]) -> Result<Ndcg> {
    let mut grades = BTreeMap::new();
    for judgment in judgments {
        if judgment.grade > 2
            || grades
                .insert(judgment.target.as_str(), judgment.grade)
                .is_some()
        {
            bail!("invalid relevance pool");
        }
    }
    let mut ideal: Vec<_> = grades.values().copied().collect();
    ideal.sort_unstable_by(|a, b| b.cmp(a));
    let idcg: f64 = ideal
        .iter()
        .take(10)
        .enumerate()
        .map(|(rank, grade)| gain(*grade, rank))
        .sum();
    let mut result = Ndcg {
        score: Some(0.0),
        zero_ideal: idcg == 0.0,
        judged_hits: 0,
        unjudged_hits: 0,
        duplicate_hits: 0,
    };
    let mut seen = BTreeSet::new();
    let mut dcg = 0.0;
    let mut rank = 0;
    for hit in hits {
        if !seen.insert(hit) {
            result.duplicate_hits += 1;
            continue;
        }
        if rank == 10 {
            break;
        }
        match grades.get(hit.as_str()) {
            Some(grade) => {
                result.judged_hits += 1;
                dcg += gain(*grade, rank);
            }
            None => result.unjudged_hits += 1,
        }
        rank += 1;
    }
    result.score = if result.unjudged_hits != 0 {
        None
    } else if idcg == 0.0 {
        Some(0.0)
    } else {
        Some(dcg / idcg)
    };
    Ok(result)
}

/// Workspace-qualified identity of an evaluated or predicted MENTIONS edge.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Pair {
    pub workspace: String,
    pub from: String,
    pub to: String,
}

/// Precision on the judged population, with coverage separate from correctness.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct EdgePrecision {
    pub precision: Option<f64>,
    pub coverage: Option<f64>,
    pub judged: usize,
    pub predicted: usize,
    pub judged_predicted: usize,
    pub correct: usize,
    pub unjudged: usize,
    pub duplicates: usize,
}

/// Deduplicate predictions; unjudged predictions never enter the precision denominator.
/// Empty predictions or no judged predictions yield None, never perfect precision.
pub(crate) fn edge_precision(
    judgments: &[EdgeJudgment],
    predictions: &[Pair],
) -> Result<EdgePrecision> {
    let mut population = BTreeMap::new();
    for edge in judgments {
        let pair = Pair {
            workspace: edge.workspace.clone(),
            from: edge.from.clone(),
            to: edge.to.clone(),
        };
        if edge.rel != "MENTIONS" || population.insert(pair, edge.correct).is_some() {
            bail!("invalid edge population");
        }
    }
    let unique: BTreeSet<_> = predictions.iter().collect();
    let mut result = EdgePrecision {
        precision: None,
        coverage: None,
        judged: population.len(),
        predicted: unique.len(),
        judged_predicted: 0,
        correct: 0,
        unjudged: 0,
        duplicates: predictions.len() - unique.len(),
    };
    for pair in unique {
        match population.get(pair) {
            Some(correct) => {
                result.judged_predicted += 1;
                result.correct += usize::from(*correct);
            }
            None => result.unjudged += 1,
        }
    }
    if result.judged_predicted != 0 {
        result.precision = Some(result.correct as f64 / result.judged_predicted as f64);
    }
    if result.predicted != 0 {
        result.coverage = Some(result.judged_predicted as f64 / result.predicted as f64);
    }
    Ok(result)
}
