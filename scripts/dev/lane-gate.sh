#!/usr/bin/env bash
# The PROTOCOL.md §4 lane gate in one command (OWN-79): fmt, clippy -D warnings on the touched crates, nextest ONCE,
# the nextest list delta against the lane's base list, and the *.snap.new check. Run from the lane worktree root.
# Usage: scripts/dev/lane-gate.sh <log-prefix> <base-list-file> -p <crate> [-p <crate> ...] [--no-default-kaban]
#   e.g. scripts/dev/lane-gate.sh ../specs/kaban/00-project/archive/evidence/e2-logs/e27 ../specs/kaban/00-project/archive/evidence/e2-logs/e27-base-list.txt -p kaban-core -p kaban-store-sqlite
# Writes <prefix>-gate-{fmt,clippy,clippy-nodefault,nextest}.log, <prefix>-gate-list.txt, <prefix>-gate-list-delta.txt and
# <prefix>-gate-summary.log; exits non-zero on any red. Take the base list BEFORE the first edit:
#   scripts/dev/cargo-slot.sh cargo nextest list -p <crates...> --offline 2>/dev/null | grep -v '^[[:space:]]*$' | sort > <base-list-file>
# Required NOT RUN / SKIPPED(mandatory) markers produce INCOMPLETE (exit 3), never PASS.
set -uo pipefail
prefix=${1:?usage}; base=${2:?usage}; shift 2
mkdir -p "$(dirname "$prefix")"
crates=(); nodefault=0
while [ $# -gt 0 ]; do case "$1" in -p) crates+=(-p "$2"); shift 2;; --no-default-kaban) nodefault=1; shift;; *) echo "unknown arg $1" >&2; exit 2;; esac; done
[ ${#crates[@]} -gt 0 ] || { echo "name at least one -p crate" >&2; exit 2; }
export CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0 CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-4}
S="$prefix-gate-summary.log"; : > "$S"; fail=0
run() { local name=$1; shift; "$@" > "$prefix-gate-$name.log" 2>&1; local rc=$?; echo "$name exit $rc" >> "$S"; [ $rc -eq 0 ] || fail=1; }
run fmt cargo fmt --all --check
run clippy scripts/dev/cargo-slot.sh cargo clippy "${crates[@]}" --all-targets --all-features --offline -- -D warnings
[ $nodefault -eq 1 ] && run clippy-nodefault scripts/dev/cargo-slot.sh cargo clippy -p kaban --no-default-features --all-targets --offline -- -D warnings
run nextest scripts/dev/cargo-slot.sh cargo nextest run "${crates[@]}" --offline --success-output immediate
grep -E "Summary" "$prefix-gate-nextest.log" | tail -1 >> "$S"
missing=$(grep -Ec 'NOT RUN:|SKIPPED\(mandatory\)' "$prefix-gate-nextest.log" || true)
echo "mandatory_skip_markers $missing" >> "$S"
# the list step is evidence too: a failed list or an unreadable base is a red, a non-empty delta is not (diff exit 1)
if ! scripts/dev/cargo-slot.sh cargo nextest list "${crates[@]}" --offline 2>"$prefix-gate-list.err" | grep -v '^[[:space:]]*$' | sort > "$prefix-gate-list.txt"; then echo "list exit non-zero (see $prefix-gate-list.err)" >> "$S"; fail=1; fi
[ -s "$prefix-gate-list.txt" ] || { echo "list empty" >> "$S"; fail=1; }
diff "$base" "$prefix-gate-list.txt" > "$prefix-gate-list-delta.txt"; rc=$?; [ "$rc" -le 1 ] || { echo "diff exit $rc" >> "$S"; fail=1; }
echo "list: base $(wc -l < "$base") now $(wc -l < "$prefix-gate-list.txt") added $(grep -c '^>' "$prefix-gate-list-delta.txt") removed $(grep -c '^<' "$prefix-gate-list-delta.txt")" >> "$S"
snaps=$(find crates -name '*.snap.new' | wc -l); echo "snap.new $snaps" >> "$S"; [ "$snaps" -eq 0 ] || fail=1
result=PASS; rc=0
if [ "$fail" -ne 0 ]; then result=FAIL; rc=1
elif [ "$missing" -ne 0 ]; then result=INCOMPLETE; rc=3
fi
echo "result $result" >> "$S"; cat "$S"; exit "$rc"
