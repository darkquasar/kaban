//! Typed queries (spec 05 SS-08): no query parser, the operand grammar is `fts_expr`'s (an
//! explicit phrase, or one token with a trailing prefix marker), every operand matches any of
//! the four content fields under the column weights as boosts, the filter fields join as
//! zero-score `Must` clauses so they bound the match set before the top-k and never match text,
//! and the page is cut from the total order `(score desc, rid asc)` computed over the whole
//! match set (no pruning: a pruned collector breaks ties by document address, which changes
//! across merges; SS-08 wants the same order whatever the segment layout).
//!
//! Adapter-specific ranking differences (documented, not hidden): Tantivy's BM25 scores each
//! field separately and sums the boosted scores where FTS5's `bm25()` weights term frequencies
//! inside one formula; Snowball English stems where FTS5 runs Porter; a prefix operand matches
//! at constant score; a fuzzy hit scores by the exact term's BM25 plus one constant per matched
//! fuzzy term where the trigram table ranks by trigram BM25; the `ident_terms` field is fed its
//! five inputs separately, so a term shared by two inputs counts twice where the column counts
//! it once. A prefix is never a ranking difference in membership: it expands to every term of
//! the dictionary under it, like FTS5's `token*` ([`expansions`]).

use crate::schema::{ACTIVE, CONTENT, Fields, KIND, REVISION, RID};
use crate::{Error, Result};
use kaban_core::config::Recall;
use kaban_core::ident::{QueryClass, classify};
use kaban_core::text_index::{MatchMode, Searched, SeedFilter, TextHit, TextQuery};
use kaban_core::{Ontology, stored_name};
use std::cmp::Reverse;
use std::collections::BTreeSet;
use tantivy::collector::TopDocs;
use tantivy::query::{
    Bm25StatisticsProvider, BooleanQuery, BoostQuery, ConstScoreQuery, EmptyQuery, FuzzyTermQuery,
    Occur, PhrasePrefixQuery, PhraseQuery, Query, TermQuery,
};
use tantivy::schema::{Field, IndexRecordOption};
use tantivy::{DocAddress, DocId, Index, Score, Searcher, SegmentReader, Term};

/// EN-F29 defaults for [`CONTENT`], as `recall.rs` has them.
const DEFAULT_COLUMN_WEIGHTS: [f64; 4] = [1.0, 1.5, 1.0, 2.0];
/// Code text boosts: the whole lowercased name over its parts over the stemmed prose fields.
const NAME_BOOST: f32 = 4.0;
const CODE_BOOST: f32 = 2.0;
/// A query longer than this is cut (Turso's guard; the grammar is linear, the cut is a cap).
const MAX_QUERY_BYTES: usize = 16 * 1024;
/// At most this many operands take part.
const MAX_OPERANDS: usize = 64;
/// At most this many fuzzy terms per query (the trigram expression caps its grams at 64).
const MAX_FUZZY_TERMS: usize = 32;
/// Tokens shorter than this are not fuzzy-matched (a trigram needs three characters).
const MIN_FUZZY_CHARS: usize = 3;

/// Effective `[text, summary, tags, ident_terms]` weights.
type Columns = [f32; 4];

/// The column weights: the configured defaults and the kinds with their own `column_weights`
/// (ontology order), applied as boosts per kind branch.
#[derive(Debug, Clone)]
pub(crate) struct Weights {
    defaults: Columns,
    profiles: Vec<(String, Columns)>,
}

impl Weights {
    #[allow(clippy::cast_possible_truncation)] // column weights are small configured reals
    pub(crate) fn of(ontology: &Ontology, recall: &Recall) -> Self {
        let defaults: [f64; 4] = std::array::from_fn(|i| {
            recall
                .column_weights
                .get(CONTENT[i])
                .copied()
                .unwrap_or(DEFAULT_COLUMN_WEIGHTS[i])
        });
        let profiles = ontology
            .kinds
            .iter()
            .filter_map(|(name, kind)| {
                let w = kind.column_weights.as_ref()?;
                let own = [w.text, w.summary, w.tags, w.ident_terms];
                Some((
                    stored_name(name),
                    std::array::from_fn(|i| own[i].unwrap_or(defaults[i]) as f32),
                ))
            })
            .collect();
        Self {
            defaults: defaults.map(|w| w as f32),
            profiles,
        }
    }
}

