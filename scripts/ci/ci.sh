#!/usr/bin/env bash
# Comprehensive Rust gates from the workspace root. See --help and docs/developer/CI.md.
# Exit 0: all selected gates passed; 3: incomplete; other: first failing child's status.
set -u
kaban_ci_dir=$(cd "$(dirname "$0")" && pwd -P) || exit
cd "$kaban_ci_dir/../.." || exit

if [ "$#" -eq 1 ] && [ "$1" = "--help" ]; then
  cat <<'HELP'
Usage: bash scripts/ci/ci.sh [all|quality|helpers|functional|pdf|macos|producer|gnu|musl]
No argument means all: quality + helpers + functional + PDF + GNU + musl gates.
quality: fmt, three Clippy variants, deny and dependency boundaries.
helpers: Python build/catalog tests and shell syntax (no Rust tools needed).
functional: workspace/no-default nextest suites and applicable default library doctests.
pdf: full PDF-enabled ingest nextest suite and its library doctest.
macos: focused native ARM64 boundaries, full-feature ingest and public app paths.
KABAN_MACOS_PRODUCER_QUALIFY=1 adds Darwin producer qualification; default 0.
producer: separate tools/kaban-rust-scip workspace/toolchain, fetch/fmt/Clippy/tests.
gnu: release build, MCP smoke and sizes; musl: native musl build/size gates.
Local core is quality + helpers + functional + PDF (mise run core).
Producer qualification is separate: invoke producer explicitly when applicable.
Missing/wrong required tools stop before compilation (exit 3); child failures retain
their status. Mandatory runtime markers and size misses remain incomplete (exit 3).
CARGO=<executable> selects Cargo, including paths with spaces; no toolchain override.
CARGO_TARGET_DIR selects build output; producer uses its private producer/ subdirectory.
KABAN_CI_ARTIFACT_DIR optionally stages run-local copies outside the cached target.
KABAN_CI_ISOLATION_SPLIT=1 separates refusal tests using the same compiled binaries;
requires a disposable GitHub-hosted runner and restores its namespace policy afterward.
CI_SKIP_SMOKE=1 skips only smoke and forces GNU/all incomplete; KABAN_SIZE_TABLE=1
adds optional GNU feature builds. Artifact groups require Linux and GNU utilities.
HELP
  exit 0
fi
if [ "$#" -gt 1 ]; then
  echo "Expected one group; see --help" >&2
  exit 2
fi
kaban_group=${1:-all}
case "$kaban_group" in
  all|quality|helpers|functional|pdf|macos|producer|gnu|musl) ;;
  *) echo "Unknown group: $kaban_group; see --help" >&2; exit 2 ;;
esac
# Native hosted step summaries retain incomplete status even though Actions marks
# every nonzero child as failure. Preserve the original exit if summary I/O fails.
ci_summary() {
  local rc=$?
  if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
    local verdict=FAILED
    case "$rc" in 0) verdict=PASS ;; 3) verdict=INCOMPLETE ;; esac
    printf 'Group `%s`: %s (exit %s)\n' "$kaban_group" "$verdict" "$rc" >> "$GITHUB_STEP_SUMMARY" || :
  fi
}
trap ci_summary EXIT

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
# (features `ingest`, `code-graph`, `text-index-tantivy`) stays under 40 MiB; the build with
# default features disabled still includes Tantivy and stays under 17 MiB (spec 05 OWN-Q),
# gated inside the size table below.
MAX_BINARY_BYTES=$((40 * 1024 * 1024))
MAX_NO_DEFAULT_BYTES=$((17 * 1024 * 1024))
missing=0

step() {
  local started=$SECONDS
  echo "==> $*"
  "$@"
  local rc=$?
  if [ "$kaban_group" = macos ]; then
    echo "macos phase elapsed: $((SECONDS - started))s; exit $rc: $*"
  fi
  if [ $rc -ne 0 ]; then
    echo "FAILED (exit $rc): $*" >&2
    exit $rc
  fi
}

