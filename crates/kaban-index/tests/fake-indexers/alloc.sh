#!/bin/sh
# Becomes a `dd` whose one read buffer is $1 MiB of resident memory (CG4.3
# `runner_reports_peak_rss`: VmHWM of the direct child is at least that).
exec dd if=/dev/zero of=/dev/null bs="$1"M count=400
