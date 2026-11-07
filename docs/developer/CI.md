# CI checks

Use native Cargo commands for focused work, `mise run core` for quality, helpers,
functional and PDF checks, and `bash scripts/ci/ci.sh` (or `mise run ci`) for comprehensive
checks. Standalone producer qualification is a separate explicit invocation.
No command here establishes release readiness by itself.

```text
+------------+  group =>  +---------------+  invoke =>  +------------------+
| mise / CLI | ========> | ci.sh dispatch | =========> | native commands  |
+------------+           +---------------+            +------------------+
                                                              |
                                                       status v
                                                    +------------------+
                                                    | failure / 3 / 0  |
                                                    +------------------+
```

## Working commands

Run from the repository root after setup:

```sh
bash scripts/ci/ci.sh --help
# Focused checks: choose the actual package / test you changed.
cargo nextest run -p kaban-api
cargo nextest run -p kaban-ingest --features pdf
cargo nextest run -p kaban --no-default-features
cargo nextest list --workspace

# Local core: quality + helpers + functional + PDF, stopping on failure.
mise run core
# Without mise activation, invoke those four groups in sequence:
bash scripts/ci/ci.sh quality
bash scripts/ci/ci.sh helpers
bash scripts/ci/ci.sh functional
bash scripts/ci/ci.sh pdf

# Comprehensive checks, including GNU release/smoke and native musl:
bash scripts/ci/ci.sh                 # equivalent to explicit all
mise run ci
# Standalone producer; run when producer/protocol/fixtures/drivers/tool setup
# or shared/unknown impact makes it applicable:
bash scripts/ci/ci.sh producer
```

| Group / mise task | Native checks | Exclusions |
| --- | --- | --- |
| `quality` | fmt, all three Clippy configurations, deny, store/dependency boundaries | Runtime suites and artifacts |
| `helpers` | MCP catalog tests, build helper tests, Bash syntax under scripts/hooks | Rust tools and compilation |
| `functional` | Workspace and Kaban no-default nextest; default library doctests | PDF variant, producer, artifacts and ignored/manual cases |
| `pdf` | Full PDF-enabled ingest nextest suite and its library doctest | Default workspace, producer and artifacts |
| `producer` | Locked fetch, fmt, locked workspace Clippy/tests, applicable doctests in tools/kaban-rust-scip | Root workspace and artifacts |
| `gnu` | Default GNU release, MCP smoke, graph delta, optional size table | Functional tests, musl and producer |
| `musl` | Native musl Kaban/index release and index size guard | GNU smoke, functional tests and producer |
| `all` / `ci` | quality + helpers + functional + pdf + gnu + musl | Standalone producer and ignored/manual cases |

Every group has a matching mise task (`mise run quality`, etc.; comprehensive is
`mise run ci`). `core` is a local composition, not a dispatcher group. Unknown
groups or extra arguments fail before tool discovery or checks.

Every nextest invocation uses `--success-output immediate`. Cargo tests and
doctests use `--show-output`; one runtime handler detects `NOT RUN:` and
`SKIPPED(mandatory)` in every selected Rust runner. Markers leave the group
incomplete even when the parent test passed; a failed child retains its status.
Library packages come from Cargo metadata, respecting actual library targets and
`doctest = false`. The libraries share one `cargo test --doc --locked -p <package> -p <package> ...
-- --show-output` invocation, preserving feature unification across them; ingest
runs separately with `--features pdf` in the PDF group. Kaban is bin-only, so its
no-default suite adds no library doctest. Libraries without examples honestly report zero
examples. Native metadata at the T2 source revision identifies a producer library as well
as its binary, and its `cargo test --workspace` command already includes applicable library
doctests. It needs no second doctest invocation. Root workspaces with no library
targets report no doctestable library targets.

The producer group changes to `tools/kaban-rust-scip` before tool checks and runs:

```sh
cargo fetch --locked
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked -- --show-output
```

It reads that directory's toolchain pin and probes its fmt/Clippy components.
It preserves an explicit `CARGO` executable and rejects a version mismatch rather
than replacing the toolchain. Its target is `<absolute CARGO_TARGET_DIR>/producer`
(default `<repository>/target/producer`), separate from the root workspace target.

`CARGO` is one executable path (spaces are supported), with no fallback if an
explicit path is invalid. `CARGO_TARGET_DIR` selects the output directory.
`CI_SKIP_SMOKE=1` skips only MCP smoke, keeps release/size work and forces exit 3.
`KABAN_SIZE_TABLE=1` adds the optional per-feature builds and no-default size gate;
leaving it unset does not make the run incomplete. Commands never clean outputs.

Exit 0 means the selected gates ran and passed. Exit 3 means required work is
incomplete: missing/wrong prerequisites, mandatory runtime skips, a skipped smoke
or a reported musl/graph size miss. Other child failures retain their status.
Missing required nextest/deny stops before compilation in groups that use them;
there is no Cargo-test fallback.
An installed tool's failed probe or check propagates its failure and diagnostics.

## Setup and prerequisites

`mise.toml` owns Node, Python, nextest and deny versions. `mise.lock` contains the
verified download inputs for Linux x64 and ARM64, generated with mise 2026.10.24.
With a trusted mise installation and reviewed project configuration, use
`mise install --locked`; `locked_verify_provenance = true` retains supported
provenance verification. Use `mise exec -- <command>` for native commands when
mise activation is not already putting those tools on PATH. A lock/cache hit
never substitutes for running checks.

