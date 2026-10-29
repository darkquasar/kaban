#!/usr/bin/env bash
# Observation shim for the round-2 recipes mode (M4 W2-D2, OWN-7, S1 fence).
#
# Bind-mounted read-only at /usr/local/bin/kaban in the container, in front of the real static
# binary mounted at /opt/kaban/kaban. The recipes call `kaban` from PATH unchanged; the shim
# records ONE reduced line per call to $KBN_KABAN_CALLS (tmpfs) and passes stdin, stdout, stderr
# and the exit status through untouched:
#   argv      the subcommand and its flag words only (fixed vocabulary: `hook`, the event, the host,
#             `sync` and the other M5 subcommands, the kaban flags listed below); free-text
#             arguments (remember text, path scopes, ids)
#             and any flag outside the list (including `--flag=value` spellings) are counted, not kept
#   rc        the exit status (PR-9: a 2 from `kaban hook` is a violation the harness flags)
#   ms        wall time of the call (the brief's 2 s rule)
#   out_bytes stdout size; out_shape the stdout document reduced with reduce.jq ("<non-json>")
#   stdin_bytes  size of the envelope read by the binary (counted through a tee into a tmpfs file,
#             deleted right after; the raw bytes are never kept)
# Nothing of the envelope, the briefing, the prompt text or the error text is written anywhere.
set -u
real=/opt/kaban/kaban
log="${KBN_KABAN_CALLS:-/capture/kaban-calls.jsonl}"
here="$(cd "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")" && pwd)"
reduce="${KBN_REDUCE_JQ:-$here/reduce.jq}"
[ -r "$reduce" ] || reduce=/harness/container/reduce.jq

argv=()
free=0
for a in "$@"; do
  case "$a" in
    hook|session-start|pre-tool-use|post-tool-use|post-tool-use-failure|subagent-stop|stop|fix|post-commit|\
    remember|accept|recall|why|doctor|forget|serve|bench|scan|verify-audit|rescan|sync|rebuild|resolve|export|\
    Decision|Convention|Failure|Fact|Improvement|Episode|\
    --json|--stdin-json|--host|--db|--global-db|--policy|--workspace|--audit-export|--version|--help|-V|-h|\
    --path-scope|--importance|--props|--reason|--kind|--full-text|--limit|--path|--query|--hops|--tag|\
    --link|--ref|--summary|--confidence|--agent-id|--session-id|--workspace-id|--include-archived|\
    --include-superseded|--mode|--signer|--confirmation-token|--confirmed-by-user|--token-stdin|--vacuum|\
    --stage|--repo|--take|--format|--write) argv+=("$a") ;;
    claude-code|codex|pi) argv+=("$a") ;;
    *) free=$((free + 1)) ;;
  esac
done

tmp="$(mktemp -d /capture/kbn-call.XXXXXX 2>/dev/null || mktemp -d)"
start=$(date +%s%N)
if [ -t 0 ]; then
  "$real" "$@" > "$tmp/out"
  rc=$?
  in_bytes=0
else
  tee "$tmp/in" | "$real" "$@" > "$tmp/out"
  rc=${PIPESTATUS[1]}
  in_bytes=$(wc -c < "$tmp/in" | tr -d ' ')
fi
end=$(date +%s%N)
cat "$tmp/out"
out_bytes=$(wc -c < "$tmp/out" | tr -d ' ')
shape="$(jq -c -f "$reduce" "$tmp/out" 2>/dev/null || echo '"<non-json>"')"
[ -n "$shape" ] || shape='"<empty>"'
rm -rf "$tmp"
argv_json="$(printf '%s\n' ${argv[@]+"${argv[@]}"} | jq -R . | jq -s -c 'map(select(length > 0))')"
jq -n -c --argjson argv "$argv_json" \
  --argjson free "$free" --argjson rc "$rc" --argjson ms "$(( (end - start) / 1000000 ))" \
  --argjson out_bytes "$out_bytes" --argjson in_bytes "$in_bytes" --argjson shape "$shape" \
  '{argv:$argv, free_args:$free, rc:$rc, ms:$ms, stdin_bytes:$in_bytes, out_bytes:$out_bytes, out_shape:$shape}' \
  >> "$log" 2>/dev/null || true
exit "$rc"
