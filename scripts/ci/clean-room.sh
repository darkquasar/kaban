#!/usr/bin/env bash
# Wave clean-room (../specs/kaban/specs/00-project/PROTOCOL.md §4): the ONE full verification per integration wave.
# Usage: scripts/ci/clean-room.sh <label> [log-dir]      e.g. scripts/ci/clean-room.sh coord-merge23
# Runs from the repository root whatever the cwd. When the build directory is shared between worktrees hold the
# exclusive cargo lock around it, detached if the calling shell may die:
#   setsid nohup flock -x /tmp/kaban-cargo.lock scripts/ci/clean-room.sh coord-merge23 >/dev/null 2>&1 &
# Steps: cargo clean of the workspace members, the touch rule, the ignored multi_process run, then scripts/ci/ci.sh
# with the size table (ci.sh itself runs fmt, clippy in all feature sets, the workspace nextest ONCE plus the
# feature-enabled PDF step, cargo deny, the release build, the MCP smoke and the binary size gates).
set -u
cd "$(dirname "$0")/../.."
label=${1:?usage: scripts/ci/clean-room.sh <label> [log-dir]}
L=${2:-$(cd ../.. && pwd)/../specs/kaban/specs/00-project/evidence/e1-logs}
mkdir -p "$L"
export CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-2} CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_DEV_STRIP=symbols
S=$L/$label-summary.log
echo "start $(date -u +%T) head $(git rev-parse --short HEAD) $(df -h . | tail -1)" > "$S"
fail=0
cargo clean -p kaban -p kaban-api -p kaban-codegraph -p kaban-core -p kaban-index -p kaban-ingest -p kaban-policy -p kaban-store-sqlite > "$L/$label-clean.log" 2>&1; rc=$?; echo "clean exit $rc" >> "$S"; [ "$rc" -eq 0 ] || fail=1
touch crates/*/src/lib.rs crates/kaban/src/main.rs
cargo nextest run --workspace --offline --run-ignored only -E 'test(multi_process)' > "$L/$label-multiproc.log" 2>&1; rc=$?; echo "multiproc exit $rc $(grep -E 'Summary' "$L/$label-multiproc.log" | tail -1)" >> "$S"; [ "$rc" -eq 0 ] || fail=1
# ci.sh exit 3 means every gate that could run passed but a stage is not complete (an optional tool or target
# absent, `mandatory_skips` non-zero, a reported N4 miss): recorded by name, not a red (PROTOCOL §4; OWN-13)
flock -x /tmp/kaban-ci.lock env KABAN_SIZE_TABLE=1 bash scripts/ci/ci.sh > "$L/$label-ci.log" 2>&1; rc=$?; echo "ci exit $rc $(grep -E 'Summary|release binary|size-table|RESULT|not-run|N4 MISS|mandatory_skips' "$L/$label-ci.log" | tr '\n' ';')" >> "$S"; [ "$rc" -eq 0 ] || [ "$rc" -eq 3 ] || fail=1
# the exit status is the wave's verdict (a red step is a red clean-room; session 16, astra velocity memo)
echo "end $(date -u +%T) $(df -h . | tail -1) result $([ "$fail" -eq 0 ] && echo PASS || echo FAIL)" >> "$S"
exit "$fail"
