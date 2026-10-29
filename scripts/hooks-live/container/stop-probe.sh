#!/usr/bin/env bash
# Stop-type probe (M4-16, M4-30, M4 W2-D1).
#
#   stop-probe.sh <host>      payload on stdin
#
# Mode from $KBN_STOP_MODE (set by run.sh through the entrypoint):
#   none    print nothing (default)
#   block   {"decision":"block","reason":"KBN-STOP-<hex>: ..."}   (Claude Code Stop contract)
#   system  {"systemMessage":"KBN-STOP-<hex>: ..."}               (Codex Stop channel per plan C4.4)
#   both    {"decision":"block","reason":...,"systemMessage":...}
# Fires once per session (guard file in /capture, tmpfs), so a host that honours the block
# gets exactly one continuation and the second Stop prints nothing. Exit 0 always (PR-9).
set -u
# shellcheck disable=SC1091
[ -r /capture/sentinel.env ] && . /capture/sentinel.env
token="${KBN_STOP:-KBN-STOP-00000000}"
mode="${KBN_STOP_MODE:-none}"
guard=/capture/stop-probe.done
cat >/dev/null 2>&1 || true
[ "$mode" = none ] && exit 0
if [ -e "$guard" ]; then exit 0; fi
: > "$guard"
text="$token: before stopping, reply with exactly this token on one line, then stop."
case "$mode" in
  block)  jq -n -c --arg t "$text" '{decision:"block", reason:$t}' ;;
  system) jq -n -c --arg t "$text" '{systemMessage:$t}' ;;
  both)   jq -n -c --arg t "$text" '{decision:"block", reason:$t, systemMessage:$t}' ;;
esac
exit 0
