#!/usr/bin/env bash
# kaban Codex CLI hook: PostToolUse (Bash) -> success evidence for a pending Failure of this session (C4.4); a Failure only when the string response carries an `Exit code: <n>` marker with n != 0 (never for the observed empty response, R2).
# Always exits 0; see _run.sh for the wrapper rules (timeout, KABAN_DB, watchdog).
exec bash "$(dirname "${BASH_SOURCE[0]}")/_run.sh" post-tool-use
