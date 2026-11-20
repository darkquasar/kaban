#!/usr/bin/env bash
# Install the CI-pinned producer outside the checkout; stdout is its absolute path.
set -euo pipefail
destination=${1:?usage: install-scip-go.sh /absolute/tool-directory}
case "$destination" in
  /*) ;;
  *) echo "the tool directory must be absolute" >&2; exit 64 ;;
esac
repository=$(cd "$(dirname "$0")/../.." && pwd -P)
mkdir -p -m 700 "$destination"
destination=$(cd "$destination" && pwd -P)
case "$destination/" in
  "$repository/"*) echo "install scip-go outside the checkout" >&2; exit 64 ;;
esac
case "$(uname -m)" in
  x86_64) architecture=amd64; checksum=4ec4e45ecdbe6f43055ed555254e049f46f28389ade438db3d59679651fa1292 ;;
  aarch64|arm64) architecture=arm64; checksum=81a196420b1ee64a8820a59287f4bb9cab1849bc3d4e13c97a3130f75d8540fd ;;
  *) echo "unsupported scip-go architecture" >&2; exit 69 ;;
esac
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT
archive="$scratch/scip-go.tar.gz"
curl --fail --silent --show-error --location --retry 3 \
  "https://github.com/scip-code/scip-go/releases/download/v0.2.9/scip-go-linux-${architecture}.tar.gz" \
  --output "$archive"
printf '%s  %s\n' "$checksum" "$archive" | sha256sum --check >&2
tar -xzf "$archive" -C "$scratch" scip-go
install -m 755 "$scratch/scip-go" "$destination/scip-go"
version=$("$destination/scip-go" --version)
if [ "$version" != 0.2.9 ]; then
  echo "expected scip-go 0.2.9, received $version" >&2
  exit 1
fi
echo "scip-go $version installed" >&2
printf '%s\n' "$destination/scip-go"
