#!/usr/bin/env bash
# kaban Codex CLI hook: SubagentStop -> an Episode when the payload carries last_assistant_message (field unverified live on Codex: the event never fired in the captures).
# Always exits 0; see _run.sh for the wrapper rules (timeout, KABAN_DB, watchdog).
set -u
here="$(dirname "${BASH_SOURCE[0]}")"
bash "$here/_run.sh" subagent-stop
exit 0
