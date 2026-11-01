# CI checks

Use native Cargo commands for focused work, `mise run core` for quality, helpers
and functional checks, and `bash scripts/ci/ci.sh` (or `mise run ci`) for comprehensive
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

# Local core: quality + helpers + functional, stopping on failure.
mise run core
# Without mise activation, invoke those three groups in sequence:
bash scripts/ci/ci.sh quality
bash scripts/ci/ci.sh helpers
bash scripts/ci/ci.sh functional

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
| `functional` | Workspace, ingest PDF and Kaban no-default nextest; library doctests | Producer, artifacts and ignored/manual cases |
| `producer` | Locked fetch, fmt, locked workspace Clippy/tests, applicable doctests in tools/kaban-rust-scip | Root workspace and artifacts |
| `gnu` | Default GNU release, MCP smoke, graph delta, optional size table | Functional tests, musl and producer |
| `musl` | Native musl Kaban/index release and index size guard | GNU smoke, functional tests and producer |
| `all` / `ci` | quality + helpers + functional + gnu + musl | Standalone producer and ignored/manual cases |

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
also runs separately with `--features pdf`. Kaban is bin-only, so the no-default Kaban
suite adds no library doctest. Libraries without examples honestly report zero
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
and jq; functional adds nextest, pinned Node, Go/scip-go and cc. Producer probes
its own toolchain and components, without nextest/deny or musl. Helpers need Python 3.11+
with SQLite and Bash; they report an actionable exit 3 for an older Python
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

The workflow has not yet migrated to mise provisioning. Its image/tool versions
may fail these explicit pins until T5 updates setup. Such failure is visible;
T2 does not claim hosted readiness or relax the required versions to match an image.
The existing helper job still invokes its equivalent native checks. Until T5
switches the gates job from `all` to independent groups, helpers also run there
when prerequisite checks pass. This temporary duplication is a T5 migration input;
switching the helper job to the group command belongs to that workflow scope.

## Preserved baseline and bounds

Historical measured source: `44286bb81e7d6338983f65c79bb8a68cf929c9ef`, tree
`30e65982e4eebabc34cdbe80953381e9b6517978`. Its hosted run was **incomplete, exit 3**:
eighteen namespace subchecks lacked evidence and musl index was 8,361,968 bytes,
above the strict 8,000,000-byte bound. These remain unresolved qualification and
size blockers; removing the catalogue does not resolve them.

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
| Default GNU Kaban | <25 MiB |
| Optional no-default size table | <17 MiB |
| Graph on/off delta | <900,000 bytes |
| Native musl index | <8,000,000 bytes |

GNU index and musl Kaban sizes remain informational. Actual product-suite execution
and new producer/doctest baselines are deferred to composed verification C; fixture
dispatch checks do not qualify those suites. Weekly broad qualification and release
automation remain deferred; there is no cross-run evidence handoff or new publication
pipeline.

## Build outputs and reuse

Each GNU/musl invocation creates `ci-artifacts.<random>` beneath its effective
`CARGO_TARGET_DIR` (default `target`). The directory is private to that invocation;
`all` shares it across its GNU and musl groups. Each configuration has copied
binaries, SHA-256 sidecars and a `cargo-args.txt` recording the executable and each
Cargo argument on its own line. Paths and checksums are printed in the ordinary log.
Outputs are retained for inspection; no prior run's staged files are reused.
After inspection, these disposable `ci-artifacts.*` directories beneath the effective
target directory can be removed manually. T5 cache configuration must exclude them;
this task adds no automatic pruning or cache changes.

```text
+-------------+  copy + hash =>  +------------------+  verify =>  +--------------+
| Cargo build | ==============> | staged binaries  | ==========> | smoke / size |
+-------------+                 +------------------+             +--------------+
```

| Configuration | Consumers / reuse |
| --- | --- |
| Workspace default | Default Kaban's <25 MiB guard and full GNU MCP smoke use its verified staged copy; workspace index is also saved. |
| GNU index package | Separate build retained: effective workspace/package equivalence has not been established. Its size stays informational. |
| Explicit graph-on | Kept distinct from workspace default; historical bytes differed. Paired with graph-off for the strict <900,000-byte delta. |
| Graph-off | Retains `--no-default-features --features ingest,text-index-tantivy`; Tantivy stays selected. |
| Optional no-default / md-splitter / PDF | Separate staged copies; no-default stays strictly <17 MiB, other table entries informational. |
| Native musl | Separate explicit-target Kaban/index build; index stays strictly <8,000,000 bytes. No musl runtime/smoke claim. |

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
