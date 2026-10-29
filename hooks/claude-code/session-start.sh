#!/usr/bin/env bash
# kaban Claude Code hook: SessionStart (startup|resume|compact) -> the kaban briefing (<= 5 items) as additionalContext.
# Always exits 0; see _run.sh for the wrapper rules (timeout, KABAN_DB, watchdog).
exec bash "$(dirname "${BASH_SOURCE[0]}")/_run.sh" session-start
