# kaban-rust-scip — the Rust producer of the code graph

## Status (2026-10-26, integration head 0e583ae)

`kaban-rust-scip 0.1.0-dev` is the SCIP producer `kaban-index` runs for `Cargo.toml` roots
(`--languages rust`). This independent workspace does not join the Kaban workspace and links
rust-analyzer (`ra_ap_* 0.0.358`) into neither Kaban executable. It speaks contract v2 (below),
advertises one host, `aarch64-unknown-linux-gnu` (22 qualification profiles on the release
build, the `required-features` and `derive-impls` profiles included; evidence
`../specs/kaban/02-code-graph-v4/archive/evidence/df2-evidence/`, `r2-evidence/`, `r3-evidence/`, outside the repository), and
indexes real repositories: Kaban itself at `0e583ae` in 169 s of producer time on the `none`
tier (576 files, 75,257 symbols, 425,601 edges, 209,350,656 B,
`../specs/kaban/02-code-graph-v4/archive/evidence/dogfood/shared-index-0e583ae.json`) and 212 s under `namespaces` at `2daaeb1`
(`../specs/kaban/02-code-graph-v4/archive/evidence/df2-evidence/df3-gate.md`). No other host is advertised and no macOS runtime
exists (macOS development stopped by owner decision). Prepared `index` executes Cargo, build
scripts and proc macros: invoke it only inside `kaban-index`'s runner.

Since DF-2 the producer reports instead of refusing: feature-gated member targets
(`semantic skipped targets=<names>`), proc-macro crates not built, block-local items, tokens
without a stable symbol, files shared by several crates, modules with unresolved imports,
cfg-divergent bodies (published as the union) and generic derived impls
(`semantic derived_impls_generic_skipped=<n>`); the one-line `semantic coverage:` summary on
stderr and the `coverage.json` sidecar beside the output carry the same counters, which
`kaban-index` publishes as the snapshot's `coverage:*` rows (`crates/kaban-index/src/writer.rs`,
`Coverage::KEYS`). Inherent-impl members are children of their type (R-2), derived impls in
anonymous consts are published like module-level impls (R-3, FIX-G). What still refuses:
load, build and sysroot failures, a `linker` that is not a canonical executable outside the
workspace, a `.cargo/config[.toml]` that redirects the compiler backend (the diagnostic names
the table and the key), and a `protocol_version: 1` configuration.

## Usage

```bash
cd tools/kaban-rust-scip
cargo build --release --locked --offline            # its own lockfile and toolchain pin (Rust 1.99.0)
install -m 755 target/release/kaban-rust-scip "$HOME/.local/bin/kaban-rust-scip"
kaban-rust-scip --version                           # kaban-rust-scip 0.1.0-dev
kaban-rust-scip capabilities --json                 # protocol 2, profile kaban-rust-v1, the host list
```

Install it outside the repository being indexed and outside `KABAN_HOME`; `kaban-index` finds
it on `PATH` or through `--indexer rust=/absolute/path`, checks `capabilities --json` against
its pins (`rust_capabilities_refused` otherwise) and calls `index` with a prepared
configuration it writes itself. The binary has no `--help`: an unknown invocation answers the
fixed diagnostic `semantic exporter qualification is incomplete; indexing is unavailable`.
Do not call `index` by hand on a repository you do not trust: it runs that repository's build
code as you.

## Contract v2 (RI-3d)

The prepared configuration is `protocol_version: 2`: the six v1 fields plus one required
`linker`, the canonical path of the system C compiler cargo links with. It is admitted like
the other resources before anything runs: absolute and normalized, equal to its own canonical
path (no symlink such as `/usr/bin/cc` through `/etc/alternatives`), an existing regular
executable, and not under the workspace being indexed; the diagnostic class is `linker
resource is unavailable`. The semantic child's fixed environment is the v1 set plus exactly
one entry, `CARGO_TARGET_<TRIPLE>_LINKER=<linker>` with `<TRIPLE>` derived from the config's
`target` (upper-case, `-` as `_`), since DF-2 `CC=<linker>` as well (build scripts
compiling C through `cc-rs` find no `cc` inside an isolating view), and since HD-1 the backend
pin `COMPILER_PATH=<directory of linker>`: gcc resolves `ld` and `as` through its own program
directories, then `COMPILER_PATH`, and only then `PATH`, where the toolchain directories precede
`/usr/bin`, so an `ld` or `as` placed ahead of `/usr/bin` on the child `PATH` is never executed
(astra ri4-3bc-1; `scripts/qualify-loading.sh shadow-backend` and `tests/backend.rs` prove it
against the real toolchain). A member target whose
`required-features` are not active in the default profile is skipped, not refused; its name is
reported on stderr (`semantic skipped targets=<names>`). A `protocol_version: 1` configuration is refused
(`unsupported prepared protocol version; expected 2`); there is no v1 compatibility.

