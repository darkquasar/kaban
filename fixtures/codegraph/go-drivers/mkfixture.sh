#!/usr/bin/env bash
# Go fixture producer (stage1-spec §8.1 `go-drivers`; stage1-plan CG3.1).
#
# Builds the scripted repository from `repo/` (the C1 tree, committed here verbatim) in a scratch
# directory: C1 = the tree as committed; C2 = C1 with `drivers/mem.go` renamed to
# `drivers/memory.go` and one line added to `PostgresDriver.Open`. Both commits use fixed
# author, committer and dates, so re-running reproduces the same two commit ids (asserted
# against `COMMITS` once that file exists). Then runs scip-go 0.2.7 at C2 twice, with tests
# (`index.scip`) and with `--skip-tests` (`index-notest.scip`), and writes both files beside
# this script (only when absent or REGEN=1; see the note at the end). The fixture imports only the standard library, so `GOPROXY=off` and
# `GOFLAGS=-mod=readonly` hold and nothing is downloaded (OWN-7: scip-go over the fixture tree
# is the only process run).
#
# Usage: bash fixtures/codegraph/go-drivers/mkfixture.sh   (SCIP_GO=<path> overrides ~/go/bin/scip-go)
#        bash fixtures/codegraph/go-drivers/mkfixture.sh --print-config
#
# Producer configuration (RU1-8): the one executable this script runs is `$SCIP_GO`, else
# `~/go/bin/scip-go`; PATH is never consulted. `--print-config` prints it with the version it
# reports and the inner pin; a run ends with one `provenance` line per output file carrying that
# same executable's version and the file's sha256 (what `scripts/dev/regen_fixtures.sh` records). A
# version other than the pin exits 4 naming both unless `ACCEPT_NEW_VERSIONS=1` (set by the outer
# `--accept-new-versions`).
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
SCIP_GO="${SCIP_GO:-$HOME/go/bin/scip-go}"
PIN="0.2.7"

version="$("$SCIP_GO" --version 2>/dev/null || echo absent)"
if [ "${1:-}" = "--print-config" ]; then
  echo "tool=scip-go version=$version pin=$PIN ran=host:$SCIP_GO"
  exit 0
fi
if [ "$version" = absent ]; then
  echo "scip-go not found at $SCIP_GO" >&2
  exit 2
fi
if [ "$version" != "$PIN" ] && [ "${ACCEPT_NEW_VERSIONS:-0}" != 1 ]; then
  echo "scip-go $version found at $SCIP_GO; the inner pin is $PIN (stage1-plan §3): pass --accept-new-versions to scripts/dev/regen_fixtures.sh to accept it" >&2
  exit 4
fi

# A fixed scratch path: scip-go embeds `project_root` (file://<path>) and its argv in the index
# metadata, so a random mktemp name would make the bytes differ between runs.
scratch="${TMPDIR:-/tmp}/kaban-fixture-go-drivers"
rm -rf "$scratch"
mkdir -p "$scratch"
trap 'rm -rf "$scratch"' EXIT
repo="$scratch/go-drivers"
cp -R "$here/repo" "$repo"
cd "$repo"

export GIT_AUTHOR_NAME="kaban fixture" GIT_AUTHOR_EMAIL="fixture@kaban.invalid"
export GIT_COMMITTER_NAME="kaban fixture" GIT_COMMITTER_EMAIL="fixture@kaban.invalid"
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null
git init -q -b main
git add -A
GIT_AUTHOR_DATE="2026-10-26T08:16:02Z" GIT_COMMITTER_DATE="2026-10-26T08:16:02Z" \
  git commit -q -m "C1: drivers fixture"
c1="$(git rev-parse HEAD)"

git mv drivers/mem.go drivers/memory.go
# one line added to PostgresDriver.Open (for code_impact and last_change)
tab="$(printf '\t')"
sed -i "s|^${tab}_ = marker\$|${tab}_ = marker\n${tab}_ = dsn|" drivers/postgres.go
grep -q "^${tab}_ = dsn\$" drivers/postgres.go
git add -A
GIT_AUTHOR_DATE="2026-10-26T09:04:36Z" GIT_COMMITTER_DATE="2026-10-26T09:04:36Z" \
  git commit -q -m "C2: rename mem.go, extend PostgresDriver.Open"
c2="$(git rev-parse HEAD)"

# C3 (stage1-plan CG10.2, walkthrough Step 6): MemDriver renamed to MemoryDriver in
# drivers/memory.go (the test file does not name it). The indexes below are C2's: the tree is
# checked back out at C2 before scip-go runs, and C3 is history only (the rename-over-time
# test derives its C3 snapshot from the committed C2 index until an indexer is on the box).
sed -i 's/MemDriver/MemoryDriver/g' drivers/memory.go
grep -q "MemoryDriver" drivers/memory.go
git add -A
GIT_AUTHOR_DATE="2026-10-26T09:53:10Z" GIT_COMMITTER_DATE="2026-10-26T09:53:10Z" \
  git commit -q -m "C3: rename MemDriver to MemoryDriver"
c3="$(git rev-parse HEAD)"
git checkout -q "$c2"

commits="C1 $c1
C2 $c2
C3 $c3"
if [ -f "$here/COMMITS" ]; then
  if [ "$(cat "$here/COMMITS")" != "$commits" ]; then
    echo "commit ids differ from COMMITS; the scripted history is not reproducible" >&2
    printf '%s\n' "$commits" >&2
    exit 1
  fi
else
  printf '%s\n' "$commits" > "$here/COMMITS"
fi

export GOFLAGS=-mod=readonly GOPROXY=off GOTOOLCHAIN=local
export GOCACHE="$scratch/gocache" GOPATH="$scratch/gopath"
# relative output names (argv is embedded in the metadata, so no host path belongs there)
"$SCIP_GO" index -q --module-version "$c2" --output index.scip
"$SCIP_GO" index -q --module-version "$c2" --skip-tests --output index-notest.scip

printf 'C1 %s\nC2 %s\nC3 %s\n' "$c1" "$c2" "$c3"
# scip-go 0.2.7 orders a document's SymbolInformation by Go map iteration, so two runs over the
# same commit differ in byte order while holding the same content. The committed bytes are one
# run's output (sha256 in ../VERSIONS); they are replaced only on REGEN=1 or when absent, and
# the regeneration guard that compares content is CG3.4's.
for f in index.scip index-notest.scip; do
  if [ "${REGEN:-0}" = 1 ] || [ ! -f "$here/$f" ]; then
    mv "$f" "$here/$f"
    echo "written $f"
  else
    echo "kept committed $f (fresh run sha256 $(sha256sum "$f" | cut -c1-16)…; REGEN=1 replaces it)"
  fi
done
for f in index.scip index-notest.scip; do
  echo "provenance tool=scip-go version=$version ran=host:$SCIP_GO fixture=go-drivers file=$f sha256=$(sha256sum "$here/$f" | cut -d' ' -f1)"
done
