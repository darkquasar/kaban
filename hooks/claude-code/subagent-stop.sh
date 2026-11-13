#!/usr/bin/env bash
# kaban Claude Code hook: SubagentStop -> an Episode when the payload carries a summary.
# Always exits 0; see _run.sh for the wrapper rules (timeout, KABAN_DB, watchdog).
set -u
here="$(dirname "${BASH_SOURCE[0]}")"
bash "$here/_run.sh" subagent-stop
exit 0
