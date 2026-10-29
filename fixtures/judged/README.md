# Retained judged fixture (E3.5 early slice)

These are agent-authored judgments by the E3.5 writer, 2026-10-26, not external
human review, generated truth, producer output, or measured retrieval quality.
The writer inspected the actual sequential corpus generator and walkthrough
excerpt before explicitly assigning the checked-in labels. JSONL serialization
enumerated explicit assignments; no rank, lexical matcher or seed assigns grades.

`queries.jsonl` contains 100 distinct query strings, 25 each of `exact_id`,
`error_string`, `natural_language`, `identifier`. Error-string case/quote variants
are intentional and correlated: these counts do not establish 100 independent
topics or support confidence intervals. Each query has a finite two-candidate
judged pool, not exhaustive judgments over a future 100k store.

Relevance rubric: 0 unrelated to requested identity/token/fact; 1 partial identifier
or incomplete answer; 2 directly supplies requested identity/token/fact. Inspect
the candidate text and the row's evidence, never infer relevance from its rank.
Natural-language grades judge the answer stated in the material, not hypothetical
embedding similarity. Unknown returned hits are explicitly unjudged and make that
query's nDCG unavailable, rather than quietly grading them zero. A zero ideal gain
scores zero and increments a separately available zero-ideal indicator.

`edges.jsonl` contains 100 distinct workspace-qualified Gazetteer MENTIONS
candidate pairs: ten inspected source passages against ten candidate entities.
Every verdict in that matrix was explicitly reviewed. Correct means the passage
actually names that target identity. Incorrect includes absence, a different path,
self-reference without a citation, or ambiguity. Bare `Cart` cannot resolve to a
unique entity in the walkthrough and is incorrect for a plain emitted edge.
`Decision 0007` in the rename narrative resolves to the discounts ADR. A path glob
is not an exact file mention; a path basename `tukaka.rs` is not the bare vocabulary
entity `tukaka`. These explicit ambiguous-case choices may be revisited by review;
tests establish structure/counts, not the truth of the author's judgment.
No SIMILAR_TO allocation, embeddings grading or unavailable producer evidence.

`walkthrough.jsonl` is a source catalog: only the judged excerpts from
`../specs/kaban/specs/03-engine-v5/walkthrough-example.md`, plus inspected synthetic notes and
candidate entity labels. Walkthrough text is illustrative, not a pipeline result.
Synthetic references use seed `0x9E3779B97F4A7C15`, zero-based sequential ordinals
0, 1, 2, 4, workspace `ws1`. The generator's first five notes have no Decision
CodeRef draw, so they match the runner's initial sequence. Catalog text omits tags;
future corpus mapping must verify text/workspace and ordinal before using labels.

Stable catalog ids (`corpus:1`, `w:architecture`, `t:discount`, etc.) are not runtime
UUIDs. Future admitted harness work must retain stable-id -> actual node-id mapping.
`exact_id` query `{node_id:<stable-id>}` is an explicit template: substitute the
mapped actual id before calling recall; never submit it literally or claim it is
currently measured. Candidate records will need deterministic materialization
beside the unchanged corpus generator. That CLI/harness slice is not shipped here.

Metrics: nDCG@10 uses gain `2^grade-1` and log2(rank+2) discount. Caller ordering
breaks score ties by stable id; first duplicate wins and consumes no extra rank.
Precision is correct / predicted-within-judged-population; coverage is judged
predictions / all unique predictions. Report judged/predicted/correct/unjudged and
duplicate counts. Empty predictions yield unavailable precision/coverage, not 1.
Actual Gazetteer precision belongs to E3.12 after producer admission.