/// One lexical operand: an explicit phrase or one token, `prefix` for a trailing `*`.
#[derive(Debug, PartialEq, Eq)]
struct Operand {
    words: String,
    prefix: bool,
}

/// `fts_expr`'s parse: quoted phrases keep every word; a bare word splits on separators
/// (`_-.` are kept inside a token), drops one-character tokens and carries its prefix marker
/// on its last token.
fn operands(query: &str) -> Vec<Operand> {
    let mut end = query.len().min(MAX_QUERY_BYTES);
    while !query.is_char_boundary(end) {
        end -= 1;
    }
    let split = |s: &str| -> Vec<String> {
        s.split(|c: char| !c.is_alphanumeric() && !"_-.".contains(c))
            .filter(|w| !w.is_empty())
            .map(str::to_owned)
            .collect()
    };
    let mut out = Vec::new();
    let mut rest = query[..end].trim();
    while !rest.is_empty() && out.len() < MAX_OPERANDS {
        if let Some(quoted) = rest.strip_prefix('"') {
            let (phrase, tail) = quoted.split_once('"').unwrap_or((quoted, ""));
            let words = split(phrase);
            if !words.is_empty() {
                out.push(Operand {
                    words: words.join(" "),
                    prefix: false,
                });
            }
            rest = tail.trim_start();
        } else {
            let end = rest
                .find(|c: char| c.is_whitespace() || c == '"')
                .unwrap_or(rest.len());
            let word = &rest[..end];
            let prefix = word.ends_with('*');
            let tokens: Vec<String> = split(word.trim_end_matches('*'))
                .into_iter()
                .filter(|t| t.len() >= 2)
                .collect();
            let last = tokens.len().saturating_sub(1);
            for (i, words) in tokens.into_iter().enumerate() {
                out.push(Operand {
                    words,
                    prefix: prefix && i == last,
                });
            }
            rest = rest[end..].trim_start();
        }
    }
    out.truncate(MAX_OPERANDS);
    out
}

/// The token texts of `text` under `field`'s analyzer.
fn tokens(index: &Index, field: Field, text: &str) -> Result<Vec<String>> {
    let mut analyzer = index.tokenizer_for_field(field)?;
    let mut stream = analyzer.token_stream(text);
    let mut out = Vec::new();
    while let Some(token) = stream.next() {
        out.push(token.text.clone());
    }
    Ok(out)
}

fn raw_term(field: Field, value: &str) -> Box<dyn Query> {
    Box::new(TermQuery::new(
        Term::from_field_text(field, value),
        IndexRecordOption::Basic,
    ))
}

fn raw_term_with(field: Field, value: Option<&String>) -> Box<dyn Query> {
    value.map_or_else(
        || Box::new(EmptyQuery) as Box<dyn Query>,
        |v| raw_term(field, v),
    )
}

fn any_of(field: Field, values: &[String]) -> Box<dyn Query> {
    Box::new(BooleanQuery::new(
        values
            .iter()
            .map(|v| (Occur::Should, raw_term(field, v)))
            .collect(),
    ))
}

/// The index-side filters at constant score zero: lifecycle `active`, the workspace, any of
/// the kinds, any of the tags. The candidate exclusion and the `CodeRef` link stay with the
/// caller's revalidation (SS-11).
fn filters(fields: &Fields, filter: &SeedFilter<'_>) -> Vec<(Occur, Box<dyn Query>)> {
    let mut out: Vec<Box<dyn Query>> = vec![raw_term(fields.lifecycle, ACTIVE)];
    if let Some(ws) = filter.workspace_id {
        out.push(raw_term(fields.workspace, ws));
    }
    if !filter.kinds.is_empty() {
        out.push(any_of(fields.kind, filter.kinds));
    }
    if !filter.tags.is_empty() {
        out.push(any_of(fields.tag, filter.tags));
    }
    out.into_iter()
        .map(|q| {
            (
                Occur::Must,
                Box::new(ConstScoreQuery::new(q, 0.0)) as Box<dyn Query>,
            )
        })
        .collect()
}

/// The first byte string above every one that starts with `prefix` (`None`: nothing is above
/// a prefix of `0xFF` bytes), the end of the dictionary range a prefix covers.
fn prefix_end(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut end = prefix.to_owned();
    while let Some(last) = end.pop() {
        if last < u8::MAX {
            end.push(last + 1);
            return Some(end);
        }
    }
    None
}

