#!/usr/bin/env bash
# Stage read-only copies of the sandbox's existing auth state for one container run
# (M4 W2-D1, OWN-15 credential passover). Called by run.sh; usable alone for inspection.
#
#   stage-creds.sh <host> <stage-dir>
#
# Rules: copy, never print; least material (Claude: only the claudeAiOauth block, not the
# plugin mcpOAuth tokens; Codex: auth.json plus a config.toml reduced to top-level keys and
# [model_providers.*]; pi: models.json, which carries the brokered provider entry). Files are
# 0600 under a 0700 directory that run.sh deletes when the container exits. No interactive
# login is ever run. Prints file names and byte sizes only.
set -euo pipefail
host="${1:?host}"
stage="${2:?stage dir}"
umask 077
mkdir -p "$stage"
chmod 700 "$stage"

case "$host" in
  claude-code)
    src="$HOME/.claude/.credentials.json"
    [ -r "$src" ] || { echo "missing $src" >&2; exit 3; }
    mkdir -p "$stage/claude"
    jq -c '{claudeAiOauth}' "$src" > "$stage/claude/credentials.json"
    jq -e '.claudeAiOauth.accessToken | type == "string"' "$stage/claude/credentials.json" >/dev/null \
      || { echo "claudeAiOauth block absent in $src" >&2; exit 3; }
    if [ -r "$HOME/.claude.json" ]; then
      jq -c '{hasCompletedOnboarding: (.hasCompletedOnboarding // true), lastOnboardingVersion, installMethod, oauthAccount, numStartups: 1}' \
        "$HOME/.claude.json" > "$stage/claude/claude.json"
    fi
    ;;
  codex)
    ch="${CODEX_HOME:-$HOME/.codex}"
    [ -r "$ch/auth.json" ] || { echo "missing $ch/auth.json" >&2; exit 3; }
    mkdir -p "$stage/codex"
    cp "$ch/auth.json" "$stage/codex/auth.json"
    if [ -r "$ch/config.toml" ]; then
      # keep top-level keys and [model_providers.*]; drop [mcp_servers.*], [projects.*] and others
      awk '
        /^\[/ { keep = ($0 ~ /^\[model_providers\./) }
        BEGIN { keep = 1 }
        { if (keep) print }
      ' "$ch/config.toml" > "$stage/codex/config.toml"
    fi
    ;;
  pi)
    src="$HOME/.pi/agent/models.json"
    [ -r "$src" ] || { echo "missing $src" >&2; exit 3; }
    mkdir -p "$stage/pi"
    cp "$src" "$stage/pi/models.json"
    ;;
  *) echo "unknown host $host" >&2; exit 2 ;;
esac
chmod -R u=rwX,go= "$stage"
find "$stage" -type f -printf '%P %s bytes mode %m\n' | sort
