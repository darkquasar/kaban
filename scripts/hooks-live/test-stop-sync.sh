#!/usr/bin/env bash
# Offline test of the detached `kaban sync` in the Stop and SubagentStop wrappers (core-plan C5.3
# as amended by `reviews/m5-prep.md` M5-20; M5 review R5-9; no Docker, no model, no real binary).
#
#   bash scripts/hooks-live/test-stop-sync.sh
#
# A fake `kaban` (KABAN_BIN) records its argv, the targets of its descriptors 0, 1 and 2, its
# session id, its pid, the pid of a child it forks and KABAN_DB to a log, answers `{}` to `hook`
# and sleeps 30 s on `sync` (it ignores `--deadline`, so what is observed here is the shell
# layer of the bound). For each of hooks/{claude-code,codex}/{stop,subagent-stop}.sh the test
# asserts:
#   - the wrapper exits 0 and returns, with its stdout pipe closed, in under 2 s (a sync that
#     inherited the pipe would hold the command substitution open for the fake's 30 s);
#   - stdout is exactly one JSON document (the `{}` of the hook call, nothing from the sync);
#   - the fake was invoked once with exactly `sync --json --deadline 60` (never `--stage`), with
#     every descriptor on /dev/null, in a session of its own (setsid) and with KABAN_DB set;
#   - with KABAN_HOOK_SYNC=0 the sync is not started and stdout is still one JSON document.
# Then, with KABAN_SYNC_DEADLINE=1 (grace 6 s), the group kill of the shell layer (R5-9):
#   - Linux path (`setsid timeout -k`): the fake and the child it forked are both gone within
#     the grace period, and three Stop events in a row leave no live sync behind;
#   - macOS fallback (`set -m` plus the sleeper), exercised by hiding `setsid` and `timeout` from
#     PATH: the wrapper still exits 0 in under 2 s with one JSON document, the fake runs in its
#     own process group with every descriptor on /dev/null, and the fake and its child are gone
#     within the grace period.
# Linux exercises setsid/timeout and the job-control fallback; Darwin uses system Bash
# and native descriptor/process-group observations without setsid or GNU timeout.
# Exit 1 on any failed assertion. Requires Python 3 and jq.
set -u
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
command -v jq >/dev/null || { echo "jq is required" >&2; exit 2; }
tmp="$(mktemp -d /tmp/kbn-stopsync.XXXXXX)"
cleanup() {
  # the fakes only sleep; end the ones a 60 s deadline would still be bounding
  for log in "$tmp"/*.log; do
    [ -f "$log" ] || continue
    sed -n 's/.*|pid=\([0-9]*\)|child=\([0-9]*\)|.*/\1 \2/p' "$log" | tr ' ' '\n' | while read -r p; do
      [ -n "$p" ] && [ "$p" != 0 ] && kill -KILL "$p" 2>/dev/null
    done
  done
  rm -rf "$tmp"
}
trap cleanup EXIT
fail=0
say() { printf '%s\n' "$*"; }
check() { # check <name> <cmd...>
  local name="$1"; shift
  if "$@" >/dev/null 2>&1; then say "ok   $name"; else say "FAIL $name"; fail=1; fi
}
test_python="$(command -v python3)" || exit 2
export KABAN_TEST_PYTHON="$test_python"
native=0
hook_path="$PATH"
if [ "$(uname -s)" = Darwin ]; then
  native=1
  mkdir -p "$tmp/native-bin"
  for tool in bash sh env dirname basename ps tr sleep kill cat printf sed grep uname; do
    if [ "$tool" = bash ]; then p=/bin/bash; else p="$(command -v "$tool")"; fi
    ln -s "$p" "$tmp/native-bin/$tool"
  done
  hook_path="$tmp/native-bin"
  check "native: system Bash is selected" test "$("$hook_path/bash" -c 'printf %s "$BASH_VERSION"')" = "$(/bin/bash -c 'printf %s "$BASH_VERSION"')"
  check "native: neither setsid nor GNU timeout is available" \
    /bin/bash -c '! PATH="$1" command -v setsid && ! PATH="$1" command -v timeout' _ "$hook_path"
fi
now_ms() { "$test_python" -c 'import time; print(time.monotonic_ns() // 1000000)'; }

mkdir -p "$tmp/bin"
fake="$tmp/bin/kaban"
cat > "$fake" <<'EOF'
#!/usr/bin/env bash
# fake kaban for test-stop-sync.sh: one log line per call, then the behaviour of the subcommand
set -u
log="${FAKE_LOG:?}"
if [ "$(uname -s)" = Darwin ]; then
  # Snapshot the fake's original stdio before command substitution redirects stdout.
  exec 3<&0 4>&1 5>&2
  nulls="$("$KABAN_TEST_PYTHON" -c 'import os,stat; n=os.stat("/dev/null"); print(" ".join("/dev/null" if stat.S_ISCHR(os.fstat(fd).st_mode) and os.fstat(fd).st_rdev == n.st_rdev else "other" for fd in (3,4,5)))')"
  exec 3<&- 4>&- 5>&-
  read -r f0 f1 f2 <<< "$nulls"
  sid=unavailable
