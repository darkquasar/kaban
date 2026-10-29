#!/usr/bin/env bash
# End-to-end S1 fence test for everything the harness writes under /out (M4 R4-1, RL-1..RL-3, OWN-7).
#
#   bash scripts/hooks-live/test-out-fence.sh [--kaban <static kaban binary>] [--out <dir>]
#                                             [--hosts "claude-code codex pi"]
#
# Needs Docker and the image kaban-hooks-live:m4; no model call and no credentials are used:
# every session is a dry run (KBN_DRY_CMD replaces the host CLI by a shell command inside the
# container). For each host the dry command plants invented canaries where host data normally
# enters the harness:
#   prompt           --prompt text (run.log)
#   filename         a file created in the scratch repo (work-ls)
#   event name       hook_event_name / pi type in a crafted payload and the hint argv of
#                    capture.sh (as a custom event in a rendered config would pass it)
#   stderr           a hook that prints to stderr and the host's own stderr (stderr classes)
#   tool response    tool_response / result.content[].text in a crafted payload
#   host error       an error item in the host's JSON stream (host errors)
#   assistant text   a text item and a tool-use name in the stream (verdict)
#   non-JSON line    (pi) a raw text line in the host's stream (transcript shape, tool counts)
#   multiline values (RL-1) a tool name whose first line is a valid identifier and whose second
#                    line is a canary (`bash\nKBNCANARY-TOOL-…`), and a Claude result subtype whose
#                    first line is a valid enum member (`success\nKBNCANARY-SUBTYPE-…`)
#   run metadata     (RL-2) canaries in the session name, --variant and --model (run.log, verdict)
#   kaban argv       (recipes mode, --kaban given) an unknown flag and a free argument of a kaban
#                    call, and a Failure recorded with canary command/error text (kaban-calls,
#                    recipes-verdict)
# After each run every file under the session's /out directory and the run.sh console are
# searched for every canary: the test passes only with zero hits. Structural checks confirm the
# reduced files exist (counts, booleans, hashes, fixed vocabulary), and the observed-input checks
# (RL-3) assert that every planted input was seen: exact event counts (known and other), positive
# stderr and host-error class counts, the planted file, the prompt hash and length, the exact
# tool-use counts and the reduced run metadata. An empty capture or a no-op session fails.
#
# Tool names planted here are NOT bare identifiers on purpose: a tool name matching
# ^[A-Za-z_][A-Za-z0-9_.:-]{0,40}$ is matcher vocabulary and travels by design (W2-D1
# deviation, kept by R4-1); the identifier rule itself is covered by test-capture.sh.
set -u
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
kaban_bin=""; out_root=/tmp/m4corr2/fence-out; hosts="claude-code codex pi"
while [ $# -gt 0 ]; do
  case "$1" in
    --kaban) kaban_bin="$2"; shift 2 ;;
    --out) out_root="$2"; shift 2 ;;
    --hosts) hosts="$2"; shift 2 ;;
    *) echo "unknown option $1" >&2; exit 2 ;;
  esac
done
command -v docker >/dev/null || { echo "docker not available" >&2; exit 2; }
fail=0
say() { printf '%s\n' "$*"; }
check() { # check <name> <cmd...>
  local name="$1"; shift
  if "$@" >/dev/null 2>&1; then say "ok   $name"; else say "FAIL $name"; fail=1; fi
}
hex() { head -c 4 /dev/urandom | od -An -tx1 | tr -d ' \n'; }
sha16() { printf '%s' "$1" | sha256sum | cut -c1-16; }
stamp="$(date -u +%H%M%S)"
mkdir -p "$out_root"