The root and `tools/kaban-rust-scip/rust-toolchain.toml` files own Rust versions;
there is no second Rust pin in mise. Install the root channel, rustfmt, Clippy and
the host-native musl target through rustup before running comprehensive checks.
The producer is a separate workspace: commands must run in its directory to use
its toolchain, with its own target directory. Producer fmt/Clippy also need those
components even though its toolchain file uses the minimal profile.

`ci.sh` reuses the small `preflight.py` probe helpers before compilation. The
comprehensive path retains the full prerequisite check; individual groups require
only their selected tools. Rust groups need Python 3.11+ for authoritative TOML
pins and check pinned Python/Cargo/Rust versions. Quality adds fmt/Clippy, deny
and jq; functional adds nextest, pinned Node, Go/scip-go and cc. PDF needs only
nextest, cc and Git beyond the common pinned Python/Cargo/Rust checks; it does not
probe Node, Go/scip-go, deny, musl or namespace capabilities. Producer probes
its own toolchain and components, without nextest/deny or musl. Helpers need Python 3.11+
with SQLite, Git and Bash; they report an actionable exit 3 for an older Python
without requiring the exact mise pin. Artifact groups require native Linux/GNU utilities;
musl checks its configured linker and installed target, and GNU smoke requires
strace unless explicitly skipped. Comprehensive retains all these requirements.
The helper checks paths/versions, not executable headers or host attestation, and
does not install or repair tools or qualify kernel namespace support.

Go and scip-go pins/checksums remain in the existing
[installers](../../scripts/README.md). Keep the existing namespace-visible Go
layout under `/usr/lib`, its `/usr/bin/go` fixture alias and `GOROOT`/`SCIP_GO`
setup in the scoped runner. Product checks of Go/Node executable ownership,
permissions and namespace paths remain authoritative. Do not change host policy
or expose broader filesystems to bypass refusal.

The existing heavy CI settings remain `CARGO_BUILD_JOBS=2`, `CARGO_INCREMENTAL=0`,
`CARGO_PROFILE_DEV_DEBUG=0` and `CARGO_PROFILE_DEV_STRIP=symbols`. Local concurrent
work follows the existing Cargo-slot policy and uses private targets. Quota tests
still need adequate `/tmp` space; actual namespace and resource assertions remain
in the suites. macOS can run focused commands and helpers; comprehensive GNU/musl
checks need a suitable native Linux environment.

## Native ARM64 macOS setup

Native setup and application qualification use macOS 15.6.1 (24G90), ARM64,
Command Line Tools 17.0.0 with SDK 15.5 and Rust/Cargo 1.99.0. The focused native
qualification recipe below uses fresh private data and the existing suites.
It does not establish compatibility with every macOS 15 patch or hosted CI.

```text
+------------------+  locked tools  +------------------+  focused checks  +----------------+
| Native ARM64 Mac | =============> | Private scratch  | ===============> | Native evidence|
+------------------+                +------------------+                  +----------------+
```

Install ordinary Apple Command Line Tools if absent; full Xcode and signing tools
are not required for these unsigned CLI probes. Record the actual environment:

```sh
sw_vers
uname -m
xcode-select -p
xcrun --show-sdk-path
xcrun --show-sdk-version
df -h .
sysctl -n hw.memsize
vm_stat
```

