#!/usr/bin/env bash
# Build the live hook capture image (M4 W2-D1, OWN-15 grant: one pinned base image, three
# exact CLI versions, inside the image only). Records the base digest, the image digest and
# size in $KBN_HOOKS_LOG_DIR (default ./build-logs, git-ignored by the caller) and prints them.
#
#   scripts/hooks-live/build.sh            # pulls node:22-bookworm once, pins its digest
#   BASE_IMAGE=node:22-bookworm@sha256:... scripts/hooks-live/build.sh   # reuse a pin
#
# Proxy: HTTP_PROXY/HTTPS_PROXY/NO_PROXY and PROXY_CA_CERT_B64 are taken from the sandbox
# environment. The CA is a public certificate; no credential enters the build.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
IMAGE_NAME="${IMAGE_NAME:-kaban-hooks-live:m4}"
BASE_IMAGE="${BASE_IMAGE:-node:22-bookworm}"
LOG_DIR="${KBN_HOOKS_LOG_DIR:-/tmp/m4w2d/build-logs}"
mkdir -p "$LOG_DIR"

: "${PROXY_CA_CERT_B64:?PROXY_CA_CERT_B64 must be set (sandbox proxy CA)}"

# 1. pull the base once and pin by digest
if [[ "$BASE_IMAGE" != *@sha256:* ]]; then
  echo "pulling $BASE_IMAGE"
  docker pull "$BASE_IMAGE" 2>&1 | tee "$LOG_DIR/pull.log"
  digest="$(docker inspect --format '{{index .RepoDigests 0}}' "$BASE_IMAGE")"
  BASE_IMAGE="$digest"
fi
echo "base image: $BASE_IMAGE" | tee "$LOG_DIR/base-digest.txt"

# 2. build; the proxy vars are docker's predefined build-args, exported to every RUN
docker build \
  --build-arg BASE_IMAGE="$BASE_IMAGE" \
  --build-arg HTTP_PROXY="${HTTP_PROXY:-}" \
  --build-arg HTTPS_PROXY="${HTTPS_PROXY:-}" \
  --build-arg NO_PROXY="${NO_PROXY:-}" \
  --build-arg http_proxy="${http_proxy:-${HTTP_PROXY:-}}" \
  --build-arg https_proxy="${https_proxy:-${HTTPS_PROXY:-}}" \
  --build-arg no_proxy="${no_proxy:-${NO_PROXY:-}}" \
  --build-arg PROXY_CA_CERT_B64="$PROXY_CA_CERT_B64" \
  -t "$IMAGE_NAME" \
  -f "$here/Dockerfile" "$here" 2>&1 | tee "$LOG_DIR/build.log"

# 3. record image id and size
{
  echo "image: $IMAGE_NAME"
  echo "base:  $BASE_IMAGE"
  echo "id:    $(docker inspect --format '{{.Id}}' "$IMAGE_NAME")"
  echo "size:  $(docker image inspect --format '{{.Size}}' "$IMAGE_NAME") bytes"
  echo "built: $(date -u +%FT%TZ)"
} | tee "$LOG_DIR/image.txt"
