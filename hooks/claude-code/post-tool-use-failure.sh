#!/usr/bin/env bash
# kaban Claude Code hook: PostToolUseFailure (Bash) -> a Failure with pending_fix: true (command, error_text, stack_head).
# Always exits 0; see _run.sh for the wrapper rules (timeout, KABAN_DB, watchdog).
exec bash "$(dirname "${BASH_SOURCE[0]}")/_run.sh" post-tool-use-failure
