#!/bin/sh
set -eu
# Runs the locally built bin/kaban with a repo-local writable home (gitignored).
repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd -P)
if [ "${KABAN_HOME+x}" = x ]; then
  [ -n "$KABAN_HOME" ] || { printf '%s\n' 'KABAN_HOME must not be empty.' >&2; exit 1; }
  printf '%s\n' 'Using explicit KABAN_HOME configuration.' >&2
else
  KABAN_HOME=$repo_root/runtime-home
  export KABAN_HOME
  umask 077
  mkdir -p "$KABAN_HOME"
  if [ ! -e "$KABAN_HOME/config.yaml" ] && [ ! -L "$KABAN_HOME/config.yaml" ]; then
    cp "$repo_root/examples/local-runtime-config.yaml" "$KABAN_HOME/config.yaml"
  fi
fi
if [ "${KABAN_DB+x}" = x ]; then
  [ -n "$KABAN_DB" ] || { printf '%s\n' 'KABAN_DB must not be empty.' >&2; exit 1; }
  printf '%s\n' 'Using explicit KABAN_DB.' >&2
else
  KABAN_DB=$KABAN_HOME/kaban.db
  export KABAN_DB
fi
exec "$repo_root/bin/kaban" "$@"