/// The number of terms of `field`'s dictionary under `prefix` in the segment holding the most
/// of them, at least one. `PhrasePrefixQuery` cuts its expansion per segment at
/// `max_expansions` (50 by default, which silently drops lexemes and lets a filtered-out
/// workspace's terms consume the budget); set from the dictionary, the cut is never reached,
/// so a prefix matches every term under it whatever the segment layout.
fn expansions(searcher: &Searcher, field: Field, prefix: &[u8]) -> Result<u32> {
    let end = prefix_end(prefix);
    let mut most: u32 = 1;
    for segment in searcher.segment_readers() {
        let inverted = segment.inverted_index(field)?;
        let mut range = inverted.terms().range().ge(prefix);
        if let Some(end) = &end {
            range = range.lt(end);
        }
        let mut stream = range.into_stream()?;
        let mut count: u32 = 0;
        while stream.advance() {
            count = count.saturating_add(1);
        }
        most = most.max(count);
    }
    Ok(most)
}

/// `operand` on `field`: a term, a phrase, or a prefix (one token or a phrase) expanded over
/// the whole dictionary ([`expansions`]).
fn operand_on(
    searcher: &Searcher,
    field: Field,
    tokens: &[String],
    prefix: bool,
) -> Result<Box<dyn Query>> {
    let terms: Vec<Term> = tokens
        .iter()
        .map(|t| Term::from_field_text(field, t))
        .collect();
    let Some(last) = terms.last().cloned() else {
        return Ok(Box::new(EmptyQuery));
    };
    Ok(match (terms.len(), prefix) {
        (1, false) => Box::new(TermQuery::new(last, IndexRecordOption::WithFreqs)),
        (_, false) => Box::new(PhraseQuery::new(terms)),
        (_, true) => {
            let under = expansions(searcher, field, last.serialized_value_bytes())?;
            let mut query = PhrasePrefixQuery::new(terms);
            query.set_max_expansions(under);
            Box::new(query)
        }
    })
}

/// The content match under one weight profile: every operand (`Must`) or any (`Should`), each
/// operand matching any of the four content fields, analyzed once with the query analyzer
/// (the `text` field's: the FTS5 twin on both sides).
fn content(
    searcher: &Searcher,
    fields: &Fields,
    ops: &[Operand],
    occur: Occur,
    weights: Columns,
) -> Result<Option<Box<dyn Query>>> {
    let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
    for op in ops {
        let stemmed = tokens(searcher.index(), fields.content[0], &op.words)?;
        if stemmed.is_empty() {
            continue;
        }
        let mut per_field: Vec<(Occur, Box<dyn Query>)> = Vec::with_capacity(CONTENT.len() + 2);
        // code text: the whole name (exact, strongest) and its parts as a phrase or prefix
        let name = tokens(searcher.index(), fields.name, &op.words)?;
        let code = tokens(searcher.index(), fields.code, &op.words)?;
        for (field, tokens, boost) in [
            (fields.name, name, NAME_BOOST),
            (fields.code, code, CODE_BOOST),
        ] {
            if !tokens.is_empty() {
                per_field.push((
                    Occur::Should,
                    Box::new(BoostQuery::new(
                        operand_on(searcher, field, &tokens, op.prefix)?,
                        boost,
                    )),
                ));
            }
        }
        for (field, weight) in fields.content.iter().zip(weights) {
            per_field.push((
                Occur::Should,
                Box::new(BoostQuery::new(
                    operand_on(searcher, *field, &stemmed, op.prefix)?,
                    weight,
                )),
            ));
        }
        clauses.push((occur, Box::new(BooleanQuery::new(per_field))));
    }
    Ok((!clauses.is_empty()).then(|| Box::new(BooleanQuery::new(clauses)) as Box<dyn Query>))
}

