#!/usr/bin/env bash
# Session driver, runs INSIDE the capture container (M4 W2-D1 capture mode, W2-D2 recipes mode; OWN-7).
#
# Env (set by run.sh): KBN_HOST (claude-code|codex|pi), KBN_SESSION (name), KBN_STOP_MODE
# (none|block|system|both), KBN_PROMPT, KBN_MODEL (optional), KBN_HOST_TIMEOUT (s, default 540),
# KBN_VARIANT (host-specific switch, see below), KBN_MODE (capture|recipes).
#
# Recipes mode (round 2, M5 C5.3): `kaban` is on PATH (the logging shim in front of the static binary),
# a database is seeded in the scratch repo with the CLI (one Decision carrying the sentinel
# KBN-DEC-<hex> with path scope notes.txt; it creates the database, which hooks never do, and is
# never briefed or injected), the REAL recipe configs from /work/hooks are rendered for the host
# with the capture hook kept alongside (sentinel probes off), the prompt runs, and `kaban recall`
# plus the shim's call log produce the reduced verdict (booleans and counts only, S1 fence).
# Mounts: /harness (ro: scripts/hooks-live; this file is /harness/container/entrypoint.sh), $HOME/.kbn-creds-ro (ro copies of the
# sandbox auth state), /work (tmpfs scratch repo), /capture (tmpfs), /out (reduced results only).
#
# Everything the host writes (its home, the scratch repo, raw transcript, raw hook payloads)
# lives in tmpfs and dies with the container. Only reduced artefacts are copied to /out.
#
# S1 fence on /out (R4-1, RL-1, RL-2): every string that leaves is fixed vocabulary, a type
# marker, an identifier matching the capture.sh tool regex, a hash prefix, a count or a boolean.
# The prompt is logged as sha256 prefix and length; stderr and host diagnostics become per-line
# class counts (classify_lines); the scratch listing becomes counts and booleans; event names
# pass the closed per-host vocabulary (events-vocab.json). Every enum comparison is on the WHOLE
# value (enum_of: literal members, no line-oriented grep), tool names are validated as complete
# JSON strings (tool_counts), and the run metadata the operator supplies is reduced too: the
# session as sha256 prefix and length, variant and model as closed enums, versions as digits.
# Tested end to end by test-out-fence.sh.
set -u
HARNESS=/harness/container
TPL=/harness/templates
CREDS="$HOME/.kbn-creds-ro"
OUT=/out
CAP=/capture
WORK=/work
HOST="${KBN_HOST:?}"
SESSION="${KBN_SESSION:?}"
STOP_MODE="${KBN_STOP_MODE:-none}"
PROMPT="${KBN_PROMPT:-Create the file notes.txt with one line, then run the command \`false\`, then stop.}"
HOST_TIMEOUT="${KBN_HOST_TIMEOUT:-540}"
VARIANT="${KBN_VARIANT:-}"
MODE="${KBN_MODE:-capture}"
export KBN_CAPTURE_FILE="$CAP/events.jsonl"
export KBN_STOP_MODE="$STOP_MODE"
export KBN_KABAN_CALLS="$CAP/kaban-calls.jsonl"
if [ "$MODE" = recipes ]; then
  # the same variables the recipes tell users to share with the MCP server entry
  export KABAN_HOME="$HOME/.kaban" KABAN_DB="$WORK/kaban.db" KABAN_POLICY=/policy.yaml
  export KBN_PROBES=off KABAN_HOOK_DEBUG=1
fi

