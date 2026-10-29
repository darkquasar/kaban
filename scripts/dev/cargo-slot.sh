#!/usr/bin/env bash
# A counting semaphore for heavy cargo invocations (PROTOCOL.md §5, OWN-79 (3)): at most N compiles run at once on
# the box, whatever the number of lanes. Default 4 since session 17 (measured: 517 samples, lowest available RAM 5.06 GB of 7.9
# with up to ten concurrent rustc at three slots beside a clean-room; summed compiler RSS peaked at 1.9 GB). A lane prefixes its build, clippy and nextest commands with this script;
# with every slot taken it waits (checking every 3 s) and starts when one frees. The slot is a flock on
# /tmp/kaban-cargo-slots/<i>, held by the exec'd command and released by the kernel when it exits, so a killed
# command never leaks a slot. Darwin without flock(1) uses Python 3's fcntl.flock on the same files.
# Missing locking prerequisites refuse execution. nextest list can compile and needs a slot too.
# Usage: scripts/dev/cargo-slot.sh <command...>       e.g. scripts/dev/cargo-slot.sh cargo nextest run -p kaban-api --offline
# Env: KABAN_CARGO_SLOTS (default 4), KABAN_CARGO_SLOT_WAIT (max seconds to wait, default 1800).
# KABAN_CARGO_SLOT_DIR overrides the shared lock directory for isolated helper tests only.
set -u
n=${KABAN_CARGO_SLOTS:-4}; max=${KABAN_CARGO_SLOT_WAIT:-1800}; dir=${KABAN_CARGO_SLOT_DIR:-/tmp/kaban-cargo-slots}
if [[ ! "$n" =~ ^[1-9][0-9]*$ || ! "$max" =~ ^(0|[1-9][0-9]*)$ ]] || [ "$#" -eq 0 ]; then
  echo "cargo-slot: require a positive slot count, nonnegative wait and a command" >&2
  exit 64
fi
if ! command -v flock >/dev/null 2>&1; then
  if ! command -v python3 >/dev/null 2>&1; then
    echo "cargo-slot: need flock or Python 3 with fcntl; command not run" >&2
    exit 69
  fi
  # Python makes newly opened FDs close-on-exec by default. Explicitly retain the lock
  # across exec, just like shell FD 9 below; never unlock between admission and spawn.
  exec python3 -c '
import errno
import fcntl
import os
import sys
import time

directory, slots, maximum, *command = sys.argv[1:]
slots, maximum = int(slots), int(maximum)
os.makedirs(directory, exist_ok=True)
waited = 0
while True:
    for slot in range(1, slots + 1):
        fd = os.open(os.path.join(directory, str(slot)), os.O_WRONLY | os.O_CREAT, 0o666)
        try:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except OSError as error:
            os.close(fd)
            if error.errno not in (errno.EACCES, errno.EAGAIN):
                raise
            continue
        os.set_inheritable(fd, True)
        if waited:
            print(f"cargo-slot: slot {slot} after {waited}s", file=sys.stderr)
        os.execvp(command[0], command)
    if waited >= maximum:
        print(f"cargo-slot: no slot in {maximum}s", file=sys.stderr)
        sys.exit(75)
    time.sleep(3)
    waited += 3
' "$dir" "$n" "$max" "$@"
fi
mkdir -p "$dir" || exit 73
waited=0
while :; do
  for ((i=1; i<=n; i++)); do
    exec 9>"$dir/$i" || exit 73
    flock -n 9
    rc=$?
    if [ "$rc" -eq 0 ]; then
      [ "$waited" -gt 0 ] && echo "cargo-slot: slot $i after ${waited}s" >&2
      exec "$@"
    fi
    exec 9>&-
    [ "$rc" -eq 1 ] || { echo "cargo-slot: flock failed (exit $rc); command not run" >&2; exit "$rc"; }
  done
  [ "$waited" -ge "$max" ] && { echo "cargo-slot: no slot in ${max}s" >&2; exit 75; }
  sleep 3; waited=$((waited+3))
done
