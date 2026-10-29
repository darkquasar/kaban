#!/usr/bin/env bash
# Behaviour test for the logging hook (capture.sh + reduce.jq): the S1 secret fence.
# Invented data only. Runs in the sandbox or in the container; needs bash and jq.
#
#   bash scripts/hooks-live/test-capture.sh
#
# Checks (public behaviour of `capture.sh <host> [event-hint]` reading a payload on stdin):
#   1. no value from the payload survives: strings, numbers and booleans become type markers,
#      keys and nesting are kept; stdout stays empty; exit is 0;
#   2. a non-JSON payload is recorded as "<unparsed>" with its byte count only, exit 0, no stdout;
#   3. a secret-looking key (api_token) is listed with its type only, never its value;
#   4. control-field allowlist: tool_name/toolName is recorded as `tool` only when it is a bare
#      identifier (matcher vocabulary); anything else is dropped; a string tool_response yields
#      boolean format markers only (exit-code marker, line count), never text;
#   5. closed event vocabulary (R4-1): `event` and `hint` are exported only when they are in the
#      host's vocabulary (container/events-vocab.json, derived from hooks/HOST-EVENTS.md); anything
#      else is exported as "other" with `event_unknown: true` and the text never appears.
set -u
here="$(cd "$(dirname "$0")" && pwd)"
capture="$here/container/capture.sh"
fail=0
say() { printf '%s\n' "$*"; }
check() { # check <name> <cmd...>
  local name="$1"; shift
  if "$@" >/dev/null; then say "ok   $name"; else say "FAIL $name"; fail=1; fi
}

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
out="$tmp/events.jsonl"

# 1. invented Claude-style payload with secret-looking values
payload='{"session_id":"sess-INVENTED-0001","hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"echo INVENTED_SECRET_VALUE_42","api_token":"tok-INVENTED-9f8e","n":3,"ok":true,"z":null,"list":[{"k":"INVENTED_V"},2,"INVENTED_S","x","y"]}}'
stdout="$(printf '%s' "$payload" | KBN_CAPTURE_FILE="$out" bash "$capture" claude-code 2>"$tmp/err1")"
rc=$?
check "exit 0 on a valid payload" test "$rc" -eq 0
check "stdout empty on a valid payload" test -z "$stdout"
check "capture file has one line" test "$(wc -l < "$out")" -eq 1
check "no payload value survives (INVENTED)" bash -c '! grep -q INVENTED "$1"' _ "$out"
check "no payload value survives (literal command text)" bash -c '! grep -q "echo " "$1"' _ "$out"
check "host and event recorded" jq -e '.host=="claude-code" and .event=="PreToolUse"' "$out"
check "types reduced, keys kept" jq -e '.shape.session_id=="<string>" and .shape.tool_input.command=="<string>" and .shape.tool_input.n=="<number>" and .shape.tool_input.ok=="<bool>" and .shape.tool_input.z=="<null>"' "$out"
check "secret-looking key listed with type only" jq -e '.shape.tool_input.api_token=="<string>"' "$out"
check "arrays reduced element-wise and capped" jq -e '(.shape.tool_input.list|type)=="array" and .shape.tool_input.list[0].k=="<string>" and .shape.tool_input.list[1]=="<number>" and (.shape.tool_input.list|length)<=4 and (.shape.tool_input.list|last|tostring|test("more"))' "$out"
check "byte count recorded as a number" jq -e '(.bytes|type)=="number" and .bytes>0' "$out"

# 2. non-JSON payload
stdout="$(printf 'not json {{{ INVENTED_RAW_TEXT' | KBN_CAPTURE_FILE="$out" bash "$capture" codex PostToolUse 2>"$tmp/err2")"
rc=$?
check "exit 0 on a non-JSON payload" test "$rc" -eq 0
check "stdout empty on a non-JSON payload" test -z "$stdout"
check "non-JSON payload recorded as <unparsed>, hint used as event" bash -c 'tail -1 "$1" | jq -e ".shape==\"<unparsed>\" and .event==\"PostToolUse\" and .host==\"codex\"" >/dev/null' _ "$out"
check "non-JSON payload text never written" bash -c '! grep -q INVENTED_RAW "$1"' _ "$out"

