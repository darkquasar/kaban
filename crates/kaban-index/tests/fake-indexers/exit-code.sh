#!/bin/sh
# Writes $2 bytes into "$3/out/index.scip" (the scratch out directory) and exits with $1
# (CG4.3 `runner_raw_files_always_deleted`: the success and failure cases).
dd if=/dev/zero of="$3/out/index.scip" bs="$2" count=1 2>/dev/null
exit "$1"