/// The lexical query: the per-kind weight profiles as disjoint branches (a kind term at
/// constant score selects the branch), the default profile for every other kind, then the
/// filters. An identifier query doubles the `ident_terms` weight, as `recall.rs` does.
fn lexical(
    searcher: &Searcher,
    fields: &Fields,
    weights: &Weights,
    ops: &[Operand],
    occur: Occur,
    identifier: bool,
    filter: &SeedFilter<'_>,
) -> Result<Option<Box<dyn Query>>> {
    let boost = |mut w: Columns| {
        if identifier {
            w[3] *= 2.0;
        }
        w
    };
    let Some(default) = content(searcher, fields, ops, occur, boost(weights.defaults))? else {
        return Ok(None);
    };
    let text: Box<dyn Query> = if weights.profiles.is_empty() {
        default
    } else {
        let mut branches: Vec<(Occur, Box<dyn Query>)> = Vec::new();
        let mut others: Vec<(Occur, Box<dyn Query>)> = vec![(Occur::Must, default)];
        for (kind, profile) in &weights.profiles {
            if let Some(q) = content(searcher, fields, ops, occur, boost(*profile))? {
                branches.push((
                    Occur::Should,
                    Box::new(BooleanQuery::new(vec![
                        (
                            Occur::Must,
                            Box::new(ConstScoreQuery::new(raw_term(fields.kind, kind), 0.0)),
                        ),
                        (Occur::Must, q),
                    ])),
                ));
            }
            others.push((Occur::MustNot, raw_term(fields.kind, kind)));
        }
        branches.push((Occur::Should, Box::new(BooleanQuery::new(others))));
        Box::new(BooleanQuery::new(branches))
    };
    let mut all = vec![(Occur::Must, text)];
    all.extend(filters(fields, filter));
    Ok(Some(Box::new(BooleanQuery::new(all))))
}

/// The fuzzy query: per distinct query token (folded, three characters or more, at most
/// [`MAX_FUZZY_TERMS`]) the exact term's BM25 plus a distance-1 fuzzy term (transpositions
/// cost one; a `*`-marked token matches as a fuzzy prefix); any token may match; then the
/// filters.
fn fuzzy(
    searcher: &Searcher,
    fields: &Fields,
    text: &str,
    filter: &SeedFilter<'_>,
) -> Result<Option<Box<dyn Query>>> {
    let mut seen = BTreeSet::new();
    for word in text.split_whitespace() {
        let prefix = word.ends_with('*');
        for token in tokens(searcher.index(), fields.fuzzy, word.trim_end_matches('*'))? {
            if token.chars().count() >= MIN_FUZZY_CHARS && seen.len() < MAX_FUZZY_TERMS {
                seen.insert((token, prefix));
            }
        }
    }
    if seen.is_empty() {
        return Ok(None);
    }
    let clauses: Vec<(Occur, Box<dyn Query>)> = seen
        .into_iter()
        .map(|(token, prefix)| {
            let term = Term::from_field_text(fields.fuzzy, &token);
            let fuzzy = if prefix {
                FuzzyTermQuery::new_prefix(term.clone(), 1, true)
            } else {
                FuzzyTermQuery::new(term.clone(), 1, true)
            };
            let exact = TermQuery::new(term, IndexRecordOption::WithFreqs);
            (
                Occur::Should,
                Box::new(BooleanQuery::new(vec![
                    (Occur::Should, Box::new(exact) as Box<dyn Query>),
                    (Occur::Should, Box::new(fuzzy)),
                ])) as Box<dyn Query>,
            )
        })
        .collect();
    let mut all: Vec<(Occur, Box<dyn Query>)> =
        vec![(Occur::Must, Box::new(BooleanQuery::new(clauses)))];
    all.extend(filters(fields, filter));
    Ok(Some(Box::new(BooleanQuery::new(all))))
}

/// BM25 statistics with one document frequency per term across the four content fields (the
/// largest of the four), the way FTS5's `bm25()` counts a term's rows whichever column holds
/// it. Per-field counting gave the short derived `ident_terms` field a large idf for an
/// ordinary word that takes an identifier shape in a few rows (`cell` from `T-cell`), and the
/// configured 2.0 boost multiplied it, so those rows out-ranked the prose matches on prose
/// queries (`SciFact`, `s31-ss13a-accuracy/results-production.md`). Field lengths stay per field.
struct SharedIdf<'a> {
    searcher: &'a Searcher,
    content: [Field; 4],
}

impl Bm25StatisticsProvider for SharedIdf<'_> {
    fn total_num_tokens(&self, field: Field) -> tantivy::Result<u64> {
        self.searcher.total_num_tokens(field)
    }

    fn total_num_docs(&self) -> tantivy::Result<u64> {
        self.searcher.total_num_docs()
    }

    fn doc_freq(&self, term: &Term) -> tantivy::Result<u64> {
        let value = term.value();
        let (true, Some(text)) = (self.content.contains(&term.field()), value.as_str()) else {
            return self.searcher.doc_freq(term);
        };
        let mut most = 0;
        for field in self.content {
            most = most.max(
                self.searcher
                    .doc_freq(&Term::from_field_text(field, text))?,
            );
        }
        Ok(most)
    }
}

