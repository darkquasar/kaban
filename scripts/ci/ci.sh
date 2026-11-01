#!/usr/bin/env bash
# Comprehensive Rust gates from the workspace root. See --help and docs/developer/CI.md.
# Exit 0: all selected gates passed; 3: incomplete; other: first failing child's status.
set -u
kaban_ci_dir=$(cd "$(dirname "$0")" && pwd -P) || exit
cd "$kaban_ci_dir/../.." || exit

if [ "$#" -eq 1 ] && [ "$1" = "--help" ]; then
  cat <<'HELP'
Usage: bash scripts/ci/ci.sh [--help]
Runs prerequisite checks, fmt, three Clippy variants, workspace/PDF/no-default
nextest suites, deny, dependency boundaries, release builds, MCP smoke and sizes.
Requires pinned tools (mise.toml / rust-toolchain.toml), Linux and native musl.
Missing/wrong required tools stop before compilation (exit 3); child failures retain
their status. Mandatory skipped work and size misses remain incomplete (exit 3).
CARGO=<executable> selects Cargo, including paths with spaces.
CI_SKIP_SMOKE=1 skips only smoke and forces incomplete; KABAN_SIZE_TABLE=1 adds
optional feature builds. CARGO_TARGET_DIR selects the build output directory.
Helper/catalog/shell checks run separately until T2; see docs/developer/CI.md.
Standalone producer qualification is a separate scope; this command does not run it.
HELP
  exit 0
fi
if [ "$#" -ne 0 ]; then
  echo "Usage: bash scripts/ci/ci.sh [--help]" >&2
  exit 2
fi
if ! command -v python3 >/dev/null 2>&1; then
  echo "not-run: python3 absent; install the Python version in mise.toml; exit 3" >&2
  exit 3
fi