# 3. empty stdin
stdout="$(printf '' | KBN_CAPTURE_FILE="$out" bash "$capture" pi tool_execution_end 2>"$tmp/err3")"
rc=$?
check "exit 0 on empty stdin" test "$rc" -eq 0
check "empty stdin recorded with bytes 0" bash -c 'tail -1 "$1" | jq -e ".bytes==0 and .event==\"tool_execution_end\"" >/dev/null' _ "$out"

# 5. control-field allowlist
payload='{"hook_event_name":"PostToolUse","tool_name":"Bash","tool_input":{"command":"INVENTED cmd"},"tool_response":"INVENTED line one\nExit code: 1\nINVENTED line three"}'
printf '%s' "$payload" | KBN_CAPTURE_FILE="$out" bash "$capture" codex >/dev/null 2>&1
check "identifier tool_name recorded as tool" bash -c 'tail -1 "$1" | jq -e ".tool==\"Bash\"" >/dev/null' _ "$out"
check "string tool_response yields markers only" bash -c 'tail -1 "$1" | jq -e ".response_markers.exit_code_marker==true and .response_markers.lines==3 and (.response_markers|has(\"text\")|not)" >/dev/null' _ "$out"
check "no value leaks through the allowlist" bash -c '! tail -1 "$1" | grep -q INVENTED' _ "$out"
# R2 format markers (W2-D2): fixed-vocabulary booleans that settle a Bash response format
payload='{"hook_event_name":"PostToolUse","tool_name":"Bash","tool_input":{"command":"INVENTED cmd"},"tool_response":"Process exited with code 1 INVENTED tail"}'
printf '%s' "$payload" | KBN_CAPTURE_FILE="$out" bash "$capture" codex >/dev/null 2>&1
check "'exited with code N' marker is reported as booleans only" bash -c 'tail -1 "$1" | jq -e ".response_markers.exited_with_code_marker==true and .response_markers.starts_exit_code==false and .response_markers.empty==false and .response_markers.exit_code_marker==false" >/dev/null && ! tail -1 "$1" | grep -q INVENTED' _ "$out"
payload='{"hook_event_name":"PostToolUse","tool_name":"Bash","tool_input":{"command":"INVENTED cmd"},"tool_response":""}'
printf '%s' "$payload" | KBN_CAPTURE_FILE="$out" bash "$capture" codex >/dev/null 2>&1
check "an empty string response is reported as empty" bash -c 'tail -1 "$1" | jq -e ".response_markers.empty==true and .response_markers.lines==1 and .response_markers.chars==0 and .response_markers.exited_with_code_marker==false" >/dev/null' _ "$out"
payload='{"hook_event_name":"PostToolUse","tool_name":"Bash","tool_input":{"command":"INVENTED cmd"},"tool_response":"Exit code: 2\nWall time: 0.1 seconds\nTotal output lines: 0\nOutput:\nINVENTED"}'
printf '%s' "$payload" | KBN_CAPTURE_FILE="$out" bash "$capture" codex >/dev/null 2>&1
check "a leading 'Exit code:' line sets starts_exit_code" bash -c 'tail -1 "$1" | jq -e ".response_markers.starts_exit_code==true and .response_markers.exit_code_marker==true and .response_markers.words.wall_time==true and .response_markers.total_output_lines==true" >/dev/null && ! tail -1 "$1" | grep -q INVENTED' _ "$out"