/// Hits `skip..skip + want` of the total order `(score desc, rid asc)`: the sort key is the
/// score and the `rid` fast value, computed for every match, so the order is the same whatever
/// the segments and never falls back to a document address. The key is never made up: every
/// segment's `rid` column is opened fallibly before the collector runs (a segment without it
/// is [`Error::Corrupt`]), and a document without a value carries `None`, which [`hit`]
/// refuses; so a malformed segment is reported, never ranked under a synthetic key.
fn top(
    searcher: &Searcher,
    fields: &Fields,
    query: &dyn Query,
    skip: usize,
    want: usize,
) -> Result<Vec<TextHit>> {
    for segment in searcher.segment_readers() {
        segment
            .fast_fields()
            .i64(RID)
            .map_err(|_| Error::Corrupt("rid column"))?;
    }
    let order =
        TopDocs::with_limit(want)
            .and_offset(skip)
            .tweak_score(|segment: &SegmentReader| {
                // the same readers as above: opened there, this cannot fail; `None` stays `None`
                let rid = segment.fast_fields().i64(RID).ok();
                move |doc: DocId, score: Score| {
                    (score, Reverse(rid.as_ref().and_then(|c| c.first(doc))))
                }
            });
    let statistics = SharedIdf {
        searcher,
        content: fields.content,
    };
    searcher
        .search_with_statistics_provider(query, &order, &statistics)?
        .into_iter()
        .map(|((score, Reverse(rid)), address)| hit(searcher, address, rid, score))
        .collect()
}

/// A hit from the fast columns of its segment: no document store is read, and a document
/// without one of its keys is [`Error::Corrupt`].
fn hit(
    searcher: &Searcher,
    address: DocAddress,
    rid: Option<i64>,
    score: Score,
) -> Result<TextHit> {
    let rid = rid.ok_or(Error::Corrupt("rid"))?;
    let fast = searcher.segment_reader(address.segment_ord).fast_fields();
    let revision = fast
        .i64(REVISION)?
        .first(address.doc_id)
        .ok_or(Error::Corrupt("revision"))?;
    let kinds = fast.str(KIND)?.ok_or(Error::Corrupt("kind"))?;
    let ord = kinds
        .term_ords(address.doc_id)
        .next()
        .ok_or(Error::Corrupt("kind"))?;
    let mut kind = String::new();
    if !kinds.ord_to_str(ord, &mut kind)? {
        return Err(Error::Corrupt("kind"));
    }
    Ok(TextHit {
        rid,
        revision,
        score: f64::from(score),
        kind,
    })
}