Use a trusted Mise installation. The existing verified local bootstrap is
`~/.local/share/mise/bootstrap/mise-2026.10.24`; it need not be on PATH. Its raw
macos-arm64 SHA256 is
`ac0ee145cb145affe3ea60623a4abdb253e5ad1f938def299e633e816c0b8523`, checked against
the [upstream checksum manifest](https://github.com/jdx/mise/releases/download/v2026.10.24/SHASUMS256.txt).
Normal trust and provenance checks stay enabled. From the repository root:

```sh
MISE="$HOME/.local/share/mise/bootstrap/mise-2026.10.24"  # or your trusted mise path
"$MISE" trust ./mise.toml
"$MISE" install --locked
"$MISE" exec -- python3 --version
"$MISE" exec -- node --version
"$MISE" exec -- cargo nextest --version
"$MISE" exec -- cargo deny --version
```

The lock now covers Darwin ARM64 as well as both existing Linux architectures.
Regeneration, when needed, uses
`mise lock --platform linux-x64,linux-arm64,macos-arm64`; inspect its diff, retain
Linux entries and install with `--locked`. Do not hand-edit download metadata.

Read both Rust pins rather than inventing a Mac-specific toolchain:

```sh
kaban_root_rust=$("$MISE" exec -- python3 -c 'import pathlib,tomllib; print(tomllib.loads(pathlib.Path("rust-toolchain.toml").read_text())["toolchain"]["channel"])')
kaban_producer_rust=$("$MISE" exec -- python3 -c 'import pathlib,tomllib; print(tomllib.loads(pathlib.Path("tools/kaban-rust-scip/rust-toolchain.toml").read_text())["toolchain"]["channel"])')
rustup toolchain install "$kaban_root_rust" --component rustfmt --component clippy --component rust-src
rustup toolchain install "$kaban_producer_rust" --component rustfmt --component clippy --component rust-src
rustc -vV
cargo --version
(cd tools/kaban-rust-scip && rustc -vV && cargo --version)
```

External producers live outside the checkout and `KABAN_HOME`. The current Linux
`install-go.sh`/`install-scip-go.sh` scripts are not Darwin installers. For a new
user-owned tools directory, these native downloads match the existing fixture
versions. Run with shell failure checking; do not overwrite an existing install:

```sh
set -euo pipefail
kaban_tools="$HOME/.local/share/kaban-tools/macos-arm64"
test ! -e "$kaban_tools"
mkdir -p -m 700 "$kaban_tools/bin"
kaban_downloads=$(mktemp -d "${TMPDIR:-/tmp}/kaban-native-downloads.XXXXXXXX")
curl -fSL https://dl.google.com/go/go1.26.0.darwin-arm64.tar.gz -o "$kaban_downloads/go.tar.gz"
printf '%s  %s\n' b1640525dfe68f066d56f200bef7bf4dce955a1a893bd061de6754c211431023 "$kaban_downloads/go.tar.gz" | shasum -a 256 --check
tar -xzf "$kaban_downloads/go.tar.gz" -C "$kaban_tools"
curl -fSL https://github.com/scip-code/scip-go/releases/download/v0.2.7/scip-go-darwin-arm64.tar.gz -o "$kaban_downloads/scip-go.tar.gz"
printf '%s  %s\n' f21b505452a8dbb270c18ec66690e583801f9e96cff1d72e84c004b7374ec672 "$kaban_downloads/scip-go.tar.gz" | shasum -a 256 --check
tar -xzf "$kaban_downloads/scip-go.tar.gz" -C "$kaban_tools/bin" scip-go
chmod -R go-w "$kaban_tools/go"
"$MISE" exec -- npm install --prefix "$kaban_tools/typescript" --save-exact @sourcegraph/scip-typescript@0.4.0
export GOROOT="$kaban_tools/go"
export SCIP_GO="$kaban_tools/bin/scip-go"
export PATH="$GOROOT/bin:$kaban_tools/bin:$kaban_tools/typescript/node_modules/.bin:$PATH"
go version
"$SCIP_GO" --version
"$MISE" exec -- scip-typescript --version
```

Go's checksum comes from the [official download metadata](https://go.dev/dl/?mode=json&include=all);
scip-go's comes from the [v0.2.7 release](https://github.com/scip-code/scip-go/releases/tag/v0.2.7).
The native setup produced TypeScript 5.9.3 through scip-typescript 0.4.0; retain
the external installation's package lock and integrity metadata. Installation
and version checks do not prove real producer-to-query behavior. Building the
standalone Rust producer and qualifying all three languages belong to later M4.

Use disposable data and one private target for an authorized native check:

```sh
export KABAN_HOME=$(mktemp -d "${TMPDIR:-/tmp}/kaban-native-home.XXXXXXXX")
export CARGO_TARGET_DIR=$(mktemp -d "${TMPDIR:-/tmp}/kaban-native-target.XXXXXXXX")
export CARGO_BUILD_JOBS=2 CARGO_INCREMENTAL=0 KABAN_CARGO_SLOTS=1
export CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_DEV_STRIP=symbols
"$MISE" exec -- bash scripts/ci/ci.sh helpers
# Prefix authorized focused Cargo checks with the existing slot helper:
# "$MISE" exec -- bash scripts/dev/cargo-slot.sh cargo <focused arguments>
```

Keep at least 2,000,000,000 bytes free before heavy work and measure memory/disk;
start with one heavy local invocation. The first bounded 218 MB SQLite probe is
not a full-product resource estimate. Do not run `ci.sh all`, GNU/musl artifact
groups or change host isolation policy on this Mac. Comprehensive preflight now
passes pinned tool discovery and intentionally exits 3 at its Linux requirement;
that is incomplete comprehensive coverage, not a product failure or a native CI
pass. M6 owns the future focused Mac dispatcher/workflow. No sudo, Gatekeeper
change, real user database or shared Homebrew permission change is needed.

## Hosted isolation selections

The GitHub-hosted functional job sets `KABAN_CI_ISOLATION_SPLIT=1`. It checks that
both namespace conditions can be established before compiling, then builds the
workspace test executables once through nextest's binaries-only metadata command.
Two filtered executions reuse those executables and the same target directory:

```text
+---------------------+  reuse =>  +----------------------------------+
| Workspace test build| =========> | Two isolation-unavailable tests  |
+---------------------+            +----------------------------------+
          |
          | reuse =>
          v
+----------------------------------+
| Remaining tests: isolation works |
+----------------------------------+
```

Only `cli_index_required_without_isolating_tier_is_exit_8` and
`rust_required_unavailable_never_runs_native_producer_probes` run with
`kernel.apparmor_restrict_unprivileged_userns=1`. The remaining workspace tests
run with that restriction set to `0`. Both executions are attempted after a
successful build even if one fails, and their failures and mandatory markers
remain visible. The original setting is restored on exit. This changes only the
disposable GitHub VM's runtime setting, with no persistent configuration file.
Local and Tart commands retain their normal policy; opting in outside a
GitHub-hosted job is refused. PDF, no-default and doctest selections stay separate.

The existing per-group Cargo caches accelerate subsequent runs. The two isolation
executions share their already-built files directly, without artifact transfer,
another container image or another provisioned job.

## Historical capability assessment and remaining gap

The initial 2026-11-04 T4 assessment is a bounded blocker report, not Linux runtime
qualification. The available local host is Darwin ARM64 with Rust/Cargo 1.99.0
and Python 3.14.6. The actual comprehensive command, under `cargo-slot.sh`, stops
before compilation with exit 3 because cargo-nextest is absent, its first reported
prerequisite gap; later comprehensive prerequisites were not evaluated. The actual producer
command also stops before compilation with exit 3: Python 3.14.6 differs from the
3.12.16 pin. Mise, unshare and strace are absent from the inspected local PATH.
No current product namespace, doctor or refusal test was executed by this assessment.

The source requires Linux user, network, mount and PID namespaces through the
trusted `unshare` wrapper and the product's in-run self-check. The namespace suite
is Linux-only; passing macOS helpers or installing tool versions cannot qualify
it. The following existing branches still need actual evidence for the candidate:

| Required branch | Existing checks / missing evidence |
| --- | --- |
| Namespaces available | `tests/namespaces.rs`: read-only checkout/headers, restricted mounts/network, helper invocation/handle closure, self-check refusal, cache behavior and privilege-drop enforcement. All runtime assertions remain unproved here. |
| Namespaced tool/resource view | `tests/cli.rs`: Rust probe resources, Go module-cache tier comparison, partial-result refusal, narrow toolchain roots, doctor inside-isolation probe and CGO default. The namespaced halves remain unproved here. |
| Namespaced Rust admission | `tests/rust_admission.rs`: `host_registry_is_an_input_of_the_rust_run_on_the_selected_tier` and `checkout_cc_on_path_is_never_the_linker_on_the_selected_tier`. The host-registry input and refusal of a checkout compiler as linker remain unproved here. |
| Isolation unavailable | `cli_index_required_without_isolating_tier_is_exit_8` and `rust_required_unavailable_never_runs_native_producer_probes` need a real unavailable tier: refusal/exit 8 and no native producer probe. A capable host may leave these branches NOT RUN instead. |

Test paths above are relative to `crates/kaban-index/`. See the existing
[namespace tests](../../crates/kaban-index/tests/namespaces.rs),
[CLI tests](../../crates/kaban-index/tests/cli.rs) and
[Rust admission tests](../../crates/kaban-index/tests/rust_admission.rs), plus the
[required namespace flags](../../crates/kaban-index/src/isolate/namespaces.rs).
The historical hosted observation below remains 18 missing namespace subchecks;
it is not a fresh count or a claim about this candidate. This revision does not
combine complementary results from different runs or suppress mandatory markers.
Both capability branches need a concrete executable arrangement before complete
readiness can be claimed; T4 may finish with this gap documented.
The Go module-cache tier comparison returns without a mandatory marker when
namespaces are unavailable; its output must demonstrate that the comparison
actually ran. The historical 18-marker count does not include that unmarked gap.

The subsequent T5-E assessment selected Tart/Packer as the practical local
environment. A stock Ubuntu22.04 ARM64 tools-only image passed all13 namespace
integration tests plus the selected doctor CLI test, which selected namespaces.
Those runs had no mandatory NOT RUN markers. The remaining CLI/Rust-admission,
unavailable-tier, producer/doctest and artifact checks are separate qualification
work under C. This subset does not discharge OD-4 or the historical musl size miss.
Ubuntu24.04 investigation is explicitly deferred; no guest/host security policy
was changed. Other provider comparisons are deferred too.

The versioned [Tart runner setup](../../runners/tart/README.md) contains the actual
Packer recipe, provisioner, checksum-pinned bootstraps and input staging tool.
Use the existing pins/installers and fresh clones with private targets. These
ARM64 observations do not establish current GitHub x86_64 behavior or service
semantics. No runner was registered and no workflow activated by this assessment.
Complementary unavailable-tier evidence still needs a concrete admitted arrangement;
do not suppress markers or introduce a generic counterpart engine.

## Native CI jobs

The workflow checks PR merge candidates, main pushes, merge-group candidates and
manual dispatches. Selected quality, functional, PDF, GNU and musl checks run as independent jobs after
a small scope-selection job; helpers start independently. A selected producer job
also runs independently under its own toolchain. Every job checks out the same
native event candidate. None depends on quality, and a failed job does not cancel
its siblings. Only a superseded PR run can cancel an earlier run of that PR;
main, merge-group and manual runs have distinct run-based concurrency groups.
No weekly schedule is active.

```text
+----------------+  dispatch =>  +--------------------+  results =>  +------------------+
| Selection [CI] | ============> | Native Rust groups | ===========> | Readiness [CI]   |
+----------------+               +--------------------+              +------------------+
                                                                              ^
                                                                      results |
                                                                     +------------------+
                                                                     | Helpers [CI]     |
                                                                     +------------------+
```

`Rust gates (scripts/ci/ci.sh)` retains its required-status name as a short,
always-evaluated readiness job. It requires successful selection and helpers,
and every selected quality, functional, PDF, GNU, musl and producer job. A Rust
job may be skipped only when successful selection explicitly excludes its scope.
Both selection flags must be valid, even when the jobs are omitted. Missing, failed,
cancelled or unexpectedly skipped work fails readiness. `Helper script unit tests`
retains its name and invokes `ci.sh helpers` once. No hosted job invokes `all`.
Exit 3 remains a failed hosted step; its summary explicitly says `INCOMPLETE`.
A result from another candidate or run cannot replace a missing check.

Documentation/metadata/helper-only PRs and main pushes run selection, helpers and
readiness, without compiling or testing Rust. The allowlist covers prose/images
under `docs/`, the named root project documents and licence files, release notice
bundles, and Linguist-only `.gitattributes` edits. Changes to checkout-affecting
attributes (for example `filter`, `text` or `eol`), including their removal, keep
Rust checks selected. Attribute macros in either the base or head file also
retain those checks, even when the macro itself was not edited. Only simple ASCII
attribute syntax is eligible for the shortcut; quoted patterns, carriage returns
and other byte encodings conservatively retain Rust checks. Other file types
under `docs/` are not assumed to be prose.

Root `AGENTS.md` is repository guidance, so it also omits Rust. A `.gitignore`
edit qualifies only when its changed rules are the exact root `/REPOLOCAL.md`
entry, comments or empty lines. Other ignore-rule changes retain Rust because
they can affect fixture discovery. An unanchored rule, control-character variant
or mixed change does not qualify. These checks use literal byte lines, not a
general Git-ignore interpreter.

The helper-only allowlist covers direct Python modules under `scripts/tests/`,
the MCP catalog unit test, and the three manually invoked drivers
`scripts/ci/clean-room.sh`, `scripts/dev/cargo-slot.sh` and
`scripts/dev/lane-gate.sh`. The helper job executes their existing tests and shell
syntax checks. Named script/hook/producer README and host-event documentation
also qualify; arbitrary Markdown under fixtures does not. Some fixture READMEs
are embedded in Rust tests and must retain Rust coverage.

The five release scripts (`authorize.py`, `build.sh`, `licenses.py`, `notes.py`,
`package.py`) omit duplicate root Rust checks. Their paths independently select
package validation on PRs; ordinary main pushes do not run the release workflow.
Authorization is covered by helper tests and executes on tags; release-note
rendering runs during tag publication. PR package builds do not execute either
tag-only step. Release/MCP unit-test changes run in helpers without launching
package builds. Changes to the runtime MCP smoke
or catalog validator retain Rust and package checks. CI orchestration, preflight,
tool installers, dependency gates, hook implementations and unknown scripts are
not helper-only. No changed-line parser tries to classify Rust or shell comments.

Rust source, test fixtures, workflow/build scripts, manifests, locks, tool setup,
and unknown paths retain the complete Rust checks, except the named helper-only
paths above. A mixed diff retains Rust whenever any path needs it.
Unavailable base/diff data, empty diffs, manual runs and merge-group candidates
select everything. This deliberately uses a small safe allowlist rather than
assuming that every change outside `.rs` files is irrelevant to the application.
Release-related notice changes still trigger the release workflow's own package
validation. A change to this CI workflow itself runs the full graph once, so its
routing can be validated before metadata-only changes start omitting jobs.

All actions use immutable commit revisions. Mise 2026.10.24 provisions the selected
locked tools without restoring or saving executable tool caches. Rust setup reads
the owning toolchain file and installs fmt/Clippy; musl adds its native target.
Functional/GNU preserve verified Go under `/usr/lib` and its `/usr/bin/go` alias,
SCIP_GO and pinned Node ownership/modes. Language tools live under `/opt/kaban-mise`.
These paths do not establish namespace qualification. Workflow permissions remain
`contents: read`, and checkout does not persist credentials.

Each Rust job has `target/ci-<group>`, a runner-local scratch directory and the
existing heavy settings above; producer uses `target/ci-producer/producer`.
Heavy jobs retain 90-minute timeouts; helpers and small orchestration jobs use 15.
Setup, dependency restore, fetch, checks and post-save are native log phases;
`ci.sh` prints each native command. Group summaries expose exit status and final
resource/cache information. Compilation/tests within a group share its check step;
its timing alone does not separate all compiler and test work.

## Native application and full download profile qualification

Use the pinned setup above, canonical `TMPDIR=/private/tmp`, a private
`KABAN_HOME`/`CARGO_TARGET_DIR`, two build jobs and one Cargo slot. Prefix each
Cargo command below with `"$MISE" exec -- bash scripts/dev/cargo-slot.sh`.
Run checks serially and retain the targets/download caches. The application
profile includes default memory, ingest, codegraph and Tantivy search, plus PDF
and semantic Markdown splitting; Cargo source defaults remain lean.

```sh
cargo nextest run --locked -p kaban \
  --features kaban-ingest/pdf,kaban-ingest/md-splitter \
  --test walkthrough --test cli_sync --test cli_pack --test recall_determinism \
  --test code_smoke --test acceptance_north_star \
  --test hook_claude --test hook_hosts --test hook_pairing --test hooks_roundtrip \
  --test hook_post_commit --test mcp_control --test mcp_scrub --test mcp_field_gate \
  --test mcp_prescan --test mcp_docs --test mcp_structured --test cli_gazetteer \
  --test cli_ingest --test ingest_blackbox_cli --test ingest_blackbox_mcp -j 1
cargo nextest run --locked -p kaban-ingest --features pdf,md-splitter \
  --test golden_markdown --test chunk --test golden_pdf --test incremental \
  --test gate_parity --test derived_sanitization --test hostile --test gazetteer \
  --test ingest_path_security -j 1
cargo nextest run --locked -p kaban-store-sqlite \
  --test tantivy_search --test tantivy_projector --test tantivy_capture \
  --test recall --test sync_store --test ingest_acceptance --test protection \
  --test forget_mentions -j 1
cargo test --locked --doc --workspace \
  --features kaban-ingest/pdf,kaban-ingest/md-splitter
"$MISE" exec -- /bin/bash scripts/hooks-live/test-stop-sync.sh
```

The hook harness uses `/bin/bash` on Darwin and a restricted PATH without
`setsid` or GNU `timeout`. It checks prompt JSON-only return, stdio detachment,
independent process groups, opt-out, repeated Stop events and termination of
TERM-resistant descendants for Claude/Codex Stop/SubagentStop. Existing Rust
fixtures exercise Claude/Codex/pi ingestion/recall without paid agents/models.
Graph smoke and north-star tests are fixture-derived public-query checks; their
real-producer exclusions are explicit. Real producer qualification has its own
[recipe and restrictions](code-graph.md), and need not be repeated for unrelated
application/hook qualification changes.

Gazetteer `build_peak_delta` and `build_hwm_delta` are Linux `/proc` observations.
On Darwin their existing integer fields are zero to mean **unavailable**, not
measured zero RSS; no sampling thread is started. Retained automaton bytes remain
measured and budgeted. This is distinct from the unavailable external-producer
RSS metric and does not disable wall-clock/output/scratch quotas. Darwin child
observation in the shared MCP smoke uses `/bin/ps`; denied/failed observation
fails that check. Sampling is a bounded observation, not a process sandbox.

Build both optimized application binaries together, once, using the canonical
full profile. A deployment target controls the Mach-O load command; it does not
by itself prove launch on an untested patch.

```sh
MACOSX_DEPLOYMENT_TARGET=15.0 cargo build --release --locked \
  -p kaban -p kaban-index --features kaban-ingest/pdf,kaban-ingest/md-splitter
file "$CARGO_TARGET_DIR/release/kaban" "$CARGO_TARGET_DIR/release/kaban-index"
otool -L "$CARGO_TARGET_DIR/release/kaban"
otool -L "$CARGO_TARGET_DIR/release/kaban-index"
otool -l "$CARGO_TARGET_DIR/release/kaban"
otool -l "$CARGO_TARGET_DIR/release/kaban-index"
"$MISE" exec -- python3 scripts/mcp/mcp_smoke.py \
  "$CARGO_TARGET_DIR/release/kaban" "$KABAN_HOME/native-smoke.db" \
  examples/policy.yaml
```

Record executable hashes/sizes, `LC_BUILD_VERSION` minimum/SDK, imported system
libraries/APIs, code-signature status and actual launch/test OS. The PDF backend
is compiled `pdf_oxide` (no external renderer/service); scanned files need OCR and
remain partial, malformed/unsupported encryption fails with a fixed class. OCR
is not included. Producers and language runtimes are separate trusted installs.
Keep macOS 15.6.1 as the tested patch floor until earlier runtime/API evidence
establishes otherwise. Local linker ad-hoc signing supplies no publisher identity;
this qualification makes no quarantine/notarization or release-readiness claim.
Full-profile size observations do not alter existing release size gates; any
overrun requires owner disposition at packaging. M6/M7 own CI/packaging changes.

## Focused native macOS CI

`mise run macos` (or `bash scripts/ci/ci.sh macos`) runs on native ARM64 Darwin
with the application Rust pin and locked Python, Node and nextest. It refuses
other hosts/architectures, a non-native Rust host, missing selected tools and
less than 2,000,000,000 free bytes before compiling. Use the native setup above,
`TMPDIR=/private/tmp`, a private `KABAN_HOME`/target and one Cargo slot with two
build jobs. The Mac group does not invoke Linux `all`.

The focused selections reuse proven M4/M5 binaries: core process; codegraph open
and discover; index writer, runner and trust; five CLI isolation/no-probe and
revalidation cases; full-feature Markdown/chunk/PDF ingest; memory walkthrough,
sync/recall, graph queries, MCP control/privacy/docs, and installed-host hook paths.
Linux retains complete generic, feature, doctest, quality, size and namespace
coverage. The broader M5 manual recipe above remains qualification evidence;
its extra ingestion/store cases are outside routine Mac CI. Fixture graph queries
are complemented by real Rust publication/query when producer qualification is
selected. Linux namespace-only cases are unselected on Mac; required exit-8/no-probe,
trusted-none and native process/FD/cleanup counterparts remain selected. Mandatory
runtime NOT RUN markers still produce incomplete/exit 3, and child failures retain
their exit status. No marker is suppressed.

The single `macos` workflow job uses the approved `macos-15` label, depends only
on selection and uses its existing `rust` boolean. It checks actual `uname -m`,
`RUNNER_ARCH` and Rust host; a label alone is not ARM64 proof. Readiness retains
its existing status name and requires Mac success for a selected candidate;
failure, cancellation, missing result or unexpected skip prevents success.
Only an explicitly unselected job may skip. Docs/metadata/allowlisted helpers
therefore avoid Mac compilation, with unknown/build/fixture/tool/workflow changes
conservatively selected. Linux jobs remain independently dependent on selection.

The adjacent `macos-producer` boolean scopes the expensive standalone producer
and existing 27-profile semantic/refusal oracle. It selects producer source,
Rust consumer/admission, shared core/process/environment/protocol/fixture,
manifest/lock/toolchain/pin/setup and unknown changes, plus initial/manual qualification. Merge-group scope uses its base SHA while
preserving Linux's existing conservative selection. Named unrelated ingest,
store-sqlite, Tantivy, memory CLI/tests, hooks, ingest/walkthrough fixtures and
examples omit producer qualification; manifests/build inputs inside those trees
still select it. Unknown paths and mixed relevant changes retain qualification. Linux's existing producer selection is unchanged.
Locally set `KABAN_MACOS_PRODUCER_QUALIFY=1` for explicit qualification; the normal
Mac group defaults to 0. Qualification builds the producer under its own toolchain
and target, installs a fresh executable outside the checkout, uses existing trusted
fixtures/oracles, and runs the real Rust publication-to-MCP-query acceptance case.
Scratch, private homes and targets are retained. This executes trusted code with
user access (`none`), with no Linux namespace-isolation claim.

Actions remain pinned and permissions `contents: read`; no release secrets are
used. Mise executable caches and Cargo bin caching remain disabled. Cargo keys
separate OS, architecture, owning toolchains, full-profile configuration and group,
with native GitHub ref restrictions; app and standalone mutable targets stay
separate. Every hit still executes checks. Ordinary logs/summaries retain setup,
dependency restore/fetch, Cargo build/test timings, exits, cache permission and
memory/disk observations. Native build/test durations appear in nextest output;
phase elapsed time includes both. Actual hosted fit, ARM64 execution and cold/warm
runner minutes remain unproved until an authorized relevant PR run. Local Mac
results cannot establish hosted readiness. Release workflow/packaging changes are
reserved for M7 after M6's hosted acceptance and the required Git grant.

```text
+-----------+  rust scope  +---------------+  result  +-----------+
| Selection | ==========> | macos [CI]    | =======> | Readiness |
+-----------+             +---------------+         +-----------+
      | rust scope  +-------------------+  result        ^
      +===========> | Linux jobs [CI]   | ================+
                    +-------------------+
```

## PDF parallelization spike

The PDF job moves the complete existing `kaban-ingest --features pdf` suite and
its doctest out of functional CI. It keeps its own `target/ci-pdf` and cache key,
with Python/nextest setup and no language-producer installation or namespace-policy
changes. PDF remains required by the readiness job. Local `all` and `mise run core`
still run both configurations; `mise run functional` selects the default suites,
and `mise run pdf` selects the PDF configuration.

```text
+-----------------+  default tests =>  +------------------+  status =>  +----------------+
| Selection [CI]  | =================> | Functional [CI]  | ==========> | Readiness [CI] |
+-----------------+                    +------------------+             +----------------+
         | PDF tests =>  +------------------+          status =>                 ^
         +============> | PDF [CI]         | ===================================+
                        +------------------+
```

The separate job compiles its own test configuration and can increase total runner
work even when it shortens the critical path. Compare the PR's functional/PDF job
and check-step durations, compiler `Finished` timings, actual dependency cache hits,
whole-workflow elapsed time, and summed job durations with main run `41799905215`.
That main run had no exact Rust cache hits; its GNU job was the critical path.
A comparison with a warm PR run alone cannot establish an end-to-end speedup from
PDF isolation. Keep the complete suite during this spike; choosing narrower PDF
coverage is a separate decision. Hosted observations determine whether to retain
the split; no compilation or elapsed-time savings are assumed here.

## Cache scopes and observation limits

[The pinned Rust cache action](https://github.com/Swatinem/rust-cache/tree/d1527c8f67ef9af357290475d4af4b0fab8f12e0)
keys dependencies by Rust/environment/manifests and lockfiles. Additional keys
separate platform, architecture, group, mise inputs and the producer toolchain.
The producer workspace maps to its actual private target. Cargo bin caching is
disabled. Staged smoke/size artifacts use `KABAN_CI_ARTIFACT_DIR` under the runner's
temporary directory, outside every cached target; local default staging is unchanged.

Compatible group prefixes are shared across events, allowing a new PR to restore
default-branch dependencies. [GitHub's native cache restrictions](https://docs.github.com/en/actions/reference/workflows-and-actions/dependency-caching#restrictions-for-accessing-a-cache)
keep PR-written entries confined to that PR's merge ref; default-branch runs
cannot restore them. Only push/manual runs on the default branch write entries
in that trusted scope. PR runs may save in their own scope; merge-group and
other-ref manual runs restore but do not save. Every group retains a separate
key and target. No custom per-PR prefix forces all new PR builds cold.

`cache-on-failure: true` permits dependency saves after a later check failure in
an allowed save scope. The pinned action's post-step runs on success or that flag,
then checks `save-if`. An exact cache hit, service availability/eviction, cleanup,
job cancellation or a post-step error can prevent a save. The summary records
permission to save; only the action's post-step log proves an actual save.
Every hit still executes the selected group. Native job overlap, actual cache
saves, footprint and elapsed gains await hosted observation during C; local
fixtures and upstream action inspection do not establish those results.

New producer/doctest obligations need actual execution baselines before workflow
activation. Historical native discovery/doctest observations below are retained;
ordinary producer execution is still unproved. T5's local producer probe stops
incomplete before compilation because available Python 3.14 differs from the
3.12.16 pin. No tool installation, full product campaign or assertion repair is
part of that initial workflow edit. Subsequent hosted runs passed producer and
doctest baselines, and PR #3 now exercises the parallel workflow. The hosted
isolation split above addresses the recorded capability gap; its actual selected
test results determine completeness. The current musl index limit is 12 MB.

`clean-room.sh` remains an explicit costly clean/reproducibility tool, outside
routine validation. Its summary and exit are `INCOMPLETE`/3 when its CI child is
incomplete, unless another step failed, which yields `FAIL`/1. Success is `PASS`/0.
It invokes the same comprehensive `ci.sh`; no duplicate gate implementation exists.

## Preserved baseline and bounds

Historical measured source: `44286bb81e7d6338983f65c79bb8a68cf929c9ef`, tree
`30e65982e4eebabc34cdbe80953381e9b6517978`. Its hosted run was **incomplete, exit 3**:
eighteen namespace subchecks lacked evidence and musl index was 8,361,968 bytes,
above the then-current strict 8,000,000-byte bound. On 2026-11-04 the owner raised
the musl index bound to 12,000,000 bytes; that measured size fits the new bound.
The historical run remains incomplete, and its namespace coverage gap is separate.

Linux ARM64 native discovery with Rust 1.99.0 and nextest 0.9.150 found:

| Configuration | Ordinary | Ignored |
| --- | ---: | ---: |
| Workspace default | 1,735 | 26 |
| Ingest PDF | 219 | 1 |
| Kaban no-default | 206 | 4 |
| Separate producer | 43 | 1 |

These suites overlap; counts are historical observations, not a maintained test
registry. Native runners discover new ordinary cases automatically. Eight distinct
compile-fail doctests passed (core 2, index 5, ingest 1); PDF repeated ingest's one
example. Other libraries and the producer reported zero examples. T2 metadata confirms
the producer has a library target; the earlier bin-only characterization was
incorrect. This
was discovery/doctest evidence, not ordinary runtime or artifact qualification.

All existing product assertions, Tantivy coverage and budgets remain:

| Check | Strict bound |
| --- | --- |
| Default GNU Kaban | <40 MiB |
| Optional no-default size table | <17 MiB |
| Graph on/off delta | <900,000 bytes |
| Native musl index | <12,000,000 bytes |

GNU index and musl Kaban sizes remain informational. Actual product-suite execution
and current producer/doctest baselines remain required before activation and composed
readiness; fixture dispatch checks do not qualify those suites. Weekly broad
qualification remains deferred. Release packaging and publication are described separately in [RELEASE.md](RELEASE.md); ordinary CI has no cross-run
artifact handoff.

## Build outputs and reuse

Each GNU/musl invocation creates `ci-artifacts.<random>` beneath
`KABAN_CI_ARTIFACT_DIR` when set, otherwise its effective `CARGO_TARGET_DIR` (default
`target`). The directory is private to that invocation;
`all` shares it across its GNU and musl groups. Each configuration has copied
binaries, SHA-256 sidecars and a `cargo-args.txt` recording the executable and each
Cargo argument on its own line. Paths and checksums are printed in the ordinary log.
Outputs are retained for inspection; no prior run's staged files are reused.
After inspection, these disposable `ci-artifacts.*` directories beneath the effective
target directory can be removed manually. Hosted jobs stage them outside cached targets;
local invocations add no automatic pruning.

```text
+-------------+  copy + hash =>  +------------------+  verify =>  +--------------+
| Cargo build | ==============> | staged binaries  | ==========> | smoke / size |
+-------------+                 +------------------+             +--------------+
```

| Configuration | Consumers / reuse |
| --- | --- |
| Workspace default | Default Kaban's <40 MiB guard and full GNU MCP smoke use its verified staged copy; workspace index is also saved. |
| GNU index package | Separate build retained: effective workspace/package equivalence has not been established. Its size stays informational. |
| Explicit graph-on | Kept distinct from workspace default; historical bytes differed. Paired with graph-off for the strict <900,000-byte delta. |
| Graph-off | Retains `--no-default-features --features ingest,text-index-tantivy`; Tantivy stays selected. |
| Optional no-default / md-splitter / PDF | Separate staged copies; no-default stays strictly <17 MiB, other table entries informational. |
| Native musl | Separate explicit-target Kaban/index build; index stays strictly <12,000,000 bytes. No musl runtime/smoke claim. |

Before each size/smoke consumer, the script checks that its staged binary exists,
is executable and matches its saved SHA-256. Missing or changed bytes fail the
selected group. After variant work, GNU restores the verified workspace-default
Kaban and index copies to their release paths. It copies each to a temporary sibling
and renames it into place, then compares the bytes. Replacing the path preserves
Cargo's hardlinked cached variant outputs; writing through an existing output inode
would corrupt those cached bytes. Executable permissions are preserved.
With the optional table enabled, this removes the two former default restore
builds; without the table it removes one. All distinct feature/target builds stay.

T3 fixture tests show the intended build calls and exact byte restoration, including
same-size but different configuration contents. Actual build logs/checksums during
C must establish real reuse and elapsed effects. No numeric speedup or Linux smoke
qualification is inferred here; missing applicable strace still prevents a full
smoke success claim.

## M1.5 retain/remove map and rollback

| Surface | Decision and maintenance removed |
| --- | --- |
| Mise settings/lock, Rust toolchains, Go/SCIP installers | Retained authoritative pins and verified acquisition. |
| `ci.sh` and product safety tests | Retained checks/bounds; required tools now fail before compilation. |
| `preflight.py` | Replaced deep admission with small version/path/target checks; no binary parsing, distribution hashing or host model. |
| `contract.py`, `verify.py`, `obligations.json`, `tool-pins.json` | Removed parser/catalogue, impact/result engine, stub runner and copied pin registry. |
| `test_ci_contract.py`, `test_ci_preflight.py` | Removed exclusive engine tests; useful public prerequisite/failure cases now exercise `ci.sh` in `test_ci_gates.py`. |
| Mise tasks | M1.5 removed unimplemented tasks; T2 supplies working `ci`, `core` and explicit group wrappers. |

There is one maintained comprehensive script. For rollback, inspect the pre-M1.5
PR #3 revision `869fe1e9c30c17fb76a02a3c8bf559c4c82eabcc` (PR base
`0557398e6917f254bfc545b72714f7d7f46ea346`) in a separately authorized checkout.
Restoring that revision restores its known failures and unimplemented interfaces;
it is not a route to qualification. No duplicate fallback engine is retained.

For the pre-T5 orchestration, inspect accepted T3 revision
`c2dd842f30e6924c4f4b40c51e077b161d7f110d` in a separately authorized checkout.
Its workflow retains the known Python provisioning mismatch and duplicate helpers;
rollback does not establish readiness. No second mutable comprehensive script is kept.
