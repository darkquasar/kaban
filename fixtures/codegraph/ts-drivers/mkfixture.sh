#!/usr/bin/env bash
# TypeScript fixture producer (stage1-spec §8.1 `ts-drivers`; stage1-plan CG3.1; OWN-57).
#
# Runs scip-typescript 0.4.0 (Apache-2.0) inside a throwaway `node:22-bookworm` container only:
# the fixture tree `repo/` is bind-mounted read-only, one output directory is mounted
# read-write, `npm install -g @sourcegraph/scip-typescript@0.4.0` happens inside the container
# and the container is removed on exit (`--rm`). Nothing is installed on the host and nothing
# but `index.scip` lands in the repository. The fixture imports nothing, so no `node_modules`.
#
# The sandbox egress proxy is passed through (HTTP(S)_PROXY, NO_PROXY and the proxy CA from
# PROXY_CA_CERT_B64, staged under a host temp directory outside the repository). A refused
# registry fetch (HTTP 403 or an unresolvable registry.npmjs.org) is reported with the exact
# error and exit 3: the TS half is then skipped, not worked around.
#
# Usage: bash fixtures/codegraph/ts-drivers/mkfixture.sh   (IMAGE=<ref> overrides node:22-bookworm)
#        bash fixtures/codegraph/ts-drivers/mkfixture.sh --print-config
#
# Producer configuration (RU1-8): the version installed inside the container is
# `$SCIP_TYPESCRIPT_VERSION`, else the inner pin; PATH is never consulted. `--print-config` prints
# it; a run ends with one `provenance` line carrying the version the installed executable
# actually reported (`scip-typescript --version` inside the container) and the file's sha256. A
# configured version other than the pin exits 4 naming both unless `ACCEPT_NEW_VERSIONS=1`; an
# installed executable reporting another version than configured exits 5.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
IMAGE="${IMAGE:-node:22-bookworm}"
PIN="0.4.0"
VERSION="${SCIP_TYPESCRIPT_VERSION:-$PIN}"

if [ "${1:-}" = "--print-config" ]; then
  echo "tool=scip-typescript version=$VERSION pin=$PIN ran=container:$IMAGE"
  exit 0
fi
if [ "$VERSION" != "$PIN" ] && [ "${ACCEPT_NEW_VERSIONS:-0}" != 1 ]; then
  echo "scip-typescript $VERSION configured; the inner pin is $PIN (OWN-57): pass --accept-new-versions to scripts/dev/regen_fixtures.sh to accept it" >&2
  exit 4
fi

stage="$(mktemp -d "${TMPDIR:-/tmp}/kaban-fixture-ts.XXXXXX")"
trap 'rm -rf "$stage"' EXIT
mkdir -p "$stage/out"
ca_args=()
if [ -n "${PROXY_CA_CERT_B64:-}" ]; then
  printf '%s' "$PROXY_CA_CERT_B64" | base64 -d > "$stage/proxy-ca.crt"
  ca_args=(
    -v "$stage/proxy-ca.crt:/etc/kaban/proxy-ca.crt:ro"
    -e NODE_EXTRA_CA_CERTS=/etc/kaban/proxy-ca.crt
  )
  # NODE_EXTRA_CA_CERTS alone: setting npm's `cafile` as well makes npm 10.9 fail with
  # UNABLE_TO_GET_ISSUER_CERT_LOCALLY (observed; either variable alone works).
fi

# The container's script: install the pinned indexer, index the read-only tree from a
# writable copy (scip-typescript writes nothing into the tree, the copy is belt and braces),
# then hand the index to /out.
read -r -d '' inner <<'EOS' || true
set -eu
npm install -g --no-fund --no-audit "@sourcegraph/scip-typescript@${VERSION}"
echo "scip-typescript-version=$(scip-typescript --version)"
cp -R /fixture /work
cd /work
scip-typescript index --output /out/index.scip
EOS

set +e
docker run --rm --name kaban-fixture-ts \
  -v "$here/repo:/fixture:ro" \
  -v "$stage/out:/out" \
  -e HTTP_PROXY -e HTTPS_PROXY -e NO_PROXY -e http_proxy -e https_proxy -e no_proxy \
  -e VERSION="$VERSION" \
  ${ca_args[@]+"${ca_args[@]}"} \
  "$IMAGE" bash -c "$inner" > "$stage/run.log" 2>&1
rc=$?
set -e
cat "$stage/run.log"
if [ "$rc" -ne 0 ]; then
  if grep -qiE "403|E403|ENOTFOUND|EAI_AGAIN|ECONNREFUSED|registry.npmjs.org" "$stage/run.log"; then
    echo "TS fixture skipped: the registry fetch inside the container was refused (see the lines above)" >&2
    exit 3
  fi
  echo "scip-typescript run failed (exit $rc)" >&2
  exit "$rc"
fi
actual="$(grep -oE '^scip-typescript-version=.*' "$stage/run.log" | head -n1 | cut -d= -f2- | tr -d '[:space:]')"
if [ "$actual" != "$VERSION" ]; then
  echo "the container's scip-typescript reports '${actual:-nothing}', not the configured $VERSION; index.scip not taken" >&2
  exit 5
fi
cp "$stage/out/index.scip" "$here/index.scip"
echo "provenance tool=scip-typescript version=$actual ran=container:$IMAGE fixture=ts-drivers file=index.scip sha256=$(sha256sum "$here/index.scip" | cut -d' ' -f1)"
