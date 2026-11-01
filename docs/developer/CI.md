# CI checks

Use native Cargo commands for focused work and `bash scripts/ci/ci.sh` for the
existing comprehensive Rust gates. `mise run ci` wraps that same script. Group
commands and combined helper execution arrive in T2; producer qualification is
separate. No command here establishes release readiness by itself.

```text
+------------+  invoke =>  +----------------+  ready =>  +----------------+
| mise / CLI | =========> | ci.sh + tools  | =========> | existing gates |
+------------+            +----------------+            +----------------+
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

# Comprehensive Rust checks, including release builds and smoke:
bash scripts/ci/ci.sh
# Equivalent, with mise-provisioned tools on PATH:
mise run ci

# Small helper checks, also run separately by the existing CI workflow:
python3 -m unittest discover -s scripts/tests -v
(cd scripts/mcp && python3 -m unittest -v test_mcp_catalog)
find scripts hooks -name '*.sh' -print0 | xargs -0 -n1 bash -n
```

The comprehensive command retains fmt, all three Clippy configurations, workspace,
PDF and no-default nextest suites, deny, store/dependency boundaries, GNU build,
MCP smoke, graph-feature comparison and native musl sizes. It does not run the
standalone producer, ignored/manual real-tool and scale cases, or applicable
library doctests. T2 will add the explicit producer/doctest commands and extend
mandatory-marker handling beyond the current workspace nextest run. These are
known scope limits, not evidence that those checks passed.

`CARGO` is one executable path (spaces are supported), with no fallback if an
explicit path is invalid. `CARGO_TARGET_DIR` selects the output directory.
`CI_SKIP_SMOKE=1` skips only MCP smoke, keeps release/size work and forces exit 3.
`KABAN_SIZE_TABLE=1` adds the optional per-feature builds and no-default size gate;
leaving it unset does not make the run incomplete. Commands never clean outputs.

Exit 0 means the selected gates ran and passed. Exit 3 means required work is
incomplete: missing/wrong prerequisites, mandatory runtime skips, a skipped smoke
or a reported musl/graph size miss. Other child failures retain their status.
Missing nextest/deny stops before compilation; there is no Cargo-test fallback.
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

`ci.sh` calls the small `preflight.py` helper before compilation. It uses Python
3.11+ to read authoritative TOML, checks the exact pinned Python/Node/Cargo/Rust/
nextest/deny versions, probes fmt/Clippy, prints effective paths and checks presence
of Git, jq, cc, Go and scip-go. The comprehensive artifact path requires Linux,
a configured native musl linker and installed target libraries; smoke also needs
strace unless explicitly skipped. Use GNU coreutils/sed and Python with SQLite
for the actual gates. The helper is a prerequisite check, not host attestation
or proof of kernel namespace support. It does not install or repair tools.

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
M1.5 does not claim hosted readiness or relax the required versions to match an image.

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
example. Other libraries and the bin-only producer reported zero examples. This
was discovery/doctest evidence, not ordinary runtime or artifact qualification.

All existing product assertions, Tantivy coverage and budgets remain:

| Check | Strict bound |
| --- | --- |
| Default GNU Kaban | <25 MiB |
| Optional no-default size table | <17 MiB |
| Graph on/off delta | <900,000 bytes |
| Native musl index | <8,000,000 bytes |

GNU index and musl Kaban sizes remain informational. Existing rebuilds are retained
until T3 can establish equivalent outputs and reuse them safely. No speedup is
claimed by this reduction. Weekly broad qualification and release automation remain
deferred; there is no cross-run evidence handoff or new publication pipeline.

## M1.5 retain/remove map and rollback

| Surface | Decision and maintenance removed |
| --- | --- |
| Mise settings/lock, Rust toolchains, Go/SCIP installers | Retained authoritative pins and verified acquisition. |
| `ci.sh` and product safety tests | Retained checks/bounds; required tools now fail before compilation. |
| `preflight.py` | Replaced deep admission with small version/path/target checks; no binary parsing, distribution hashing or host model. |
| `contract.py`, `verify.py`, `obligations.json`, `tool-pins.json` | Removed parser/catalogue, impact/result engine, stub runner and copied pin registry. |
| `test_ci_contract.py`, `test_ci_preflight.py` | Removed exclusive engine tests; useful public prerequisite/failure cases now exercise `ci.sh` in `test_ci_gates.py`. |
| Mise focused/core/qualification/release tasks | Removed unimplemented tasks; one working `ci` task remains. Group names wait for T2. |

There is one maintained comprehensive script. For rollback, inspect the pre-M1.5
PR #3 revision `869fe1e9c30c17fb76a02a3c8bf559c4c82eabcc` (PR base
`0557398e6917f254bfc545b72714f7d7f46ea346`) in a separately authorized checkout.
Restoring that revision restores its known failures and unimplemented interfaces;
it is not a route to qualification. No duplicate fallback engine is retained.