/// Run `q` against `searcher`. Lexical: the AND expression first, widened to OR when it holds
/// fewer than `sufficient` hits in all; the decision is made on the unpaged result (the strict
/// collection reaches at least `sufficient` hits from the start when they exist), so every page
/// of one query comes from one expression and a continuation never repeats a hit.
pub(crate) fn run(
    fields: &Fields,
    weights: &Weights,
    searcher: &Searcher,
    q: &TextQuery<'_>,
) -> Result<Searched> {
    if q.limit == 0 {
        return Ok(Searched::Hits {
            hits: vec![],
            exhausted: false,
        });
    }
    let empty = Searched::Hits {
        hits: vec![],
        exhausted: true,
    };
    let found = match q.mode {
        MatchMode::Lexical => {
            let ops = operands(q.text);
            let identifier = classify(q.text) == QueryClass::Identifier;
            let Some(strict) = lexical(
                searcher,
                fields,
                weights,
                &ops,
                Occur::Must,
                identifier,
                &q.filter,
            )?
            else {
                return Ok(empty);
            };
            // the strict result is read from the start through this page and at least
            // `sufficient` rows, so the widening decision is the unpaged one for every page
            let through = q.skip.saturating_add(q.limit).saturating_add(1);
            let strict_hits = top(
                searcher,
                fields,
                strict.as_ref(),
                0,
                through.max(q.sufficient),
            )?;
            if strict_hits.len() >= q.sufficient {
                strict_hits.into_iter().skip(q.skip).collect()
            } else {
                match lexical(
                    searcher,
                    fields,
                    weights,
                    &ops,
                    Occur::Should,
                    identifier,
                    &q.filter,
                )? {
                    Some(widened) => top(searcher, fields, widened.as_ref(), q.skip, q.limit + 1)?,
                    None => strict_hits.into_iter().skip(q.skip).collect(),
                }
            }
        }
        MatchMode::Exact => {
            let name = tokens(searcher.index(), fields.name, q.text.trim())?;
            let mut all = filters(fields, &q.filter);
            all.push((Occur::Must, raw_term_with(fields.name, name.first())));
            top(
                searcher,
                fields,
                &BooleanQuery::new(all),
                q.skip,
                q.limit + 1,
            )?
        }
        MatchMode::Fuzzy => {
            let Some(query) = fuzzy(searcher, fields, q.text, &q.filter)? else {
                return Ok(empty);
            };
            top(searcher, fields, query.as_ref(), q.skip, q.limit + 1)?
        }
    };
    let exhausted = found.len() <= q.limit;
    Ok(Searched::Hits {
        hits: found.into_iter().take(q.limit).collect(),
        exhausted,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion
mod tests {
    use super::{Operand, Weights, operands, run};
    use crate::analyzers::register;
    use crate::schema::{ACTIVE, Fields, schema};
    use kaban_core::text_index::{MatchMode, Searched, SeedFilter, TextQuery};
    use kaban_core::{RecallQuery, config::Recall};
    use tantivy::{Index, TantivyDocument};

    /// An empty in-memory index with the adapter's schema, analyzers and default weights.
    fn raw_index() -> (Index, Fields, Weights) {
        let (schema, fields) = schema();
        let index = Index::create_in_ram(schema);
        register(&index);
        let ontology = kaban_core::shipped::resolved().unwrap();
        let weights = Weights::of(&ontology, &Recall::default());
        (index, fields, weights)
    }

    /// An active `Fact` of revision 1 in `ws` with `text`.
    fn doc(fields: &Fields, rid: i64, ws: &str, text: &str) -> TantivyDocument {
        let mut d = TantivyDocument::default();
        d.add_text(fields.id, format!("id-{rid}"));
        d.add_i64(fields.rid, rid);
        d.add_i64(fields.revision, 1);
        d.add_text(fields.kind, "Fact");
        d.add_text(fields.workspace, ws);
        d.add_text(fields.lifecycle, ACTIVE);
        d.add_text(fields.content[0], text);
        d
    }

    /// The `(rid, score)` page `skip..skip + limit` of the lexical `q` on a fresh searcher,
    /// with its `exhausted` flag.
    fn page(
        index: &Index,
        fields: &Fields,
        weights: &Weights,
        q: &RecallQuery,
        sufficient: usize,
        skip: usize,
        limit: usize,
    ) -> (Vec<(i64, f64)>, bool) {
        let searcher = index.reader().unwrap().searcher();
        match run(
            fields,
            weights,
            &searcher,
            &TextQuery {
                text: &q.query,
                mode: MatchMode::Lexical,
                filter: SeedFilter::of(q),
                limit,
                sufficient,
                skip,
            },
        )
        .unwrap()
        {
            Searched::Hits { hits, exhausted } => (
                hits.into_iter()
                    .map(|h| (h.rid, h.score))
                    .collect::<Vec<_>>(),
                exhausted,
            ),
            Searched::Unavailable => panic!("built"),
        }
    }

    /// The operand parse is `fts_expr`'s: phrases kept whole, one-character tokens dropped,
    /// the prefix marker on the last token of its word only.
    #[test]
    fn operands_mirror_fts_expr() {
        let op = |words: &str, prefix| Operand {
            words: words.into(),
            prefix,
        };
        assert_eq!(
            operands(r#"a "two words" foo/bar_baz* x"#),
            vec![
                op("two words", false),
                op("foo", false),
                op("bar_baz", true)
            ]
        );
        assert_eq!(operands("  "), vec![]);
        let long = "quokka ".repeat(5_000);
        assert_eq!(operands(&long).len(), super::MAX_OPERANDS);
    }

    /// Determinism across segments and a merge (SS-08): identical texts score the same, so
    /// the order among them is the `rid` order, whatever the segment a document lives in and
    /// whatever its document address; after merging the segments into one the page is the same.
    /// Two commits put the lower rid in the later segment, where an address tie-break would
    /// list it last.
    #[test]
    fn equal_scores_order_by_rid_across_segments_and_after_a_merge() {
        let (index, fields, weights) = raw_index();
        let mut writer = index.writer_with_num_threads(1, 32 << 20).unwrap();
        for rid in [7, 9] {
            writer
                .add_document(doc(&fields, rid, "default", "quokka wombat crossing"))
                .unwrap();
        }
        writer.commit().unwrap();
        for rid in [2, 5] {
            writer
                .add_document(doc(&fields, rid, "default", "quokka wombat crossing"))
                .unwrap();
        }
        writer.commit().unwrap();
        let reader = index.reader().unwrap();
        assert_eq!(reader.searcher().segment_readers().len(), 2);
        let q = RecallQuery {
            query: "quokka".into(),
            ..Default::default()
        };
        let page = |skip: usize, limit: usize| page(&index, &fields, &weights, &q, 1, skip, limit);
        let before = page(0, 10);
        assert_eq!(
            before.0.iter().map(|(rid, _)| *rid).collect::<Vec<_>>(),
            [2, 5, 7, 9]
        );
        assert!(before.1);
        assert_eq!(
            page(1, 2),
            (before.0[1..3].to_vec(), false),
            "a continuation"
        );
        let segments = index.searchable_segment_ids().unwrap();
        writer.merge(&segments).wait().unwrap();
        assert_eq!(index.searchable_segment_ids().unwrap().len(), 1);
        assert_eq!(
            page(0, 10),
            before,
            "the same order and scores after the merge"
        );
    }

    /// A prefix expands to every lexeme under it (SS-08 prefix functionality; T1 review
    /// t1-1): sixty lexemes share `quok`, fifty of them in another workspace and first in
    /// dictionary order, five of the wanted workspace's ten in the same segment and five in a
    /// second one. Tantivy's default expansion stops at fifty terms per segment, so the wanted
    /// rows past the cut vanished (the filtered-out workspace consumed the budget) and a merge
    /// changed the page. The page is the wanted ten before and after the merge, for the
    /// one-token prefix and the phrase prefix alike, and the other workspace's fifty are all
    /// found.
    #[test]
    fn a_prefix_matches_every_lexeme_under_it_whatever_the_segment_layout() {
        let (index, fields, weights) = raw_index();
        let mut writer = index.writer_with_num_threads(1, 32 << 20).unwrap();
        let text = |n: i64| format!("shore quok{n:02} crossing");
        for n in 0..50 {
            writer
                .add_document(doc(&fields, n, "other", &text(n)))
                .unwrap();
        }
        for n in 50..55 {
            writer
                .add_document(doc(&fields, n, "wanted", &text(n)))
                .unwrap();
        }
        writer.commit().unwrap();
        for n in 55..60 {
            writer
                .add_document(doc(&fields, n, "wanted", &text(n)))
                .unwrap();
        }
        writer.commit().unwrap();
        assert_eq!(index.searchable_segment_ids().unwrap().len(), 2);
        let wanted: Vec<i64> = (50..60).collect();
        let in_ws = |text: &str, ws: &str| RecallQuery {
            query: text.into(),
            workspace_id: Some(ws.into()),
            ..Default::default()
        };
        let rids = |q: &RecallQuery, limit: usize| -> Vec<i64> {
            page(&index, &fields, &weights, q, 10, 0, limit)
                .0
                .into_iter()
                .map(|(rid, _)| rid)
                .collect()
        };
        let both = || {
            (
                rids(&in_ws("quok*", "wanted"), 10),
                rids(&in_ws("shore_quok*", "wanted"), 10),
            )
        };
        let before = both();
        assert_eq!(before.0, wanted, "one-token prefix over two segments");
        assert_eq!(before.1, wanted, "phrase prefix over two segments");
        assert_eq!(rids(&in_ws("quok*", "other"), 100).len(), 50);
        let segments = index.searchable_segment_ids().unwrap();
        writer.merge(&segments).wait().unwrap();
        assert_eq!(index.searchable_segment_ids().unwrap().len(), 1);
        assert_eq!(both(), before, "the same page after the merge");
        assert_eq!(rids(&in_ws("quok*", "other"), 100).len(), 50);
    }
}
