#!/usr/bin/env bash
# kaban Codex CLI hook: Stop (never SessionEnd) -> the one-time "how was it fixed" prompt as {"decision":"block","reason":...} (C4.4; the channel Codex delivers to the model, M4-16),
# then a fully detached, deadline-bounded `kaban sync` (C5.3, M5-20, R5-9): the .kaban/*.md flush runs in its own session with no inherited
# descriptor, so the hook's stdout stays exactly one JSON document and Codex never waits for it. Never `--stage`.
# Set KABAN_HOOK_SYNC=0 to skip the sync; KABAN_SYNC_DEADLINE (seconds, default 60) bounds it. Always exits 0; see _run.sh for the wrapper rules (timeout, KABAN_DB, watchdog).
set -u
here="$(dirname "${BASH_SOURCE[0]}")"
bash "$here/_run.sh" stop
if [ "${KABAN_HOOK_SYNC:-1}" != "0" ]; then
  KABAN_BIN="${KABAN_BIN:-kaban}"
  export KABAN_DB="${KABAN_DB:-$PWD/kaban.db}"
  # R5-9: two layers bound the detached sync on every host. (1) `kaban sync --deadline` is a
  # wall-clock bound inside the process (fixed stderr line, exit 4, partial state safe); (2) the
  # shell kills the sync's whole process group a few seconds later, so a git descendant dies with
  # it: `setsid timeout -k` on Linux, a job-control subshell with a sleeper on macOS (no setsid,
  # no GNU timeout). Only fixed strings and a digits-only number reach argv.
  deadline="${KABAN_SYNC_DEADLINE:-60}"
  case "$deadline" in ''|*[!0-9]*) deadline=60 ;; esac
  grace=$((deadline + 5))
  if command -v setsid >/dev/null 2>&1; then
    if command -v timeout >/dev/null 2>&1; then
      setsid timeout -k 5 "$grace" "$KABAN_BIN" sync --json --deadline "$deadline" </dev/null >/dev/null 2>&1 &
    else
      setsid "$KABAN_BIN" sync --json --deadline "$deadline" </dev/null >/dev/null 2>&1 &
    fi
  else
    # macOS: `set -m` puts the background sync in its own process group (`$!` is its leader),
    # the sleeper kills that group after the grace period, and the supervisor subshell cancels
    # the sleeper when the sync ends first; every descriptor is on /dev/null, nothing is awaited
    (
      set -m
      "$KABAN_BIN" sync --json --deadline "$deadline" </dev/null >/dev/null 2>&1 &
      pid=$!
      ( sleep "$grace"; kill -TERM -- "-$pid"; sleep 5; kill -KILL -- "-$pid" ) </dev/null >/dev/null 2>&1 &
      killer=$!
      wait "$pid"
      # TERM may end the leader before a TERM-resistant descendant. Keep the
      # escalation alive until the remaining group is killed; cancel it only
      # after the entire sync group is gone.
      if kill -0 -- "-$pid" 2>/dev/null; then
        wait "$killer"
      else
        kill -KILL -- "-$killer" 2>/dev/null
      fi
    ) </dev/null >/dev/null 2>&1 &
  fi
fi
exit 0
