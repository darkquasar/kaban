#!/usr/bin/env bash
# kaban Codex CLI hook: Stop (never SessionEnd) -> the one-time "how was it fixed" prompt as {"decision":"block","reason":...} (C4.4; the channel Codex delivers to the model, M4-16).
# Always exits 0; see _run.sh for the wrapper rules (timeout, KABAN_DB, watchdog).
set -u
here="$(dirname "${BASH_SOURCE[0]}")"
bash "$here/_run.sh" stop
exit 0
