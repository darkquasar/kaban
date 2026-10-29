#!/usr/bin/env bash
# Pre-warm a lane worktree's build directory from a CLEAN integration build (PROTOCOL.md §5, OWN-78, OWN-79).
# Usage: scripts/dev/lane-target.sh <integration-repo> <lane-worktree>
#   e.g. scripts/dev/lane-target.sh spike/second-brain-mcp spike/kaban-wt-e2
# External-crate artefacts under target/debug/deps (identical in every worktree, about 3.3 GB) are HARDLINKED and
# made read-only, so a forbidden in-place rewrite (a dependency or flag change in a lane) fails loudly instead of
# corrupting a sibling (a hardlink shares its inode: never chmod them in the integration tree either); the workspace
# libraries and binaries, .fingerprint, build/ and the top-level binaries (about 0.5 GB) are COPIED; workspace test and
# bench binaries are SKIPPED (a lane rebuilds the ones it tests). The source must not be mid-build. Refuses a destination that already has a target/debug.
set -euo pipefail
src=${1:?usage: scripts/dev/lane-target.sh <integration-repo> <lane-worktree>}
dst=${2:?usage: scripts/dev/lane-target.sh <integration-repo> <lane-worktree>}
S="$src/target/debug"; D="$dst/target/debug"
[ -d "$S/deps" ] || { echo "no clean build at $S" >&2; exit 2; }
[ ! -e "$D" ] || { echo "$D exists: cargo clean the lane first" >&2; exit 3; }
if pgrep -f "cargo (build|test|nextest|clippy|check).*$(cd "$src" && pwd)" >/dev/null 2>&1; then
  echo "a cargo process is running in $src: wait for it" >&2; exit 4
fi
mkdir -p "$D/deps"
[ -f "$src/target/CACHEDIR.TAG" ] && cp -a "$src/target/CACHEDIR.TAG" "$dst/target/"
for d in .fingerprint build examples incremental; do [ -d "$S/$d" ] && cp -a "$S/$d" "$D/$d"; done
# top-level binaries, their .d files and cargo's lock
find "$S" -maxdepth 1 -type f -exec cp -a {} "$D/" \;
# A workspace artefact is one whose dep-info file (`<stem>-<hash>.d`) names a source under the integration repository's
# crates/: library, binary and every test or bench target (`cli_review-<hash>`, `mcp_snapshots-<hash>`, ...), whatever its
# name. Everything else came from the registry and is identical in every worktree. Session 17: the earlier `kaban*` name
# test hardlinked the workspace test binaries read-only, and since a hardlink shares its inode the integration tree's own
# artefacts became read-only too (`Permission denied` on the next rebuild there).
# classify: ext (registry: hardlink), ws (workspace lib or bin: copy), test (workspace test or bench binary: skip; a lane
# rebuilds the test binaries of every crate it touches and never runs the others', so copying them (1.8 GB, 868 files
# at session 17) buys nothing; the base list and the gate compile the touched crates' tests once, under the semaphore)
srcabs=$(cd "$src" && pwd)
classify() {  # $1 = artefact path in the source deps dir
  local stem; stem=$(basename "$1"); stem=${stem%%.*}; stem=${stem#lib}
  local dfile="$S/deps/$stem.d"
  [ -f "$dfile" ] || { echo ext; return; }
  # the sources of the first rule only (dep-info's first token is the artefact's own path under this repository)
  local srcs; srcs=$(head -1 "$dfile" | sed 's/^[^:]*: *//')
  case " $srcs " in
    *" crates/"*|*" $srcabs/crates/"*)
      case "$srcs" in crates/*/tests/*|crates/*/benches/*|"$srcabs"/crates/*/tests/*|"$srcabs"/crates/*/benches/*) echo test ;; *) echo ws ;; esac ;;
    *"/.cargo/registry/"*|*"/.cargo/git/"*) echo ext ;;
    *) echo ws ;;
  esac
}
linked=0; copied=0; skipped=0
for f in "$S"/deps/*; do
  b=$(basename "$f")
  case "$(classify "$f")" in
    ws) cp -a "$f" "$D/deps/$b"; copied=$((copied+1)) ;;
    test) skipped=$((skipped+1)) ;;
    ext) ln "$f" "$D/deps/$b" && chmod a-w "$D/deps/$b"; linked=$((linked+1)) ;;
  esac
done
echo "lane target ready: $D ($linked external artefacts hardlinked read-only, $copied workspace artefacts copied, $skipped test binaries skipped); du: $(du -sh "$D" | cut -f1) apparent, $(du -sh --apparent-size "$D" 2>/dev/null | cut -f1) total"
