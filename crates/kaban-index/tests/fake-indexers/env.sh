#!/bin/sh
# Prints the environment as NAME=VALUE lines, sorted, then the working directory (CG4.3
# `runner_env_allowlist_only`). Writes nothing anywhere.
env | sort
echo "cwd=$(pwd)"
