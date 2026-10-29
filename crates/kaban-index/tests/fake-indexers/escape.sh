#!/bin/sh
# A hostile indexer inside the namespaces view (U2 RU2-1, `ns_indexer_cannot_remount_or_mount_
# after_privilege_drop`): prints the capability lines of /proc/self/status, then attempts what
# retained namespace privileges would allow: remount the read-only checkout bind writable and
# write into it, mount a fresh tmpfs under /scratch, open a nested user namespace. One
# `name=<rc> errno=<n>` line per attempt (mount(2) and unshare(2) through python3's ctypes so
# the result is the raw syscall's, not a tool's). Exits 0 whatever happened: the test asserts
# on the gated log and on the host checkout.
echo "--- caps"
grep -E '^(CapInh|CapPrm|CapEff|CapBnd|CapAmb|NoNewPrivs):' /proc/self/status
echo "--- attempts"
/usr/bin/python3 - "$PWD" <<'EOF'
import ctypes, os, sys
libc = ctypes.CDLL(None, use_errno=True)
MS_REMOUNT, MS_BIND = 32, 4096
CLONE_NEWUSER = 0x10000000
checkout = sys.argv[1]

def attempt(name, call):
    rc = call()
    err = ctypes.get_errno() if rc != 0 else 0
    print(f"{name}={rc} errno={err}")

attempt("remount_rw", lambda: libc.mount(b"none", checkout.encode(), None, MS_REMOUNT | MS_BIND, None))
try:
    with open(os.path.join(checkout, "escaped.txt"), "w", encoding="utf-8") as f:
        f.write("escaped\n")
    print("checkout_write=0 errno=0")
except OSError as e:
    print(f"checkout_write=1 errno={e.errno}")
os.makedirs("/scratch/m", exist_ok=True)
attempt("new_tmpfs", lambda: libc.mount(b"none", b"/scratch/m", b"tmpfs", 0, None))
attempt("nested_userns", lambda: libc.unshare(CLONE_NEWUSER))
EOF
echo "--- done"
