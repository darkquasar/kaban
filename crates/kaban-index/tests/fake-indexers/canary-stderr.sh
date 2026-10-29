#!/bin/sh
# Prints a canary on stderr and a plain line on stdout (CG4.3 `runner_log_gated_before_write`):
# the gated log must hold the marker, the canary must reach no file.
echo "indexing started" 
echo "token $1 leaked" >&2
echo "done"
