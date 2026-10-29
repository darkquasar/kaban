#!/usr/bin/env bash
# kaban Claude Code hook: PreToolUse (Edit|Write|MultiEdit) -> the records governing the edited file as additionalContext; kaban never blocks an edit.
# Always exits 0; see _run.sh for the wrapper rules (timeout, KABAN_DB, watchdog).
exec bash "$(dirname "${BASH_SOURCE[0]}")/_run.sh" pre-tool-use
