#!/bin/sh
# Prints the mount points of this process's mount namespace (field 5 of
# /proc/self/mountinfo), one per line, sorted (CG4.4 `ns_view_only_listed_paths`), then its
# pid as seen inside the pid namespace and the NSpid line (`ns_helper_single_invocation`),
# then the targets of its open descriptors (`ns_helper_closes_wrapper_handle`); the read-only
# mount points follow the mount list.
echo "--- mounts"
cut -d' ' -f5 /proc/self/mountinfo | sort
echo "--- ro"
# the mount points whose per-mount options (field 6) hold `ro` (`ns_usr_include_is_readonly`)
while read -r _ _ _ _ point options _; do
  case ",$options," in *,ro,*) echo "$point";; esac
done < /proc/self/mountinfo | sort
echo "--- pid=$$"
grep '^NSpid:' /proc/self/status
echo "--- fds"
ls -l "/proc/$$/fd" | sed -n 's/.* \([0-9][0-9]*\) -> /fd \1 -> /p'
echo "--- tcp"
if [ -x /usr/bin/python3 ]; then
  /usr/bin/python3 -c 'import socket
s = socket.socket()
try:
    s.connect(("127.0.0.1", 9))
    print("tcp connected")
except OSError as e:
    print("tcp errno", e.errno, e.strerror)'
else
  echo "tcp python3 absent"
fi
