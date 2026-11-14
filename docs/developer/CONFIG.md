# Engine configuration

Kaban reads one engine configuration file: `<KABAN_HOME>/config.yaml`. An absent file
uses the register below. Unknown keys, wrong types and zero resource budgets are
configuration errors (CLI exit 4); errors name the field or fixed category, not its
value. Maps replace the whole default map: specify every entry you want to retain.

The default is thin: lexical full-text search is on (served by the store's Tantivy text index, the one
text index since spec 05 OWN-M; fuzzy matching is built into it), automatic gazetteer linking is off,
and extractors require explicit selection. `recall` chooses its match modes per request
(`modes: ["lexical", "fuzzy"]`); there is no configuration key for them. Configuration
reporting does not create optional indexes, tables or artifacts. `ingest.default_with`
is reserved: missing/empty is accepted, every nonempty value is refused with
`not_available_in_this_release`. A qualification report does not enable enrollment.

## Resource budgets

Every budget is overridable with a positive integer. `doctor` reports `default`,
`effective`, `source` (`default`, `config`, or a run-local `flag`) and `cap_lifted`.
An explicit value equal to its default is reported as `default`: provenance compares
effective values, not the presence of YAML syntax. Byte quotas retain their full
unsigned 64-bit value. The other RSS/automaton limits use their documented MB units;
`text.build_rss_mb` and `ingest.lru_mib` use MiB. `stats.producers` lists each budget under
the producer it bounds; `text.build_rss_mb` bounds the text index.

| Key | Default | Measurement that justifies an override |
| --- | ---: | --- |
| `text.build_rss_mb` | 64 | Text index build peak memory in MiB on representative input |
| `gazetteer.max_automaton_mb` | 25 | Dictionary automaton allocation and linking coverage |
| `ingest.lru_mib` | 8 | Reserved cache capacity; cache implementation deferred |
| `index.deadline_secs` | 600 | End-to-end indexer elapsed time |
| `index.quota.file_bytes` | 1073741824 | Largest admitted indexer output file |
| `index.quota.run_dir_bytes` | 4294967296 | Combined indexer run-directory bytes |
| `index.quota.tmp_bytes` | 536870912 | Peak temporary-directory bytes |

The 64 MiB text index build allowance (OWN-119) reflects the prior observed 56 MiB whole-process
VmHWM, including in-process corpus build. The overall 100 MiB promise at 100k notes
is unchanged; this default correction is not a fresh 100k qualification.

`--symbols` explicitly bypasses only the gazetteer automaton cap. Its budget row keeps
the configured numeric value, reports `source: flag`, `flag: --symbols`, and
`cap_lifted: true`; it does not invent an infinite cap or lift another resource budget.
Doctor itself has no symbols override and uses default run flags.

OWN-54 guidance permits larger process budgets (roughly 1.6–2 GB when useful); that is
operator guidance, not a validated ceiling or a promise that every configuration fits
the host. Measure before raising a cap. Disabled or failed optional producers remain visible
and do not remove usable sibling statistics.

## Remaining defaults register

These are configuration defaults, not claims that every reserved capability ships.
The sections of removed features, `reconcile`, `er`, `why`, `briefing`, `discovered` and `embed`,
are removed, and so are the renamed `l3` (now `extractors`) and `budget` (its `l0_rss_mb` is now
`text.build_rss_mb`): a file that still spells one is refused like any unknown key, naming the
section. There are no aliases.
Use quality/latency measurements from your own workload before changing a knob.

| Key | Default | Measurement or trigger |
| --- | --- | --- |
| `ontology.pattern_size_limit_kib` | 1024 | Pattern compiler allocation |
| `ontology.golden_strict` | false | Explicit strict ontology qualification |
| `gazetteer.min_pattern_len` | 3 | Short-pattern false positives |
| `gazetteer.common_word_max_len` | 4 | Common-word false positives |
| `gazetteer.corpus_freq_cutoff` | 0.05 | Pattern frequency and match quality |
| `gazetteer.max_targets` | 5 | Ambiguity fan-out |
| `gazetteer.max_chunk_ms` | 1 | Chunk matching elapsed time |
| `gazetteer.v4_symbols` | true | Explicit symbol inclusion and dictionary size |
| `extractors.holdout_min_f1` | 0.50 | Reserved qualification threshold; default enrollment deferred |
| `extractors.holdout_min_spans` | 200 | Reserved qualification sample floor |
| `recall.fusion` | rrf(60) | Reserved alternative-fusion choice; qualification deferred |
| `recall.rrf_k` | 60 | Judged ranked recall quality |
| `recall.judged_min_queries` | 100 | Judged query sample size |
| `recall.column_weights.text` | 1.0 | Judged text-column recall |
| `recall.column_weights.summary` | 1.5 | Judged summary-column recall |
| `recall.column_weights.tags` | 1.0 | Judged tag-column recall |
| `recall.column_weights.ident_terms` | 2.0 | Judged identifier recall |
| `ask.template_depth` | 1 | Reserved template traversal depth |
| `ask.template_depth_max` | 3 | Reserved template traversal cap |
| `ingest.finish_cap` | 10000 | Submitted-record count and finish latency |
| `ingest.retention_days` | 7 | Persistent job retention need |
| `ingest.metadata_allowance_kib` | 4 | Admitted metadata size |
| `ingest.lru_entries` | 4 | Reserved cache entry capacity; cache deferred |
| `ingest.default_with` | unset | Reserved enrollment; nonempty values unavailable |
| `pipeline.retire_batch_rows` | 1000 | Retirement latency and transaction duration |
| `index.isolation` | preferred | Indexer isolation availability |
| `index.share_caches` | unset | Explicit cache-sharing policy |
| `index.container_image` | unset | Explicit indexer container selection |
| `index.container_cli` | unset | Explicit container runtime selection |
| `link.default` | none | Explicit automatic linking selection |
| `bench.min_samples` | 30 | Benchmark sampling sufficiency |
| `bench.why_regression_pct` | 10 | Benchmark regression tolerance |

Indexer isolation, cache-sharing, deadline and quota semantics are specified in the
[v4 stage-one plan](../../../specs/kaban/02-code-graph-v4/stage1-plan.md) CG4.2. The complete executable
register is [the default fixture](../../crates/kaban-core/fixtures/config/defaults.yaml),
whose test verifies equality with `EngineConfig::default()`.
