#!/usr/bin/env bash
# Regenerate the code-graph fixtures, the synthetic indexes and the goldens (stage1-plan CG3.4;
# stage1-spec §8.1, §8.2; review unit U1 RU1-8). Human-run; never run by CI or by a test (the
# committed `index.scip` bytes are the inputs of every test).
# Rust A8 consumer inputs are deliberately not regenerated here: see
# fixtures/codegraph/rust-drivers/README.md for isolated qualification/provenance.
#
#   scripts/dev/regen_fixtures.sh [--check] [--accept-new-versions] [--skip-indexers] [--skip-cargo]
#
# One producer configuration per language, resolved once and used for the version check, the
# run and the `VERSIONS` row:
#   Go  the executable `go-drivers/mkfixture.sh` will run: `$SCIP_GO`, else `~/go/bin/scip-go`
#       (its `--version` is the found version; PATH is never consulted);
#   TS  the pinned install inside the `ts-drivers/mkfixture.sh` container
#       (`$SCIP_TYPESCRIPT_VERSION`, else the script's pin).
# Each fixture script reports that configuration with `--print-config` and, after a run, one
# `provenance` line per output file (tool, the version the executable actually reported, where it
# ran, file, sha256); the `VERSIONS` rows are written from those lines only, so a version can never
# be recorded beside bytes another executable produced.
#
# Guard: the found version is compared with the latest row per tool in `fixtures/codegraph/VERSIONS`
# and with the fixture script's inner pin. A mismatch prints both and exits 4 unless
# `--accept-new-versions` is given, which also lets the inner pins accept the found version
# (`ACCEPT_NEW_VERSIONS=1`). `--check` stops after the guard (exit 0 or 4). `--skip-indexers` skips
# the producers (synthetic indexes and goldens only); `--skip-cargo` skips the ignored regen tests
# (producers only). Review the golden diff against §8.2 before committing: a difference is a defect
# or a producer change, never a golden edit. Save the run's output as a log (coordinator lane rule).
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
VERSIONS="$ROOT/fixtures/codegraph/VERSIONS"
GO_SCRIPT="$ROOT/fixtures/codegraph/go-drivers/mkfixture.sh"
TS_SCRIPT="$ROOT/fixtures/codegraph/ts-drivers/mkfixture.sh"
CHECK=0
ACCEPT=0
SKIP_INDEXERS=0
SKIP_CARGO=0
for arg in "$@"; do
  case "$arg" in
    --check) CHECK=1 ;;
    --accept-new-versions) ACCEPT=1 ;;
    --skip-indexers) SKIP_INDEXERS=1 ;;
    --skip-cargo) SKIP_CARGO=1 ;;
    -h|--help) sed -n '2,25p' "$0"; exit 0 ;;
    *) echo "unknown argument: $arg" >&2; exit 2 ;;
  esac
done
export ACCEPT_NEW_VERSIONS=$ACCEPT

# The latest recorded version of a tool (first column), or "absent".
recorded_version() {
  awk -v tool="$1" '$1 == tool { v = $2 } END { print (v == "" ? "absent" : v) }' "$VERSIONS"
}

# One `key=value` field of a `--print-config` or `provenance` line.
field() {
  local line=$1 key=$2
  printf '%s\n' "$line" | tr ' ' '\n' | awk -F= -v k="$key" '$1 == k { sub(/^[^=]*=/, ""); print; exit }'
}

mismatch=0
for script in "$GO_SCRIPT" "$TS_SCRIPT"; do
  config=$(bash "$script" --print-config)
  tool=$(field "$config" tool)
  found=$(field "$config" version)
  pin=$(field "$config" pin)
  where=$(field "$config" ran)
  recorded=$(recorded_version "$tool")
  [ -n "$found" ] || found=unparsed
  if [ "$recorded" = "$found" ] && [ "$pin" = "$found" ]; then
    echo "versions: $tool recorded $recorded, inner pin $pin, found $found at $where (ok)"
  else
    echo "versions: $tool recorded $recorded, inner pin $pin, found $found at $where (MISMATCH)" >&2
    mismatch=1
  fi
done

if [ $mismatch -ne 0 ] && [ $ACCEPT -ne 1 ]; then
  echo "refusing: producer versions differ from fixtures/codegraph/VERSIONS or the inner pins (pass --accept-new-versions to regenerate anyway)" >&2
  exit 4
fi
if [ $CHECK -eq 1 ]; then
  exit 0
fi

cd "$ROOT"
provenance=()
producer_out=$(mktemp "${TMPDIR:-/tmp}/kaban-regen.XXXXXX")
trap 'rm -f "$producer_out"' EXIT
# Run one producer, echo its output, and collect its `provenance` lines.
producer() {
  "$@" > "$producer_out"
  cat "$producer_out"
  mapfile -t -O "${#provenance[@]}" provenance < <(grep '^provenance ' "$producer_out" || true)
}
if [ $SKIP_INDEXERS -ne 1 ]; then
  echo "==> go-drivers (scip-go, REGEN=1)"
  REGEN=1 producer bash "$GO_SCRIPT"
  echo "==> ts-drivers (scip-typescript)"
  producer bash "$TS_SCRIPT"
  if [ "${#provenance[@]}" -ne 3 ]; then
    echo "refusing: expected 3 provenance lines from the producers, got ${#provenance[@]}; VERSIONS not written" >&2
    exit 5
  fi
fi

if [ $SKIP_CARGO -ne 1 ]; then
  echo "==> synthetic indexes and goldens (ignored regen tests)"
  cargo test -p kaban-index --test writer --offline -- --ignored regen_synthetic_fixtures
  cargo test -p kaban-index --test goldens --offline -- --ignored regen_goldens
fi

if [ $SKIP_INDEXERS -ne 1 ]; then
  echo "==> VERSIONS (append-only, from the producers' provenance lines)"
  for line in "${provenance[@]}"; do
    tool=$(field "$line" tool); version=$(field "$line" version); where=$(field "$line" ran)
    fixture=$(field "$line" fixture); file=$(field "$line" file); sha=$(field "$line" sha256)
    committed=$(sha256sum "fixtures/codegraph/$fixture/$file" | cut -d' ' -f1)
    if [ "$sha" != "$committed" ]; then
      echo "refusing: $fixture/$file sha256 $committed differs from the producer's $sha" >&2
      exit 5
    fi
    printf '%-18s %-8s %-11s %-14s %-11s %-41s %-18s %s\n' "$tool" "$version" "(see above)" "$where" "$fixture" "$(date -u +%Y-%m-%dT%H:%M:%SZ) regen" "$file" "$sha" >> "$VERSIONS"
  done
fi
echo "done: review 'git diff fixtures/codegraph' against stage1-spec §8.2 before committing"
