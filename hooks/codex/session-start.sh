#!/usr/bin/env bash
# kaban Codex CLI hook: SessionStart -> the kaban briefing (<= 5 items) as hookSpecificOutput.additionalContext (consumed by Codex, observed live).
# Always exits 0; see _run.sh for the wrapper rules (timeout, KABAN_DB, watchdog).
exec bash "$(dirname "${BASH_SOURCE[0]}")/_run.sh" session-start
