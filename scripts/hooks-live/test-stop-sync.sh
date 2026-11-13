#!/usr/bin/env bash
# Offline test that the Stop and SubagentStop wrappers start no `kaban sync` (the detached sync of
# core-plan C5.3, M5-20, is removed with the governance surfaces; no Docker, no model, no real
# binary).
#
#   bash scripts/hooks-live/test-stop-sync.sh
#
# A fake `kaban` (KABAN_BIN) appends its argv to a log and answers `{}` to `hook`. For each of
# hooks/{claude-code,codex}/{stop,subagent-stop}.sh the test asserts:
#   - the wrapper exits 0 and stdout is exactly one JSON document (the `{}` of the hook call);
#   - the fake was invoked exactly once, as `hook <event> --host <host> --stdin-json`;
#   - after a short grace period no other call (in particular no `sync`) was logged, so nothing
#     was started in the background either.
# Exit 1 on any failed assertion. Requires jq.
set -u
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
command -v jq >/dev/null || { echo "jq is required" >&2; exit 2; }
tmp="$(mktemp -d "${TMPDIR:-/tmp}/kbn-stopsync.XXXXXX")"
trap 'rm -rf "$tmp"' EXIT
fail=0
say() { printf '%s\n' "$*"; }
check() { # check <name> <cmd...>
  local name="$1"; shift
  if "$@" >/dev/null 2>&1; then say "ok   $name"; else say "FAIL $name"; fail=1; fi
}

fake="$tmp/kaban"
cat > "$fake" <<'EOF'
#!/usr/bin/env bash
# fake kaban for test-stop-sync.sh: one log line per call; `{}` for `hook`
set -u
printf 'argv=%s\n' "$*" >> "${FAKE_LOG:?}"
[ "${1:-}" = hook ] && printf '{}\n'
exit 0
EOF
chmod +x "$fake"

for host in claude-code codex; do
  for name in stop subagent-stop; do
    script="$repo/hooks/$host/$name.sh"
    [ "$name" = stop ] && event=Stop || event=SubagentStop
    log="$tmp/$host-$name.log"; : > "$log"
    envelope="{\"session_id\":\"sess-INVENTED\",\"hook_event_name\":\"$event\",\"cwd\":\"$tmp\"}"
    out="$(cd "$tmp" && printf '%s' "$envelope" | KABAN_BIN="$fake" FAKE_LOG="$log" bash "$script")"
    status=$?
    sleep 0.5 # a call started in the background would log within this window
    check "$host/$name: the wrapper exited 0 (never 2)" test "$status" -eq 0
    check "$host/$name: stdout is exactly one JSON document" \
      bash -c '[ "$(printf "%s\n" "$1" | grep -c .)" -eq 1 ] && printf "%s" "$1" | jq -e "type == \"object\""' _ "$out"
    check "$host/$name: exactly one kaban call, 'hook $name --host $host --stdin-json'" \
      bash -c '[ "$(grep -c . "$1")" -eq 1 ] && grep -qxF "argv=hook $2 --host $3 --stdin-json" "$1"' _ "$log" "$name" "$host"
    check "$host/$name: no kaban sync was started" bash -c '! grep -q "^argv=sync" "$1"' _ "$log"
  done
done

if [ "$fail" -eq 0 ]; then say "ALL OK"; else say "FAILURES"; fi
exit "$fail"
