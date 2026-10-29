#!/usr/bin/env bash
# Shared wrapper of the kaban Claude Code hooks (core-spec F8, §7; core-plan C4.3 as amended by
# M4-6, M4-8, M4-9): `_run.sh <event>` pipes the host's stdin envelope into
# `kaban hook <event> --stdin-json` and never blocks the host.
#
# - exit 0 whatever happens (the host reads decisions from the JSON on stdout, never from the
#   exit status; exit 2 would block and is never produced);
# - `timeout 2` where GNU coreutils provides it (Linux); on macOS `timeout` is absent and the
#   command runs bare: kaban's own 1.8 s watchdog answers `{}` and exits 0 in time either way;
# - the database is passed explicitly: set KABAN_DB (and KABAN_HOME) to the same values the
#   kaban MCP server uses; the default is `<project dir>/kaban.db`. A hook never creates or
#   migrates a database: without one it prints `{}` and says so on stderr (debug log only).
set -u
event="${1:?usage: _run.sh <event>}"
KABAN_BIN="${KABAN_BIN:-kaban}"
export KABAN_DB="${KABAN_DB:-${CLAUDE_PROJECT_DIR:-$PWD}/kaban.db}"
if command -v timeout >/dev/null 2>&1; then
  timeout 2 "$KABAN_BIN" hook "$event" --host claude-code --stdin-json || exit 0
else
  "$KABAN_BIN" hook "$event" --host claude-code --stdin-json || exit 0
fi
exit 0
