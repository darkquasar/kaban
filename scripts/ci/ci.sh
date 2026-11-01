#!/usr/bin/env bash
# Comprehensive Rust gates from the workspace root. See --help and docs/developer/CI.md.
# Exit 0: all selected gates passed; 3: incomplete; other: first failing child's status.
set -u
kaban_ci_dir=$(cd "$(dirname "$0")" && pwd -P) || exit
cd "$kaban_ci_dir/../.." || exit

if [ "$#" -eq 1 ] && [ "$1" = "--help" ]; then
  cat <<'HELP'
Usage: bash scripts/ci/ci.sh [all|quality|helpers|functional|producer|gnu|musl]
No argument means all: quality + helpers + functional + GNU + musl gates.
quality: fmt, three Clippy variants, deny and dependency boundaries.
helpers: Python build/catalog tests and shell syntax (no Rust tools needed).
functional: workspace/PDF/no-default nextest suites and applicable library doctests.
producer: separate tools/kaban-rust-scip workspace/toolchain, fetch/fmt/Clippy/tests.
gnu: release build, MCP smoke and sizes; musl: native musl build/size gates.
Local core is quality + helpers + functional (mise run core).
Producer qualification is separate: invoke producer explicitly when applicable.
Missing/wrong required tools stop before compilation (exit 3); child failures retain
their status. Mandatory runtime markers and size misses remain incomplete (exit 3).
CARGO=<executable> selects Cargo, including paths with spaces; no toolchain override.
CARGO_TARGET_DIR selects build output; producer uses its private producer/ subdirectory.
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
  all|quality|helpers|functional|producer|gnu|musl) ;;
  *) echo "Unknown group: $kaban_group; see --help" >&2; exit 2 ;;
esac
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
        if group in ("quality", "functional"):
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
        if group in ("functional", "gnu"):
            probe([executable("node"), "--version"], f"v{pins['node']}")
            for tool in ("go", os.environ.get("SCIP_GO", "scip-go")):
                executable(tool)
        if group in ("functional", "producer", "gnu", "musl"):
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
  count=$(printf '%s\n' "$names" | grep -c .)
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
  local metadata packages package has_pdf=0
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
    if [ "$package" = kaban-ingest ]; then has_pdf=1; fi
  done
  if [ "${#selections[@]}" -gt 0 ]; then
    step rust_runner "${CARGO:-cargo}" test --doc --locked "${selections[@]}" -- --show-output
  fi
  # Match the PDF-enabled functional variant's library examples too.
  if [ "$has_pdf" -eq 1 ]; then
    step rust_runner "${CARGO:-cargo}" test --doc --locked -p kaban-ingest --features pdf -- --show-output
  fi
}

functional() {
  step rust_runner "${CARGO:-cargo}" nextest run --workspace --no-fail-fast --success-output immediate
  step rust_runner "${CARGO:-cargo}" nextest run -p kaban-ingest --features pdf --success-output immediate
  step rust_runner "${CARGO:-cargo}" nextest run -p kaban --no-default-features --success-output immediate
  step doctests
}

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

gnu() {
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
}

musl() {
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
