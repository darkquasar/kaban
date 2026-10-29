#!/bin/sh
# Writes one file into "$1/out" and hangs (CG4.3 `runner_raw_files_always_deleted`: timeout).
echo raw > "$1/out/partial"
exec sleep 30
