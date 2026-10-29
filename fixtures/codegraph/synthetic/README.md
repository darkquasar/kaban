# Synthetic SCIP indexes

This directory holds `*.scip` files written from code, not by an indexer: small `scip::types::Index`
values serialised by a `#[test] #[ignore] regen` in `crates/kaban-index` (stage1-plan CG3.2 writes the
first ones; CG7.3 adds the generated large index for the N2 bench). The CG3.1 gate tests build their
synthetic indexes in memory (`crates/kaban-index/tests/gate.rs`).

Files (builder: `crates/kaban-index/tests/common/mod.rs` `synthetic_index`; writer:
`cargo test -p kaban-index --test writer -- --ignored regen_synthetic_fixtures`; every reading test
asserts the committed bytes still equal the builder's output):

| file | purpose | read by |
|---|---|---|
| `duplicate.scip` | one symbol declared twice in one document (G1-F1) | `writer_duplicates_counted` |
| `malformed.scip` | one symbol with an unbalanced backtick, its definition, a reference and a relationship (G1-F1, G1-S1 step 2) | `writer_grammar_failure_skips_row` |
| `big-doc.scip` | a 5 KiB documentation block and a 2,000-byte signature (G1-F2) | `writer_signature_doc_bounds` |
| `typed-only.scip` | `F()` enclosing a reference to `G()`, every range in the typed oneofs (`[EN-amend]`) | `edges_from_typed_enclosing_range` |
| `legacy-only.scip` | the same, every range in the legacy `repeated int32` fields | `edges_from_legacy_enclosing_range` |
| `mixed.scip` | the same, definitions typed and the reference site legacy (`scip_range_form = mixed`) | `mixed_index_reports_mixed` |
| `canary-sym.scip` | the §8.3 step 7 variant: `CANARY_SYM` as a struct and method identifier beside a clean `Plain#` | `writer_structural_hit_skips_row` |

The goldens of the two producer fixtures live in `../go-drivers/goldens/` and `../ts-drivers/goldens/`
(sorted row dumps of `files`, `symbols`, `edges`, `occurrences`, `edge_types`, `meta` and the snapshot
byte size, written by `cargo test -p kaban-index --test goldens -- --ignored regen_goldens` under
`examples/policy-test-canaries.yaml`; compared byte for byte by `writer_golden_symbols_<fixture>`;
`scripts/dev/regen_fixtures.sh` runs both regen tests after its `VERSIONS` guard).

Rules for a file in this directory:

- it is produced by a named test in the crate and committed; the test that reads it names the writer;
- it carries no real repository content: symbols use the `example.com/...` and `@example/...`
  packages of the §8.1 fixtures, documentation is placeholder text, and the only secrets are the
  `CANARY_*` strings matched by the test policy;
- the committed Go and TypeScript indexes stay under `../go-drivers` and `../ts-drivers` with their
  provenance in `../VERSIONS`; a synthetic index never pretends to be a producer's output.
