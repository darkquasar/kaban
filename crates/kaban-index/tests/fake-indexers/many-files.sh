#!/bin/sh
# Writes $3 files of $2 bytes each under $1 (home, tmp, out and cache in turn), then waits
# (CG4.3 `runner_quota_total_kills`: the total across every child-writable path counts).
i=0
while [ "$i" -lt "$3" ]; do
  for d in home tmp out cache; do
    dd if=/dev/zero of="$1/$d/f$i" bs="$2" count=1 2>/dev/null
  done
  i=$((i + 1))
done
exec sleep 30
