# Rust producer qualification fixtures (R0)

These are invented, explicitly trusted qualification inputs, **not** a supported
Rust indexer integration or accepted graph goldens. Ordinary Kaban tests do not
run them. They contain no registry dependencies. The workspace fixture deliberately
executes a build script and a procedural macro; only run through the isolated
qualification recipe. No host Cargo home, configuration or registry is shared.

## Independent oracle (written before the first producer run)

For `minimal`, the expected source-level facts are:

- `DatabaseDriver` is a trait with method `execute`. Exactly `Postgres` and
  `Sqlite` implement it. `NotADriver` does not. Both implementation methods must
  be distinguishable from the trait declaration and from each other.
- `production`, `middle`, `leaf`, `function_value`, `café` and `Postgres::new`
  are production definitions, even though `src/lib.rs` also contains tests.
- `production` references `middle`; `middle` references `leaf`; `function_value`
  references `leaf` as a value. These are REFERENCES, never measured CALLS.
- Inline `tests::production_works` and integration `integration_works` are tests
  referencing `production`. Neither the entire library file nor `production`
  may be labelled a test. The inline test scope is `#[cfg(test)] mod tests`.
- The identifier `café` and the string `é` exercise UTF-8 byte columns; `local`
  inside that function is a document-local symbol. Its use must resolve.
- `std::mem::size_of` is an external reference; missing sysroot source is a
  qualification limit, not permission to drop the expected fact silently.

For `workspace`, package `rust-qualification-app` uses the path-only library
`qualification-support` and proc-macro crate `qualification-macros`:

- The build script creates `build-script-ran.txt` only inside its `OUT_DIR`.
- The procedural macro creates `proc-macro-ran.txt` only inside `TMPDIR`, and
  expands `generated_value` to a function returning 7.
- `answer` references `qualification_support::base_value` and `generated_value`.
- Successful output alone is insufficient: both execution markers must exist
  in scratch and the generated definition/reference must be present in SCIP.

Missing implementation relationships are an R0 contract decision. Do not invent
them from descriptor spelling or weaken the oracle to fit producer output.
The producer may report success after a build/sysroot error: inspect stderr and
decoded SCIP as well as its exit status. This fixture does not qualify all Rust
build configurations, network isolation in Kaban, or macOS execution.

## Reproduction

Use `bash scripts/dev/qualify-rust-scip.sh <absolute-analyzer> <absolute-toolchain>
<minimal|workspace> <existing-empty-scratch>`. The script is Linux ARM64-only and
requires existing `bwrap`, `timeout`, and the real (non-rustup-proxy) compiler
toolchain. It downloads/installs nothing and never changes user configuration.
It uses the existing Cargo slot helper and retains evidence/scratch for inspection.
The release pin and observed limitations are recorded in
`../specs/kaban/00-project/archive/macos-rust-execution.md`.