payload='{"hook_event_name":"PreToolUse","tool_name":"not an identifier INVENTED-TOKEN","tool_input":{}}'
printf '%s' "$payload" | KBN_CAPTURE_FILE="$out" bash "$capture" codex >/dev/null 2>&1
check "non-identifier tool_name is dropped" bash -c 'tail -1 "$1" | jq -e "(.tool==null) or (.tool==\"\")" >/dev/null && ! tail -1 "$1" | grep -q INVENTED' _ "$out"
payload='{"type":"tool_execution_end","toolName":"bash","isError":true,"result":{"content":[{"type":"text","text":"INVENTED"}]}}'
printf '%s' "$payload" | KBN_CAPTURE_FILE="$out" bash "$capture" pi >/dev/null 2>&1
check "pi toolName recorded as tool, type used as event" bash -c 'tail -1 "$1" | jq -e ".tool==\"bash\" and .event==\"tool_execution_end\"" >/dev/null && ! tail -1 "$1" | grep -q INVENTED' _ "$out"
# RL-1: the identifier rule is a whole-value rule; a trailing newline (which jq's `$` would accept) or
# a valid first line followed by anything else is not an identifier
payload='{"hook_event_name":"PreToolUse","tool_name":"Bash\n","tool_input":{}}'
printf '%s' "$payload" | KBN_CAPTURE_FILE="$out" bash "$capture" claude-code >/dev/null 2>&1
check "tool_name with a trailing newline is dropped (whole-value identifier rule)" bash -c 'tail -1 "$1" | jq -e "has(\"tool\") | not" >/dev/null' _ "$out"
payload='{"hook_event_name":"PreToolUse","tool_name":"Bash\nINVENTED-SECOND-LINE","tool_input":{}}'
printf '%s' "$payload" | KBN_CAPTURE_FILE="$out" bash "$capture" claude-code >/dev/null 2>&1
check "tool_name with a valid first line and a second line is dropped" bash -c 'tail -1 "$1" | jq -e "has(\"tool\") | not" >/dev/null && ! tail -1 "$1" | grep -q INVENTED' _ "$out"

# 7. closed event vocabulary (R4-1): event names and hints outside hooks/HOST-EVENTS.md never leave
payload='{"session_id":"s","hook_event_name":"INVENTED_CustomEvent_x9","tool_name":"Bash"}'
printf '%s' "$payload" | KBN_CAPTURE_FILE="$out" bash "$capture" claude-code INVENTED_HintEvent_y7 >/dev/null 2>&1
check "unknown hook_event_name exports as other with event_unknown" bash -c 'tail -1 "$1" | jq -e ".event==\"other\" and .event_unknown==true and .hint==\"other\"" >/dev/null && ! tail -1 "$1" | grep -q INVENTED' _ "$out"
payload='{"type":"INVENTED_pi_event","toolName":"bash"}'
printf '%s' "$payload" | KBN_CAPTURE_FILE="$out" bash "$capture" pi >/dev/null 2>&1
check "unknown pi type exports as other with event_unknown" bash -c 'tail -1 "$1" | jq -e ".event==\"other\" and .event_unknown==true" >/dev/null && ! tail -1 "$1" | grep -q INVENTED' _ "$out"
payload='{"session_id":"s","hook_event_name":"PostToolUseFailure","tool_name":"Bash"}'
printf '%s' "$payload" | KBN_CAPTURE_FILE="$out" bash "$capture" claude-code PostToolUseFailure >/dev/null 2>&1
check "vocabulary event exported as is, no event_unknown marker" bash -c 'tail -1 "$1" | jq -e ".event==\"PostToolUseFailure\" and .hint==\"PostToolUseFailure\" and (has(\"event_unknown\")|not)" >/dev/null' _ "$out"
# a Claude event name is not pi vocabulary (and the other way round): the vocabulary is per host
payload='{"type":"PostToolUse"}'
printf '%s' "$payload" | KBN_CAPTURE_FILE="$out" bash "$capture" pi >/dev/null 2>&1
check "vocabulary is per host (Claude name under pi is other)" bash -c 'tail -1 "$1" | jq -e ".event==\"other\" and .event_unknown==true" >/dev/null' _ "$out"
payload='{"type":"kbn_stop_probe_cannot_continue"}'
printf '%s' "$payload" | KBN_CAPTURE_FILE="$out" bash "$capture" pi >/dev/null 2>&1
check "harness marker events are pi vocabulary" bash -c 'tail -1 "$1" | jq -e ".event==\"kbn_stop_probe_cannot_continue\" and (has(\"event_unknown\")|not)" >/dev/null' _ "$out"

# 6. stderr of the hook never carries payload values either
check "stderr carries no payload values" bash -c '! cat "$1"/err1 "$1"/err2 "$1"/err3 | grep -q INVENTED' _ "$tmp"

if [ "$fail" -eq 0 ]; then say "ALL OK"; else say "FAILURES"; fi
exit "$fail"