else
f0="$(readlink "/proc/$$/fd/0" 2>/dev/null || echo unknown)"
f1="$(readlink "/proc/$$/fd/1" 2>/dev/null || echo unknown)"
f2="$(readlink "/proc/$$/fd/2" 2>/dev/null || echo unknown)"
sid="$(ps -o sid= -p "$$" | tr -d ' ')"
fi
pgid="$(ps -o pgid= -p "$$" | tr -d ' ')"
child=0
case "${1:-}" in
  sync)
    if [ "${FAKE_RESIST_TERM:-0}" = 1 ]; then
      ( trap '' TERM; sleep 30 ) </dev/null >/dev/null 2>&1 &
    else
      sleep 30 </dev/null >/dev/null 2>&1 &
    fi
    child=$! ;;
esac
argv=""; for a in "$@"; do argv="$argv $a"; done
printf 'argv=%s|fd0=%s|fd1=%s|fd2=%s|sid=%s|pgid=%s|pid=%s|child=%s|db=%s\n' "${argv# }" "$f0" "$f1" "$f2" "$sid" "$pgid" "$$" "$child" "${KABAN_DB:-unset}" >> "$log"
case "${1:-}" in
  sync) wait "$child"; exit 0 ;;
  hook) printf '{}\n'; exit 0 ;;
esac
exit 0
EOF
chmod +x "$fake"
my_sid=unavailable
[ "$native" = 1 ] || my_sid="$(ps -o sid= -p "$$" | tr -d ' ')"
my_pgid="$(ps -o pgid= -p "$$" | tr -d ' ')"

run_wrapper() { # run_wrapper <host> <script> <event name> [env...]
  local host="$1" script="$2" event="$3"; shift 3
  local log="$tmp/$host-$(basename "$script" .sh)-$RANDOM.log"
  : > "$log"
  local envelope="{\"session_id\":\"sess-INVENTED\",\"hook_event_name\":\"$event\",\"cwd\":\"$tmp\"}"
  local start end out status
  start="$(now_ms)"
  # the command substitution returns only when every writer of the pipe has closed it
  out="$(cd "$tmp" && printf '%s' "$envelope" | env PATH="$hook_path" "$@" KABAN_BIN="$fake" FAKE_LOG="$log" bash "$script")"
  status=$?
  end="$(now_ms)"
  WRAP_MS=$((end - start)); WRAP_OUT="$out"; WRAP_LOG="$log"; WRAP_STATUS=$status
}

wait_for_sync_line() { # wait_for_sync_line <log> [count]
  local want="${2:-1}"
  for _ in $(seq 1 40); do
    [ "$(grep -c '^argv=sync' "$1" 2>/dev/null)" -ge "$want" ] && return 0
    sleep 0.1
  done
  return 1
}

field() { printf '%s' "$1" | sed -n "s/.*|$2=\([^|]*\)|.*/\1/p"; }

alive() { kill -0 "$1" 2>/dev/null; }

