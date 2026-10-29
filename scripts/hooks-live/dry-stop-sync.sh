#!/usr/bin/env bash
# Dry Stop probe for the detached `kaban sync` of the Stop recipes (core-plan C5.3 as amended by
# `reviews/m5-prep.md` M5-20, M5-28; OWN-7: Docker, no model, no credentials).
#
#   bash scripts/hooks-live/dry-stop-sync.sh <claude-code|codex> --kaban <static kaban binary> [--out <dir>]
#
# One recipes-mode session of run.sh whose host CLI is replaced (KBN_DRY_CMD) by a shell command
# that pipes ONE invented Stop envelope through the REAL Stop recipe of the host as the entrypoint
# installed it (Claude: /work/hooks/claude-code/stop.sh; Codex: the copy under
# ~/.codex/hooks/kaban/stop.sh), then exits. The entrypoint waits (bounded) for the detached sync
# and reports `sync_detached` in recipes-verdict.json. This script asserts, from the reduced
# verdict only: the dry host command returned promptly (the sync did not hold its stdout), one
# `stop` hook call with exit 0, `sync_detached.ran`, no `--stage`, `kaban_files_after >= 3`
# (adr/INDEX.md, the record file, .gitignore) and `decisions_md_exists`. Exit 1 on any failure.
set -u
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
host="${1:?host: claude-code or codex}"; shift
kaban_bin=""; out_root=/tmp/m5w2c/dry-stop
while [ $# -gt 0 ]; do
  case "$1" in
    --kaban) kaban_bin="$2"; shift 2 ;;
    --out) out_root="$2"; shift 2 ;;
    *) echo "unknown option $1" >&2; exit 2 ;;
  esac
done
case "$host" in claude-code|codex) ;; *) echo "host must be claude-code or codex" >&2; exit 2 ;; esac
[ -n "$kaban_bin" ] || { echo "--kaban <static kaban binary> is required" >&2; exit 2; }
command -v docker >/dev/null || { echo "docker not available" >&2; exit 2; }
fail=0
say() { printf '%s\n' "$*"; }
check() { local name="$1"; shift; if "$@" >/dev/null 2>&1; then say "ok   $name"; else say "FAIL $name"; fail=1; fi; }

case "$host" in
  claude-code) script='/work/hooks/claude-code/stop.sh'; event=Stop ;;
  codex) script='$HOME/.codex/hooks/kaban/stop.sh'; event=Stop ;;
esac
# the dry command: time the recipe call itself (the wrapper must return with its stdout closed
# while the fake-free, real sync runs detached), print the elapsed ms as the host's "stream"
dry="start=\$(date +%s%N); out=\$(printf '%s' '{\"session_id\":\"sess-dry\",\"cwd\":\"/work\",\"hook_event_name\":\"$event\"}' | bash $script); end=\$(date +%s%N);
  printf '{\"type\":\"dry\",\"stop_wrapper_ms\":%d,\"stdout_docs\":%d}\n' \$(( (end - start) / 1000000 )) \$(printf '%s\n' \"\$out\" | grep -c .); exit 0"
session="drystop$(date -u +%H%M%S)"
mkdir -p "$out_root"
KBN_DRY_CMD="$dry" "$here/run.sh" "$host" "$session" --recipes --kaban "$kaban_bin" --out "$out_root" >"$out_root/$host-$session.console" 2>&1
rc=$?
dir="$out_root/$host-$session"
say "run.sh exit=$rc; results in $dir"
check "$host: recipes-verdict.json exists" test -f "$dir/recipes-verdict.json"
check "$host: one stop hook call, exit 0, under 2 s" \
  jq -e '.hook_calls_by_event.stop == 1 and .hook_exit_codes == [0] and .any_exit_2 == false and .any_hook_over_2000ms == false' "$dir/recipes-verdict.json"
check "$host: the detached sync ran (shim saw sync --json), exit 0, never --stage" \
  jq -e '.sync_detached.ran == true and .sync_detached.calls == 1 and .sync_detached.exit_codes == [0] and .sync_detached.staged_flag_seen == false' "$dir/recipes-verdict.json"
check "$host: .kaban files appeared afterwards (>= 3) with adr/INDEX.md" \
  jq -e '.sync_detached.kaban_files_after >= 3 and .sync_detached.decisions_md_exists == true' "$dir/recipes-verdict.json"
# the dry "stream" line is reduced by reduce.jq in transcript-shape.jsonl: numbers become "<number>",
# so the wrapper time is read from the shim log instead: the stop hook call's ms is the bound the
# host waited for kaban; the wrapper's own return is proven offline by test-stop-sync.sh
check "$host: the shim's stop call took under 2000 ms" \
  bash -c 'jq -e "select(.argv[0]==\"hook\" and .argv[1]==\"stop\") | .ms < 2000" "$1/kaban-calls.jsonl" | grep -q true' _ "$dir"
say "sync_detached: $(jq -c '.sync_detached' "$dir/recipes-verdict.json" 2>/dev/null)"
if [ "$fail" -eq 0 ]; then say "ALL OK"; else say "FAILURES"; fi
exit "$fail"