# one dry-run session; prints nothing but the check lines
run_session() { # run_session <host> <session> <dry command> [run.sh options...]
  local host="$1" session="$2" dry="$3"; shift 3
  local dir="$out_root/${host}-${session}" console="$out_root/${host}-${session}.console"
  rm -rf "$dir"
  KBN_DRY_CMD="$dry" "$here/run.sh" "$host" "$session" --out "$out_root" "$@" > "$console" 2>&1
  local rc=$?
  check "$host/$session: run.sh exit 0 (got $rc)" test "$rc" -eq 0
  check "$host/$session: verdict.json written" test -s "$dir/verdict.json"
}
sweep() { # sweep <host> <session> <canary>...
  local host="$1" session="$2"; shift 2
  local dir="$out_root/${host}-${session}" console="$out_root/${host}-${session}.console" c hits
  for c in "$@"; do
    hits="$(grep -rF -- "$c" "$dir" "$console" 2>/dev/null | wc -l | tr -d ' ')"
    check "$host/$session: canary '$c' absent from /out and console (hits $hits)" test "$hits" -eq 0
  done
  # a canary split into words or lines (a multi-word or multiline tool name) must not leave either
  hits="$(grep -rF -- KBNCANARY "$dir" "$console" 2>/dev/null | wc -l | tr -d ' ')"
  check "$host/$session: no KBNCANARY token anywhere under /out or the console (hits $hits)" test "$hits" -eq 0
}
structure() { # structure <host> <session>
  local host="$1" session="$2" dir="$out_root/${host}-${session}"
  check "$host/$session: run.log carries the prompt hash and length, not the text" \
    bash -c 'grep -qE "prompt_sha256=[0-9a-f]{16} prompt_chars=[0-9]+" "$1/run.log"' _ "$dir"
  check "$host/$session: stderr-classes.json is counts and fixed vocabulary" \
    jq -e '(.lines|type)=="number" and (.chars|type)=="number" and (.classes|type)=="object" and ([.classes|keys[]] - ["hook_trust_warning","timeout_clamp","auth_error","rate_limit","network_error","node_warning","result_status_line","empty","other"] | length == 0)' "$dir/stderr-classes.json"
  check "$host/$session: host-errors.json is counts and fixed vocabulary" \
    jq -e '(.items|type)=="number" and (.classes|type)=="object"' "$dir/host-errors.json"
  check "$host/$session: work-ls.json is counts and booleans" \
    jq -e '(.entries|type)=="number" and (.files_created|type)=="number" and (.notes_txt_exists|type)=="boolean" and (has("names")|not)' "$dir/work-ls.json"
  check "$host/$session: no legacy free-text exports left" \
    bash -c '[ ! -e "$1/stderr-scrubbed.txt" ] && [ ! -e "$1/host-errors.txt" ] && [ ! -e "$1/work-ls.txt" ]' _ "$dir"
  check "$host/$session: host-config-shape.json events are vocabulary or other" \
    bash -c 'jq -e --slurpfile v "$2" --arg h "$3" "(.events|type)==\"object\" and ([.events|keys[]] - (\$v[0][\$h] + [\"other\"]) | length == 0)" "$1/host-config-shape.json"' _ "$dir" "$here/container/events-vocab.json" "$host"
  check "$host/$session: every exported event name is vocabulary or other" \
    jq -e -s --slurpfile v "$here/container/events-vocab.json" --arg h "$host" \
      'all(.[]; .event as $e | $e == "other" or (($v[0][$h] | index($e)) != null)) and all(.[]; .event != "other" or .event_unknown == true)' "$dir/events.jsonl"
}
# the planted inputs were observed (RL-3): exact counts, never a vacuous pass over an empty capture
observed() { # observed <host> <session> <events> <known event> <stderr lines> <host-error items> <transcript lines> <assistant chars> <tool_uses json> <prompt> <variant> <model>
  local host="$1" session="$2" events="$3" known="$4" stderr_lines="$5" err_items="$6" tlines="$7" achars="$8" tools="$9" prompt="${10}" variant="${11}" model="${12}"
  local dir="$out_root/${host}-${session}"
  check "$host/$session: events.jsonl has exactly $events lines, one other (event_unknown) and one $known" \
    jq -e -s --argjson n "$events" --arg k "$known" \
      'length == $n and ([.[] | select(.event == "other" and .event_unknown == true)] | length) == (if $n > 0 then 1 else 0 end) and ([.[] | select(.event == $k)] | length) == (if $n > 0 then 1 else 0 end)' "$dir/events.jsonl"
  check "$host/$session: stderr-classes.json counts the $stderr_lines planted stderr lines as other" \
    jq -e --argjson n "$stderr_lines" '.lines == $n and .classes.other == $n and .chars > 0' "$dir/stderr-classes.json"
  check "$host/$session: host-errors.json counts the $err_items planted error items as other" \
    jq -e --argjson n "$err_items" '.items == $n and .classes.other == $n and .chars > 0' "$dir/host-errors.json"
  if [ "$host" = claude-code ]; then
    check "$host/$session: the Claude result item is present, is_error, and its multiline subtype is other" \
      jq -e '.result.present == true and .result.is_error == true and .result.num_turns == 1 and .result.subtype == "other"' "$dir/host-errors.json"
  fi
  check "$host/$session: work-ls.json saw the planted file (files_created 1, no notes.txt)" \
    jq -e '.files_created == 1 and .notes_txt_exists == false and .files >= 2' "$dir/work-ls.json"
  check "$host/$session: run.log carries exactly the planted prompt's sha256 prefix and length" \
    grep -qF -- "prompt_sha256=$(sha16 "$prompt") prompt_chars=${#prompt}" "$dir/run.log"
  check "$host/$session: run.log reduces session, variant and model (sha256 prefix and length, closed enums)" \
    grep -qE -- "^[0-9:]{8} host=$host session_sha256=$(sha16 "$session") session_chars=${#session} mode=(capture|recipes) stop_mode=none variant=$variant model=$model$" "$dir/run.log"
  check "$host/$session: verdict.json observed the session (counts, exact tool_uses, reduced metadata)" \
    jq -e --argjson n "$events" --argjson t "$tlines" --argjson a "$achars" --argjson tools "$tools" \
      --arg s "$(sha16 "$session")" --argjson sc "${#session}" --arg v "$variant" --arg m "$model" \
      '.exit_code == 0 and .hook_events_captured == $n and .transcript_lines == $t and .assistant_chars == $a and .tool_uses == $tools
       and .session == {sha256: $s, chars: $sc} and .variant == $v and .model == $m and .stop_mode == "none"' "$dir/verdict.json"
}

