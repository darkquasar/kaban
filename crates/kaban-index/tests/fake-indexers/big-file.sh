#!/bin/sh
# Writes $2 bytes of zeros into one file under $1 ("$1/out/big"), then waits (CG4.3
# `runner_quota_file_kills`: 1 GiB + 1 byte with a 2 s deadline must be killed by the quota).
set -e
dd if=/dev/zero of="$1/out/big" bs=1048576 count=$(( $2 / 1048576 )) 2>/dev/null
dd if=/dev/zero bs=1 count=$(( $2 % 1048576 )) >> "$1/out/big" 2>/dev/null
exec sleep 30