# Hosted runners may pass non-CLOEXEC descriptors (including fd 142) to the job.
# Start the gates with only their standard streams; keep the parent's streams,
# environment and exit status, and leave the production Refuse policy intact.
kaban_ci_inherited=$(python3 -c '
import os
inherited = False
for name in os.listdir("/dev/fd"):
    fd = int(name)
    if fd > 2:
        try:
            inherited |= os.get_inheritable(fd)
        except OSError:
            pass
print(int(inherited))
') || exit
if [ "$kaban_ci_inherited" = 1 ]; then
  exec python3 -c '
import subprocess, sys
status = subprocess.call(["bash", *sys.argv[1:]], close_fds=True)
sys.exit(status if status >= 0 else 128 - status)
' "$kaban_ci_dir/ci.sh" "$@"
fi
kaban_target_dir=${CARGO_TARGET_DIR:-target}

# core-spec N1 (OWN-64, OWN-110, OWN-112) as amended by spec 05 OWN-B (2026-10-26, SS-13b): the default build
# (features `ingest`, `code-graph`, `text-index-tantivy`) stays under 25 MiB; the build with
# default features disabled still includes Tantivy and stays under 17 MiB (spec 05 OWN-Q),
# gated inside the size table below.
MAX_BINARY_BYTES=$((25 * 1024 * 1024))
MAX_NO_DEFAULT_BYTES=$((17 * 1024 * 1024))
missing=0

step() {
  echo "==> $*"
  "$@"
  local rc=$?
  if [ $rc -ne 0 ]; then
    echo "FAILED (exit $rc): $*" >&2
    exit $rc
  fi
}

if ! command -v "${CARGO-cargo}" >/dev/null 2>&1; then
  echo "not-run: CARGO=${CARGO-cargo} absent or not executable; select the pinned Cargo; exit 3" >&2
  exit 3
fi
# CI colours human-facing output; discovery alone needs plain command names.
kaban_cargo_commands=$(CARGO_TERM_COLOR=never RUSTUP_AUTO_INSTALL=0 "${CARGO:-cargo}" --list)
kaban_discovery_status=$?
if [ "$kaban_discovery_status" -ne 0 ]; then
  printf '%s\n' "$kaban_cargo_commands"
  echo "FAILED (exit $kaban_discovery_status): Cargo command discovery" >&2
  exit "$kaban_discovery_status"
fi
for tool in nextest deny; do
  if ! printf '%s\n' "$kaban_cargo_commands" | awk '{print $1}' | grep -qx "$tool"; then
    echo "not-run: cargo-$tool absent; install the version pinned in mise.toml" >&2
    echo "RESULT: incomplete (required tool absent); exit 3" >&2
    exit 3
  fi
done

step python3 "$kaban_ci_dir/preflight.py"

step "${CARGO:-cargo}" fmt --all --check
step "${CARGO:-cargo}" clippy --workspace --all-targets --all-features -- -D warnings
# the build without docs extractors (`--no-default-features`, the N1 size-table configuration
# below) stays lint-clean too (docs-ingest-plan.md §16 item 3 (d), DI2.0): the `cfg(not(feature
# = "ingest"))` arms are compiled only here
step "${CARGO:-cargo}" clippy -p kaban --no-default-features --all-targets -- -D warnings
step "${CARGO:-cargo}" clippy -p kaban-ingest --all-targets --no-default-features -- -D warnings

# `mandatory_skips` (stage1-plan CG10.3, RV-10): the one workspace nextest run is captured so the
# tests that could not run on this host are counted by name. The tree prints `NOT RUN: <test>:
# <reason>` from a passing test (an indexer or an isolation tier absent), and nextest hides a
# passing test's output unless `--success-output immediate`, so without it the count is always 0.
nextest_log="$(mktemp)"
nextest_workspace() {
  "${CARGO:-cargo}" nextest run --workspace --no-fail-fast --success-output immediate 2>&1 | tee "$nextest_log"
  return "${PIPESTATUS[0]}"
}
mandatory_skips() {
  local names count
  names=$(grep -E 'NOT RUN:|SKIPPED\(mandatory\)' "$nextest_log" \
    | sed -E 's/^.*NOT RUN: ([A-Za-z0-9_]+):.*$/\1/; t; s/^.*[^A-Za-z0-9_]([A-Za-z0-9_]+) NOT RUN:.*$/\1/; t; s/^[[:space:]]+//' \
    | sort -u)
  count=$(printf '%s\n' "$names" | grep -c .)
  echo "mandatory_skips: ${count}"
  if [ "$count" -gt 0 ]; then
    printf '%s\n' "$names" | sed 's/^/  /'
    echo "stage 1 is not complete while mandatory_skips is non-zero (stage1-plan CG10.3); exit 3"
    missing=1
  fi
  return 0
}

step nextest_workspace
mandatory_skips
# `pdf` is opt-in for 3.0.0 (OWN-70, DI2.1b): the workspace run above skips the `golden_pdf`
# binary (`required-features = ["pdf"]`), so the PDF tests stay in the gate through this one
# feature-enabled run of the ingest crate
step "${CARGO:-cargo}" nextest run -p kaban-ingest --features pdf
# the feature-off binary (no `ingest`, no `code-graph`) keeps its own test set green
# (stage1-plan CG7.3): the ingest and code-graph test binaries are `cfg`-gated out, the
# catalog test subtracts the gated tools, and nothing else may depend on a feature
step "${CARGO:-cargo}" nextest run -p kaban --no-default-features

step "${CARGO:-cargo}" deny check

# store-boundary dependency check (engine-spec EN-E10a hygiene, engine-plan E1.0, SO-5):
#   kaban-api and kaban-ingest declare no direct normal `rusqlite` dependency (dev-only is fine),
#   and the inverse normal tree of rusqlite under kaban-api is empty (today) or names
#   kaban-codegraph as the only direct parent (after v4 CG5.2). Any other parent fails.
store_boundary_deps() {
  local meta direct parents
  meta=$("${CARGO:-cargo}" metadata --format-version 1 --no-deps) || return 1
  direct=$(printf '%s' "$meta" | jq -r '.packages[]
    | select(.name == "kaban-api" or .name == "kaban-ingest")
    | .name as $pkg | .dependencies[]
    | select(.name == "rusqlite" and .kind == null)
    | "\($pkg) -> rusqlite (normal)"') || return 1
  if [ -n "$direct" ]; then
    echo "FAILED: direct rusqlite dependency outside the store crate:" >&2
    echo "$direct" >&2
    return 1
  fi
  # inverse tree: depth 0 is rusqlite itself, depth 1 its direct parents; "nothing to print"
  # (rusqlite dev-only) and "did not match any packages" (rusqlite absent) are both the empty tree
  local tree rc
  tree=$("${CARGO:-cargo}" tree -e normal -p kaban-api -i rusqlite --prefix depth 2>&1)
  rc=$?
  if [ $rc -ne 0 ] && ! printf '%s' "$tree" | grep -q 'did not match any packages'; then
    echo "FAILED: cargo tree -i rusqlite -p kaban-api exited $rc:" >&2
    echo "$tree" >&2
    return 1
  fi
  parents=$(printf '%s\n' "$tree" | awk '/^1[^0-9]/ { sub(/^1/, ""); print $1 }' | sort -u)
  if [ -n "$parents" ] && [ "$parents" != "kaban-codegraph" ]; then
    echo "FAILED: rusqlite reaches kaban-api through a parent other than kaban-codegraph:" >&2
    echo "$parents" >&2
    return 1
  fi
  echo "store boundary: no direct rusqlite in kaban-api/kaban-ingest; kaban-api inverse tree parents: ${parents:-none}"
}
if command -v jq >/dev/null 2>&1; then
  step store_boundary_deps
else
  echo "not-run: jq absent (store-boundary dependency check)"
  missing=1
fi

# v4 dependency allowlist (stage1-spec G1-E3, cg-join-v1 §2; stage1-plan CG2.4; after the
# engine's store_boundary_deps step per the SO-12 allocation table): no forbidden
# crate edge for kaban-codegraph / kaban-index, store -> codegraph dev-only, core never -> codegraph
if command -v jq >/dev/null 2>&1; then
  step env CARGO="${CARGO:-cargo}" bash scripts/ci/deps_allowlist.sh
else
  echo "not-run: jq absent (dependency allowlist)"
  missing=1
fi

# release build and size gate are mandatory (core-spec N1); only the smoke can be skipped
step "${CARGO:-cargo}" build --release
size=$(stat -c '%s' "${kaban_target_dir}/release/kaban")
echo "release binary: ${size} bytes"
if [ "$size" -ge "$MAX_BINARY_BYTES" ]; then
  echo "FAILED: binary size ${size} >= ${MAX_BINARY_BYTES} (N1)" >&2
  exit 1
fi
if [ "${CI_SKIP_SMOKE:-0}" = "1" ]; then
  echo "not-run: MCP smoke skipped by CI_SKIP_SMOKE=1"
  missing=1
elif command -v python3 >/dev/null 2>&1; then
  smoke_db="$(mktemp -d)/smoke.db"
  step python3 scripts/mcp/mcp_smoke.py "${kaban_target_dir}/release/kaban" "$smoke_db" examples/policy.yaml
else
  echo "not-run: python3 absent (smoke)"
  missing=1
fi

# v4 binary sizes (stage1-spec G1-N4; stage1-plan CG2.4, CG7.3, §18). Enforced from CG7.3:
# `kaban(code-graph on) - kaban(code-graph off) < 900,000 B` (OWN-86 (4)), where "off" is
# `--no-default-features --features ingest,text-index-tantivy` (the one feature toggled; the docs
# extractors and the Tantivy adapter stay in, so the pair measures the code-graph reader delta and
# not the ingest crate's or Tantivy's), and the
# default `kaban` build stays under the 25 MiB N1 gate already applied above (OWN-64, OWN-112, OWN-B; the plan's
# 12,000,000 B figure predates the docs extractors). `kaban-index` is printed for the GNU build
# here and gated on the musl release in `musl_release` below (`< 8,000,000 B`, G1-N4, owner decision 2026-10-26). A missing
# crate prints `skipped: ...`. The pair rebuilds the default kaban binary last so
# target/release/kaban stays the default build.
MAX_CODE_GRAPH_DELTA_BYTES=900000
binary_sizes() {
  local size on off delta
  size=$(stat -c '%s' "${kaban_target_dir}/release/kaban") || return 1
  echo "size: kaban ${size} (gate: < ${MAX_BINARY_BYTES}, applied above)"
  if [ -f crates/kaban-index/Cargo.toml ]; then
    "${CARGO:-cargo}" build --release -p kaban-index || return 1
    size=$(stat -c '%s' "${kaban_target_dir}/release/kaban-index") || return 1
    echo "size: kaban-index ${size} (gnu, informational; the musl release below carries the G1-N4 gate)"
  else
    echo "skipped: kaban-index not yet present"
  fi
  if grep -Eq '^code-graph *=' crates/kaban/Cargo.toml; then
    "${CARGO:-cargo}" build --release -p kaban --features code-graph || return 1
    on=$(stat -c '%s' "${kaban_target_dir}/release/kaban") || return 1
    echo "size: kaban(code-graph on) ${on}"
    "${CARGO:-cargo}" build --release -p kaban --no-default-features --features ingest,text-index-tantivy || return 1
    off=$(stat -c '%s' "${kaban_target_dir}/release/kaban") || return 1
    echo "size: kaban(code-graph off) ${off}"
    delta=$((on - off))
    echo "size: kaban(code-graph on) - kaban(code-graph off) ${delta} (gate: < ${MAX_CODE_GRAPH_DELTA_BYTES})"
    "${CARGO:-cargo}" build --release || return 1
    if [ "$delta" -ge "$MAX_CODE_GRAPH_DELTA_BYTES" ]; then
      # a measured miss is reported, never worked around (OWN-13): the figure goes to PROGRESS and the owner
      # decides between the budget and a size lane; the stage is not complete until then (exit 3)
      echo "N4 MISS (reported, OWN-13; budget set by OWN-86 (4)): code-graph size delta ${delta} >= ${MAX_CODE_GRAPH_DELTA_BYTES} (G1-N4)" >&2
      missing=1
    fi
  else
    echo "skipped: kaban code-graph on/off pair not yet present"
  fi
}
step binary_sizes

# musl release of both binaries (stage1-spec §8.4, G1-N4; stage1-plan CG10.3): the host's musl
# triple only (`.cargo/config.toml` names `musl-gcc` as the linker of both musl targets, and a
# box has the linker of its own architecture; an installed foreign target has std but no linker,
# so the installed list is never iterated). The target is installed when `rustup target list
# --installed` names it, or, without rustup on PATH, when the sysroot has `lib/rustlib/<triple>`.
# `kaban-index` musl `< 8,000,000 B` is binding (owner decision 2026-10-26) (N4 is a musl budget); the `kaban` musl size is
# informational (N1 is the default gnu gate above). An absent target prints `not-run: musl target
# absent` and the stage is not complete (exit 3).
MAX_INDEX_MUSL_BYTES=8000000
musl_target_installed() {
  if command -v rustup >/dev/null 2>&1; then
    rustup target list --installed 2>/dev/null | grep -qx "$1"
  else
    [ -d "$(rustc --print sysroot 2>/dev/null)/lib/rustlib/$1" ]
  fi
}
musl_release() {
  local t size_index size_kaban
  t="$(uname -m)-unknown-linux-musl"
  if ! musl_target_installed "$t"; then
    echo "not-run: musl target absent (${t}; the G1-N4 budget is a musl budget, so the stage is not complete)"
    missing=1
    return 0
  fi
  "${CARGO:-cargo}" build --release --target "$t" -p kaban -p kaban-index || return 1
  size_index=$(stat -c '%s' "${kaban_target_dir}/$t/release/kaban-index") || return 1
  size_kaban=$(stat -c '%s' "${kaban_target_dir}/$t/release/kaban") || return 1
  echo "size: kaban-index (musl ${t}) ${size_index} (gate: < ${MAX_INDEX_MUSL_BYTES}, G1-N4)"
  echo "size: kaban (musl ${t}) ${size_kaban} (informational; N1 is the default gnu gate above)"
  if [ "$size_index" -ge "$MAX_INDEX_MUSL_BYTES" ]; then
    # a miss is reported, never worked around (OWN-13): the figure goes to PROGRESS and the owner
    # decides between the budget and a size lane; the stage is not complete until then (exit 3)
    echo "N4 MISS (reported, OWN-13; budget set by owner decision 2026-10-26): kaban-index musl size ${size_index} >= ${MAX_INDEX_MUSL_BYTES} (G1-N4)" >&2
    missing=1
  fi
}
step musl_release

# per-feature size table (core-spec N1, OWN-110, spec 05 OWN-Q): `--no-default-features` gated at 17 MiB,
# `kaban-ingest/md-splitter` and `kaban-ingest/pdf` informational (icu segmenter data; `pdf_oxide`,
# OWN-70); the default build is rebuilt last so target/release/kaban stays the default build.
# Three extra release builds, so it runs
# only with KABAN_SIZE_TABLE=1 (coordinator, lane and milestone boundaries); skipping it does
# not mark the run incomplete.
size_table() {
  local size
  echo "size-table: kaban(default) ${1} (gate: < ${MAX_BINARY_BYTES})"
  "${CARGO:-cargo}" build --release -p kaban --no-default-features || return 1
  size=$(stat -c '%s' "${kaban_target_dir}/release/kaban") || return 1
  echo "size-table: kaban(--no-default-features) ${size} (gate: < ${MAX_NO_DEFAULT_BYTES})"
  if [ "$size" -ge "$MAX_NO_DEFAULT_BYTES" ]; then
    echo "FAILED: --no-default-features binary size ${size} >= ${MAX_NO_DEFAULT_BYTES} (N1)" >&2
    return 1
  fi
  if grep -Eq '^md-splitter *=' crates/kaban-ingest/Cargo.toml; then
    "${CARGO:-cargo}" build --release -p kaban --features kaban-ingest/md-splitter || return 1
    size=$(stat -c '%s' "${kaban_target_dir}/release/kaban") || return 1
    echo "size-table: kaban(md-splitter) ${size} (gate: informational)"
  else
    echo "skipped: md-splitter feature not present"
  fi
  # `pdf` is opt-in for 3.0.0 (OWN-70, DI2.1b): the PDF-capable build is measured here and
  # printed informationally; the N1 gates above are unchanged
  if grep -Eq '^pdf *=' crates/kaban-ingest/Cargo.toml; then
    "${CARGO:-cargo}" build --release -p kaban --features kaban-ingest/pdf || return 1
    size=$(stat -c '%s' "${kaban_target_dir}/release/kaban") || return 1
    echo "size-table: kaban(--features kaban-ingest/pdf) ${size} (gate: informational)"
  else
    echo "skipped: pdf feature not present"
  fi
  "${CARGO:-cargo}" build --release || return 1
  size=$(stat -c '%s' "${kaban_target_dir}/release/kaban") || return 1
  if [ "$size" != "$1" ]; then
    echo "FAILED: default rebuild size ${size} differs from the gated build ${1}" >&2
    return 1
  fi
}
if [ "${KABAN_SIZE_TABLE:-0}" = "1" ]; then
  step size_table "$size"
else
  echo "skipped: size table (KABAN_SIZE_TABLE=1 enables it)"
fi

if [ $missing -ne 0 ]; then
  echo "RESULT: incomplete (a gate was skipped or a measured G1-N4 miss was reported: CI_SKIP_SMOKE=1, musl target absent, mandatory_skips non-zero, or an N4 MISS line above); exit 3"
  exit 3
fi
echo "RESULT: all gates passed"