## A7 output boundary

The private extractor walks each Cargo target and selected production/test context,
retaining production dependencies. It enumerates module/trait/impl declarations,
fields, variants and unused macro-generated declarations, then resolves source
tokens through public RA semantic APIs. Standard SCIP contains typed definitions,
exact UTF-8 byte ranges, source-definition enclosers, local/external references,
signatures and concrete positive implementation/member relationships. References
are never CALLS. Generated declarations are located at committed invocation source.

Semantic anchor/signature disagreement across cfgs, shared-source roots and
load/build/resource failures refuse rather than publish partial data; since DF-2 a
function body whose references differ between cfg contexts is published as the union
with per-mode tags and reported (`semantic cfg/body divergent bodies=<n>`). Relationships are emitted
only when present in every selected context including production; generic,
conditional/where-qualified, negative, reference/tuple-self and generic-ADT impls
are not unconditional nominal edges. This is not exhaustive trait satisfaction.
Cargo's explicit `--all-targets` includes `test=false` library tests;
`harness=false` removes `--test`, not Cargo's explicit `--cfg test`.

Active block-local items (including impls and unused declarations from nested
macro expansions) are not published: module enumeration does not provide complete
nested declarations/enclosers. Since DF-2 they are counted and reported on stderr
(`semantic skipped block items=<n>`) instead of refusing the run (the `block-*`
qualification profiles now expect output). One exception since R-3: an impl inside
an anonymous const declared directly in a module (`const _: () = { impl Serialize
for X { .. } };`, the shape serde's, thiserror's and clap's derives expand to) is
published like a module-level impl, reached through rust-analyzer's unnamed-const
traversal: its declaration under the module with the lexical ordinal of its const
(`$const3:impl#[X][Serialize]`, counted in source order before cfg pruning; a derived
one at its item's position with the meta slot, the derive-list position and the
expansion-local ordinal, `$const3-0-0-0:impl#[X][Serialize]`), its members
(`$const3:impl#[X][Serialize]serialize().`)
and the same contract-v1 relationships; a generic or predicate-bearing one keeps its
declaration without an edge and is counted (`semantic derived_impls_generic_skipped=<n>`).
Items beside the impl in the const (serde's helpers) and impls inside function bodies
stay skipped block items. The `derive-impls` qualification profile (a local derive
macro, a hand-written and a generic anonymous-const impl) decodes this. Ordinary
expression-only bodies/macros and inactive cfg items remain supported. Full pinned offline sysroot
metadata is required; no-deps and stitched fallback graphs are not success.

All extraction and serialization complete before output staging. A no-replace
atomic hard link publishes the complete synced bytes; the scratch-only staging
link is retained. Existing output/staging paths are never overwritten. Loading
failure creates neither output nor staging data. Caller deadlines, isolation,
quotas and descendant settlement remain required.

The independent original minimal and workspace decoded oracles pass actual output
in the isolated qualification recipe. Additional cases and exact final gates are
bound in the external `a7-report.md`; no production consumer, release, Mac runtime
or supported-host acceptance follows merely from an exporter run.

Historical A0–A6 primitives and qualification findings (superseded by A7 above):

- Repository-free `--version` and bounded `capabilities --json` probes. The latter
  names protocol 2 (1 before RI-3d), profile `kaban-rust-v1`, producer `0.1.0-dev`, candidate
  `ra_ap_version: "0.0.358"`, and `supported_host_triples` (historically `[]`; now
  `["aarch64-unknown-linux-gnu"]`, see the top of this file). Consumers must reject
  an unlisted host. A profile name is not evidence that its oracle passed.
- A typed target namespace using Cargo package name/version, target kind/name and
  a checkout-relative source-root discriminator. Item descriptors remain semantic
  exporter inputs, not guesses from source text. No absolute path, commit or
  analysis database ID is included.
- Source-level candidate API checks and named collision/escaping regressions.
  Compiling these is not loading a fixture or proving semantic output.
- Read-only prepared-invocation validation for `index`: four unique required
  options, exactly four threads, absolute normalized paths, strict seven-field
  config-v2 JSON (maximum 16 KiB; six fields in v1), native target and required
  resource-file presence. Unknown/missing/duplicate keys and unsupported versions are errors.
  Repository and ancestor Cargo configuration and toolchain selectors are the consumer's
  concern (DF-2): it admits their content and passes the resolved sysroot, whose own
  `cargo` runs here (no rustup proxy, `RUSTUP_TOOLCHAIN` never set).
  Diagnostics are bounded below 4 KiB; output is never opened or overwritten.
  Valid-shaped inputs still fail with the unqualified-exporter diagnostic.
  A5 adds a typed prepared view and private semantic loader. It re-execs with a
  fixed environment (`CARGO`, `RUSTC`, `HOME`, `CARGO_HOME`, `CARGO_TARGET_DIR`,
  `TMPDIR`, `PATH`, `RUSTDOC`, five offline constants and, since contract v2,
  `CARGO_TARGET_<TRIPLE>_LINKER` and `CC`, both the admitted linker, and `COMPILER_PATH`, its
  directory) before RA resolves Cargo/PATH, revalidates
  admission in the child and rejects a direct child invocation unless the entire
  environment matches that fixed view. No process-global environment mutation is used.
  Missing clean scratch directories or noncanonical offline Cargo-home config
  fail before tool execution. Real non-proxy toolchain paths and Rust/Cargo 1.99.0
  versions are checked. Metadata fallback errors, build failures, missing sysroot
  sources, absent server and per-crate proc-macro loading errors are not success.
  Module declarations are materialized, including macro expansion. This is a
  loading checkpoint, not semantic-profile/export acceptance or production
  isolation qualification; deadlines/group teardown remain caller-owned.

## Candidate pins and API findings

`Cargo.lock` records registry checksums; direct `ra_ap_*` dependencies are all
exactly `0.0.358`. The toolchain/MSRV is the existing Rust 1.99.0; no automatic
installation or root workspace toolchain change is permitted.

Historical 0.0.352 dependency resolution exposed two upstream compatibility constraints:

1. `ra-ap-rustc_lexer 0.166.0` requires equal Unicode table versions.
   `unicode-properties 0.1.4` uses Unicode 17; newer `unicode-ident 1.0.27` uses
   Unicode 18 and fails its compile-time assertion. The old pin was `unicode-ident =1.0.22`,
   whose source uses Unicode 17, matching the lexer's declared minimum.
2. `ra_ap_span 0.0.352` uses handwritten `salsa_unstable` intern/hash APIs from
   `salsa 0.28.2`. `0.28.5` breaks them. The old pins for `salsa`, `salsa-macros`,
   and `salsa-macro-rules` were `=0.28.2`.

The pin remains a candidate until actual loading, proc-macro/sysroot compatibility
and the complete original fixture oracle pass. Initial failed compilation logs
are retained in the lane's ignored `evidence/` directory and external A report.
The A2 continuation's full offline advisory gate **failed** on Salsa 0.28.2:
[RUSTSEC-2026-0308](https://rustsec.org/advisories/RUSTSEC-2026-0308), a
use-after-free through safe APIs, fixed in 0.28.5. That fixed version conflicts
with the historical 0.0.352 RA API family as described above. Those failed audits
and compilation logs are retained unchanged.

A3 researched the public sparse registry on 2026-10-26: the newest published,
non-yanked matched direct RA family is 0.0.358 (declared MSRV 1.98). Its span,
base-db, HIR and IDE DB dependencies require Salsa `^0.28.5`; the newest published
Salsa is 0.28.5 (MSRV 1.88). Exact Salsa/macro pins now select that patched release.
RA HIR DEF requires `unicode-ident =1.0.24` (MSRV 1.71; Unicode 17 tables),
so that exact requirement replaces the historical workaround. A targeted Cargo
update changed only the 32 matched RA packages, three Salsa packages and
unicode-ident; the existing lexer remains locked. No blanket update or waiver.
Registry records/checksums and full audit evidence are retained in the external
A3 report. Full deny passes against the recorded advisory DB, with duplicate and
unused-license-allowance warnings. A4 subsequently compiled the matched graph and
passed standalone clippy and all 16 inherited tests on pinned Rust 1.99.0. A5
qualifies actual minimal/workspace loading separately using
`scripts/qualify-loading.sh`; full outputs, input identities and precise observed
scope are in the external A5 report. Neither gate accepts a working SCIP exporter.
Do not distribute or enable this candidate before requalification.
The initial checkpoint on Linux ARM64 with the existing Rust 1.99.0 passed
`cargo check --locked --offline --all-targets --all-features`, formatting,
and the license-only deny check.
The continuation executed all eight inherited tests successfully and added
public-CLI admission regressions with observed behavioral red/green evidence.
The historical A2 gate did not load a semantic workspace. No SCIP artifact or
native macOS qualification exists; see A4/A5 for later bounded evidence.

Inspected library behavior that the eventual loader must handle explicitly:

- `ra_ap_load_cargo::load_workspace_at` logs build-script errors and continues;
  use `ProjectWorkspace::run_build_scripts` and inspect `WorkspaceBuildScripts::error`.
- `ProjectWorkspaceKind::Cargo.error` can record a failed metadata load after a
  successful `--no-deps` fallback. Reject that partial load.
- `Sysroot::{error,metadata_error,is_rust_lib_src_empty}` expose incomplete sysroot
  loading. Missing source is not permission to omit external references.
- `load_workspace` can return no proc-macro client after an error. A successful
  wrapper result is not full semantic qualification.
- `ra_ap_toolchain::Tool::prefer_proxy` chooses a command from process
  `CARGO_HOME`/`CARGO`/`PATH` before `CargoConfig.extra_env` is applied. That map
  alone cannot pin Cargo to the admitted real executable. The future loader
  uses A5's clean-child dispatch, not unsafe process-global environment edits.
- `StaticIndex` visits each source file once and does not by itself establish
  correct multi-target contexts, semantic implementation edges or unused generated
  definitions. These remain exporter work, not capabilities advertised here.

## Proposed descriptor examples

A6 adds a private declaration inventory over actual Cargo target roots and selected
production/test contexts. Only the selected root receives `cfg(test)`; dependencies
retain production cfg. Same-named lib/bin targets with distinct roots are retained;
shared roots and conflicting declaration anchors/signatures fail closed. Public RA
module traversal, monikers and call-site navigation provide semantic facts directly.
This is not SCIP output: reference extraction, typed edges, full cfg_attr equivalence,
Cargo test/harness metadata, and the unchanged decoded original oracle remain pending.
Loading/count diagnostics alone do not prove a particular generated definition's range.

The standard `scip 0.10.0` formatter/parser owns escaping. For package `demo`:

```text
kaban-rust-scip cargo demo 0.1.0 `lib:demo:src/shared.rs`/Driver#
kaban-rust-scip cargo demo 0.1.0 `bin:demo:src/shared.rs`/Driver#
kaban-rust-scip cargo demo 0.1.0 `lib:demo:src/é``x.rs`/`café`#
```

These are primitive test vectors, **not emitted SCIP or accepted consumer fixtures**.
Two checkout/two commit equality and actual target identity mapping remain pending.

## Verification recipe

Run from this workspace after explicit resource admission. Its target is independent;
root `clean-room.sh` does not verify it. Preserve at least 2 GB = 2,000,000,000 bytes
free disk under the current owner override, with a separately admitted proactive
monitoring margin and bounded target/scratch cap. Use the
root `scripts/dev/cargo-slot.sh`, and set `KABAN_CARGO_SLOTS=1`, `CARGO_BUILD_JOBS=2`,
`CARGO_INCREMENTAL=0`, `CARGO_PROFILE_DEV_DEBUG=0`. Each Cargo command below is
prefixed with `bash ../../scripts/dev/cargo-slot.sh`:

```text
cargo fmt --all --check
cargo clippy --locked --offline --all-targets --all-features -- -D warnings
cargo nextest run --locked --offline
cargo nextest list --locked --offline
cargo deny --offline check
```

There was no producer crate at `PLAN_BASE`, so the baseline test list is empty.
The initial suite contains four identity tests, three CLI probes and one
API-surface test. A2 executed that inherited eight-test baseline and adds eight
public admission tests (16 total); its actual list/run is retained in the external
report. Dependency advisory failure remains distinct from passing runtime tests.

Required remaining work: complete target/cfg context qualification and deterministic
SCIP definitions/references/enclosers;
typed concrete impl/member edges; located unused macro definitions; all unchanged
`fixtures/codegraph/rust-qualification/README.md` facts and named collision/conditional
regressions; isolated scratch markers; full producer gates and notice audit.
Apple ARM64 cross-build and real-Mac acceptance are separately pending.

## Provenance

All producer source in this checkpoint is original. No CodeQL, Glean or
rust-analyzer source was copied or adapted; only public library interfaces were
inspected. There is no TRAP, QL, Glean or CodeQL CLI/runtime dependency.
Dependency licenses and the exact inspected files are described in `NOTICE`.
CodeQL's pinned Rust loader/translator were inspected as architecture precedents;
their TRAP translation and absolute-path/dependency-hash crate identities are not
reused. Direct public RA APIs supply the required semantic facts without that runtime.