for host in claude-code codex; do
  for name in stop subagent-stop; do
    script="$repo/hooks/$host/$name.sh"
    [ "$name" = stop ] && event=Stop || event=SubagentStop
    run_wrapper "$host" "$script" "$event"
    check "$host/$name: the wrapper exited 0 (never 2)" test "$WRAP_STATUS" -eq 0
    check "$host/$name: wrapper returned with its stdout closed in under 2 s (${WRAP_MS} ms)" test "$WRAP_MS" -lt 2000
    check "$host/$name: stdout is exactly one JSON document" \
      bash -c '[ "$(printf "%s\n" "$1" | grep -c .)" -eq 1 ] && printf "%s" "$1" | jq -e "type == \"object\""' _ "$WRAP_OUT"
    hook_line="$(grep -E "^argv=hook $name --host $host --stdin-json\\|" "$WRAP_LOG" || true)"
    check "$host/$name: the hook call ran as before (hook $name --host $host --stdin-json)" test -n "$hook_line"
    # the detached sync: wait for the fake to log its line (it logs before sleeping)
    wait_for_sync_line "$WRAP_LOG"
    sync_line="$(grep '^argv=sync' "$WRAP_LOG" || true)"
    check "$host/$name: exactly one sync call" test "$(grep -c '^argv=sync' "$WRAP_LOG")" -eq 1
    check "$host/$name: the sync argv is exactly 'sync --json --deadline 60' (never --stage)" \
      bash -c 'printf "%s" "$1" | grep -qE "^argv=sync --json --deadline 60\|"' _ "$sync_line"
    check "$host/$name: the sync has no inherited descriptor (fd 0, 1, 2 on /dev/null)" \
      bash -c 'printf "%s" "$1" | grep -qE "\|fd0=/dev/null\|fd1=/dev/null\|fd2=/dev/null\|"' _ "$sync_line"
    if [ "$native" = 1 ]; then
      check "$host/$name: the sync has its own process group" \
        bash -c 'g="$(printf "%s" "$1" | sed -n "s/.*|pgid=\\([0-9]*\\)|.*/\\1/p")"; p="$(printf "%s" "$1" | sed -n "s/.*|pid=\\([0-9]*\\)|.*/\\1/p")"; [ -n "$g" ] && [ "$g" = "$p" ] && [ "$g" != "$2" ]' _ "$sync_line" "$my_pgid"
    else
    check "$host/$name: the sync runs in its own session (setsid; wrapper session $my_sid)" \
      bash -c 'sid="$(printf "%s" "$1" | sed -n "s/.*|sid=\([0-9]*\)|.*/\1/p")"; [ -n "$sid" ] && [ "$sid" != "$2" ]' _ "$sync_line" "$my_sid"
    fi
    check "$host/$name: KABAN_DB is set for the sync" \
      bash -c 'printf "%s" "$1" | grep -qE "\|db=.+kaban\.db$"' _ "$sync_line"
    # opt-out
    run_wrapper "$host" "$script" "$event" KABAN_HOOK_SYNC=0
    sleep 0.3
    check "$host/$name: KABAN_HOOK_SYNC=0 starts no sync and keeps one JSON document" \
      bash -c '! grep -q "^argv=sync" "$1" && printf "%s" "$2" | jq -e "type == \"object\""' _ "$WRAP_LOG" "$WRAP_OUT"
  done
done

# --- R5-9: the shell layer of the bound, with a 1 s deadline (grace 6 s) -----------------------
stop="$repo/hooks/claude-code/stop.sh"

# Linux path: setsid + timeout -k. Darwin selects the real job-control fallback.
# Both platforms exercise three Stop events in a row.
run_wrapper claude-code "$stop" Stop KABAN_SYNC_DEADLINE=1
check "deadline/default: the wrapper exited 0 in under 2 s with a 1 s deadline (${WRAP_MS} ms)" \
  test "$WRAP_STATUS" -eq 0 -a "$WRAP_MS" -lt 2000
linux_log="$WRAP_LOG"
wait_for_sync_line "$linux_log"
linux_line="$(grep '^argv=sync' "$linux_log" || true)"
check "deadline/default: the sync argv carries the configured deadline (sync --json --deadline 1)" \
  bash -c 'printf "%s" "$1" | grep -qE "^argv=sync --json --deadline 1\|"' _ "$linux_line"
linux_pid="$(field "$linux_line" pid)"; linux_child="$(field "$linux_line" child)"
check "deadline/default: the fake and its child are alive right after the Stop" \
  bash -c 'kill -0 "$1" && kill -0 "$2"' _ "$linux_pid" "$linux_child"
repeat_log="$tmp/repeat.log"; : > "$repeat_log"
repeat_logs=""
for _ in 1 2 3; do
  run_wrapper claude-code "$stop" Stop KABAN_SYNC_DEADLINE=1
  check "deadline/repeat: Stop exited 0 in under 2 s (${WRAP_MS} ms)" test "$WRAP_STATUS" -eq 0 -a "$WRAP_MS" -lt 2000
  repeat_logs="$repeat_logs $WRAP_LOG"
done
for log in $repeat_logs; do
  wait_for_sync_line "$log"   # the fake logs asynchronously, after the wrapper returned
  cat "$log" >> "$repeat_log"
done
check "deadline/repeat: three Stop events started three bounded syncs" \
  test "$(grep -c '^argv=sync' "$repeat_log")" -eq 3

# macOS fallback: a PATH without setsid and timeout (bash, coreutils and procps otherwise)
mkdir -p "$tmp/nosetsid"
for tool in bash sh env dirname basename readlink ps tr sleep kill cat printf sed grep uname; do
  if [ "$native" = 1 ] && [ "$tool" = bash ]; then p=/bin/bash; else p="$(command -v "$tool" 2>/dev/null || true)"; fi
  [ -n "$p" ] && ln -sf "$p" "$tmp/nosetsid/$tool"
done
check "deadline/macos: the restricted PATH has neither setsid nor timeout" \
  bash -c '! PATH="$1" command -v setsid && ! PATH="$1" command -v timeout' _ "$tmp/nosetsid"