log() { printf '%s %s\n' "$(date -u +%T)" "$*" | tee -a "$OUT/run.log" >&2; }
log_line() { while IFS= read -r l; do log "$l"; done; }
rand8() { head -c 4 /dev/urandom | od -An -tx1 | tr -d ' \n'; }
render() { # render <template> <dest>
  sed -e "s#__HARNESS__#$HARNESS#g" -e "s#__HOST__#$HOST#g" "$1" > "$2"
}
VOCAB="$HARNESS/events-vocab.json"
IDENT='^[A-Za-z_][A-Za-z0-9_.:-]{0,40}$'   # the capture.sh tool identifier rule
# enum_of <value> <member>...: the value when the WHOLE string equals one literal member, else
# "other" (RL-1: never a line-oriented predicate; `success\nX` is not `success`)
enum_of() {
  local v="$1" m; shift
  for m in "$@"; do [ "$v" = "$m" ] && { printf '%s' "$v"; return; }; done
  printf other
}
# the closed vocabularies of the run metadata (RL-2); anything else is exported as "other"
MODELS=(sonnet opus haiku gpt-5.6-sol gpt-6.1-sol)
VARIANTS=(hooks-json config-toml project-hooks features)
STOP_MODES=(none block system both)
KABAN_STATUSES=(created existing accepted proposed none superseded invalidated)
version_of() { # version_of <cmd...>: the first x.y[.z] the command prints, digits and dots only (no tags), else "?"
  "$@" 2>/dev/null | grep -oE '[0-9]+\.[0-9]+(\.[0-9]+)?' | head -1 | grep . || printf '?'
}
sha16() { printf '%s' "$1" | sha256sum | cut -c1-16; }
# classify_lines <file> -> {lines, chars, classes:{<fixed class>: count}}: one class per line from a
# fixed vocabulary, no text. Used for the host's stderr and for host diagnostic items.
classify_lines() {
  awk '
    { n++; chars += length($0)
      if ($0 ~ /^[[:space:]]*$/) c = "empty"
      else if ($0 ~ /dangerously-bypass-hook-trust|[Hh]ooks? .*(trust|review)/) c = "hook_trust_warning"
      else if ($0 ~ /clamping .*hook timeout/) c = "timeout_clamp"
      else if ($0 ~ /DeprecationWarning|ExperimentalWarning|\(node:[0-9]+\)|--trace-warnings/) c = "node_warning"
      else if ($0 ~ /^result subtype=/) c = "result_status_line"
      else if ($0 ~ /(^|[^0-9])(401|403)([^0-9]|$)|[Uu]nauthori[sz]ed|[Aa]uthenticat|[Ll]og ?in|invalid_api_key|[Cc]redential|OAuth|[Tt]oken expired/) c = "auth_error"
      else if ($0 ~ /(^|[^0-9])429([^0-9]|$)|[Rr]ate.?limit|[Qq]uota|overloaded/) c = "rate_limit"
      else if ($0 ~ /ECONNREFUSED|ENOTFOUND|ETIMEDOUT|ECONNRESET|EAI_AGAIN|[Pp]roxy|fetch failed|[Nn]etwork|(^|[^0-9])(502|503|504)([^0-9]|$)/) c = "network_error"
      else c = "other"
      cls[c]++ }
    END { printf "{\"lines\":%d,\"chars\":%d,\"classes\":{", n, chars
          sep = ""; for (k in cls) { printf "%s\"%s\":%d", sep, k, cls[k]; sep = "," }
          print "}}" }' "$1"
}
# config_shape <hooks json file> -> reduced host hook config: per event (vocabulary or "other") the
# number of hook commands, entry counts and where the commands point (harness, recipe, other)
config_shape() {
  jq -c --slurpfile v "$VOCAB" --arg h "$HOST" '
    (.hooks // {}) as $hooks
    | {events: ($hooks | to_entries
                 | map(.key as $k | {key: (if (($v[0][$h] // []) | index($k)) != null then $k else "other" end),
                                    value: ([.value[]?.hooks[]?] | length)})
                 | group_by(.key) | map({(.[0].key): (map(.value) | add)}) | add // {}),
       entries: ([$hooks[]?[]?] | length),
       hook_commands: ([$hooks[]?[]?.hooks[]?] | length),
       commands_harness: ([$hooks[]?[]?.hooks[]?.command | select(test("/harness/container/(capture|brief|pre-probe|stop-probe)\\.sh( |$)"))] | length),
       commands_recipe: ([$hooks[]?[]?.hooks[]?.command | select(test("hooks/(claude-code|kaban)/[a-z-]+\\.sh"))] | length)}
    | .commands_other = (.hook_commands - .commands_harness - .commands_recipe)' "$1"
}
# tool_counts: stdin a JSON array of tool names -> {<identifier or "other">: count}. Each complete
# string is validated (RL-1): a name is never split into lines, and a newline anywhere (jq's `$`
# would accept one trailing newline) makes it "other".
tool_counts() {
  jq -c --arg re "$IDENT" 'if type == "array" then . else [] end
    | map(if type == "string" and (contains("\n") | not) and test($re) then . else "other" end)
    | group_by(.) | map({(.[0]): length}) | add // {}'
}
# tool_names <jq filter>: the transcript's tool names as ONE JSON array of complete strings (each
# line parsed on its own, a non-JSON line skipped; a name is a JSON string, never split into lines)
tool_names() {
  jq -R -c "fromjson? | $1" "$CAP/transcript.jsonl" 2>/dev/null | jq -s -c '.'
}

# the run metadata as exported (RL-2): the session only as sha256 prefix and length, the rest as
# closed enums (run.sh validates host, mode and stop mode; the container reduces again)
SESSION_SHA="$(sha16 "$SESSION")"; SESSION_CHARS=${#SESSION}
HOST_X="$(enum_of "$HOST" claude-code codex pi)"; MODE_X="$(enum_of "$MODE" capture recipes)"
STOP_X="$(enum_of "$STOP_MODE" "${STOP_MODES[@]}")"
VARIANT_X="$([ -n "$VARIANT" ] && enum_of "$VARIANT" "${VARIANTS[@]}" || printf none)"
MODEL_X="$([ -n "${KBN_MODEL:-}" ] && enum_of "$KBN_MODEL" "${MODELS[@]}" || printf default)"

mkdir -p "$OUT" "$CAP"
: > "$OUT/run.log"
log "host=$HOST_X session_sha256=$SESSION_SHA session_chars=$SESSION_CHARS mode=$MODE_X stop_mode=$STOP_X variant=$VARIANT_X model=$MODEL_X"
log "versions: claude $(version_of claude --version) | codex $(version_of codex --version) | pi $(version_of pi --version) | jq $(version_of jq --version)"

# sentinels (tmpfs only; the tokens themselves are harmless and are reported)
KBN_BRIEF="KBN-BRIEF-$(rand8)"; KBN_PRE="KBN-PRE-$(rand8)"; KBN_STOP="KBN-STOP-$(rand8)"
printf 'KBN_BRIEF=%s\nKBN_PRE=%s\nKBN_STOP=%s\n' "$KBN_BRIEF" "$KBN_PRE" "$KBN_STOP" > "$CAP/sentinel.env"
log "sentinels: $KBN_BRIEF $KBN_PRE $KBN_STOP"

# scratch repo
cd "$WORK" || exit 1
git init -q . && git config user.email live@example.invalid && git config user.name live
printf '# scratch\n' > README.md && git add README.md && git commit -q -m init
log "scratch repo ready"

# recipes mode: seed the database (the only writes that create it; hooks never do, M4-6) ------
KBN_DEC=""; SEED_ID=""
if [ "$MODE" = recipes ]; then
  mkdir -p "$KABAN_HOME"
  log "kaban: $(version_of kaban --version) at $(command -v kaban) (shim) over /opt/kaban/kaban"
  KBN_DEC="KBN-DEC-$(rand8)"
  seed="$(kaban --json remember Decision "$KBN_DEC: notes.txt holds exactly one line; never append a second line to it." \
            --path-scope notes.txt --importance 0.9 --props '{"rationale":"live harness seed (W2-D2)"}' 2>"$CAP/seed-err.txt")"
  SEED_ID="$(printf '%s' "$seed" | jq -r '.id // empty' 2>/dev/null)"
  if [ -n "$SEED_ID" ]; then
    log "seed: remember status=$(enum_of "$(printf '%s' "$seed" | jq -r '.status // "?"')" "${KABAN_STATUSES[@]}") db=$([ -f "$KABAN_DB" ] && echo present || echo absent)"
  else
    log "seed FAILED (remember printed no id; stderr lines: $(wc -l < "$CAP/seed-err.txt"))"
  fi
  printf 'kaban.db*\n' > .gitignore
  log "sentinel decision: $KBN_DEC"
fi
# capture-only entries of a template (the sentinel probes removed), for recipes mode
capture_only() { # capture_only <rendered template> -> stdout
  jq -c '.hooks |= with_entries(.value |= (map(.hooks |= map(select(.command | test("capture\\.sh")))) | map(select(.hooks | length > 0))))' "$1"
}
merge_hooks() { # merge_hooks <recipe json> <capture-only json> -> stdout: per event, recipe entries then capture entries
  jq -n -c --slurpfile r "$1" --slurpfile c "$2" \
    '{hooks: (reduce ((($r[0].hooks | keys) + ($c[0].hooks | keys)) | unique[]) as $k ({}; .[$k] = (($r[0].hooks[$k] // []) + ($c[0].hooks[$k] // []))))}'
}
toml_capture() { # toml_capture <event>... -> [[hooks.<E>]] tables calling capture.sh
  for e in "$@"; do
    printf '\n[[hooks.%s]]\n[[hooks.%s.hooks]]\ntype = "command"\ncommand = "%s/capture.sh %s %s"\ntimeout = 5\n' "$e" "$e" "$HARNESS" "$HOST" "$e"
  done
}

# host setup -----------------------------------------------------------------------------
case "$HOST" in
  claude-code)
    mkdir -p "$HOME/.claude"
    if [ -r "$CREDS/claude/credentials.json" ]; then
      install -m 0600 "$CREDS/claude/credentials.json" "$HOME/.claude/.credentials.json"
      log "claude credentials installed (copy, 0600)"
    else
      log "claude credentials copy missing"
    fi
    [ -r "$CREDS/claude/claude.json" ] && install -m 0600 "$CREDS/claude/claude.json" "$HOME/.claude.json"
    if [ "$MODE" = recipes ]; then
      render "$TPL/claude-settings.json" "$CAP/claude-capture.json"
      capture_only "$CAP/claude-capture.json" > "$CAP/claude-capture-only.json"
      merge_hooks "$WORK/hooks/claude-code/settings.hooks.json" "$CAP/claude-capture-only.json" > "$HOME/.claude/settings.json"
      log "claude hooks: real recipe settings.hooks.json (commands via \$CLAUDE_PROJECT_DIR/hooks/claude-code) + capture entries"
    else
      render "$TPL/claude-settings.json" "$HOME/.claude/settings.json"
    fi
    config_shape "$HOME/.claude/settings.json" > "$OUT/host-config-shape.json"
    CMD=(claude -p "$PROMPT" --output-format stream-json --verbose)
    [ -n "${KBN_MODEL:-}" ] && CMD+=(--model "$KBN_MODEL")
    ;;
  codex)
    mkdir -p "$CODEX_HOME"
    [ -r "$CREDS/codex/auth.json" ] && install -m 0600 "$CREDS/codex/auth.json" "$CODEX_HOME/auth.json" && log "codex auth installed (copy, 0600)"
    [ -r "$CREDS/codex/config.toml" ] && install -m 0600 "$CREDS/codex/config.toml" "$CODEX_HOME/config.toml" && log "codex config installed (reduced copy)"
    if [ "$MODE" = recipes ]; then
      # the user-level layout the README describes: scripts under $CODEX_HOME/hooks/kaban/
      mkdir -p "$CODEX_HOME/hooks/kaban"
      cp "$WORK"/hooks/codex/*.sh "$CODEX_HOME/hooks/kaban/"
      render "$TPL/codex-hooks.json" "$CAP/codex-capture.json"
      capture_only "$CAP/codex-capture.json" > "$CAP/codex-capture-only.json"
      case "$VARIANT" in
        config-toml)
          # steering (W2-D2): [hooks] tables appended to config.toml, no hooks.json at all
          { printf '\n# kaban hooks (recipe hooks/codex/config.hooks.toml)\n'; cat "$WORK/hooks/codex/config.hooks.toml";
            toml_capture SessionStart UserPromptSubmit PreToolUse PostToolUse SubagentStart SubagentStop Stop SessionEnd; } >> "$CODEX_HOME/config.toml"
          HOOKS_AT="$CODEX_HOME/config.toml"
          # the TOML as JSON (tmpfs), then the same reduced shape as the JSON registrations
          if python3 -c 'import tomllib,json,sys; print(json.dumps(tomllib.load(open(sys.argv[1],"rb"))))' "$HOOKS_AT" > "$CAP/config-toml.json" 2>/dev/null; then
            config_shape "$CAP/config-toml.json" > "$OUT/host-config-shape.json"
            log "config.toml parses: yes; hook events declared: $(jq -r '.events | length' "$OUT/host-config-shape.json")"
          else
            printf '{"parse_error":true}\n' > "$OUT/host-config-shape.json"
            log "config.toml parses: no"
          fi
          ;;
        *)
          merge_hooks "$WORK/hooks/codex/hooks.json" "$CAP/codex-capture-only.json" > "$CODEX_HOME/hooks.json"
          HOOKS_AT="$CODEX_HOME/hooks.json"
          config_shape "$HOOKS_AT" > "$OUT/host-config-shape.json"
          ;;
      esac
      log "codex hooks: real recipe (${HOOKS_AT/$HOME/~}) + capture entries; scripts at ~/.codex/hooks/kaban"
    else
      case "$VARIANT" in
        project-hooks) mkdir -p "$WORK/.codex"; render "$TPL/codex-hooks.json" "$WORK/.codex/hooks.json"; HOOKS_AT="$WORK/.codex/hooks.json" ;;
        *) render "$TPL/codex-hooks.json" "$CODEX_HOME/hooks.json"; HOOKS_AT="$CODEX_HOME/hooks.json" ;;
      esac
      config_shape "$HOOKS_AT" > "$OUT/host-config-shape.json"
      log "codex hooks at ${HOOKS_AT/$HOME/~}"
    fi
    CMD=(codex exec --json --skip-git-repo-check -C "$WORK")
    [ -n "${KBN_MODEL:-}" ] && CMD+=(-m "$KBN_MODEL")
    case "$VARIANT" in
      features*) CMD+=(-c 'features.hooks=true') ;;
    esac
    CMD+=("$PROMPT")
    ;;
  pi)
    mkdir -p "$HOME/.pi/agent"
    [ -r "$CREDS/pi/models.json" ] && install -m 0600 "$CREDS/pi/models.json" "$HOME/.pi/agent/models.json" && log "pi models.json installed (copy)"
    render "$TPL/pi-capture.ts" "$CAP/pi-capture.ts"
    # pi has no hook config file: the capture extension's EVENTS list is the registration
    grep -oE '^  "[a-z_]+",$' "$TPL/pi-capture.ts" | tr -d ' ",' \
      | jq -R -s -c --slurpfile v "$VOCAB" --arg h "$HOST" 'split("\n") | map(select(length > 0))
          | map(. as $e | if (($v[0][$h] // []) | index($e)) != null then $e else "other" end)
          | group_by(.) | map({(.[0]): length}) | add // {}
          | {events: ., entries: ([.[]] | add // 0), hook_commands: 0, commands_harness: 0, commands_recipe: 0, commands_other: 0}' \
      > "$OUT/host-config-shape.json"
    EXT_ARGS=(-e "$CAP/pi-capture.ts")
    if [ "$MODE" = recipes ]; then
      EXT_ARGS=(-e "$WORK/hooks/pi/kaban-hooks.ts" -e "$CAP/pi-capture.ts")
      log "pi extensions: real recipe hooks/pi/kaban-hooks.ts + capture extension (probes off)"
    fi
    CMD=(pi --print --mode json --provider openai-codex --model "${KBN_MODEL:-gpt-5.6-sol}" "${EXT_ARGS[@]}" --no-extensions --no-skills --no-prompt-templates --no-themes --no-session "$PROMPT")
    ;;
  *) log "unknown host"; exit 2 ;;
esac

# run --------------------------------------------------------------------------------------
if [ -n "${KBN_DRY_CMD:-}" ]; then
  # harness self-test: replace the host CLI by a shell command (no model call, no credentials
  # used); it typically pipes invented envelopes through the rendered hook commands
  CMD=(bash -c "$KBN_DRY_CMD")
  log "DRY RUN: the host command is replaced by a harness shell command"
fi
log "running: ${CMD[0]} ${CMD[1]} ... prompt_sha256=$(printf '%s' "$PROMPT" | sha256sum | cut -c1-16) prompt_chars=${#PROMPT}"
start=$(date +%s)
timeout -k 10 "$HOST_TIMEOUT" "${CMD[@]}" > "$CAP/transcript.jsonl" 2> "$CAP/stderr.txt"
rc=$?
end=$(date +%s)
log "host exit=$rc duration=$((end - start))s"

# reduce and export -------------------------------------------------------------------------
[ -f "$CAP/events.jsonl" ] && cp "$CAP/events.jsonl" "$OUT/events.jsonl" || : > "$OUT/events.jsonl"
# the host's stderr: per-line class counts from a fixed vocabulary, never text (R4-1)
classify_lines "$CAP/stderr.txt" > "$OUT/stderr-classes.json"
head -n 300 "$CAP/transcript.jsonl" | while IFS= read -r line; do
  printf '%s' "$line" | jq -c -f "$HARNESS/reduce.jq" 2>/dev/null || echo '"<non-json-line>"'
done > "$OUT/transcript-shape.jsonl"
# the scratch repo after the session: counts and booleans, no names (R4-1)
fsize() { [ -f "$1" ] && stat -c %s "$1" || echo 0; }
jq -n -c \
  --argjson entries "$(find "$WORK" -mindepth 1 -maxdepth 1 | wc -l)" \
  --argjson files "$(find "$WORK" -mindepth 1 -maxdepth 1 -type f | wc -l)" \
  --argjson dirs "$(find "$WORK" -mindepth 1 -maxdepth 1 -type d | wc -l)" \
  --argjson files_created "$(find "$WORK" -mindepth 1 -maxdepth 1 -type f ! -name README.md ! -name .gitignore ! -name 'kaban.db*' | wc -l)" \
  --argjson notes "$([ -e "$WORK/notes.txt" ] && echo true || echo false)" --argjson notes_bytes "$(fsize "$WORK/notes.txt")" \
  --argjson notes2 "$([ -e "$WORK/notes2.txt" ] && echo true || echo false)" \
  --argjson db "$([ -e "$WORK/kaban.db" ] && echo true || echo false)" --argjson db_bytes "$(fsize "$WORK/kaban.db")" \
  '{entries:$entries, files:$files, dirs:$dirs, files_created:$files_created, notes_txt_exists:$notes, notes_txt_bytes:$notes_bytes,
    notes2_txt_exists:$notes2, kaban_db_exists:$db, kaban_db_bytes:$db_bytes}' > "$OUT/work-ls.json"
# host diagnostics (error items emitted by the host itself): texts stay in tmpfs, classes leave
case "$HOST" in
  claude-code) jq -r 'select(.type=="result" and (.is_error // false)) | (.result // "" | tostring)' "$CAP/transcript.jsonl" 2>/dev/null ;;
  codex) jq -r 'select(.item.type=="error" or .type=="error") | (.item.message // .message // "" | tostring)' "$CAP/transcript.jsonl" 2>/dev/null ;;
  pi) jq -r 'select(.type=="error" or .error? != null) | (.error // .message // "" | tostring)' "$CAP/transcript.jsonl" 2>/dev/null ;;
esac > "$CAP/host-errors.txt"
host_errors="$(classify_lines "$CAP/host-errors.txt" | jq -c '{items: .lines, chars: .chars, classes: .classes}')"
if [ "$HOST" = claude-code ]; then
  # the final result item: fixed subtype vocabulary, booleans and counts
  result_line="$(jq -c 'select(.type=="result")' "$CAP/transcript.jsonl" 2>/dev/null | tail -1)"
  if [ -n "$result_line" ]; then
    # the whole subtype string against the literal members (RL-1): a multiline value is "other"
    subtype="$(enum_of "$(printf '%s' "$result_line" | jq -r '.subtype // "" | tostring')" \
      success error_max_turns error_during_execution error_max_budget_usd error_max_structured_output_retries)"
    host_errors="$(printf '%s' "$result_line" | jq -c --argjson base "$host_errors" --arg subtype "$subtype" \
      '$base + {result: {present: true, subtype: $subtype, is_error: ((.is_error // false) == true), num_turns: ((.num_turns // 0) | tonumber? // 0)}}')"
  else
    host_errors="$(printf '%s' "$host_errors" | jq -c '. + {result: {present: false}}')"
  fi
fi
printf '%s\n' "$host_errors" > "$OUT/host-errors.json"

# assistant text extraction per host (for the sentinel verdicts only; text never leaves tmpfs)
case "$HOST" in
  claude-code)
    jq -r 'select(.type=="assistant") | .message.content[]? | select(.type=="text") | .text' "$CAP/transcript.jsonl" 2>/dev/null > "$CAP/assistant.txt"
    tool_names 'select(.type=="assistant") | .message.content[]? | select(.type=="tool_use") | .name // ""' | tool_counts > "$CAP/tools.json"
    ;;
  codex)
    jq -r 'select(.type=="item.completed" and .item.type=="agent_message") | .item.text' "$CAP/transcript.jsonl" 2>/dev/null > "$CAP/assistant.txt"
    tool_names 'select(.type=="item.completed") | .item.type // ""' | tool_counts > "$CAP/tools.json"
    ;;
  pi)
    jq -r '.. | objects | select(.role? == "assistant") | .content | if type=="array" then .[] | select(.type?=="text") | .text else . end' "$CAP/transcript.jsonl" 2>/dev/null > "$CAP/assistant.txt"
    tool_names '.. | objects | select(.type? == "toolCall") | .name // ""' | tool_counts > "$CAP/tools.json"
    ;;
esac
[ -s "$CAP/tools.json" ] && jq -e 'type == "object"' "$CAP/tools.json" >/dev/null 2>&1 || echo '{}' > "$CAP/tools.json"
yn() { grep -q -- "$1" "$2" 2>/dev/null && echo yes || echo no; }
jq -n -c \
  --arg host "$HOST_X" --arg session_sha "$SESSION_SHA" --argjson session_chars "$SESSION_CHARS" \
  --arg stop_mode "$STOP_X" --arg variant "$VARIANT_X" --arg model "$MODEL_X" \
  --argjson rc "$rc" --argjson duration "$((end - start))" \
  --arg brief_any "$(yn "$KBN_BRIEF" "$CAP/transcript.jsonl")" --arg brief_assistant "$(yn "$KBN_BRIEF" "$CAP/assistant.txt")" \
  --arg pre_any "$(yn "$KBN_PRE" "$CAP/transcript.jsonl")" --arg pre_assistant "$(yn "$KBN_PRE" "$CAP/assistant.txt")" \
  --arg stop_any "$(yn "$KBN_STOP" "$CAP/transcript.jsonl")" --arg stop_assistant "$(yn "$KBN_STOP" "$CAP/assistant.txt")" \
  --arg stop_fired "$([ -e "$CAP/stop-probe.done" ] && echo yes || echo no)" \
  --arg pre_fired "$([ -e "$CAP/pre-probe.done" ] && echo yes || echo no)" \
  --argjson transcript_lines "$(wc -l < "$CAP/transcript.jsonl")" \
  --argjson assistant_chars "$(wc -c < "$CAP/assistant.txt")" \
  --argjson tools "$(cat "$CAP/tools.json")" \
  --argjson events "$(wc -l < "$OUT/events.jsonl")" \
  --arg notes_txt "$([ -e "$WORK/notes.txt" ] && echo present || echo absent)" \
  '{host:$host, session:{sha256:$session_sha, chars:$session_chars}, stop_mode:$stop_mode, variant:$variant, model:$model,
    exit_code:$rc, duration_s:$duration,
    sentinel:{brief:{assistant:$brief_assistant, anywhere:$brief_any},
              pre:{fired:$pre_fired, assistant:$pre_assistant, anywhere:$pre_any},
              stop:{fired:$stop_fired, assistant:$stop_assistant, anywhere:$stop_any}},
    transcript_lines:$transcript_lines, assistant_chars:$assistant_chars, tool_uses:$tools,
    hook_events_captured:$events, notes_txt:$notes_txt}' > "$OUT/verdict.json"
log "verdict: $(cat "$OUT/verdict.json")"
# event names were validated by capture.sh (vocabulary or "other"); the shim's argv words are vocabulary
log "events by name: $(jq -r '.event' "$OUT/events.jsonl" | sort | uniq -c | awk '{print $2":"$1}' | tr '\n' ' ')"

# recipes mode: what the real handlers wrote and what the model consumed (reduced) -------------
if [ "$MODE" = recipes ]; then
  calls="$KBN_KABAN_CALLS"; [ -f "$calls" ] || : > "$calls"
  # the shim's call lines (argv words, rc, ms, byte counts, stdout shape): exported as they are
  cp "$calls" "$OUT/kaban-calls.jsonl"
  hook_calls="$(jq -c 'select(.argv[0]=="hook")' "$calls")"
  # kaban's view after the session (inside the container; reduced before export)
  # every captured Failure has the text `"<command>" failed: …` (M4-10), so "failed" finds them all
  kaban --json recall "failed" --kind Failure --full-text --limit 20 > "$CAP/recall.json" 2>"$CAP/recall-err.txt" || true
  failure_ids="$(jq -r '.results[]?.id // empty' "$CAP/recall.json" 2>/dev/null)"
  id_echoed=no
  for fid in $failure_ids; do grep -q -- "$fid" "$CAP/assistant.txt" 2>/dev/null && id_echoed=yes; done
  first_assistant="$(head -c 4000 "$CAP/assistant.txt" 2>/dev/null)"
  jq -n -c \
    --arg dec "$KBN_DEC" --arg seed_id "$SEED_ID" \
    --argjson calls "$(jq -s -c '.' <<< "$hook_calls")" \
    --argjson recall "$(jq -c '{count: (.count // 0), failures: [.results[]? | {pending_fix: (.props.pending_fix // null), evidence_runs: ((.props.evidence_runs // []) | length), has_command: ((.props.command // "") != ""), has_error_text: ((.props.error_text // "") != "")}]}' "$CAP/recall.json" 2>/dev/null || echo '{"count":null,"failures":[]}')" \
    --arg dec_assistant "$(yn "$KBN_DEC" "$CAP/assistant.txt")" --arg dec_anywhere "$(yn "$KBN_DEC" "$CAP/transcript.jsonl")" \
    --arg dec_first_reply "$(printf '%s' "$first_assistant" | grep -q -- "$KBN_DEC" && echo yes || echo no)" \
    --arg failure_id_echoed "$id_echoed" --arg unresolved_echoed "$(yn unresolved "$CAP/assistant.txt")" \
    '($calls | map(select(.argv[1]=="session-start"))) as $ss
     | ($calls | map(select(.argv[1]=="pre-tool-use"))) as $pre
     | ($calls | map(select(.argv[1]=="post-tool-use" or .argv[1]=="post-tool-use-failure"))) as $post
     | ($calls | map(select(.argv[1]=="stop"))) as $stop
     | ($calls | map(select(.argv[1]=="fix"))) as $fix
     | {sentinel_decision: $dec, seed_recorded: ($seed_id != ""),
        hook_calls: ($calls | length), hook_calls_by_event: ($calls | group_by(.argv[1]) | map({(.[0].argv[1]): length}) | add // {}),
        hook_exit_codes: ($calls | map(.rc) | unique), any_exit_2: ($calls | any(.rc == 2)),
        hook_ms_max: ($calls | map(.ms) | max // 0), any_hook_over_2000ms: ($calls | any(.ms > 2000)),
        briefing: {hook_output: ($ss | any(.out_shape.hookSpecificOutput.additionalContext == "<string>")),
                   injected_first_reply: $dec_first_reply, injected_assistant: $dec_assistant, anywhere: $dec_anywhere},
        pre_context: {hook_calls: ($pre | length), hook_output: ($pre | any(.out_shape.hookSpecificOutput.additionalContext == "<string>")),
                      note: "pre-tool-use records the edit and injects nothing; hook_output stays false"},
        failure_recorded: ($recall.failures | any(.pending_fix == true)),
        evidence_appended: ($recall.failures | any(.evidence_runs > 0)),
        recall: $recall,
        stop: {hook_calls: ($stop | length), prompt_printed: ($stop | any(.out_shape.decision == "<string>")),
               second_stop_empty: (($stop | length) >= 2 and ($stop | last | .out_shape == {})),
               failure_id_echoed: $failure_id_echoed, unresolved_echoed: $unresolved_echoed,
               fix_calls: ($fix | length), fix_status_printed: ($fix | any(.out_shape.status == "<string>"))}}' \
    > "$OUT/recipes-verdict.json"
  log "recipes verdict: $(cat "$OUT/recipes-verdict.json")"
  echo "recall stderr lines: $(wc -l < "$CAP/recall-err.txt")" | log_line
fi
exit 0
