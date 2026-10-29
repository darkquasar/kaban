#!/usr/bin/env bash
# Logging hook for the live capture harness (M4 W2-D1, OWN-7, S1 fence).
#
#   capture.sh <host> [event-hint]      payload on stdin
#
# Reads the host's hook payload from stdin, reduces it IN MEMORY to field names and JSON
# types (reduce.jq), appends one line to $KBN_CAPTURE_FILE (default /capture/events.jsonl,
# a tmpfs inside the container) and exits 0. It prints nothing on stdout, so it never
# changes host behaviour. The raw payload is never written anywhere.
#
# Recorded per line: host, event (hook_event_name, else the hint), hint, bytes (raw payload
# size), argc (extra argv after host and hint, counted only), env (names only of variables
# starting with CLAUDE_, CODEX_ or PI_), shape (the reduced payload or "<unparsed>"), plus a
# narrow control-field allowlist (S1/OWN-10 control fields, recorded as a documented deviation).
# `event` and `hint` are exported only when they are in the host's closed vocabulary
# (events-vocab.json, derived from hooks/HOST-EVENTS.md; R4-1); anything else is exported as
# "other" plus `event_unknown: true`, so a crafted event name never leaves the container.
#   tool              tool_name / toolName when the WHOLE string is a bare identifier
#                     ^[A-Za-z_][A-Za-z0-9_.:-]{0,40}$ (matcher vocabulary such as Bash, apply_patch,
#                     Write; a value with a newline anywhere never qualifies, RL-1), otherwise absent;
#   response_markers  for a STRING tool_response only: {lines, chars, empty, exit_code_marker,
#                     starts_exit_code, exited_with_code_marker, total_output_lines, words{...}}
#                     (booleans and counts for fixed vocabulary words; the text never leaves memory).
#                     The R2 markers (W2-D2) settle the Codex `Bash` response format: whether the
#                     string is empty, begins with an `Exit code:` line, or carries the
#                     `exited with code N` phrase the Codex binary uses for shell output.
set -u
host="${1:-unknown}"
hint="${2:-}"
shift $(( $# >= 2 ? 2 : $# ))
argc=$#
out="${KBN_CAPTURE_FILE:-/capture/events.jsonl}"
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

payload="$(cat 2>/dev/null || true)"
bytes=$(printf '%s' "$payload" | wc -c | tr -d ' ')

shape='"<unparsed>"'
if [ "$bytes" -gt 0 ]; then
  if reduced="$(printf '%s' "$payload" | jq -c -f "$here/reduce.jq" 2>/dev/null)" && [ -n "$reduced" ]; then
    shape="$reduced"
  fi
fi
event="$(printf '%s' "$payload" | jq -r 'if type=="object" then (.hook_event_name // .type // empty) else empty end' 2>/dev/null || true)"
[ -n "$event" ] || event="$hint"
# closed per-host vocabulary (R4-1): the name itself travels only when it is listed
vocab="$here/events-vocab.json"
in_vocab() { # in_vocab <name>  -> 0 when <name> is in the host's vocabulary
  [ -n "$1" ] && jq -e --arg h "$host" --arg n "$1" '(.[$h] // []) | index($n) != null' "$vocab" >/dev/null 2>&1
}
event_unknown=false
if ! in_vocab "$event"; then event=other; event_unknown=true; fi
if [ -n "$hint" ] && ! in_vocab "$hint"; then hint=other; fi

# the whole string must be an identifier (RL-1): jq's `$` accepts one trailing newline, so a newline
# anywhere disqualifies the value explicitly
tool="$(printf '%s' "$payload" | jq -r 'if type=="object" then ((.tool_name // .toolName // "") | if type=="string" and (contains("\n") | not) and test("^[A-Za-z_][A-Za-z0-9_.:-]{0,40}$") then . else "" end) else "" end' 2>/dev/null || true)"
markers="$(printf '%s' "$payload" | jq -c 'if type=="object" and (.tool_response|type)=="string" then
    {lines: ([.tool_response | splits("\n")] | length),
     chars: (.tool_response | length),
     empty: (.tool_response | length == 0),
     exit_code_marker: (.tool_response | test("[Ee]xit[ _]?[Cc]ode[: =]+[0-9]+")),
     starts_exit_code: (.tool_response | test("^\\s*[Ee]xit [Cc]ode:")),
     exited_with_code_marker: (.tool_response | test("exited with (code|status) [0-9]+"; "i")),
     total_output_lines: (.tool_response | test("Total output lines"; "i")),
     words: {exit: (.tool_response | test("exit"; "i")), status: (.tool_response | test("status"; "i")),
             code: (.tool_response | test("code"; "i")), digit: (.tool_response | test("[0-9]")),
             error: (.tool_response | test("error"; "i")), fail: (.tool_response | test("fail"; "i")),
             output: (.tool_response | test("output"; "i")), stderr: (.tool_response | test("stderr"; "i")),
             stdout: (.tool_response | test("stdout"; "i")), wall_time: (.tool_response | test("wall[ _]?time"; "i")),
             json_object: (.tool_response | test("^\\s*\\{"))}}
  else null end' 2>/dev/null || echo null)"
[ -n "$markers" ] || markers=null

envnames="$(env | sed -n 's/^\(\(CLAUDE\|CODEX\|PI\)_[A-Za-z0-9_]*\)=.*/\1/p' | sort | jq -R . | jq -s -c . 2>/dev/null || echo '[]')"

mkdir -p "$(dirname "$out")" 2>/dev/null || true
jq -n -c \
  --arg host "$host" --arg event "$event" --arg hint "$hint" \
  --argjson bytes "$bytes" --argjson argc "$argc" \
  --argjson env "$envnames" --argjson shape "$shape" \
  --arg tool "$tool" --argjson markers "$markers" --argjson unknown "$event_unknown" \
  '{host:$host, event:$event, hint:$hint, bytes:$bytes, argc:$argc, env:$env, shape:$shape}
   + (if $unknown then {event_unknown:true} else {} end)
   + (if $tool != "" then {tool:$tool} else {} end)
   + (if $markers != null then {response_markers:$markers} else {} end)' \
  >> "$out" 2>/dev/null || true
exit 0