run_wrapper claude-code "$stop" Stop KABAN_SYNC_DEADLINE=1 PATH="$tmp/nosetsid"
check "deadline/macos: the wrapper exited 0 in under 2 s (${WRAP_MS} ms)" \
  test "$WRAP_STATUS" -eq 0 -a "$WRAP_MS" -lt 2000
check "deadline/macos: stdout is exactly one JSON document" \
  bash -c '[ "$(printf "%s\n" "$1" | grep -c .)" -eq 1 ] && printf "%s" "$1" | jq -e "type == \"object\""' _ "$WRAP_OUT"
mac_log="$WRAP_LOG"
wait_for_sync_line "$mac_log"
mac_line="$(grep '^argv=sync' "$mac_log" || true)"
check "deadline/macos: exactly one sync call with 'sync --json --deadline 1'" \
  bash -c '[ "$(grep -c "^argv=sync" "$1")" -eq 1 ] && printf "%s" "$2" | grep -qE "^argv=sync --json --deadline 1\|"' _ "$mac_log" "$mac_line"
check "deadline/macos: the sync has no inherited descriptor (fd 0, 1, 2 on /dev/null)" \
  bash -c 'printf "%s" "$1" | grep -qE "\|fd0=/dev/null\|fd1=/dev/null\|fd2=/dev/null\|"' _ "$mac_line"
mac_pgid="$(field "$mac_line" pgid)"; mac_pid="$(field "$mac_line" pid)"; mac_child="$(field "$mac_line" child)"
check "deadline/macos: the sync runs in its own process group (set -m; wrapper group $my_pgid)" \
  bash -c '[ -n "$1" ] && [ "$1" != "$2" ] && [ "$1" = "$3" ]' _ "$mac_pgid" "$my_pgid" "$mac_pid"
check "deadline/macos: the fake and its child are alive right after the Stop" \
  bash -c 'kill -0 "$1" && kill -0 "$2"' _ "$mac_pid" "$mac_child"

# the grace period (deadline 1 s + 5 s) passes: every bounded sync and its child are gone
for _ in $(seq 1 100); do
  live=0
  for p in $linux_pid $linux_child $mac_pid $mac_child $(sed -n 's/.*|pid=\([0-9]*\)|child=\([0-9]*\)|.*/\1 \2/p' "$repeat_log"); do
    [ "$p" != 0 ] && alive "$p" && live=$((live + 1))
  done
  [ "$live" -eq 0 ] && break
  sleep 0.1
done
check "deadline/default: the fake sync was killed within the grace period" bash -c '! kill -0 "$1"' _ "$linux_pid"
check "deadline/default: the descendant the sync forked was killed with its group" bash -c '! kill -0 "$1"' _ "$linux_child"
check "deadline/repeat: no live sync is left behind after three Stop events" test "$live" -eq 0
check "deadline/macos: the fake sync was killed within the grace period (group kill)" bash -c '! kill -0 "$1"' _ "$mac_pid"
check "deadline/macos: the descendant the sync forked was killed with its group" bash -c '! kill -0 "$1"' _ "$mac_child"

# A sync leader may die on TERM while its child ignores TERM. Escalation must survive
# the leader and kill the remaining group; all four wrappers share this contract.
resistant_logs=""
for host in claude-code codex; do
  for name in stop subagent-stop; do
    [ "$name" = stop ] && event=Stop || event=SubagentStop
    run_wrapper "$host" "$repo/hooks/$host/$name.sh" "$event" \
      KABAN_SYNC_DEADLINE=1 FAKE_RESIST_TERM=1 PATH="$tmp/nosetsid"
    check "$host/$name resistant: wrapper returned with one JSON document" \
      test "$WRAP_STATUS" -eq 0 -a "$WRAP_MS" -lt 2000 -a "$WRAP_OUT" = '{}'
    check "$host/$name resistant: sync was started" wait_for_sync_line "$WRAP_LOG"
    resistant_logs="$resistant_logs $WRAP_LOG"
  done
done
for _ in $(seq 1 140); do
  remaining=0
  for log in $resistant_logs; do
    line="$(grep '^argv=sync' "$log")"
    for p in "$(field "$line" pid)" "$(field "$line" child)"; do
      [ -n "$p" ] && alive "$p" && remaining=$((remaining + 1))
    done
  done
  [ "$remaining" -eq 0 ] && break
  sleep 0.1
done
for log in $resistant_logs; do
  line="$(grep '^argv=sync' "$log")"
  pid="$(field "$line" pid)"; child="$(field "$line" child)"
  check "$(basename "$log") resistant: leader and descendant were killed" \
    bash -c '[ -n "$1" ] && [ -n "$2" ] && ! kill -0 "$1" 2>/dev/null && ! kill -0 "$2" 2>/dev/null' _ "$pid" "$child"
done

if [ "$fail" -eq 0 ]; then say "ALL OK"; else say "FAILURES"; fi
exit "$fail"
