#!/usr/bin/env bash
# kaban Codex CLI hook: PreToolUse (apply_patch; Edit|Write if exposed) -> the records governing the files named in the patch headers as additionalContext; kaban never blocks an edit.
# Always exits 0; see _run.sh for the wrapper rules (timeout, KABAN_DB, watchdog).
exec bash "$(dirname "${BASH_SOURCE[0]}")/_run.sh" pre-tool-use
