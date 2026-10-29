# Engine configuration

Kaban reads one engine configuration file: `<KABAN_HOME>/config.yaml`. An absent file
uses the register below. Unknown keys, wrong types and zero resource budgets are
configuration errors (CLI exit 4); errors name the field or fixed category, not its
value. Maps replace the whole default map: specify every entry you want to retain.

The default is thin: lexical full-text search is on (served by the store's Tantivy text index, the one
text index since spec 05 OWN-M; fuzzy matching is built into it), automatic gazetteer linking is off,
embedding is unavailable in this release (OWN-94), and extractors require explicit selection. Configuration
reporting does not create optional indexes, tables or artifacts. `ingest.default_with`
is reserved: missing/empty is accepted, every nonempty value is refused with
`not_available_in_this_release`. A qualification report does not enable enrollment.
`embed.model` is also reserved: missing, null or an empty map is accepted; any
nonempty map is refused with the same fixed category. A manually constructed model
does not enable L2.

## Resource budgets

Every budget is overridable with a positive integer. `doctor` reports `default`,
`effective`, `source` (`default`, `config`, or a run-local `flag`) and `cap_lifted`.
An explicit value equal to its default is reported as `default`: provenance compares
effective values, not the presence of YAML syntax. Byte quotas retain their full
unsigned 64-bit value. The other RSS/automaton limits use their documented MB units;
`budget.l0_rss_mb` and `ingest.lru_mib` use MiB.

| Key | Default | Measurement that justifies an override |
| --- | ---: | --- |
| `budget.l0_rss_mb` | 64 | L0 peak memory in MiB on representative input |
| `gazetteer.max_automaton_mb` | 25 | Dictionary automaton allocation and linking coverage |
| `embed.max_scan_rss_mb` | 32 | Embedding scan working-set peak |
| `ingest.lru_mib` | 8 | Reserved cache capacity; cache implementation deferred |
| `index.deadline_secs` | 600 | End-to-end indexer elapsed time |
| `index.quota.file_bytes` | 1073741824 | Largest admitted indexer output file |
| `index.quota.run_dir_bytes` | 4294967296 | Combined indexer run-directory bytes |
| `index.quota.tmp_bytes` | 536870912 | Peak temporary-directory bytes |

The 64 MiB L0 allowance (OWN-119) reflects the prior observed 56 MiB whole-process
VmHWM, including in-process corpus build. The overall 100 MiB promise at 100k notes
is unchanged; this default correction is not a fresh 100k qualification.

`--symbols` explicitly bypasses only the gazetteer automaton cap. Its budget row keeps
the configured numeric value, reports `source: flag`, `flag: --symbols`, and
`cap_lifted: true`; it does not invent an infinite cap or lift another resource budget.
Doctor itself has no symbols override and uses default run flags.

OWN-54 guidance permits larger process budgets (roughly 1.6–2 GB when useful); that is
operator guidance, not a validated ceiling or a promise that every configuration fits
the host. Measure before raising a cap. Disabled/failed optional layers remain visible
and do not remove usable sibling-layer statistics.

## Remaining defaults register

These are configuration defaults, not claims that every reserved capability ships.
Use quality/latency measurements from your own workload before changing a knob.

