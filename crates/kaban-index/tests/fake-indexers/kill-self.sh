#!/bin/sh
# Dies by SIGKILL before writing anything (PERF-1 `failed_run_reports_how_it_died`: the host's
# OOM killer seen from the runner).
kill -9 $$
