#!/usr/bin/env bash
# kaban Claude Code hook: PostToolUse (Bash) -> success evidence for a pending Failure of this session (C4.4).
# Always exits 0; see _run.sh for the wrapper rules (timeout, KABAN_DB, watchdog).
exec bash "$(dirname "${BASH_SOURCE[0]}")/_run.sh" post-tool-use
