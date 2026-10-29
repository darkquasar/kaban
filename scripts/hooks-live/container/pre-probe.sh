#!/usr/bin/env bash
# PreToolUse additionalContext probe (M4-30, M4 W2-D1).
#
#   pre-probe.sh <host>      payload on stdin
#
# Emits `hookSpecificOutput.additionalContext` with KBN-PRE-<8 hex> once per session
# (guard file in /capture, tmpfs). Whether the token shows up in a later assistant message
# tells whether the host injects PreToolUse context. Exit 0 always.
set -u
# shellcheck disable=SC1091
[ -r /capture/sentinel.env ] && . /capture/sentinel.env
token="${KBN_PRE:-KBN-PRE-00000000}"
guard=/capture/pre-probe.done
payload="$(cat 2>/dev/null || true)"
if [ -e "$guard" ]; then exit 0; fi
: > "$guard"
event="$(printf '%s' "$payload" | jq -r '.hook_event_name // "PreToolUse"' 2>/dev/null || echo PreToolUse)"
text="$token: include this exact token in your next message."
jq -n -c --arg e "$event" --arg t "$text" \
  '{hookSpecificOutput:{hookEventName:$e, additionalContext:$t}}'
exit 0