# Resolve an explicit executable before the producer changes working directory.
if [ "$kaban_group" != helpers ]; then
  if ! CARGO=$(command -v "${CARGO-cargo}"); then
    echo "not-run: CARGO absent or not executable; select the pinned Cargo; exit 3" >&2
    exit 3
  fi
  case "$CARGO" in /*) ;; *) CARGO="$PWD/$CARGO" ;; esac
  export CARGO
fi

prerequisites() {
  # Share the existing actionable probe helpers; choose only this group's tools.
  PYTHONDONTWRITEBYTECODE=1 python3 - "$kaban_ci_dir" "$kaban_group" <<'CHECK'
import os
from pathlib import Path
import sys
sys.path.insert(0, sys.argv[1])
from preflight import executable, incomplete, probe, main
try:
    group = sys.argv[2]
    if sys.version_info < (3, 11):
        incomplete("Python 3.11+ needed for CI helpers/TOML pins; install Python from mise.toml")
    if group == "all":
        cargo = executable(os.environ["CARGO"])
        commands = probe([cargo, "--list"])
        for tool in ("nextest", "deny"):
            if tool not in [line.split()[0] for line in commands.splitlines() if line.split()]:
                incomplete(f"cargo-{tool} absent; install the version pinned in mise.toml")
        main()
    elif group != "helpers":
        import tomllib
        root = Path(sys.argv[1]).parents[1]
        pins = tomllib.loads((root / "mise.toml").read_text())["tools"]
        rust = tomllib.loads(Path("rust-toolchain.toml").read_text())["toolchain"]["channel"]
        cargo = executable(os.environ["CARGO"])
        probe([cargo, "--version"], f"cargo {rust}")
        rustc = executable(os.environ.get("RUSTC", "rustc"))
        probe([rustc, "--version"], f"rustc {rust}")
        probe([sys.executable, "--version"], f"Python {pins['python']}")
        if group == "macos":
            import shutil
            if probe(["uname", "-s"]) != "Darwin" or probe(["uname", "-m"]) != "arm64":
                incomplete("macos requires native Darwin ARM64; no cross-build qualification")
            if "host: aarch64-apple-darwin" not in probe([rustc, "-vV"]).splitlines():
                incomplete("macos requires the native aarch64-apple-darwin Rust host")
            if shutil.disk_usage(root).free < 2_000_000_000:
                incomplete("macos requires at least 2,000,000,000 free bytes")
            if os.environ.get("KABAN_MACOS_PRODUCER_QUALIFY", "0") not in ("0", "1"):
                incomplete("KABAN_MACOS_PRODUCER_QUALIFY must be 0 or 1")
            if os.environ.get("KABAN_MACOS_PRODUCER_QUALIFY", "0") == "1":
                producer_rust = tomllib.loads((root / "tools/kaban-rust-scip/rust-toolchain.toml").read_text())["toolchain"]["channel"]
                rustup = executable("rustup")
                toolchain = Path(probe([rustup, "run", producer_rust, "rustc", "--print", "sysroot"]))
                for resource in ("lib/rustlib/src/rust/library/Cargo.toml", "libexec/rust-analyzer-proc-macro-srv"):
                    if not (toolchain / resource).is_file():
                        incomplete(f"producer resource absent: {toolchain / resource}; install pinned rust-src/toolchain")
        if group in ("quality", "functional", "pdf", "macos"):
            commands = probe([cargo, "--list"])
            tool = "deny" if group == "quality" else "nextest"
            if tool not in [line.split()[0] for line in commands.splitlines() if line.split()]:
                incomplete(f"cargo-{tool} absent; install the version pinned in mise.toml")
            key = "aqua:EmbarkStudios/cargo-deny" if tool == "deny" else "aqua:nextest-rs/nextest/cargo-nextest"
            probe([cargo, tool, "--version"], f"cargo-{tool} {pins[key]}")
        if group in ("quality", "producer"):
            probe([cargo, "fmt", "--version"])
            probe([cargo, "clippy", "--version"])
        if group == "quality":
            executable("jq")
        if group in ("functional", "gnu", "macos"):
            probe([executable("node"), "--version"], f"v{pins['node']}")
            if group != "macos":
                for tool in ("go", os.environ.get("SCIP_GO", "scip-go")):
                    executable(tool)
        if group in ("functional", "pdf", "producer", "gnu", "musl", "macos"):
            executable("cc")
        executable("git")
        if group in ("gnu", "musl"):
            if probe(["uname", "-s"]) != "Linux":
                incomplete("artifact gates require native Linux and GNU utilities")
        if group == "gnu" and os.environ.get("CI_SKIP_SMOKE") != "1":
            executable("strace")
        if group == "musl":
            target = probe(["uname", "-m"]) + "-unknown-linux-musl"
            config = tomllib.loads((root / ".cargo/config.toml").read_text())
            linker = os.environ.get("CARGO_TARGET_" + target.upper().replace("-", "_") + "_LINKER",
                                    config.get("target", {}).get(target, {}).get("linker"))
            if not linker:
                incomplete(f"no configured musl linker for {target}")
            executable(linker)
            libdir = probe([rustc, "--print", "target-libdir", "--target", target])
            if not libdir or not Path(libdir).is_dir():
                incomplete(f"musl target absent ({target}); use rustup target add")
except (OSError, ValueError, KeyError) as error:
    incomplete(str(error))
CHECK
}

# One runtime marker handler for nextest, Cargo tests and doctests.
rust_runner() {
  local log rc tee_rc names count
  log=$(mktemp) || return 1
  "$@" 2>&1 | tee "$log"
  local statuses=("${PIPESTATUS[@]}")
  rc=${statuses[0]}
  tee_rc=${statuses[1]}
  if [ "$rc" -eq 0 ] && [ "$tee_rc" -ne 0 ]; then rc=$tee_rc; fi
  names=$(grep -E 'NOT RUN:|SKIPPED\(mandatory\)' "$log" \
    | sed -E -e 's/^.*NOT RUN: ([A-Za-z0-9_]+):.*$/\1/' -e t \
      -e 's/^.*[^A-Za-z0-9_]([A-Za-z0-9_]+) NOT RUN:.*$/\1/' -e t -e 's/^[[:space:]]+//' \
    | sort -u)
  count=$(printf '%s\n' "$names" | awk 'NF { n++ } END { print n+0 }') || return $?
  echo "mandatory_skips: $count"
  if [ "$count" -gt 0 ]; then
    printf '%s\n' "$names" | sed 's/^/  /'
    echo "selected scope incomplete while mandatory_skips is non-zero; exit 3"
    missing=1
  fi
  rm -f "$log"
  return "$rc"
}

# Cargo metadata supplies actual workspace library targets, including doctest=false.
doctests() {
  local metadata packages package
  local selections=()
  metadata=$(mktemp) || return 1
  "${CARGO:-cargo}" metadata --format-version 1 --no-deps > "$metadata"
  local rc=$?
  if [ "$rc" -ne 0 ]; then rm -f "$metadata"; return "$rc"; fi
  packages=$(python3 - "$metadata" <<'LIBRARIES'
import json, sys
with open(sys.argv[1]) as stream:
    metadata = json.load(stream)
for package in metadata["packages"]:
    if package["id"] in metadata["workspace_members"] and any(
        target.get("doctest", True) and any(kind in ("lib", "proc-macro", "rlib") for kind in target["kind"])
        for target in package["targets"]
    ):
        print(package["name"])
LIBRARIES
  )
  rc=$?
  rm -f "$metadata"
  [ "$rc" -eq 0 ] || return "$rc"
  if [ -z "$packages" ]; then echo "doctests: no doctestable library targets"; fi
  for package in $packages; do
    selections+=( -p "$package" )
  done
  if [ "${#selections[@]}" -gt 0 ]; then
    step rust_runner "${CARGO:-cargo}" test --doc --locked "${selections[@]}" -- --show-output
  fi
}

# Only the disposable hosted job may change its kernel policy. Both selections
# reuse nextest's build metadata; local functional runs retain their native policy.
hosted_isolation_tests() (
  set -e
  if [ "${GITHUB_ACTIONS:-}" != true ] || [ "${RUNNER_ENVIRONMENT:-}" != github-hosted ]; then
    echo "isolation split requires a disposable GitHub-hosted runner" >&2
    exit 2
  fi
  key=kernel.apparmor_restrict_unprivileged_userns
  original=$(sysctl -n "$key")
  metadata=$(mktemp)
  restore_isolation_policy() {
    rc=$?
    trap - EXIT
    if ! sudo -n sysctl -q -w "$key=$original"; then
      echo "failed to restore original namespace policy" >&2
      [ "$rc" -ne 0 ] || rc=1
    fi
    rm -f "$metadata"
    exit "$rc"
  }
  trap restore_isolation_policy EXIT

  # Refuse an unsuitable runner before spending time compiling.
  sudo -n sysctl -q -w "$key=0"
  unshare --user --map-root-user --net --mount --pid --fork true
  sudo -n sysctl -q -w "$key=1"
  if unshare --user --map-root-user --net --mount --pid --fork true; then
    echo "namespace restriction did not produce the unavailable context" >&2
    exit 1
  fi
  "$CARGO" nextest list --workspace --locked --list-type binaries-only --message-format json > "$metadata"
  refusal='binary_id(kaban-index::cli) & (test(=cli_index_required_without_isolating_tier_is_exit_8) | test(=rust_required_unavailable_never_runs_native_producer_probes))'
  echo "isolation: unavailable; run only the two refusal tests from existing binaries"
  refusal_rc=0
  "$CARGO" nextest run --binaries-metadata "$metadata" -E "$refusal" --no-fail-fast --success-output immediate || refusal_rc=$?
  sudo -n sysctl -q -w "$key=0"
  echo "isolation: available; run the remaining workspace tests from existing binaries"
  capable_rc=0
  "$CARGO" nextest run --binaries-metadata "$metadata" -E "not ($refusal)" --no-fail-fast --success-output immediate || capable_rc=$?
  [ "$refusal_rc" -eq 0 ] || exit "$refusal_rc"
  exit "$capable_rc"
)

functional() {
  if [ "${KABAN_CI_ISOLATION_SPLIT:-0}" = 1 ]; then
    step rust_runner hosted_isolation_tests
  else
    step rust_runner "${CARGO:-cargo}" nextest run --workspace --no-fail-fast --success-output immediate
  fi
  step rust_runner "${CARGO:-cargo}" nextest run -p kaban --no-default-features --success-output immediate
  step doctests
}

pdf() {
  step rust_runner "${CARGO:-cargo}" nextest run -p kaban-ingest --features pdf --success-output immediate
  step rust_runner "${CARGO:-cargo}" test --doc --locked -p kaban-ingest --features pdf -- --show-output
}

# Native boundary binaries plus representative public paths already qualified in
# M4/M5. Linux owns the complete generic/feature/namespace matrices.
macos() {
  local selection
  selection='binary_id(=kaban-core::process) | binary_id(=kaban-codegraph::open) | binary_id(=kaban-codegraph::discover) | binary_id(=kaban-index::writer) | binary_id(=kaban-index::runner) | binary_id(=kaban-index::trust)'
  selection="$selection | (binary_id(=kaban-index::cli) & (test(=cli_index_required_without_isolating_tier_is_exit_8) | test(=rust_required_unavailable_never_runs_native_producer_probes) | test(=trust_revalidated_before_each_spawn) | test(=indexer_replaced_after_admission_is_refused) | test(=checkout_replaced_between_revalidation_and_spawn_is_refused)))"
  selection="$selection | (package(kaban-ingest) & (binary(golden_markdown) | binary(chunk) | binary(golden_pdf)))"
  selection="$selection | (package(kaban) & (binary(walkthrough) | binary(cli_sync) | binary(recall_determinism) | binary(code_smoke) | binary(mcp_control) | binary(mcp_scrub) | binary(mcp_docs) | binary(hook_hosts) | binary(hooks_roundtrip)))"
  step rust_runner "${CARGO:-cargo}" nextest run --locked \
    -p kaban-core -p kaban-codegraph -p kaban-index -p kaban-ingest -p kaban \
    --test process --test open --test discover --test writer --test runner --test trust --test cli \
    --test golden_markdown --test chunk --test golden_pdf --test walkthrough --test cli_sync \
    --test recall_determinism --test code_smoke --test mcp_control --test mcp_scrub --test mcp_docs \
    --test hook_hosts --test hooks_roundtrip \
    --features kaban-ingest/pdf,kaban-ingest/md-splitter \
    -E "$selection" -j 1 --no-fail-fast --success-output immediate
  if [ "${KABAN_MACOS_PRODUCER_QUALIFY:-0}" = 1 ]; then
    step macos_producer_qualification
  fi
}

# This costly qualification is selected only for producer/shared/setup changes or
# explicit manual qualification. Checked tests remain separate; this campaign uses
# the installed producer's normal release profile. Trusted fixtures provide no sandbox.
macos_producer_qualification() (
  set -e
  local scratch producer_target sysroot producer oracle profile expectation
  scratch=$(mktemp -d "${TMPDIR:-/tmp}/kaban-macos-producer.XXXXXXXX")
  scratch=$(cd "$scratch" && pwd -P)
  chmod 700 "$scratch"
  case "$kaban_target_dir" in /*) ;; *) kaban_target_dir="$PWD/$kaban_target_dir" ;; esac
  producer_target="$kaban_target_dir/producer"
  # Own toolchain, tests and mutable target; no executable tool cache is restored.
  CARGO_TARGET_DIR="$kaban_target_dir" bash scripts/ci/ci.sh producer
  (
    cd tools/kaban-rust-scip
    CARGO_TARGET_DIR="$producer_target" step "$CARGO" build --release --locked --bins --examples
    rustc --print sysroot > "$scratch/sysroot"
  )
  sysroot=$(cat "$scratch/sysroot")
  mkdir -m 700 "$scratch/bin"
  install -m 755 "$producer_target/release/kaban-rust-scip" "$scratch/bin/kaban-rust-scip"
  producer="$scratch/bin/kaban-rust-scip"
  oracle="$producer_target/release/examples/qualify-output"
  python3 - "$producer" <<'IDENTITY'
import hashlib, pathlib, sys
path = pathlib.Path(sys.argv[1])
print(f"Darwin producer profile: release; sha256: {hashlib.sha256(path.read_bytes()).hexdigest()}")
IDENTITY
  export PATH="$sysroot/bin:$PATH"
  # The existing M4 oracle and loading fixtures, with no new case registry/framework.
  for profile in minimal workspace target-contexts semantic-extra cargo-contexts local-contexts \
    operator-ranges required-features block-references shared-contexts member-collisions \
    cfg-conflict block-function block-impl block-generated block-attribute block-derive \
    cfg-body-conflict shadow-backend derive-impls metadata-failure build-failure shared-root \
    missing-server missing-sysroot sysroot-metadata-failure sysroot-manifest-failure; do
    mkdir -m 700 "$scratch/$profile"
    expectation=--expect-scip
    case "$profile" in
      metadata-failure|build-failure|shared-root|missing-server|missing-sysroot|sysroot-metadata-failure|sysroot-manifest-failure)
        expectation=no-output ;;
    esac
    step bash tools/kaban-rust-scip/scripts/qualify-loading.sh \
      "$producer" "$sysroot" "$profile" "$scratch/$profile" "$expectation"
    if [ "$expectation" = --expect-scip ]; then
      case "$profile" in
        required-features|block-function|block-impl|block-generated|block-attribute|block-derive|cfg-body-conflict) ;;
        *)
          local decoded=$profile
          [ "$profile" != shadow-backend ] || decoded=workspace
          step "$oracle" "$decoded" "$scratch/$profile/out/index.scip" "$scratch/$profile/work" ;;
      esac
    fi
  done
  step "$CARGO" build --locked -p kaban-index
  export KABAN_TEST_RUST_PRODUCER="$producer" KABAN_TEST_RUST_SYSROOT="$sysroot"
  export KABAN_TEST_INDEX_BIN="$kaban_target_dir/debug/kaban-index"
  step rust_runner "$CARGO" nextest run --locked -p kaban --test mcp_rust_acceptance \
    --features kaban-ingest/pdf,kaban-ingest/md-splitter --run-ignored only \
    -E 'test(=rust_snapshot_public_queries_through_the_mcp_server)' -j 1 --success-output immediate
  [ "$missing" -eq 0 ] || exit 3
  echo "Darwin producer qualification retained: $scratch (trusted none, no Linux namespace isolation)"
)

helpers() {
  (cd scripts/mcp && python3 -m unittest -v test_mcp_catalog) || return $?
  python3 -m unittest discover -s scripts/tests -v || return $?
  # pipefail retains a failed filesystem traversal as well as a syntax failure.
  (set -o pipefail; find scripts hooks -name '*.sh' -print0 | xargs -0 -n1 bash -n)
}

producer() {
  step "${CARGO:-cargo}" fetch --locked
  step "${CARGO:-cargo}" fmt --all --check
  step "${CARGO:-cargo}" clippy --workspace --all-targets --locked -- -D warnings
  # Cargo's default workspace test selection includes applicable library doctests.
  step rust_runner "${CARGO:-cargo}" test --workspace --locked -- --show-output
}

quality() {
  step "${CARGO:-cargo}" fmt --all --check
  step "${CARGO:-cargo}" clippy --workspace --all-targets --all-features -- -D warnings
  # the build without docs extractors (`--no-default-features`, the N1 size-table configuration
  # below) stays lint-clean too (docs-ingest-plan.md §16 item 3 (d), DI2.0): the `cfg(not(feature
  # = "ingest"))` arms are compiled only here
  step "${CARGO:-cargo}" clippy -p kaban --no-default-features --all-targets -- -D warnings
  step "${CARGO:-cargo}" clippy -p kaban-ingest --all-targets --no-default-features -- -D warnings

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
}

# Each artifact configuration is copied once into this invocation's private directory.
kaban_artifacts=
build_artifacts() {
  local config=$1 output=$2 binaries=$3 binary path digest artifact_root
  shift 3
  if [ -z "$kaban_artifacts" ]; then
    case "$kaban_target_dir" in /*) ;; *) kaban_target_dir="$PWD/$kaban_target_dir" ;; esac
    mkdir -p "$kaban_target_dir" || return $?
    artifact_root=${KABAN_CI_ARTIFACT_DIR:-$kaban_target_dir}
    mkdir -p "$artifact_root" || return $?
    kaban_artifacts=$(mktemp -d "$artifact_root/ci-artifacts.XXXXXX") || return $?
    echo "artifacts: $kaban_artifacts"
  fi
  mkdir "$kaban_artifacts/$config" || return $?
  printf '%s\n' "${CARGO:-cargo}" "$@" > "$kaban_artifacts/$config/cargo-args.txt" || return $?
  step "${CARGO:-cargo}" "$@"
  for binary in $binaries; do
    path="$kaban_artifacts/$config/$binary"
    cp "$kaban_target_dir/$output/$binary" "$path" || return $?
    digest=$(python3 -c 'import hashlib, sys
with open(sys.argv[1], "rb") as stream:
    print(hashlib.file_digest(stream, "sha256").hexdigest())' "$path") || return $?
    printf '%s\n' "$digest" > "$path.sha256" || return $?
    echo "artifact: $config/$binary sha256=$digest"
  done
}

artifact_file() {
  local path="$kaban_artifacts/$1/$2" expected digest
  if [ ! -f "$path" ] || [ ! -x "$path" ]; then
    echo "FAILED: missing/non-executable staged artifact $path" >&2
    return 1
  fi
  expected=$(cat "$path.sha256") || return $?
  digest=$(python3 -c 'import hashlib, sys
with open(sys.argv[1], "rb") as stream:
    print(hashlib.file_digest(stream, "sha256").hexdigest())' "$path") || return $?
  if [ "$digest" != "$expected" ]; then
    echo "FAILED: staged artifact checksum mismatch: $path" >&2
    return 1
  fi
  printf '%s\n' "$path"
}

artifact_size() {
  local path
  path=$(artifact_file "$@") || return $?
  stat -c '%s' "$path"
}

gnu() {
  local size default_binary on off delta index_size table_size binaries=kaban binary restored
  if [ -f crates/kaban-index/Cargo.toml ]; then binaries='kaban kaban-index'; fi
  step build_artifacts default release "$binaries" build --release
  size=$(artifact_size default kaban) || return $?
  echo "release binary: ${size} bytes"
  if [ "$size" -ge "$MAX_BINARY_BYTES" ]; then
    echo "FAILED: binary size ${size} >= ${MAX_BINARY_BYTES} (N1)" >&2
    return 1
  fi
  if [ "${CI_SKIP_SMOKE:-0}" = "1" ]; then
    echo "not-run: MCP smoke skipped by CI_SKIP_SMOKE=1"
    missing=1
  else
    default_binary=$(artifact_file default kaban) || return $?
    smoke_db="$(mktemp -d)/smoke.db"
    step python3 scripts/mcp/mcp_smoke.py "$default_binary" "$smoke_db" examples/policy.yaml
  fi

  echo "size: kaban ${size} (gate: < ${MAX_BINARY_BYTES}, applied above)"
  # Keep the package build: workspace/package feature equivalence is not established.
  if [ -f crates/kaban-index/Cargo.toml ]; then
    step build_artifacts gnu-index release kaban-index build --release -p kaban-index
    index_size=$(artifact_size gnu-index kaban-index) || return $?
    echo "size: kaban-index ${index_size} (gnu, informational; musl carries the G1-N4 gate)"
  else
    echo "skipped: kaban-index not yet present"
  fi
  if grep -Eq '^code-graph *=' crates/kaban/Cargo.toml; then
    # Historical workspace/default and explicit graph-on bytes differed; keep both builds.
    step build_artifacts graph-on release kaban build --release -p kaban --features code-graph
    on=$(artifact_size graph-on kaban) || return $?
    echo "size: kaban(code-graph on) ${on}"
    step build_artifacts graph-off release kaban build --release -p kaban --no-default-features --features ingest,text-index-tantivy
    off=$(artifact_size graph-off kaban) || return $?
    echo "size: kaban(code-graph off) ${off}"
    delta=$((on - off))
    echo "size: kaban(code-graph on) - kaban(code-graph off) ${delta} (gate: < 900000)"
    if [ "$delta" -ge 900000 ]; then
      echo "N4 MISS (reported, OWN-13; budget set by OWN-86 (4)): code-graph size delta ${delta} >= 900000 (G1-N4)" >&2
      missing=1
    fi
  else
    echo "skipped: kaban code-graph on/off pair not yet present"
  fi

  if [ "${KABAN_SIZE_TABLE:-0}" = "1" ]; then
    echo "size-table: kaban(default) ${size} (gate: < ${MAX_BINARY_BYTES})"
    step build_artifacts no-default release kaban build --release -p kaban --no-default-features
    table_size=$(artifact_size no-default kaban) || return $?
    echo "size-table: kaban(--no-default-features) ${table_size} (gate: < ${MAX_NO_DEFAULT_BYTES})"
    if [ "$table_size" -ge "$MAX_NO_DEFAULT_BYTES" ]; then
      echo "FAILED: --no-default-features binary size ${table_size} >= ${MAX_NO_DEFAULT_BYTES} (N1)" >&2
      return 1
    fi
    if grep -Eq '^md-splitter *=' crates/kaban-ingest/Cargo.toml; then
      step build_artifacts md-splitter release kaban build --release -p kaban --features kaban-ingest/md-splitter
      table_size=$(artifact_size md-splitter kaban) || return $?
      echo "size-table: kaban(md-splitter) ${table_size} (gate: informational)"
    else
      echo "skipped: md-splitter feature not present"
    fi
    if grep -Eq '^pdf *=' crates/kaban-ingest/Cargo.toml; then
      step build_artifacts pdf release kaban build --release -p kaban --features kaban-ingest/pdf
      table_size=$(artifact_size pdf kaban) || return $?
      echo "size-table: kaban(--features kaban-ingest/pdf) ${table_size} (gate: informational)"
    else
      echo "skipped: pdf feature not present"
    fi
  else
    echo "skipped: size table (KABAN_SIZE_TABLE=1 enables it)"
  fi
  # Cargo may hardlink release binaries to cached deps outputs. Replace the path
  # instead of writing through that inode, preserving both cached variants and modes.
  for binary in $binaries; do
    default_binary=$(artifact_file default "$binary") || return $?
    restored=$(mktemp "$kaban_target_dir/release/.$binary-restore.XXXXXX") || return $?
    cp -p "$default_binary" "$restored" || return $?
    mv -f "$restored" "$kaban_target_dir/release/$binary" || return $?
    cmp -s "$default_binary" "$kaban_target_dir/release/$binary" || return $?
    echo "restored: default $binary from staged bytes (no rebuild)"
  done
}

musl() {
  local target size_index size_kaban
  target="$(uname -m)-unknown-linux-musl"
  step build_artifacts musl "$target/release" 'kaban kaban-index' build --release --target "$target" -p kaban -p kaban-index
  size_index=$(artifact_size musl kaban-index) || return $?
  size_kaban=$(artifact_size musl kaban) || return $?
  echo "size: kaban-index (musl ${target}) ${size_index} (gate: < 12000000, G1-N4)"
  echo "size: kaban (musl ${target}) ${size_kaban} (informational; N1 is the default gnu gate)"
  if [ "$size_index" -ge 12000000 ]; then
    echo "N4 MISS (reported, OWN-13; budget set by owner decision 2026-11-04): kaban-index musl size ${size_index} >= 12000000 (G1-N4)" >&2
    missing=1
  fi
}

if [ "$kaban_group" = producer ]; then
  case "$kaban_target_dir" in /*) ;; *) kaban_target_dir="$PWD/$kaban_target_dir" ;; esac
  export CARGO_TARGET_DIR="$kaban_target_dir/producer"
  cd tools/kaban-rust-scip || exit
fi
step prerequisites
case "$kaban_group" in
  all)
    step quality
    step helpers
    step functional
    step pdf
    step gnu
    step musl
    ;;
  *) step "$kaban_group" ;;
esac

if [ $missing -ne 0 ]; then
  echo "RESULT: incomplete (a gate was skipped or a measured G1-N4 miss was reported: CI_SKIP_SMOKE=1, musl target absent, mandatory_skips non-zero, or an N4 MISS line above); exit 3"
  exit 3
fi
echo "RESULT: $kaban_group gates passed (producer is separate unless selected)"
