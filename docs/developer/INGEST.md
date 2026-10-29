# Document and structured ingestion

`kaban ingest <paths...>` defaults to plain document ingestion. It places gated
Document, Section and Chunk nodes. Linking is opt-in with `--link gazetteer`.

`kaban ingest <paths...> --mode structured --with llm|gliner|none --json` emits one
extractor brief per document, in deterministic path order. `--dry-run` emits
prospective identities with `document.indexed=false`, without placing nodes,
opening jobs or pruning anything. Source code is refused in structured mode;
use the v4 indexer instead. `--with gazetteer` aliases `--with none --link gazetteer`.

Scripts and MCP clients share `ingest_brief` and `ingest_next`:

```sh
kaban ingest job brief docs/adr/0007-storage.md --with llm --json
kaban ingest job next JOB --chunk 0 --slice 0 --json
kaban ingest job next JOB --chunks 0..3 --json
kaban ingest job brief docs/adr/0007-storage.md --plan-page 1 --json
```

A real brief verifies that its document and chunk IDs resolve before claiming
`indexed=true`. Repeating the same bytes yields the same job and no new graph
changes. Source type and allowed kinds come from loaded ontology mappings and
advisory signals; an unknown explicit type warns and uses `default_kind`.
Signals do not fill record fields. Gazetteer matches are candidates only in
structured mode, never automatically promoted into mention or ABOUT edges.

The published extractor schema derives from the canonical `StructuredReturn`.
Its kind-discriminated `oneOf` pairs every enabled kind with its own props.
Internal props are removed. Converter objects are closed to declared fields:
a permissive kind with no declared props offers `{}`. This extraction-only
restriction does not change the core ontology validator. Optional nulls and
schema references are projected out; arbitrary unions are not supported.

`llm` provides instructions and JSON Schema for a host agent or grammar-constrained
engine (mistral.rs/llguidance or llama.cpp GBNF). `gliner` describes labeled spans:
it cannot compose rationale, consequences, fixes, path globs or Episode summaries,
or distinguish a Decision from an option. Closing that gap needs a second pass
outside kaban or acceptance of partial output; confidence is not calibrated.
`none` gives generic manual advice. Kaban loads no weights, calls no model and
executes no external extractor. Fixed-label NER (`rust-bert`, CoNLL models) is not
a dialect; there is no trained classifier in this release; the gazetteer links
known entities only. Nothing is accepted by extraction.

Every `ingest_next` call reopens the component-checked source, fingerprints its
bytes and applies current policy. There is no LRU cache. Content or ontology
drift makes only the scoped registry row stale (exit 4); it never republishes the
graph. Unknown, deleted and foreign jobs all refuse as `job_revoked` (exit 3).
A present expired job refuses with exit 4. Reject hits and invalid selectors
never echo their offending values.

Brief plans and complete next responses are bounded by serialized JSON bytes,
including escaping. Default next slices are at most 32 KiB under a 64 KiB response
ceiling with 4 KiB metadata allowance. A 200 KiB fence therefore takes seven
default slices. Concatenate slice text in order; source ranges remain logical
chunk ranges, not slice offsets. Range `a..b` is exclusive-ended and returns the
first unserved position in `next`. Follow `chunk_plan.pages` with `plan_page` for
large plans. Structured submit/finish publication and reconciliation are separate
downstream lifecycle work, not implemented by brief/next.
