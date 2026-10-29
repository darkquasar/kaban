#!/usr/bin/env bash
# Run one live hook session in a throwaway container (M4 W2-D1 capture mode, W2-D2 recipes mode; OWN-7).
#
#   scripts/hooks-live/run.sh <claude-code|codex|pi> <session-name> [options]
#     <session-name>                       a label ^[A-Za-z0-9][A-Za-z0-9_.-]{0,63}$: it names the
#                                          container and the result directory on this side only;
#                                          inside /out it appears as sha256 prefix and length (RL-2)
#     --stop-mode none|block|system|both   Stop-type probe emitted once (default none; capture mode)
#     --variant <name>                     host switch: codex "features" adds -c features.hooks=true,
#                                          codex "project-hooks" puts hooks.json under /work/.codex,
#                                          codex "config-toml" registers the hooks as [hooks] tables in
#                                          $CODEX_HOME/config.toml instead of hooks.json (recipes mode)
#     --model <id>                         model override (pi default gpt-5.6-sol); exported to /out only
#                                          as one of sonnet|opus|haiku|gpt-5.6-sol|gpt-6.1-sol|other
#                                          (--variant likewise: hooks-json|config-toml|project-hooks|
#                                          features|other; none when not given)
#     --prompt <text>                      prompt override (default: the fixed capture prompt)
#     --out <dir>                          result root (default /tmp/m4w2d/out)
#     --recipes                            round 2: the REAL recipes from <repo>/hooks run against a
#                                          seeded kaban database (see entrypoint.sh); needs --kaban
#     --kaban <path>                       the static kaban binary to mount (read-only) for --recipes
#
# Mounts: the scratch repo (/work) and the capture directory (/capture) and the container home
# are tmpfs; read-only copies of the sandbox auth state (stage-creds.sh) at ~/.kbn-creds-ro;
# this directory read-only at /harness (scripts under /harness/container, templates under /harness/templates); the result directory at /out
# (receives only reduced artefacts). Hook configs are generated inside the container at start.
# Recipes mode adds: <repo>/hooks read-only at /work/hooks, <repo>/examples/policy.yaml at
# /policy.yaml, the kaban binary at /opt/kaban/kaban and the logging shim container/kaban-shim.sh
# at /usr/local/bin/kaban (the recipes call `kaban` from PATH unchanged; the shim records reduced
# call lines: argv words, exit status, wall ms, byte counts, stdout shape).
# Proxy env comes from the sandbox. Default bridge network. --rm. 10-minute hard timeout.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
host="${1:?host}"; session="${2:?session name}"; shift 2
IMAGE_NAME="${IMAGE_NAME:-kaban-hooks-live:m4}"
repo="$(cd "$here/../.." && pwd)"
stop_mode=none; variant=""; model=""; prompt=""; out_root=/tmp/m4w2d/out; mode=capture; kaban_bin=""
while [ $# -gt 0 ]; do
  case "$1" in
    --stop-mode) stop_mode="$2"; shift 2 ;;
    --recipes) mode=recipes; shift ;;
    --kaban) kaban_bin="$2"; shift 2 ;;
    --variant) variant="$2"; shift 2 ;;
    --model) model="$2"; shift 2 ;;
    --prompt) prompt="$2"; shift 2 ;;
    --out) out_root="$2"; shift 2 ;;
    *) echo "unknown option $1" >&2; exit 2 ;;
  esac
done
case "$host" in claude-code|codex|pi) ;; *) echo "host must be claude-code, codex or pi" >&2; exit 2 ;; esac
case "$stop_mode" in none|block|system|both) ;; *) echo "--stop-mode must be none, block, system or both" >&2; exit 2 ;; esac
# the label names a container and a directory here; the text itself never enters /out (RL-2)
[[ "$session" =~ ^[A-Za-z0-9][A-Za-z0-9_.-]{0,63}$ ]] || { echo "session name must match ^[A-Za-z0-9][A-Za-z0-9_.-]{0,63}$" >&2; exit 2; }
extra_mounts=()
if [ "$mode" = recipes ]; then
  [ -n "$kaban_bin" ] || { echo "--recipes needs --kaban <static kaban binary>" >&2; exit 2; }
  kaban_bin="$(readlink -f "$kaban_bin")"
  [ -x "$kaban_bin" ] || { echo "kaban binary not executable: $kaban_bin" >&2; exit 2; }
  [ -r "$repo/examples/policy.yaml" ] || { echo "missing $repo/examples/policy.yaml" >&2; exit 2; }
  echo "recipes mode: kaban $(sha256sum "$kaban_bin" | cut -c1-16)… ($(stat -c %s "$kaban_bin") bytes), hooks from $repo/hooks"
  extra_mounts=(
    -v "$kaban_bin:/opt/kaban/kaban:ro"
    -v "$here/container/kaban-shim.sh:/usr/local/bin/kaban:ro"
    -v "$repo/hooks:/work/hooks:ro"
    -v "$repo/examples/policy.yaml:/policy.yaml:ro"
  )
fi

name="kbn-hooks-${host}-${session}"
out="$out_root/${host}-${session}"
mkdir -p "$out"
mkdir -p /tmp/m4w2d   # the staging root is not guaranteed to exist (pruned between waves)
stage="$(mktemp -d /tmp/m4w2d/stage.XXXXXX)"
cleanup() {
  docker rm -f "$name" >/dev/null 2>&1 || true
  rm -rf "$stage"
}
trap cleanup EXIT

if [ -n "${KBN_DRY_CMD:-}" ]; then
  # harness self-test (no model call): nothing is staged, the credential directory stays empty
  echo "dry run: no credentials staged"
else
  echo "staging read-only auth copies for $host (names and sizes only):"
  "$here/stage-creds.sh" "$host" "$stage"
fi

env_args=(
  -e HTTP_PROXY -e HTTPS_PROXY -e NO_PROXY -e http_proxy -e https_proxy -e no_proxy
  -e SBX_CRED_OPENAI_MODE
  -e KBN_HOST="$host" -e KBN_SESSION="$session" -e KBN_STOP_MODE="$stop_mode"
  -e KBN_HOST_TIMEOUT=540 -e KBN_MODE="$mode"
)
[ -n "$variant" ] && env_args+=(-e KBN_VARIANT="$variant")
[ -n "$model" ] && env_args+=(-e KBN_MODEL="$model")
[ -n "$prompt" ] && env_args+=(-e KBN_PROMPT="$prompt")
[ -n "${KBN_DRY_CMD:-}" ] && env_args+=(-e KBN_DRY_CMD)   # harness self-test, see entrypoint.sh

echo "docker run ($host), hard timeout 600 s"
set +e
timeout -k 15 600 docker run --rm --name "$name" \
  --tmpfs /home/node:rw,uid=1000,gid=1000,mode=0700,size=512m \
  --tmpfs /work:rw,uid=1000,gid=1000,mode=0755,size=64m \
  --tmpfs /capture:rw,uid=1000,gid=1000,mode=0700,size=64m \
  -v "$stage:/home/node/.kbn-creds-ro:ro" \
  -v "$here:/harness:ro" \
  -v "$out:/out" \
  ${extra_mounts[@]+"${extra_mounts[@]}"} \
  "${env_args[@]}" \
  "$IMAGE_NAME"
rc=$?
set -e
echo "docker run exit=$rc (124 = hard timeout)"
[ -f "$out/verdict.json" ] && { echo "verdict:"; cat "$out/verdict.json"; echo; }
exit "$rc"
