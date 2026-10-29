#!/bin/sh
# Forks a grandchild holding the stdout pipe, writes its pid to $1, then hangs (CG4.3
# `runner_kills_group_on_deadline`): both must be gone after the group kill.
sleep 30 &
echo $! > "$1"
exec sleep 30