for host in $hosts; do
  h="$(hex)"
  P="KBNCANARY-PROMPT-$h"; F="KBNCANARY-FILE-$h"; E="KBNCANARY-EVENT-$h"; HNT="KBNCANARY-HINT-$h"
  S="KBNCANARY-STDERR-$h"; R="KBNCANARY-RESPONSE-$h"; HE="KBNCANARY-HOSTERR-$h"; T="KBNCANARY-TEXT-$h"
  TN="KBNCANARY TOOL $h"; CMDC="KBNCANARY-CMD-$h"; RAW="KBNCANARY-RAW-$h"
  # RL-1: a valid first line, a canary second line (literal \n: a JSON escape inside the stream)
  TNL="bash\\nKBNCANARY-TOOL-$h"; SUB="success\\nKBNCANARY-SUBTYPE-$h"
  # RL-2: run metadata the operator supplies
  session="fence$stamp-KBNCANARY-SESSION-$h"; VAR="KBNCANARY-VARIANT-$h"; MOD="KBNCANARY-MODEL-$h"
  prompt="$P please create a file"
  case "$host" in
    claude-code)
      known=PostToolUse; tlines=3; tools='{"other":2}'; err_items=2
      dry="H=/harness/container; printf 'x\n' > /work/'$F'; echo '$S' >&2; bash -c \"echo '$S' >&2\";
        printf '%s' '{\"session_id\":\"s1\",\"hook_event_name\":\"$E\",\"tool_name\":\"$TN\",\"tool_input\":{\"command\":\"$CMDC\"},\"tool_response\":\"$R\"}' | \$H/capture.sh claude-code '$HNT';
        printf '%s' '{\"session_id\":\"s1\",\"hook_event_name\":\"PostToolUse\",\"tool_name\":\"Bash\",\"tool_input\":{\"command\":\"$CMDC\"},\"tool_response\":\"$R\"}' | \$H/capture.sh claude-code PostToolUse;
        printf '%s\n' '{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"$T\"},{\"type\":\"tool_use\",\"name\":\"$TN\",\"input\":{\"command\":\"$CMDC\"}},{\"type\":\"tool_use\",\"name\":\"$TNL\",\"input\":{}}]}}';
        printf '%s\n' '{\"type\":\"result\",\"subtype\":\"error_during_execution\",\"is_error\":true,\"num_turns\":1,\"result\":\"$HE\"}';
        printf '%s\n' '{\"type\":\"result\",\"subtype\":\"$SUB\",\"is_error\":true,\"num_turns\":1,\"result\":\"$HE\"}'; exit 0" ;;
    codex)
      known=PostToolUse; tlines=5; tools='{"agent_message":1,"error":1,"other":2}'; err_items=2
      dry="H=/harness/container; printf 'x\n' > /work/'$F'; echo '$S' >&2; bash -c \"echo '$S' >&2\";
        printf '%s' '{\"session_id\":\"s1\",\"hook_event_name\":\"$E\",\"tool_name\":\"$TN\",\"tool_input\":{\"command\":\"$CMDC\"},\"tool_response\":\"$R\"}' | \$H/capture.sh codex '$HNT';
        printf '%s' '{\"session_id\":\"s1\",\"hook_event_name\":\"PostToolUse\",\"tool_name\":\"Bash\",\"tool_input\":{\"command\":\"$CMDC\"},\"tool_response\":\"$R\"}' | \$H/capture.sh codex PostToolUse;
        printf '%s\n' '{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"$T\"}}';
        printf '%s\n' '{\"type\":\"item.completed\",\"item\":{\"type\":\"$TN\",\"text\":\"$T\"}}';
        printf '%s\n' '{\"type\":\"item.completed\",\"item\":{\"type\":\"$TNL\",\"text\":\"$T\"}}';
        printf '%s\n' '{\"type\":\"error\",\"message\":\"$HE\"}';
        printf '%s\n' '{\"type\":\"item.completed\",\"item\":{\"type\":\"error\",\"message\":\"$HE\"}}'; exit 0" ;;
    pi)
      known=tool_execution_end; tlines=3; tools='{"other":2}'; err_items=1
      dry="H=/harness/container; printf 'x\n' > /work/'$F'; echo '$S' >&2; bash -c \"echo '$S' >&2\";
        printf '%s' '{\"type\":\"$E\",\"toolName\":\"$TN\",\"isError\":true,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"$R\"}]}}' | \$H/capture.sh pi '$HNT';
        printf '%s' '{\"type\":\"tool_execution_end\",\"toolName\":\"bash\",\"isError\":true,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"$R\"}]}}' | \$H/capture.sh pi tool_execution_end;
        printf '%s\n' '{\"type\":\"message\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"$T\"},{\"type\":\"toolCall\",\"name\":\"$TN\",\"arguments\":{\"command\":\"$CMDC\"}},{\"type\":\"toolCall\",\"name\":\"$TNL\",\"arguments\":{}}]}}';
        printf '%s\n' '{\"type\":\"error\",\"error\":\"$HE\"}';
        printf '%s\n' 'not json: $RAW'; exit 0" ;;
  esac
  run_session "$host" "$session" "$dry" --prompt "$prompt" --variant "$VAR" --model "$MOD"
  sweep "$host" "$session" "$P" "$F" "$E" "$HNT" "$S" "$R" "$HE" "$T" "$TN" "$CMDC" "$RAW" \
    "KBNCANARY-TOOL-$h" "KBNCANARY-SUBTYPE-$h" "KBNCANARY-SESSION-$h" "$VAR" "$MOD"
  structure "$host" "$session"
  observed "$host" "$session" 2 "$known" 2 "$err_items" "$tlines" $(( ${#T} + 1 )) "$tools" "$prompt" other other
done

# recipes mode (the shim, recipes-verdict.json and the kaban-calls log), one host, no model
if [ -n "$kaban_bin" ]; then
  h="$(hex)"
  P="KBNCANARY-PROMPT-$h"; F="KBNCANARY-FILE-$h"; S="KBNCANARY-STDERR-$h"; CMDC="KBNCANARY-CMD-$h"
  ERR="KBNCANARY-ERR-$h"; FLAG="--KBNCANARY-FLAG-$h"; ARG="KBNCANARY-ARG-$h"; HE="KBNCANARY-HOSTERR-$h"
  # the session canary again; variant and model are valid enum members here, so a mapping that
  # always printed "other" would fail this run
  session="fencer$stamp-KBNCANARY-SESSION-$h"
  dry="printf 'x\n' > /work/'$F'; echo '$S' >&2;
    kaban --json recall '$ARG' --kind Failure '$FLAG' >/dev/null 2>&1;
    kaban --json recall '$ARG' --limit 1 >/dev/null 2>&1;
    printf '%s' '{\"session_id\":\"s1\",\"cwd\":\"/work\",\"hook_event_name\":\"tool_execution_end\",\"toolName\":\"bash\",\"isError\":true,\"input\":{\"command\":\"$CMDC\"},\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"$ERR\"}]}}' | kaban hook post-tool-use --host pi --stdin-json --json >/dev/null 2>&1;
    printf '%s' '{\"session_id\":\"s1\",\"cwd\":\"/work\",\"hook_event_name\":\"agent_before_settle\"}' | kaban hook stop --host pi --stdin-json --json >/dev/null 2>&1;
    printf '%s\n' '{\"type\":\"error\",\"error\":\"$HE\"}'; exit 0"
  run_session pi "$session" "$dry" --prompt "$P" --recipes --kaban "$kaban_bin" --variant hooks-json --model gpt-5.6-sol
  sweep pi "$session" "$P" "$F" "$S" "$CMDC" "$ERR" "$FLAG" "$ARG" "$HE" "KBNCANARY-SESSION-$h"
  structure pi "$session"
  observed pi "$session" 0 tool_execution_end 1 1 1 0 '{}' "$P" hooks-json gpt-5.6-sol
  dir="$out_root/pi-$session"
  check "pi/$session: the planted Failure is reported as booleans (recorded, command and error present)" \
    jq -e '.failure_recorded==true and .recall.count >= 1 and (.recall.failures|any(.has_command and .has_error_text))' "$dir/recipes-verdict.json"
  check "pi/$session: kaban-calls argv words are vocabulary only (unknown flag counted, not kept)" \
    bash -c '! grep -q KBNCANARY "$1/kaban-calls.jsonl" && jq -e "select(.argv[1]==\"recall\") | .free_args >= 1" "$1/kaban-calls.jsonl" | grep -q true' _ "$dir"
  check "pi/$session: kaban-calls saw the seed (remember, accept) and the four planted calls (two recall, two hook); the verdict counts the two hook calls" \
    bash -c 'jq -e -s "([.[] | select(.argv[1]==\"remember\")] | length) == 1 and ([.[] | select(.argv[1]==\"accept\")] | length) == 1 and ([.[] | select(.argv[1]==\"recall\")] | length) == 2 and ([.[] | select(.argv[0]==\"hook\")] | length) == 2" "$1/kaban-calls.jsonl" && jq -e ".hook_calls == 2 and .hook_calls_by_event == {\"post-tool-use\": 1, \"stop\": 1}" "$1/recipes-verdict.json"' _ "$dir"
fi

if [ "$fail" -eq 0 ]; then say "ALL OK"; else say "FAILURES"; fi
exit "$fail"