| Key | Default | Measurement or trigger |
| --- | --- | --- |
| `reconcile.tau_type` | 0.3 | Judged type-link quality |
| `reconcile.tau_id` | 0.4 | Judged identity-link quality |
| `reconcile.priors` | unset | Explicit calibrated priors file |
| `discovered.cap` | 500 | Review backlog and memory |
| `discovered.prune_after_days` | 90 | Age of unused discoveries |
| `discovered.promote_when.min_count` | 20 | Repeated evidence count |
| `discovered.promote_when.min_sources` | 3 | Independent-source coverage |
| `discovered.promote_when.min_mean_score` | 0.7 | Judged promotion quality |
| `ontology.pattern_size_limit_kib` | 1024 | Pattern compiler allocation |
| `ontology.golden_strict` | false | Explicit strict ontology qualification |
| `gazetteer.min_pattern_len` | 3 | Short-pattern false positives |
| `gazetteer.common_word_max_len` | 4 | Common-word false positives |
| `gazetteer.corpus_freq_cutoff` | 0.05 | Pattern frequency and match quality |
| `gazetteer.max_targets` | 5 | Ambiguity fan-out |
| `gazetteer.max_chunk_ms` | 1 | Chunk matching elapsed time |
| `gazetteer.v4_symbols` | true | Explicit symbol inclusion and dictionary size |
| `embed.model` | unset | Reserved; nonempty maps unavailable (OWN-94) |
| `embed.dtype` | int8 | Reserved quantization setting |
| `embed.knn_k` | 5 | Reserved neighbor count |
| `embed.tau.default` | 0.80 | Reserved similarity threshold |
| `embed.scan_window_rows` | 4096 | Reserved scan window |
| `embed.min_topk_overlap` | 0.9 | Reserved quantization qualification threshold |
| `l3.holdout_min_f1` | 0.50 | Reserved qualification threshold; default enrollment deferred |
| `l3.holdout_min_spans` | 200 | Reserved qualification sample floor |
| `recall.fusion` | rrf(60) | Reserved alternative-fusion choice; qualification deferred |
| `recall.rrf_k` | 60 | Judged ranked recall quality |
| `recall.judged_min_queries` | 100 | Judged query sample size |
| `recall.column_weights.text` | 1.0 | Judged text-column recall |
| `recall.column_weights.summary` | 1.5 | Judged summary-column recall |
| `recall.column_weights.tags` | 1.0 | Judged tag-column recall |
| `recall.column_weights.ident_terms` | 2.0 | Judged identifier recall |
| `ask.template_depth` | 1 | Reserved template traversal depth |
| `ask.template_depth_max` | 3 | Reserved template traversal cap |
| `er.m.title` | 0.9 | Match-pair title agreement |
| `er.m.aliases` | 0.8 | Match-pair alias agreement |
| `er.m.kind` | 0.95 | Match-pair kind agreement |
| `er.m.tags` | 0.6 | Match-pair tag agreement |
| `er.m.path_scope` | 0.7 | Match-pair path agreement |
| `er.m.anchors` | 0.95 | Match-pair anchor agreement |
| `er.m.summary` | 0.5 | Match-pair summary agreement |
| `er.u.title` | 0.01 | Nonmatch-pair title agreement |
| `er.u.aliases` | 0.02 | Nonmatch-pair alias agreement |
| `er.u.kind` | 0.3 | Nonmatch-pair kind agreement |
| `er.u.tags` | 0.1 | Nonmatch-pair tag agreement |
| `er.u.path_scope` | 0.05 | Nonmatch-pair path agreement |
| `er.u.anchors` | 0.001 | Nonmatch-pair anchor agreement |
| `er.u.summary` | 0.1 | Nonmatch-pair summary agreement |
| `er.match_threshold` | 8.0 | Judged entity-resolution matches |
| `er.possible_threshold` | 3.0 | Judged entity-resolution review candidates |
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
| `briefing.max_candidates` | 5 | Useful briefing size |
| `bench.min_samples` | 30 | Benchmark sampling sufficiency |
| `bench.why_regression_pct` | 10 | Benchmark regression tolerance |
| `why.anchor_prefilter` | true | Anchor query latency and result parity |

Indexer isolation, cache-sharing, deadline and quota semantics are specified in the
[v4 stage-one plan](../../../specs/kaban/specs/02-code-graph-v4/stage1-plan.md) CG4.2. The complete executable
register is [the default fixture](../../crates/kaban-core/fixtures/config/defaults.yaml),
whose test verifies equality with `EngineConfig::default()`.
